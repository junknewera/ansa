#!/usr/bin/env python3
"""Run a manifest of ANSA modes, models and guidance through Carbon."""

import argparse
from collections import Counter
from datetime import datetime, timezone
import hashlib
import itertools
import json
import os
from pathlib import Path
import shutil
import signal
import stat
import subprocess
import sys
import tempfile
import time

PROJECT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(PROJECT / "tests"))
from native_eval import decode, label, require
from carbon import MODES, TrialStopped, capture, journals
import comparison

MAX_TRIALS = 128
LIVE_ROUTES = ("siwc", "openrouter")
CARBON_HELPERS = (
    "tests/carbon_workspace_pty.py",
    "tests/cli_progress_pty.py",
    "tests/native_eval.py",
    "tests/parity/frontend_io.py",
    "tests/parity/protocol.py",
    "tests/parity/protocol_network.py",
    "tests/parity/run.py",
    "tests/parity/session_cases.py",
)


def utc():
    return datetime.now(timezone.utc).isoformat()


def digest(file):
    with file.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def write(path, obj):
    path.write_text(json.dumps(obj, ensure_ascii=False, indent=2) + "\n")


def check_fields(obj, required, optional=()):
    require(isinstance(obj, dict) and set(required) <= obj.keys() <= set(required) | set(optional),
            f"expected fields {', '.join(required)}")


def bounded(value, low, high):
    require(type(value) is int and low <= value <= high, f"expected integer in {low}..{high}")


def no_dotenv(path):
    require(not any(part == ".env" or part.startswith(".env.") for part in path.parts),
            "dotenv files are excluded")
    return path


def tree(root):
    require(root.is_dir(), "input workspace must be a directory")
    entries = {}
    for parent, dirs, files in os.walk(root, followlinks=False):
        for name in sorted(dirs + files):
            require(name not in (".git", ".ansa"), f"reserved fixture path: {name}")
            file = no_dotenv(Path(parent) / name)
            meta = file.lstat()
            relative = file.relative_to(root).as_posix()
            if stat.S_ISDIR(meta.st_mode):
                entries[relative] = {"directory": True}
            else:
                require(stat.S_ISREG(meta.st_mode) and meta.st_nlink == 1,
                        f"fixture must contain regular files or directories: {relative}")
                entries[relative] = {"sha256": digest(file), "executable": bool(meta.st_mode & 0o100)}
    return entries


def load(path):
    return load_experiment(path)[:5]


def load_experiment(path):
    raw = path.read_bytes()
    spec = decode(raw)
    check_fields(spec, ("version", "repetitions", "seed", "arms", "tasks", "budget"), ("comparison",))
    bounded(spec["version"], 1, 1)
    bounded(spec["repetitions"], 1, 16)
    require(label(spec["seed"]), "invalid seed")
    budget = spec["budget"]
    check_fields(budget, ("trial_seconds", "verifier_seconds", "total_seconds",
                          "tool_rounds", "tool_calls"))
    for key, maximum in {"trial_seconds": 7200, "verifier_seconds": 3600,
                         "total_seconds": 86400, "tool_rounds": 64, "tool_calls": 256}.items():
        bounded(budget[key], 1, maximum)

    def path_for(value):
        return no_dotenv(no_dotenv(path.parent / value).resolve(strict=True))

    arms = {}
    require(isinstance(spec["arms"], list) and spec["arms"], "expected arms")
    for arm in spec["arms"]:
        check_fields(arm, ("id", "binary", "mode", "route", "model"),
                     ("effort", "roles", "guidance", "auth_dir", "mcp_config", "image_input"))
        key = arm["id"]
        require(label(key) and key not in arms, "invalid or duplicate arm")
        require(arm["mode"] in MODES and arm["route"] in ("offline", *LIVE_ROUTES), "invalid mode or route")
        require(type(arm.get("image_input", False)) is bool, "image_input must be a boolean")
        validate_selection(arm["model"], arm.get("effort"))
        require("auth_dir" in arm if arm["route"] in LIVE_ROUTES else "auth_dir" not in arm,
                "live arms require a dedicated auth_dir")
        require(("roles" in arm) == (arm["mode"] == "orchestrate+"),
                "Orchestrate+ requires strong, medium and weak roles")
        if "roles" in arm:
            check_fields(arm["roles"], ("strong", "medium", "weak"))
            for value in arm["roles"].values():
                check_fields(value, ("model",), ("effort",))
                validate_selection(value["model"], value.setdefault("effort", None))
        for field in ("binary", "auth_dir", "mcp_config", "guidance"):
            if field in arm:
                arm[field] = path_for(arm[field])
        require(arm["binary"].is_file() and os.access(arm["binary"], os.X_OK), "invalid ansa executable")
        require("auth_dir" not in arm or arm["auth_dir"].is_dir(), "invalid auth_dir")
        arms[key] = arm | {"effort": arm.get("effort"), "binary_sha256": digest(arm["binary"])}
        for field in ("guidance", "mcp_config"):
            if field in arm:
                arms[key][f"{field}_sha256"] = digest(arm[field])

    tasks = {}
    require(isinstance(spec["tasks"], list) and spec["tasks"], "expected tasks")
    for task in spec["tasks"]:
        check_fields(task, ("id", "turns", "workspace", "verifier", "modes"),
                     ("source", "verifier_inputs", "mcp_config", "git_setup", "attachments",
                      "category", "family", "split", "oracle_fixture", "plan_requirements",
                      "clarification"))
        key = task["id"]
        require(label(key) and key not in tasks, "invalid or duplicate task")
        require(isinstance(task["turns"], list) and 0 < len(task["turns"]) <= 32,
                "expected 1..32 turns")
        require(all(isinstance(turn, str) and turn.strip() and not turn.lstrip().startswith("/")
                    and not any(ord(char) < 32 and char not in "\n\t" for char in turn)
                    and len(turn.encode()) <= 64 * 1024 for turn in task["turns"]), "invalid user turn")
        modes = task["modes"]
        require(isinstance(modes, list) and modes and all(mode in MODES for mode in modes)
                and len(set(modes)) == len(modes), "invalid task modes")
        require(len(task["turns"]) == 1 or set(modes) <= {"work", "plan"},
                "multiple user turns require Work or Plan")
        if "plan_requirements" in task:
            requirements = task["plan_requirements"]
            check_fields(requirements, ("paths",), ("check",))
            require(isinstance(requirements["paths"], list) and requirements["paths"]
                    and all(isinstance(path, str) and path and not Path(path).is_absolute()
                            and all(part not in ("", ".", "..") for part in path.split("/"))
                            for path in requirements["paths"]), "invalid plan write paths")
            check = requirements.get("check")
            require("check" not in requirements or isinstance(check, str)
                    and 0 < len(check) <= 128 and all(c.isascii() and (c.isalnum() or c in "._-") for c in check),
                    "invalid expected plan check")
            require(set(modes) <= {"planner", "orchestrate", "orchestrate+"},
                    "plan requirements need an orchestration mode")
        if "clarification" in task:
            clarification = task["clarification"]
            check_fields(clarification, ("question", "answer"))
            require("plan_requirements" in task
                    and all(isinstance(v, str) and v.strip() and not v.lstrip().startswith("/")
                            and len(v.encode()) <= 4096 and all(ord(c) >= 32 for c in v)
                            for v in clarification.values()), "invalid scripted clarification")
        if "source" in task:
            require(isinstance(task["source"], str), "invalid task source")
        task["workspace"] = path_for(task["workspace"])
        task["verifier"] = path_for(task["verifier"])
        require(task["verifier"].is_file() and os.access(task["verifier"], os.X_OK)
                and not task["verifier"].is_relative_to(task["workspace"]),
                "verifier must be an executable outside the input workspace")
        for field in ("mcp_config", "git_setup", "oracle_fixture"):
            if field in task:
                file = path_for(task[field])
                require(file.is_file() and not file.is_relative_to(task["workspace"]),
                        f"{field} must be a file outside the input workspace")
                if field in ("git_setup", "oracle_fixture"):
                    require(os.access(file, os.X_OK), f"{field} must be executable")
                task[field] = file
                task[field + "_sha256"] = digest(file)
        if "attachments" in task:
            require(isinstance(task["attachments"], list) and task["attachments"], "empty attachments")
            for attachment in task["attachments"]:
                check_fields(attachment, ("turn", "path"))
                bounded(attachment["turn"], 0, len(task["turns"]) - 1)
                file = path_for(attachment["path"])
                require(file.is_file(), "attachment must be a file")
                attachment.update(path=str(file), sha256=digest(file))
        dependencies = task.pop("verifier_inputs", [])
        require(isinstance(dependencies, list), "invalid verifier inputs")
        dependencies = [path_for(value) for value in dependencies]
        require(all(file.is_file() and not file.is_relative_to(task["workspace"])
                    for file in dependencies), "verifier inputs must be outside workspace")
        tasks[key] = task | {"inputs": tree(task["workspace"]), "verifier_sha256": digest(task["verifier"]),
                             "verifier_inputs": {str(file): digest(file) for file in dependencies}}

    rows = [{"id": f"{task}.{repeat}.{arm}", "task": task, "repeat": repeat, "arm": arm}
            for task, repeat, arm in itertools.product(tasks, range(1, spec["repetitions"] + 1), arms)
            if arms[arm]["mode"] in tasks[task]["modes"] or tasks[task].get("attachments")]
    for row in rows:
        arm = arms[row["arm"]]
        task = tasks[row["task"]]
        require("mcp_config" not in arm or "mcp_config" not in task,
                "choose either task or arm MCP configuration")
        if task.get("attachments"):
            row.update(mode=arm["mode"], model=arm["model"], route=arm["route"])
            if arm["mode"] not in task["modes"]:
                row["unsupported"] = "this task does not support the selected mode"
            elif arm["mode"] not in ("work", "plan"):
                row["unsupported"] = "Carbon accepts images in Work and Plan"
            elif arm["route"] not in LIVE_ROUTES:
                row["unsupported"] = "the offline model has no image transport"
            elif not arm.get("image_input"):
                row["unsupported"] = "image input was not declared for this binary, route and model"
    require(0 < len(rows) <= MAX_TRIALS
            and {row["arm"] for row in rows} == arms.keys()
            and {row["task"] for row in rows} == tasks.keys(), "empty or oversized matrix")
    rows.sort(key=lambda row: hashlib.sha256(f'{spec["seed"]}/{row["id"]}'.encode()).digest())
    if "comparison" in spec:
        comparison.validate(spec["comparison"], arms, tasks, rows)
    return arms, tasks, budget, rows, hashlib.sha256(raw).hexdigest(), spec.get("comparison")


def validate_selection(model, effort):
    require(isinstance(model, str) and 0 < len(model) <= 256
            and all(char.isascii() and (char.isalnum() or char in "_./:-") for char in model),
            "invalid model")
    require(effort is None or label(effort) and effort != "default", "invalid effort (use null for default)")


def environment(root, home):
    values = {key: os.environ[key] for key in ("PATH", "SSL_CERT_FILE", "SSL_CERT_DIR", "NIX_SSL_CERT_FILE")
              if key in os.environ}
    return values | {
        "HOME": str(home), "XDG_CONFIG_HOME": str(home / ".config"),
        "XDG_STATE_HOME": str(home / ".local/state"), "XDG_CACHE_HOME": str(home / ".cache"),
        "XDG_DATA_HOME": str(home / ".local/share"), "TMPDIR": str(root / "tmp"),
        "TERM": "xterm", "LC_ALL": "C", "SHELL": shutil.which("bash") or "/bin/sh",
        "PYTHONDONTWRITEBYTECODE": "1",
        "ANSA_NO_ANIMATION": "1", "GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": "/dev/null",
        "GIT_AUTHOR_DATE": "2020-01-01T00:00:00Z", "GIT_COMMITTER_DATE": "2020-01-01T00:00:00Z",
    }


def usage(traces):
    records = [record for rows in traces.values() for record in rows]
    responses = [record["response"] for record in records if record["kind"] == "response"]
    compactions = [record["compaction"]["native"] for record in records if record["kind"] == "compacted"]
    responses += [native["summary_response"] for native in compactions if "summary_response" in native]
    generations = sorted({(response["request"]["binding"]["requested_model"],
                           response["model"], response["request"].get("effort") or "")
                          for response in responses})
    tokens = [response.get("reported_usage_v1") for response in responses]
    return {
        "sessions": sorted(traces), "admitted_responses": len(responses),
        "dispatched_tools": sum(record["kind"] == "outcome" and record["result"]["dispatched"]
                                for record in records),
        "compactions": len(compactions), "stages": comparison.stages(traces),
        "reported_tokens": sum(t["input_tokens"] + t["output_tokens"] for t in tokens)
        if responses and all(tokens) else None,
        "observed_generations": [{"requested": a, "returned": b, "effort": e or None}
                                 for a, b, e in generations],
    }


def copy_inputs(task, workspace):
    source = task["workspace"]
    workspace.mkdir(mode=0o700)
    for relative, expected in sorted(task["inputs"].items()):
        path = source / relative
        target = workspace / relative
        require(path.resolve(strict=True) == path, "fixture path changed")
        if expected.get("directory"):
            require(path.is_dir(), "fixture directory changed")
            target.mkdir()
            continue
        fd = os.open(path, os.O_RDONLY | os.O_NONBLOCK | os.O_NOFOLLOW)
        with os.fdopen(fd, "rb") as original, target.open("xb") as copy:
            meta = os.fstat(original.fileno())
            require(stat.S_ISREG(meta.st_mode) and meta.st_nlink == 1, "fixture file changed")
            shutil.copyfileobj(original, copy)
        target.chmod(0o700 if expected["executable"] else 0o600)
    require(tree(workspace) == task["inputs"] and tree(source) == task["inputs"], "workspace inputs changed")


def run_trial(root, arm, task, budget, end, fixture_launch=None):
    home = root / "home"
    workspace = home / "workspace"
    home.mkdir(mode=0o700)
    (root / "tmp").mkdir(mode=0o700)
    env = environment(root, home)

    def git(cwd, *args):
        result = subprocess.run(["git", "-c", "core.hooksPath=/dev/null", *args], cwd=cwd,
                                env=env, capture_output=True, check=True, timeout=20)
        return result.stdout.decode()

    result = {}
    trace = root / "timings.jsonl"
    trace.touch(mode=0o600)
    env["ANSA_BENCHMARK_TRACE"] = str(trace)
    started = time.monotonic()
    try:
        require(digest(arm["binary"]) == arm["binary_sha256"], "ansa executable changed")
        require(digest(task["verifier"]) == task["verifier_sha256"], "verifier changed")
        require(all(digest(Path(file)) == sha for file, sha in task["verifier_inputs"].items()),
                "verifier inputs changed")
        copy_inputs(task, workspace)
        git(workspace, "init", "-q")
        git(workspace, "add", "-f", "--", ".")
        git(workspace, "-c", "user.name=ANSA Benchmark", "-c", "user.email=fixture@localhost",
            "commit", "--allow-empty", "-qm", "Benchmark inputs")
        if "git_setup" in task:
            require(digest(task["git_setup"]) == task["git_setup_sha256"], "git_setup changed")
            subprocess.run([str(task["git_setup"])], cwd=workspace, env=env,
                           capture_output=True, check=True, timeout=60)
        result["input_commit"] = git(workspace, "rev-parse", "HEAD").strip()

        config = home / ".ansa"
        config.mkdir(mode=0o700)
        write(config / "config.json", {"schema_version": 1, "tool_calls": budget["tool_calls"],
                                      "tool_rounds": budget["tool_rounds"]})
        for key, target in (("guidance", "AGENTS.md"), ("mcp_config", ".ansa/mcp.json")):
            owner = task if key in task else arm
            if key in owner:
                data = owner[key].read_bytes()
                require(hashlib.sha256(data).hexdigest() == owner[f"{key}_sha256"], f"{key} changed")
                (home / target).write_bytes(data)
                (home / target).chmod(0o600)
        if arm["route"] == "siwc":
            status = subprocess.run([str(arm["binary"]), "--auth-status", "--auth-dir", str(arm["auth_dir"])],
                                    env=env, cwd=workspace, capture_output=True, timeout=10, check=True)
            account = json.loads(status.stdout)
            require(account["signed_in"] and account["plan_enabled"],
                    "benchmark account needs a qualifying ChatGPT sign-in")
        fixture_args = fixture_launch(root, home, arm) if fixture_launch else None
        if fixture_args is not None:
            result["transport"] = "synthetic-openrouter" if arm["route"] == "openrouter" else "synthetic-siwc"
        deadline = min(end, time.monotonic() + budget["trial_seconds"])
        if deadline <= time.monotonic():
            raise TrialStopped("not_run", "experiment time budget")
        agent_started = time.monotonic()
        grading_workspace, answer = capture(arm["binary"], root, home, workspace, env, arm,
                                             task["turns"], deadline, git, result["input_commit"],
                                             fixture_args, attachments=task.get("attachments", ()),
                                             clarification=task.get("clarification"), observations=result)
        if "image_inputs" in answer:
            result["image_inputs"] = answer["image_inputs"]
        result["agent_wall_seconds"] = round(time.monotonic() - agent_started, 3)
        result["candidate"] = answer["candidate"]
        write(root / "answer.json", answer)
        result["usage"] = usage(journals(home))
        allowed = {(arm["model"], arm["effort"])} | {
            (role["model"], role["effort"]) for role in arm.get("roles", {}).values()}
        generations = result["usage"]["observed_generations"]
        require(generations and all(row["requested"] == row["returned"]
                and (row["requested"], row["effort"]) in allowed for row in generations),
                "journal generation differs from the experiment")
        require(digest(task["verifier"]) == task["verifier_sha256"]
                and all(digest(Path(file)) == sha for file, sha in task["verifier_inputs"].items()),
                "verifier changed during trial")
        timeout = min(end - time.monotonic(), budget["verifier_seconds"])
        if timeout <= 0:
            raise TrialStopped("infra_error", "no time left for verifier")
        verifier_started = time.monotonic()
        with (root / "verifier.log").open("wb") as log:
            argv = [str(task["verifier"]), str(grading_workspace), str(root / "answer.json")]
            with subprocess.Popen(argv, cwd=grading_workspace, env=env, stdin=subprocess.DEVNULL,
                                  stdout=log, stderr=subprocess.STDOUT, start_new_session=True) as verifier:
                try:
                    exit_code = verifier.wait(timeout=timeout)
                except (subprocess.TimeoutExpired, KeyboardInterrupt) as error:
                    try:
                        os.killpg(verifier.pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                    if isinstance(error, KeyboardInterrupt):
                        raise
                    raise TrialStopped("infra_error", "verifier timed out") from error
        result["verifier_seconds"] = round(time.monotonic() - verifier_started, 3)
        if "plan_requirements" in task and arm["mode"] != "planner" and exit_code in (0, 1):
            changed = (git(grading_workspace, "diff", "--no-renames", "--name-only", "-z",
                           answer["candidate"] + "^!", "--").split("\0") if answer["candidate"] else [])
            allowed_paths = set(task["plan_requirements"]["paths"])
            result["unexpected_changed_paths"] = sorted(set(filter(None, changed)) - allowed_paths)
        result.update(status={0: "pass", 1: "fail"}.get(exit_code, "infra_error"),
                      verifier_exit=exit_code)
        if result.get("unexpected_changed_paths") and exit_code == 0:
            result["status"] = "fail"
    except TrialStopped as error:
        result.update(status=error.status, reason=str(error))
    except (AssertionError, KeyError, OSError, subprocess.SubprocessError, ValueError) as error:
        result.update(status="infra_error", reason=str(error))
    if "usage" not in result:
        try:
            result["usage"] = usage(journals(home))
        except (AssertionError, KeyError, OSError, ValueError) as error:
            result.update(status="infra_error", evidence_error=str(error))
    if trace.stat().st_size:
        try:
            result.update(comparison.trace_metrics(trace, journals(home)))
        except (KeyError, OSError, ValueError) as error:
            result["timing_error"] = str(error)
            if result["status"] == "pass":
                result["status"] = "infra_error"
    if "plan_requirements" in task:
        plan_file = root / "plan-before-start.json"
        if plan_file.is_file():
            result["plan_grade"] = comparison.grade_plan(
                json.loads(plan_file.read_text()), task["plan_requirements"],
                result.get("start_available", False))
        elif result.get("clarification_verified") is False:
            result["plan_grade"] = {"status": "fail", "criteria": {"clarification": False}}
    result["wall_seconds"] = round(time.monotonic() - started, 3)
    return result


def summarize(rows, arms):
    counts = {arm: dict(Counter(row["status"] for row in rows if row["arm"] == arm)) for arm in arms}
    outcomes = {(row["task"], row["repeat"], row["arm"]): row["status"] for row in rows}
    pairs = {}
    graded = {"pass", "fail", "failed", "timeout"}
    for left, right in itertools.combinations(arms, 2):
        scores = Counter()
        for task, repeat in sorted({(row["task"], row["repeat"]) for row in rows}):
            a = outcomes.get((task, repeat, left))
            b = outcomes.get((task, repeat, right))
            if a is not None and b is not None:
                name = "ungraded"
                if a in graded and b in graded:
                    name = {(True, True): "both_pass", (True, False): "left_only",
                            (False, True): "right_only", (False, False): "both_fail"}[a == "pass", b == "pass"]
                scores[name] += 1
        pairs[f"{left}:{right}"] = dict(scores)
    return counts, pairs


def cgroup_sample():
    relative, = [line[3:] for line in Path("/proc/self/cgroup").read_text().splitlines()
                 if line.startswith("0::")]
    group = Path("/sys/fs/cgroup") / relative.lstrip("/")
    require(group.name.startswith("ansa-benchmark-") and group.name.endswith(".scope"),
            "resource measurement needs a fresh ansa-benchmark-* systemd scope")
    cpu = {name: int(value) for name, value in
           (line.split() for line in (group / "cpu.stat").read_text().splitlines())}
    return cpu["usage_usec"], int((group / "memory.peak").read_text())


def evidence_bytes(output):
    sizes = {}
    for directory, _, files in os.walk(output):
        for name in files:
            meta = (Path(directory) / name).lstat()
            if stat.S_ISREG(meta.st_mode):
                sizes[meta.st_dev, meta.st_ino] = meta.st_size
    return sum(sizes.values())


def main(argv=None, fixture_launch=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("manifest", type=Path)
    parser.add_argument("--output", type=Path, help="fresh evidence directory; omit to print the frozen plan")
    parser.add_argument("--live", action="store_true", help="authorize model requests")
    parser.add_argument("--measure-cgroup", action="store_true",
                        help="record process-tree CPU and peak memory in a fresh benchmark scope")
    parser.add_argument("--split", choices=("development", "holdout"), help="strict A/B split (default: development)")
    parser.add_argument("--frozen-plan", type=Path, help="require the exact configuration and input hashes saved in an earlier plan")
    args = parser.parse_args(argv)
    try:
        manifest = no_dotenv(no_dotenv(args.manifest).resolve())
        arms, tasks, budget, rows, manifest_hash, experiment = load_experiment(manifest)
        require(experiment or args.split is None, "--split requires a comparison")
        split = args.split or "development"
        if experiment:
            rows = [row for row in rows if tasks[row["task"]]["split"] == split]
        plan = {"manifest_sha256": manifest_hash, "driver_sha256": digest(Path(__file__)),
                "carbon_sha256": digest(Path(__file__).with_name("carbon.py")),
                "comparison_sha256": digest(Path(comparison.__file__)),
                "helper_sha256": {path: digest(PROJECT / path) for path in CARBON_HELPERS},
                "arms": {key: {field: str(value) if isinstance(value, Path) else value
                               for field, value in arm.items() if field != "auth_dir"}
                         for key, arm in arms.items()},
                "tasks": tasks, "budget": budget, "schedule": rows}
        if experiment:
            plan.update(comparison=experiment, split=split)
        if args.frozen_plan:
            frozen = decode(no_dotenv(args.frozen_plan.resolve(strict=True)).read_bytes())
            saved_helpers = frozen.get("helper_sha256") if isinstance(frozen, dict) else None
            require(isinstance(saved_helpers, dict), "frozen plan lacks Carbon helper hashes; regenerate it")
            for path, sha in plan["helper_sha256"].items():
                require(saved_helpers.get(path) == sha, f"Carbon helper differs from the frozen plan: {path}")
            require(frozen == json.loads(json.dumps(plan, default=str)),
                    "current manifest, programs, inputs or schedule differ from the frozen plan")
        if args.output is None:
            print(json.dumps(plan, ensure_ascii=False, indent=2, default=str))
            return 0
        require(args.live or all(arm["route"] == "offline" for arm in arms.values()),
                "live arms require --live")
        output = args.output.resolve()
        for task in tasks.values():
            require(not output.is_relative_to(task["workspace"]), "output is inside an input workspace")
        for arm in arms.values():
            if "auth_dir" in arm:
                require(not arm["auth_dir"].is_relative_to(output)
                        and not any(arm["auth_dir"].is_relative_to(task["workspace"]) for task in tasks.values()),
                        "auth_dir must be outside trial workspaces and evidence")
        output.mkdir(mode=0o700)
        cpu_start = cgroup_sample()[0] if args.measure_cgroup else None
        plan["tasks"] = {key: {field: str(value) if isinstance(value, Path) else value
                              for field, value in task.items()} for key, task in tasks.items()}
        write(output / "plan.json", plan)
        end = time.monotonic() + budget["total_seconds"]
        began = utc()
        if experiment:
            controls = output / "controls"
            controls.mkdir()
            for task_id in sorted({row["task"] for row in rows if "unsupported" not in row}):
                receipt = comparison.calibrate(tasks[task_id], controls / task_id, budget, end, digest, environment)
                write(controls / task_id / "receipt.json", receipt)
        trials = []
        for row in rows:
            timestamp = utc()
            archive = output / row["id"]
            if "unsupported" in row:
                archive.mkdir(mode=0o700)
                result = {"status": "unsupported", "reason": row["unsupported"],
                          "mode": arms[row["arm"]]["mode"], "model": arms[row["arm"]]["model"]}
            elif time.monotonic() < end:
                prefix = "ansa-m4-" if fixture_launch else "ansa-bench-"
                root = Path(tempfile.mkdtemp(prefix=prefix, dir="/tmp"))
                try:
                    result = run_trial(root, arms[row["arm"]], tasks[row["task"]], budget, end, fixture_launch)
                finally:
                    shutil.move(root, archive)
            else:
                archive.mkdir(mode=0o700)
                result = {"status": "not_run", "reason": "experiment time budget"}
            trial = row | result | {"started_at": timestamp, "finished_at": utc()}
            write(archive / "trial.json", trial)
            trials.append(trial)
            print(f'{row["id"]} {trial["status"]}', flush=True)
        counts, pairs = summarize(trials, arms)
        report = {"started_at": began, "finished_at": utc(), "summary": counts, "paired": pairs,
                  "trials": trials, "evidence_bytes": evidence_bytes(output)}
        if cpu_start is not None:
            cpu_finished, memory_peak = cgroup_sample()
            report["process_tree"] = {"cpu_seconds": (cpu_finished - cpu_start) / 1e6,
                                      "peak_memory_bytes": memory_peak}
        invalid_stage = False
        if experiment:
            details = comparison.report(experiment, trials, arms, tasks)
            report.update(comparison=details, split=split)
            # Keep the legacy summary consistent with stage-qualified outcomes.
            report["paired"] = {f"{experiment['left']}:{experiment['right']}":
                                dict(Counter(pair["outcome"] for pair in details["pairs"]))}
            invalid_stage = any(pair["outcome"] == "stage_missing" for pair in details["pairs"])
        write(output / "report.json", report)
        return int(invalid_stage or all(row["status"] == "unsupported" for row in trials)
                   or any(row["status"] in ("infra_error", "not_run") for row in trials))
    except (KeyError, TypeError, ValueError, OSError, subprocess.TimeoutExpired) as error:
        print(str(error), file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
