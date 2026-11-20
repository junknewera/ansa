"""Local, read-only file inspector. The selected text stays in the desktop UI."""

import difflib
import json
import os
from pathlib import Path
import selectors
import stat
import subprocess
import time


FILE_LIMIT = 256 * 1024
LINE_LIMIT = 4096
GIT_TIMEOUT = 3.0


def _read_source(project, relative):
    """Walk below a pinned Linux directory without opening a device or FIFO."""
    if not relative.parts or ".." in relative.parts:
        raise ValueError("Choose a regular file inside the project.")
    parent = os.open(project, os.O_PATH | os.O_DIRECTORY)
    try:
        for part in relative.parts[:-1]:
            child = os.open(part, os.O_PATH | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=parent)
            os.close(parent)
            parent = child
        descriptor = os.open(relative.name, os.O_PATH | os.O_NOFOLLOW, dir_fd=parent)
        try:
            info = os.fstat(descriptor)
            if not stat.S_ISREG(info.st_mode):
                raise ValueError("Choose a regular file. Links and special files are unsupported.")
            if info.st_size > FILE_LIMIT:
                raise ValueError("Selected file exceeds the 256 KiB preview limit.")
            with open(f"/proc/self/fd/{descriptor}", "rb") as stream:
                data = stream.read(FILE_LIMIT + 1)
                if len(data) > FILE_LIMIT:
                    raise ValueError("Selected file exceeds the 256 KiB preview limit.")
                after = os.fstat(stream.fileno())
                if (info.st_size, info.st_mtime_ns, info.st_ctime_ns) != (
                    after.st_size, after.st_mtime_ns, after.st_ctime_ns
                ):
                    raise ValueError("The file changed while opening. Select it again.")
            return _text(data, "Selected file"), info.st_mode
        finally:
            os.close(descriptor)
    finally:
        os.close(parent)


def _text(data, label):
    if b"\0" in data:
        raise ValueError(f"{label} contains binary data.")
    try:
        return data.decode("utf-8")
    except UnicodeDecodeError as error:
        raise ValueError(f"{label} is not UTF-8 text.") from error


def _git(project, deadline, *args, limit=16384):
    """All Git plumbing is local. Bound both output pipes and the whole lookup."""
    command = ["git", "--no-pager", "--no-optional-locks", "--literal-pathspecs",
               "-c", "core.fsmonitor=false", "-c", "core.hooksPath=/dev/null", *args]
    env = {"PATH": os.environ.get("PATH", os.defpath), "LC_ALL": "C",
           "GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": "/dev/null",
           "GIT_NO_LAZY_FETCH": "1", "GIT_NO_REPLACE_OBJECTS": "1",
           "GIT_ALLOW_PROTOCOL": "", "GIT_TERMINAL_PROMPT": "0"}
    try:
        proc = subprocess.Popen(command, cwd=project, env=env, stdin=subprocess.DEVNULL,
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    except FileNotFoundError as error:
        raise ValueError("Git executable is unavailable.") from error
    with proc, selectors.DefaultSelector() as selector:
        stdout, stderr = bytearray(), bytearray()
        selector.register(proc.stdout, selectors.EVENT_READ, (stdout, limit))
        selector.register(proc.stderr, selectors.EVENT_READ, (stderr, 8192))
        try:
            while selector.get_map():
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise TimeoutError
                for key, _ in selector.select(remaining):
                    result, maximum = key.data
                    block = os.read(key.fd, 8192)
                    if not block:
                        selector.unregister(key.fileobj)
                    elif len(result) + len(block) > maximum:
                        raise ValueError("Git output exceeds the preview limit.")
                    else:
                        result.extend(block)
            code = proc.wait(timeout=max(0, deadline - time.monotonic()))
            return code, bytes(stdout), bytes(stderr)
        except (TimeoutError, subprocess.TimeoutExpired) as error:
            raise ValueError("Git comparison timed out.") from error
        finally:
            if proc.poll() is None:
                proc.kill()


def _lines(text):
    # Git divides lines on LF, even when a file contains CRLF or Unicode separators.
    count = text.count("\n") + int(bool(text) and not text.endswith("\n"))
    if count > LINE_LIMIT:
        raise ValueError("Comparison exceeds the 4,096-line diff limit.")
    parts = text.split("\n")
    return [line + "\n" for line in parts[:-1]] + ([parts[-1]] if parts[-1] else [])


def _patch(before, source, label, name):
    def header(path):
        if any(ord(char) < 32 or char in ('"', "\\") for char in path):
            return json.dumps(path, ensure_ascii=False)
        return path

    diff = difflib.unified_diff(_lines(before), _lines(source), header(label), header("worktree/" + name))
    rows = []
    for row in diff:
        rows.append(row)
        if not row.endswith("\n"):
            rows.append("\n\\ No newline at end of file\n")
    return "".join(rows)


def _comparison(project, name, source, mode):
    deadline = time.monotonic() + GIT_TIMEOUT

    def git(*args, limit=16384):
        code, out, err = _git(project, deadline, *args, limit=limit)
        if code:
            if b"not a git repository" in err:
                raise ValueError("No Git repository for this project.")
            raise ValueError(f"Git could not read the local {args[0]} result (exit {code}).")
        return out

    if git("rev-parse", "--is-inside-work-tree").strip() != b"true":
        raise ValueError("No working tree for this project.")
    code, revision, _ = _git(project, deadline, "rev-parse", "--verify", "--quiet", "HEAD")
    if code == 1:
        raise ValueError("This repository has no HEAD commit.")
    if code:
        raise ValueError("Git could not resolve HEAD.")
    head = revision.strip().decode("ascii")
    entries = git("ls-tree", "-z", head, "--", name)
    if not entries:
        indexed = os.fsencode(name) in git("ls-files", "-z", "--", name).split(b"\0")
        status = "Staged addition" if indexed else "Untracked file"
        patch = _patch("", source, "/dev/null", name)
        return status, patch or "Empty new file.", f"{status}. The file is absent from HEAD {head[:12]}."

    fields, _, entry_name = entries.rstrip(b"\0").partition(b"\t")
    old_mode, kind, oid = fields.split()
    if entry_name != os.fsencode(name) or kind != b"blob" or old_mode not in (b"100644", b"100755"):
        raise ValueError("The entry at HEAD is not a regular file.")
    blob = oid.decode("ascii")
    if int(git("cat-file", "-s", blob)) > FILE_LIMIT:
        raise ValueError("HEAD blob exceeds the 256 KiB preview limit.")
    before = _text(git("cat-file", "blob", blob, limit=FILE_LIMIT), "HEAD blob")
    new_mode = "100755" if mode & stat.S_IXUSR else "100644"
    previous_mode = old_mode.decode("ascii")
    change = (f"old mode {previous_mode}\nnew mode {new_mode}\n"
              if new_mode != previous_mode else "")
    if source != before:
        change += _patch(before, source, "HEAD/" + name, name)
    evidence = f"HEAD {head[:12]} · blob {blob[:12]} → selected working file. Index is not the comparison base."
    return "HEAD → working file", change or "Working file matches HEAD.", evidence


def load_document(project: Path, path: Path) -> dict:
    project, path = Path(project).absolute(), Path(path)
    try:
        relative = path.relative_to(project) if path.is_absolute() else path
    except ValueError as error:
        raise ValueError("Choose a file inside the current project.") from error
    try:
        source, mode = _read_source(project, relative)
    except OSError as error:
        raise ValueError(f"Cannot open this regular project file: {error.strerror}.") from error
    name = relative.as_posix()
    try:
        comparison, diff, evidence = _comparison(project, name, source, mode)
    except (OSError, ValueError) as error:
        comparison, diff, evidence = "Diff unavailable", str(error), f"Local file · {name}. {error}"
    return {"name": name, "source": source, "diff": diff, "evidence": evidence, "comparison": comparison}
