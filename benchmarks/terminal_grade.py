"""Independent verifier for the pinned public terminal tasks (0/1/2 exit contract)."""

from collections import Counter
import hashlib
import json
import math
import os
from pathlib import Path
import re
import stat
import subprocess
import sys

BASE = "d7d3e4ba9350f634d92d39ace2b471433ee57d50"
OPERATOR = Path(__file__).resolve().parent


def cosine(left, right):
    # Default sklearn TfidfVectorizer: lowercase words of 2+ characters,
    # smoothed IDF, raw term frequency and L2 normalization across two docs.
    counts = [Counter(re.findall(r"(?u)\b\w\w+\b", s.lower())) for s in (left, right)]
    vocab = counts[0].keys() | counts[1].keys()
    if not vocab:
        return float(left == right)
    weights = {key: 1 + math.log(3 / (1 + sum(key in c for c in counts))) for key in vocab}
    a, b = [{k: n * weights[k] for k, n in c.items()} for c in counts]
    norm = math.sqrt(sum(v * v for v in a.values()) * sum(v * v for v in b.values()))
    return sum(v * b.get(k, 0) for k, v in a.items()) / norm if norm else 0.0


def git(work, *args):
    return subprocess.check_output(["git", "-c", "core.hooksPath=/dev/null", "-C", str(work), *args],
                                   text=True).strip()


def intercode_files(work):
    """Snapshot working files independently of HEAD, the index and ignore rules."""
    entries = {}
    for directory, dirs, files in os.walk(work, followlinks=False):
        parent = Path(directory)
        if parent == work and ".git" in dirs:
            dirs.remove(".git")
        for name in files + [d for d in dirs if (parent / d).is_symlink()]:
            path = parent / name
            mode = path.lstat().st_mode
            entry = {"mode": stat.S_IFMT(mode), "executable": bool(mode & stat.S_IXUSR)}
            if stat.S_ISREG(mode):
                with path.open("rb") as stream:
                    entry["sha256"] = hashlib.file_digest(stream, "sha256").hexdigest()
            elif stat.S_ISLNK(mode):
                entry["target"] = os.readlink(path)
            entries[str(path.relative_to(work))] = entry
    return entries


def intercode(work, answer):
    observation = json.loads(answer.read_text())["answers"][-1]
    expected = (OPERATOR / "expected.txt").read_text()
    # The gold command is read-only. Its diff is empty, hence the upstream
    # shared-file score is always 0.33; each extra status entry costs diff reward.
    baseline = json.loads((OPERATOR / "baseline.json").read_text())
    actual = intercode_files(work)
    changes = sorted(p for p in baseline.keys() | actual.keys() if baseline.get(p) != actual.get(p))
    reward = round(0.01 + round(0.33 * (1 - math.erf(len(changes))), 2)
                   + 0.33 + round(0.33 * cosine(observation, expected), 2), 2)
    print(json.dumps({"intercode_reward": reward, "file_changes": changes}))
    return reward == 1.0


def grade(work, answer):
    task = (OPERATOR / "task-id").read_text()
    if task == "intercode-bash-fs1-48":
        return intercode(work, answer)
    original = (OPERATOR / "author_tests.py").read_text()
    replacements = {"/app/answer.txt": work / "answer.txt", "/app/result.txt": work / "result.txt",
                    "/app/resources/patch_files/about.md": OPERATOR / "about.md",
                    "/app/resources/patch_files/default.html": OPERATOR / "default.html",
                    "/app/personal-site/_includes/about.md": work / "_includes/about.md",
                    "/app/personal-site/_layouts/default.html": work / "_layouts/default.html"}
    for path, local in replacements.items():
        original = original.replace(f'"{path}"', repr(str(local)))
    namespace = {}
    exec(compile(original, str(OPERATOR / "author_tests.py"), "exec"), namespace)
    tests = [function for name, function in namespace.items() if name.startswith("test_")]
    if not tests:
        raise ValueError("upstream tests are missing")
    try:
        for test in tests:
            test()
        if task == "count-dataset-tokens":
            # Upstream accepts a substring; enforce its integer-only instruction.
            return (work / "answer.txt").read_text().strip() == "79586"
        if task == "fix-git":
            if git(work, "branch", "--show-current") != "master":
                return False
            lost = (OPERATOR / "lost-commit").read_text()
            # The author's tests inspect working files; check both committed
            # paths too. A merge followed by a revert must not pass.
            for name, path in (("about.md", "_includes/about.md"), ("default.html", "_layouts/default.html")):
                blob = subprocess.run(["git", "-C", str(work), "show", "HEAD:" + path],
                                      capture_output=True)
                if blob.returncode or blob.stdout.strip() != (OPERATOR / name).read_bytes().strip():
                    return False
            # Require both original tips in the submitted master history.
            # They diverge, so both can be reachable only after a real merge.
            return all(subprocess.run(["git", "-C", str(work), "merge-base", "--is-ancestor", rev, "HEAD"])
                       .returncode == 0 for rev in (BASE, lost))
        return True
    except (AssertionError, FileNotFoundError, IsADirectoryError, UnicodeError) as error:
        print(error, file=sys.stderr)
        return False


if __name__ == "__main__":
    try:
        passed = grade(Path(sys.argv[1]), Path(sys.argv[2]))
    except Exception as error:
        print(f"verifier error: {error}", file=sys.stderr)
        sys.exit(2)
    sys.exit(0 if passed else 1)
