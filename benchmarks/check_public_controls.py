"""Verify that all eight public oracles accept their reference and reject their counterexample."""

import argparse
import contextlib
import json
from pathlib import Path
import time

from bench import digest, environment, load, main as benchmark, write
from check_chartjs_oracle import run as check_chartjs
from comparison import calibrate
from prepare_public import ADAPTERS
from public_images import TASK_IDS as IMAGE_TASKS


def check_pairs(manifest, output):
    # Real PTY sessions on the synthetic offline provider. Both fixture answers
    # should lose on the public questions, producing one scored pair per split.
    spec = json.loads(manifest.read_text())
    source = spec["arms"][0]
    arm = {"id": "baseline", "binary": str((manifest.parent / source["binary"]).resolve(strict=True)),
           "route": "offline", "mode": "work", "model": "gpt-6.1-sol"}
    guidance = output / "candidate.md"
    guidance.write_text("Verify the output before finishing.\n")
    spec["arms"] = [arm, arm | {"id": "candidate", "guidance": str(guidance)}]
    spec["tasks"] = [t for t in spec["tasks"] if t["id"] in ("intercode-bash-fs1-48", "mteb-retrieve")]
    spec["comparison"] = {"left": "baseline", "right": "candidate", "factor": "guidance"}
    spec["repetitions"] = 1
    for task in spec["tasks"]:
        for key in ("workspace", "verifier", "oracle_fixture"):
            task[key] = str((manifest.parent / task[key]).resolve(strict=True))
        task["verifier_inputs"] = [str((manifest.parent / file).resolve(strict=True)) for file in task["verifier_inputs"]]
    paired = output / "offline-acceptance.json"
    with paired.open("x") as stream:
        json.dump(spec, stream)
    for split in ("development", "holdout"):
        plan = output / (split + "-plan.json")
        args = [str(paired), "--split", split]
        with plan.open("x") as stream, contextlib.redirect_stdout(stream):
            if benchmark(args):
                raise ValueError(f"could not freeze {split} plan")
        run = output / split
        if benchmark([*args, "--frozen-plan", str(plan), "--output", str(run)]):
            raise ValueError(f"{split} run failed; see {run}")
        report = json.loads((run / "report.json").read_text())
        if report["paired"] != {"baseline:candidate": {"both_fail": 1}}:
            raise ValueError(f"unexpected scores on the synthetic provider; see {run}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("manifest", type=Path)
    parser.add_argument("output", type=Path, help="new evidence directory")
    args = parser.parse_args()
    _, tasks, budget, _, _ = load(args.manifest.resolve())
    expected_ids = {task_id for module in ADAPTERS for task_id in module.TASK_IDS}
    if tasks.keys() != expected_ids:
        parser.error("expected the complete public task catalog")
    output = args.output.resolve()
    output.mkdir(mode=0o700)
    results = {}
    for name, task in tasks.items():
        print(f"Checking {name}", flush=True)
        evidence = output / name
        if name == IMAGE_TASKS[1]:
            if check_chartjs(task["verifier"].parent, evidence):
                raise ValueError(f"Chart.js controls failed; see {evidence}")
            rows = json.loads((evidence / "results.json").read_text())
            receipt = {k: v["exit"] for k, v in rows.items()}
        else:
            receipt = calibrate(task, evidence, budget, time.monotonic() + 2 * budget["verifier_seconds"],
                                digest, environment)
        results[name] = receipt
        write(output / "results.json", results)
    check_pairs(args.manifest.resolve(), output)


if __name__ == "__main__":
    main()
