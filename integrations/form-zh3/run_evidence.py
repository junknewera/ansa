"""Read a user-selected frozen reviewer view. This is an archive, never a live run."""

from contextlib import ExitStack
import json
import os
from pathlib import Path
import re
import shlex
import stat


def _directory(stack, name, parent=None):
    flags = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC
    fd = os.open(name, flags, dir_fd=parent)
    stack.callback(os.close, fd)
    info = os.fstat(fd)
    if info.st_uid != os.geteuid() or info.st_mode & 0o077:
        raise ValueError("The native review directory must be private to its owner")
    return fd


def _text(directory, name, limit):
    fd = os.open(name, os.O_PATH | os.O_NOFOLLOW | os.O_CLOEXEC, dir_fd=directory)
    try:
        info = os.fstat(fd)
        if (not stat.S_ISREG(info.st_mode) or info.st_nlink != 1 or info.st_uid != os.geteuid()
                or info.st_mode & 0o077 or not 0 < info.st_size <= limit):
            raise ValueError(f"Invalid native review file, permissions or size: {name}")
        with open(f"/proc/self/fd/{fd}", "rb") as source:
            data = source.read(limit + 1)
        after = os.fstat(fd)
        if len(data) != info.st_size or after.st_ctime_ns != info.st_ctime_ns:
            raise ValueError(f"Review file changed during read: {name}")
    finally:
        os.close(fd)
    return data.decode("utf-8")


def _string(value):
    if not isinstance(value, str) or not value.strip():
        raise ValueError("Missing text in the frozen review view")
    return value


def _snapshot(path, expected_parent):
    view = path.parent
    plus = view.parent.name == "plus-review-views"
    run = view.parent.parent if plus else view.parent
    parent = run.parent
    view_pattern = r"candidate-[0-2]" if plus else r"verification-[0-2]"
    if (path.name != "approved-plan.json" or not re.fullmatch(view_pattern, view.name)
            or parent.parent.name != "orchestration"
            or not re.fullmatch(r"[A-Za-z0-9_-]{1,64}", parent.name)
            or not re.fullmatch(r"[0-9a-f]{32}", run.name)):
        raise ValueError("Choose approved-plan.json in a frozen ANSA review directory")
    if expected_parent is not None and expected_parent != parent.name:
        raise ValueError("This review belongs to another parent session")

    with ExitStack() as stack:
        root_fd = _directory(stack, parent.parent)
        parent_fd = _directory(stack, parent.name, root_fd)
        run_fd = _directory(stack, run.name, parent_fd)
        ledger_fd = _directory(stack, "ledger", run_fd)
        views_fd = _directory(stack, "plus-review-views", run_fd) if plus else run_fd
        view_fd = _directory(stack, view.name, views_fd)
        header = json.loads(_text(ledger_fd, "init.json", 16 * 1024 * 1024))
        plan_text = _text(view_fd, "approved-plan.json", 64 * 1024)
        if (header["version"] != 1 or header["identity"] != {"parent_session": parent.name, "run": run.name}
                or header["plan"] != plan_text):
            raise ValueError("Run identity or frozen plan differs from the run header")
        proposal = json.loads(plan_text)
        if proposal["schema_version"] != 1 or not isinstance(proposal["steps"], list):
            raise ValueError("Unsupported frozen plan")
        diff = _text(view_fd, "ansa-candidate.diff", 2 * 1024 * 1024)
        review = json.loads(_text(views_fd, "approved-request.json", 16 * 1024 * 1024)) if plus else {}

    plan_lines = []
    step_ids = set()
    for step in proposal["steps"]:
        name = _string(step["id"])
        step_ids.add(name)
        plan_lines.append(f'{name} · {_string(step["access"])} · {_string(step["task"])}')
    checks = []
    for check in proposal.get("command_checks", []):
        argv = check["argv"]
        if not isinstance(argv, list) or not argv or not all(isinstance(arg, str) for arg in argv):
            raise ValueError("Invalid planned check")
        checks.append((shlex.join(argv), "specified only"))
    for check in proposal.get("checks", []):
        checks.append((f'{_string(check["path"])} contains {_string(check["contains"])}', "specified only"))
    for check in proposal.get("manual_checks", []):
        checks.append((_string(check), "manual · specified only"))

    agents = []
    clarifications = []
    if plus:
        for step, item in review["approved_task_results"].items():
            if step not in step_ids:
                raise ValueError("Step result is outside the selected plan")
            result = item["result"]
            agents.append((step, f'{_string(result["status"])} · {_string(result["summary"])}'))
        if not isinstance(review["user_clarifications"], list):
            raise ValueError("Unexpected review clarification list")
        clarifications = [(_string(text), "in reviewer input") for text in review["user_clarifications"]]

    label = f"{parent.name} / {run.name} / {view.name}"
    origin = f"Saved reviewer copy · {label}"
    title = _string(review.get("user_request") or proposal["summary"])
    return {
        "demo": False, "phase": "archive", "project": parent.name,
        "title": title, "mode": "Orchestrate+" if plus else "Orchestrate",
        "step": "Frozen review input", "plan": plan_lines, "agents": agents,
        "sessions": [parent.name], "checks": checks, "clarifications": clarifications,
        "messages": [origin, _string(proposal["summary"])],
        "documents": [
            {"name": "ansa-candidate.diff", "source": diff, "diff": diff, "evidence": origin},
            {"name": "approved-plan.json", "source": json.dumps(proposal, ensure_ascii=False, indent=2),
             "diff": "", "evidence": origin},
        ],
        "release": "Release status outside this snapshot", "can_advance": False,
        "can_clarify": False, "pr_eligible": False,
    }


def open_frozen_plan(path, expected_parent=None):
    """Load the saved input to native candidate review, without executing any command."""
    try:
        return _snapshot(Path(path).absolute(), expected_parent)
    except (KeyError, TypeError, AttributeError, RecursionError) as error:
        raise ValueError("Incomplete or malformed frozen reviewer view") from error
