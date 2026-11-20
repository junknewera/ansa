"""Run a paired, deterministic no-improvement control through the installed ansa CLI."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys


def digest(data):
    return hashlib.sha256(data).hexdigest()


def encoded(value):
    return json.dumps(value, sort_keys=True, indent=2).encode() + b"\n"


def require(condition, message):
    if not condition:
        raise ValueError(message)


def capture(binary, root):
    project = root / "workspace"
    home = root / "fixture-home"
    trials = root / "trials"
    for folder in [project, home, trials]:
        folder.mkdir(mode=0o700)
    environment = {"HOME": str(home), "PATH": os.environ["PATH"], "LC_ALL": "C"}

    def run(args, stdin=None):
        result = subprocess.run(
            [str(binary), *args], cwd=project, env=environment, input=stdin,
            capture_output=True, timeout=20,
        )
        if result.returncode:
            raise RuntimeError(result.stderr.decode(errors="replace")[-1000:])
        return result.stdout

    def operator(action, *args, data=None):
        return json.loads(run(["--harness", action, "--cwd", str(project), *args],
                              encoded(data) if data is not None else None))

    baseline = {"version": 1, "guidance": "", "skills": [], "memory": []}
    candidate = dict(baseline, guidance="Return the value requested by the user.")
    profiles = {"baseline": operator("baseline", data=baseline)["activation"]["revision"]}
    # This offline provider always uses the fixture tool. A harmless guidance change
    # must produce NO_ENTRY when both arms already pass the frozen host check.
    expected = b"OMH_TOOL_OK:OMH_FIXED_VALUE_42\n"
    tasks = {"development": "Read the fixture value.", "holdout": "Return the fixture value."}
    plan = {"version": 1, "repetitions": 2}
    for split, prompt in tasks.items():
        plan[split] = {split: {"prompt_sha256": digest(prompt.encode()),
                              "inputs": {"input.txt": digest(prompt.encode())},
                              "checks": {"stdout.txt": digest(expected)}}}
    plan["per_trial"] = {"calls": 2, "tools": 1, "compactions": 0, "estimated_input_tokens": 32_000}
    plan["total"] = {field: value * 8 for field, value in plan["per_trial"].items()}
    (root / "plan.json").write_bytes(encoded(plan))
    freeze = operator("freeze", profiles["baseline"], data=plan)["freeze"]
    staged = operator("stage", freeze, data=candidate)
    proposal = staged["proposal"]
    profiles["candidate"] = staged["candidate"]
    sessions = home / ".ansa" / "sessions"
    for split, prompt in tasks.items():
        for repeat in range(1, 3):
            # Counterbalance order while resetting the same canonical project for every run.
            arms = profiles if repeat == 1 else reversed(profiles)
            for arm in arms:
                capture_dir = trials / split / str(repeat) / arm
                inputs, artifacts = capture_dir / "inputs", capture_dir / "artifacts"
                capture_dir.mkdir(parents=True, mode=0o700)
                inputs.mkdir(mode=0o700)
                artifacts.mkdir(mode=0o700)
                fixture = project / "input.txt"
                fixture.write_bytes(prompt.encode())
                shutil.copyfile(fixture, inputs / fixture.name)
                before = set(sessions.iterdir()) if sessions.exists() else set()
                stdout = run(["--offline", "--cwd", str(project), "--harness-trial", f"{proposal}:{arm}", prompt])
                (artifacts / "stdout.txt").write_bytes(stdout)
                created = set(sessions.iterdir()) - before
                require(len(created) == 1, "expected one fresh session")
                source = created.pop()
                shutil.copytree(source, capture_dir / "session")
                require(set(project.iterdir()) == {fixture}, "fixture workspace changed")
                fixture.unlink()
    evaluation = operator("evaluate", proposal, str(trials))
    (root / "evaluation.json").write_bytes(encoded(evaluation))
    pairs = evaluation["evaluation"]["trials"]
    require(len(pairs) == 4 and evaluation["evaluation"]["met_budget"], "paired schedule or budget")
    require(all(pair[arm]["eligible"] and pair[arm]["passed"]
                for pair in pairs for arm in profiles), "offline control did not pass")
    require(evaluation["status"] == "NO_ENTRY", "equal outcomes must retain baseline")
    require(operator("adopt", proposal)["status"] == "NO_ENTRY", "adoption must retain baseline")
    require(operator("status")["active"]["revision"] == profiles["baseline"], "active baseline changed")
    return {"status": "PASS", "binary_sha256": digest(binary.read_bytes()),
            "profiles": profiles, "decision": "NO_ENTRY", "paired_trials": len(pairs)}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    root = args.output.resolve()
    root.mkdir(mode=0o700, exist_ok=False)
    try:
        report = capture(args.binary.resolve(strict=True), root)
    except (OSError, ValueError, KeyError, RuntimeError, subprocess.SubprocessError) as error:
        report = {"status": "FAIL", "error": str(error)}
    (root / "report.json").write_bytes(encoded(report))
    print(json.dumps(report, sort_keys=True))
    return 0 if report["status"] == "PASS" else 1


if __name__ == "__main__":
    sys.exit(main())
