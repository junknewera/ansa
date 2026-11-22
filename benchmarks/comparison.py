"""Frozen one-factor experiments and journal-backed paired reporting."""

from collections import Counter, defaultdict
import os
from pathlib import Path
import re
import signal
import subprocess
import tempfile
import time

from native_eval import label, require


def validate(comparison, arms, tasks, rows):
    require(isinstance(comparison, dict)
            and {"left", "right", "factor"} <= comparison.keys() <= {"left", "right", "factor", "stage"},
            "invalid comparison fields")
    left, right = comparison["left"], comparison["right"]
    require(left != right and set(arms) == {left, right}, "comparison requires two named arms")
    factor = comparison["factor"]
    fields = ("binary", "model", "effort", "mode", "roles", "guidance", "mcp_config")
    require(factor in fields, "invalid comparison factor")
    require(comparison.get("stage") in (None, "planner", "reviewer", "compaction"), "invalid required stage")

    def identity(arm, key):
        if key in ("binary", "guidance", "mcp_config"):
            return arm.get(f"{key}_sha256")
        return arm.get(key)

    differences = {key for key in (*fields, "route", "auth_dir")
                   if identity(arms[left], key) != identity(arms[right], key)}
    require(differences == {factor}, f"expected {factor} alone to differ; got {sorted(differences)}")

    families = {}
    paired_splits = set()
    supported = defaultdict(set)
    for row in rows:
        if "unsupported" not in row:
            supported[row["task"]].add(row["arm"])
    for task in tasks.values():
        require(label(task.get("category")) and label(task.get("family")), "expected task category and family")
        split = task.get("split")
        require(split in ("development", "holdout"), "expected development or holdout split")
        if task["id"] in supported:
            require(task.get("oracle_fixture"), "strict comparison requires a positive/negative oracle fixture")
        require(families.setdefault(task["family"], split) == split, "task family leaks across splits")
        if supported.get(task["id"]) == {left, right}:
            paired_splits.add(split)
    require(paired_splits == {"development", "holdout"}, "each split needs a supported task compatible with both arms")


def stages(traces):
    evidence = defaultdict(list)
    for session, events in traces.items():
        initial = events[0].get("configuration", {}).get("epoch", {}).get("exact_state", {})
        parent = initial.get("orchestration_parent")
        observed = set()
        for event in events:
            kind = event["kind"]
            if kind == "compacted":
                observed.add("compaction")
            elif kind == "response":
                state = event["response"]["request"]["epoch"]["exact_state"]
                if state.get("purpose") == "orchestration_plan":
                    observed.add("planner")
                # Review children open a runtime-owned frozen view. A read-only
                # ordinary helper, or an unfinished child init, is insufficient.
                cwd = Path(initial.get("settings", {}).get("cwd", ""))
                if parent in traces and (
                        re.fullmatch(r"[a-f0-9]{64}", initial.get("review_skill_sha256", ""))
                        or (re.fullmatch(r"verification-[012]", cwd.name)
                            and cwd.parent.parent.name == parent
                            and cwd.parent.parent.parent.name == "orchestration")):
                    observed.add("reviewer")
        for stage in observed:
            evidence[stage].append(session)
    return dict(evidence)


def run_control_command(argv, cwd, env, deadline, log):
    remaining = deadline - time.monotonic()
    require(remaining > 0, "oracle control budget exhausted")
    with subprocess.Popen(argv, cwd=cwd, env=env, stdin=subprocess.DEVNULL, stdout=log,
                          stderr=subprocess.STDOUT, start_new_session=True) as proc:
        try:
            return proc.wait(timeout=remaining)
        except (KeyboardInterrupt, subprocess.TimeoutExpired):
            try:
                os.killpg(proc.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            raise


def calibrate(task, output, budget, end, digest, environment):
    """Exercise the same executable and frozen verifier dependencies as grading."""
    output.mkdir()
    receipts = {}
    for control, expected in (("pass", 0), ("fail", 1)):
        require(digest(task["oracle_fixture"]) == task["oracle_fixture_sha256"], "oracle fixture changed")
        with tempfile.TemporaryDirectory(prefix="ansa-oracle-") as temporary:
            root = Path(temporary)
            home = root / "home"
            workspace = home / "workspace"
            workspace.mkdir(parents=True)
            (root / "tmp").mkdir()
            env = environment(root, home)
            answer = root / "answer.json"
            deadline = min(end, time.monotonic() + budget["verifier_seconds"])
            with (output / f"{control}.log").open("wb") as log:
                code = run_control_command([task["oracle_fixture"], control, workspace, answer],
                                           workspace, env, deadline, log)
                require(code == 0 and answer.is_file(), f"{task['id']} {control} fixture failed")
                require(digest(task["verifier"]) == task["verifier_sha256"] and
                        all(digest(Path(path)) == sha for path, sha in task["verifier_inputs"].items()),
                        "oracle verifier or dependencies changed")
                code = run_control_command([task["verifier"], workspace, answer], workspace, env, deadline, log)
                require(code == expected, f"{task['id']} {control} control returned {code}, expected {expected}")
                receipts[control] = code
    return receipts


def report(spec, rows, arms, tasks):
    left, right = spec["left"], spec["right"]
    by_id = {(row["task"], row["repeat"], row["arm"]): row for row in rows}
    pairs = []
    groups = defaultdict(Counter)
    coverage = {}
    metrics = {}
    stage = spec.get("stage")
    graded = {"pass", "fail", "failed", "timeout"}
    for task, repeat in sorted({key[:2] for key in by_id}):
        a, b = (by_id.get((task, repeat, arm)) for arm in (left, right))
        if a is None or b is None:
            outcome = "unpaired"
        elif a["status"] not in graded or b["status"] not in graded:
            outcome = "ungraded"
        elif stage and any(not row.get("usage", {}).get("stages", {}).get(stage) for row in (a, b)):
            outcome = "stage_missing"
        else:
            outcome = {(True, True): "both_pass", (True, False): "left_only",
                       (False, True): "right_only", (False, False): "both_fail"}[
                           a["status"] == "pass", b["status"] == "pass"]
        category = tasks[task]["category"]
        groups[category][outcome] += 1
        pairs.append({"task": task, "repeat": repeat, "outcome": outcome})
    for arm in (left, right):
        runs = [row for row in rows if row["arm"] == arm]
        coverage[arm] = {"mode": arms[arm]["mode"], "model": arms[arm]["model"],
                         "scheduled_tasks": sorted({r["task"] for r in runs}),
                         "statuses": dict(Counter(r["status"] for r in runs))}
        totals = {}
        for name in ("wall_seconds", "admitted_responses", "dispatched_tools", "reported_tokens"):
            values = [row.get(name) if name == "wall_seconds" else row.get("usage", {}).get(name)
                      for row in runs]
            present = [value for value in values if value is not None]
            totals[name] = {"sum": round(sum(present), 3) if present else None,
                            "trials_reporting": len(present)}
        metrics[arm] = totals
    return {"by_category": groups, "pairs": pairs, "coverage": coverage, "metrics": metrics}
