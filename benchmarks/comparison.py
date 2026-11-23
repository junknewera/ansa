"""Frozen one-factor experiments and journal-backed paired reporting."""

from collections import Counter, defaultdict
import os
import json
import math
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
    if factor == "mode" and differences == {"mode", "roles"}:
        plus = next((arm for arm in arms.values() if arm["mode"] in ("orchestrate+", "propose+")), None)
        if plus and any(arm["mode"] + "+" == plus["mode"] for arm in arms.values()):
            require(all(role == {"model": plus["model"], "effort": plus["effort"]}
                        for role in plus["roles"].values()),
                    "Orchestrate+ roles must match the shared model and effort")
            differences.remove("roles")
    require(differences == {factor}, f"expected {factor} alone to differ; got {sorted(differences)}")

    if factor not in ("binary", "model"):
        require(arms[left].get("image_input", False) == arms[right].get("image_input", False),
                "identical builds and models must declare identical image capability")

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


def grade_plan(plan, requirements, start_available):
    paths = {path.removeprefix("./").rstrip("/") for step in plan["steps"]
             if step["access"] == "write" for path in step["paths"]}
    required = set(requirements["paths"])
    checks = [check["argv"] for check in plan.get("command_checks", [])]
    command = requirements.get("check")
    def runs_check(argv):
        if command is None:
            return bool(argv)
        executable = argv[0].removeprefix("./") if argv else ""
        return executable == command or (
            Path(executable).name in ("sh", "bash") and len(argv) > 1
            and argv[1].removeprefix("./") == command)

    checks_ok = any(runs_check(argv) for argv in checks)
    criteria = {"expected_write_paths": paths == required,
                "expected_check" if command else "command_check_present": checks_ok,
                "start_available": start_available}
    return {"status": "pass" if all(criteria.values()) else "fail", "criteria": criteria}


def trace_metrics(path, traces):
    require(path.stat().st_size <= 4 * 1024 * 1024, "benchmark timing evidence exceeds limit")
    events = [json.loads(line) for line in path.read_text().splitlines()]
    generations = {row["id"]: row for row in events if row["kind"] == "generation"}
    http = [row for row in events if row["kind"] == "http"]
    sent = [row for row in events if row["kind"] == "http_sent"]
    tools = [row for row in events if row["kind"] == "tool"]
    generation_rows = [row for row in events if row["kind"] == "generation"]
    require(len(generations) == len(generation_rows) and
            all(row["id"] in generations for row in http + sent), "unmatched provider timing")
    require(all(math.isfinite(row["seconds"]) and row["seconds"] >= 0
                for row in http + tools + generation_rows), "invalid timing duration")
    operations = {row["result"]["operation"]: session for session, rows in traces.items()
                  for row in rows if row["kind"] == "outcome" and row["result"]["dispatched"]}
    def seconds(rows):
        return round(sum(row["seconds"] for row in rows), 3)

    def requests(attempts, transmitted):
        retries = Counter(row["id"] for row in attempts)
        return {"http_attempts": len(attempts), "http_sent": len(transmitted),
                "http_retries": sum(value - 1 for value in retries.values()),
                "http_errors": sum(row["error"] is not None for row in attempts)}

    roles = defaultdict(list)
    for session, rows in traces.items():
        state = rows[0].get("configuration", {}).get("epoch", {}).get("exact_state", {})
        accepted = next((row for row in rows if row["kind"] == "accepted"), {})
        assignment = accepted.get("text", "").rsplit("\n\nШаг ", 1)
        if state.get("orchestration_parent") and len(assignment) == 2 and assignment[-1].startswith("plus-"):
            roles[plus_role(assignment[-1].split("\n", 1)[0])].append(session)
        elif state.get("orchestration_parent"):
            roles["worker"].append(session)
        else:
            roles["lead"].append(session)

    by_role = {}
    for role, sessions in sorted(roles.items()):
        ids = {row["id"] for row in generation_rows if row["session"] in sessions}
        usage = [row["response"].get("reported_usage_v1") for session in sessions
                 for row in traces[session] if row["kind"] in ("response", "side_reply")]
        usage += [native["summary_response"].get("reported_usage_v1")
                  for session in sessions for row in traces[session] if row["kind"] == "compacted"
                  for native in (row["compaction"]["native"],) if "summary_response" in native]
        by_role[role] = {"sessions": len(sessions), "admitted_responses": len(usage),
                         "reported_tokens": sum(u["input_tokens"] + u["output_tokens"] for u in usage)
                         if usage and all(usage) else None,
                         "provider_seconds": seconds([generations[id] for id in ids]),
                         "tool_seconds": seconds([row for row in tools
                                                  if operations.get(row["operation"]) in sessions])}
        if http:
            by_role[role].update(requests([row for row in http if row["id"] in ids],
                                          [row for row in sent if row["id"] in ids]))
    result = {"provider_seconds": seconds(generation_rows), "tool_seconds": seconds(tools),
              "generation_errors": sum(row["error"] is not None for row in generation_rows),
              "by_role": by_role}
    if http:
        result.update(requests(http, sent))
    return result


def plus_role(step):
    if step.startswith("plus-review-"):
        if step == "plus-review-report-repair" or re.search(r"-repair-\d+$", step):
            return "worker"
        if re.search(r"-coordinate-\d+$", step):
            return "coordinator"
        return "review"
    match = re.search(r"-(draft-replan|draft|work|check|escalate|fix|verify)-\d+-\d+$", step)
    return {"draft": "coordinator", "draft-replan": "coordinator", "check": "coordinator",
            "verify": "coordinator", "work": "worker", "fix": "worker",
            "escalate": "review"}.get(match[1], "other") if match else "other"


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
    plan_groups = defaultdict(Counter)
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
        pair = {"task": task, "repeat": repeat, "outcome": outcome}
        plans = [row.get("plan_grade") if row else None for row in (a, b)]
        if "plan_requirements" in tasks[task] or any(grade is not None for grade in plans):
            plan_outcome = ({"pass pass": "both_pass", "pass fail": "left_only",
                             "fail pass": "right_only", "fail fail": "both_fail"}
                            .get(" ".join(grade["status"] if grade else "ungraded" for grade in plans),
                                 "ungraded"))
            pair["plan_outcome"] = plan_outcome
            plan_groups[category][plan_outcome] += 1
        pairs.append(pair)
    for arm in (left, right):
        runs = [row for row in rows if row["arm"] == arm]
        coverage[arm] = {"mode": arms[arm]["mode"], "model": arms[arm]["model"],
                         "scheduled_tasks": sorted({r["task"] for r in runs}),
                         "statuses": dict(Counter(r["status"] for r in runs))}
        totals = {}
        for name in ("wall_seconds", "admitted_responses", "dispatched_tools", "reported_tokens",
                     "http_attempts", "http_sent", "http_retries", "http_errors",
                     "provider_seconds", "tool_seconds",
                     "first_visible_plan_seconds", "plan_seconds", "verifier_seconds"):
            values = [row.get(name, row.get("usage", {}).get(name))
                      for row in runs]
            present = [value for value in values if value is not None]
            totals[name] = {"sum": round(sum(present), 3) if present else None,
                            "trials_reporting": len(present)}
            if name in ("reported_tokens", "http_attempts", "http_sent", "http_retries", "http_errors"):
                solves = coverage[arm]["statuses"].get("pass", 0)
                totals[name]["per_solved_task"] = (
                    round(sum(present) / solves, 3) if solves and len(present) == len(runs) else None
                )
        metrics[arm] = totals
    return {"by_category": groups, "plan_by_category": plan_groups,
            "pairs": pairs, "coverage": coverage, "metrics": metrics}
