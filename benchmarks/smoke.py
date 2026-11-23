"""Exercise all benchmark modes against the feature-gated, offline ANSA binary."""

import argparse
import json
from pathlib import Path
import subprocess
import sys

sys.path.insert(0, str(Path(__file__).resolve().parent))
from bench import main as benchmark, write
from carbon_workspace_pty import seed_siwc


def plus_transport(root, home, arm):
    if arm["mode"] != "propose+":
        return None
    marker = root / "fixture-owner"
    marker.write_bytes(b"ansa-m4-synthetic-v1\n")
    marker.chmod(0o600)
    (root / "orchestration-plus").touch()
    seed_siwc(home)
    return ["--frontend-fixture", str(root), "siwc-carbon"]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--frontend-adapter", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    output = args.output.resolve()
    output.mkdir(mode=0o700)

    fixture = output / "inputs"
    fixture.mkdir()
    (fixture / "report.txt").write_text("before fixture\n")
    check = fixture / "check.sh"
    check.write_text('#!/bin/sh\nexec grep -q "^after fixture" report.txt\n')
    check.chmod(0o700)
    guidance = output / "guidance.md"
    guidance.write_text("Verify the result of the requested operation.\n")
    grader = output / "verify.py"
    grader.write_text(f"#!{sys.executable}\n" + """import json
from pathlib import Path
import sys

answer = json.loads(Path(sys.argv[2]).read_text())
file = Path(sys.argv[1]) / "report.txt"
if answer["plan"] and not answer["answers"]:
    assert answer["plan"]["steps"] and file.read_text() == "before fixture\\n"
elif len(answer["answers"]) == 2:
    assert all(text.startswith("OMH_TOOL_OK:") for text in answer["answers"])
    assert file.read_text() == "before fixture\\n"
elif answer["answers"] == ["PLUS_CARBON_DONE"]:
    assert file.read_text() == "after fixture reviewed\\n"
elif answer["answers"] == ["DIRECT_LEAD_BLOCKED: A person needs to be present."]:
    assert file.read_text() == "before fixture\\n"
else:
    assert len(answer["answers"]) == 1 and file.read_text() == "after fixture\\n"
""")
    grader.chmod(0o700)

    baseline = {"binary": str(args.binary.resolve()), "route": "offline", "model": "gpt-6.1-sol"}
    roles = {"strong": {"model": "gpt-6.2-sol", "effort": "high"},
             "medium": {"model": "gpt-6.1-sol", "effort": "medium"},
             "weak": {"model": "gpt-6.1-luna", "effort": "low"}}
    offline_roles = {role: {"model": baseline["model"], "effort": None} for role in roles}
    arms = [baseline | {"id": "basic", "mode": "work", "guidance": "guidance.md"},
            baseline | {"id": "plan", "mode": "plan", "model": "gpt-6.1-luna"},
            baseline | {"id": "planner", "mode": "planner"},
            baseline | {"id": "orch", "mode": "orchestrate"},
            baseline | {"id": "plus", "mode": "orchestrate+", "roles": offline_roles},
            baseline | {"id": "proposal", "mode": "propose"},
            baseline | {"id": "proposal-plus", "mode": "propose+", "roles": roles,
                        "model": "gpt-6.2-sol", "effort": "high",
                        "binary": str(args.frontend_adapter.resolve())}]
    tasks = [{"id": "patch", "modes": ["work", "planner", "propose", "propose+"],
              "turns": ["ORCHESTRATION_COMMAND_FIXTURE"]},
             {"id": "delegation", "modes": ["orchestrate", "orchestrate+"],
              "turns": ["ORCHESTRATION_DIRECT_FIXTURE"]},
             {"id": "blocked", "modes": ["orchestrate", "orchestrate+"],
              "turns": ["ORCHESTRATION_DIRECT_BLOCKED_FIXTURE"]},
             {"id": "dialogue", "modes": ["plan"], "turns": ["Read the value", "Read again"]}]
    spec = {"version": 1, "seed": "carbon-smoke", "repetitions": 1, "arms": arms,
            "budget": {"trial_seconds": 30, "verifier_seconds": 3, "total_seconds": 120,
                       "tool_rounds": 8, "tool_calls": 16},
            "tasks": [task | {"workspace": "inputs", "verifier": "verify.py"} for task in tasks]}
    manifest = output / "manifest.json"
    write(manifest, spec)
    command = [sys.executable, "-B", str(Path(__file__).with_name("bench.py")), str(manifest)]
    subprocess.run(command, stdout=subprocess.DEVNULL, check=True)
    run = output / "runs"
    assert benchmark([str(manifest), "--output", str(run)], fixture_launch=plus_transport) == 0
    report = json.loads((run / "report.json").read_text())
    trials = {(trial["task"], trial["arm"]): trial for trial in report["trials"]}
    assert all(trial["status"] == "pass" for trial in trials.values()), report
    assert all(trials["patch", arm]["candidate"] for arm in ("proposal", "proposal-plus"))
    assert trials["patch", "planner"]["candidate"] is None
    for task, response, tool_count in (
        ("delegation", "DIRECT_LEAD_COMPLETED", 2),
        ("blocked", "DIRECT_LEAD_BLOCKED: A person needs to be present.", 1),
    ):
        for arm in ("orch", "plus"):
            trial = trials[task, arm]
            answer = json.loads((run / trial["id"] / "answer.json").read_text())
            assert answer == {"answers": [response], "plan": None, "candidate": None}
            assert len(trial["usage"]["sessions"]) == 2
            assert trial["usage"]["dispatched_tools"] == tool_count
    assert trials["dialogue", "plan"]["usage"]["observed_generations"] == [
        {"requested": "gpt-6.1-luna", "returned": "gpt-6.1-luna", "effort": None}]
    assert {row["requested"] for row in trials["patch", "proposal-plus"]["usage"]["observed_generations"]} == {
        choice["model"] for choice in roles.values()}
    assert trials["delegation", "plus"]["usage"]["observed_generations"] == [
        {"requested": baseline["model"], "returned": baseline["model"], "effort": None}]


if __name__ == "__main__":
    main()
