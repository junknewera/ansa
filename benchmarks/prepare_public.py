"""Prepare selected public session tasks and freeze a Carbon benchmark manifest."""

import argparse
from pathlib import Path
import sys

import public_bfcl
import public_decompile
import public_images
import public_terminal
from bench import load, write

ADAPTERS = (public_terminal, public_bfcl, public_images, public_decompile)


def main(argv=None):
    catalog = {task_id: module for module in ADAPTERS for task_id in module.TASK_IDS}
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--list", action="store_true", help="print public task IDs")
    parser.add_argument("--task", action="append", choices=catalog, help="repeat to select tasks (default: all)")
    parser.add_argument("--output", type=Path, help="new directory for inputs and manifest")
    parser.add_argument("--binary", type=Path, help="ANSA executable")
    parser.add_argument("--model")
    parser.add_argument("--route", choices=("siwc", "openrouter", "offline"), default="offline")
    parser.add_argument("--auth-dir", type=Path, help="dedicated existing profile for live arms")
    parser.add_argument("--image-input", action="store_true", help="declare native image input for this binary, route and model")
    parser.add_argument("--candidate-guidance", type=Path, help="prepare a strict paired guidance comparison")
    args = parser.parse_args(argv)
    if args.list:
        print("\n".join(catalog))
        return 0
    if not args.output or not args.binary or not args.model:
        parser.error("preparation requires --output, --binary and --model")
    if bool(args.auth_dir) != (args.route != "offline"):
        parser.error("provide --auth-dir only for a live route")
    selected = args.task or list(catalog)
    if len(set(selected)) != len(selected):
        parser.error("repeated task")
    output = args.output.resolve()
    arm = {"id": "baseline", "binary": str(args.binary.resolve(strict=True)), "model": args.model,
           "route": args.route, "mode": "work"}
    if args.image_input:
        arm["image_input"] = True
    if args.auth_dir:
        arm["auth_dir"] = str(args.auth_dir.resolve(strict=True))
    output.mkdir(mode=0o700)
    tasks = []
    for task_id in selected:
        root = output / task_id
        root.mkdir(mode=0o700)
        print(f"Preparing {task_id}", file=sys.stderr, flush=True)
        task = catalog[task_id].prepare(task_id, root)
        if task["id"] != task_id:
            raise ValueError(f"adapter returned wrong task ID for {task_id}")
        tasks.append(task)
    manifest = output / "manifest.json"
    pending = output / "manifest.pending.json"
    spec = {"version": 1, "repetitions": 1, "seed": "public-all-around-v1",
            "arms": [arm], "tasks": tasks,
            "budget": {"tool_calls": 64, "tool_rounds": 32, "trial_seconds": 600,
                       "verifier_seconds": 300, "total_seconds": len(tasks) * 900}}
    if args.candidate_guidance:
        guidance = args.candidate_guidance.resolve(strict=True)
        spec["arms"].append(arm | {"id": "candidate", "guidance": str(guidance)})
        spec["comparison"] = {"left": "baseline", "right": "candidate", "factor": "guidance"}
        spec["budget"]["total_seconds"] *= 3  # two runs plus oracle calibration
    write(pending, spec)
    load(pending)
    pending.rename(manifest)
    print(manifest)
    return 0


if __name__ == "__main__":
    sys.exit(main())
