"""Run both prepared SWE-bench controls and assert the target regression's outcome."""

import json
from pathlib import Path
import subprocess
import sys

from public_images import INSTANCE


def run(operator, output):
    row, = json.loads((operator / "dataset.json").read_text())
    if row["instance_id"] != INSTANCE:
        raise ValueError("expected the prepared Chart.js regression")
    cases = row["FAIL_TO_PASS"]
    if len(cases) != 1:
        raise ValueError("expected one upstream FAIL_TO_PASS case")
    output.mkdir()
    results = {}
    for control, expected, group in [("pass", 0, "success"), ("fail", 1, "failure")]:
        root = output / control
        workspace = root / "workspace"
        workspace.mkdir(parents=True)
        answer = root / "answer.json"
        # Also execute the negative control when the upstream positive check fails.
        subprocess.run([operator / "control.py", control, workspace, answer], check=True)
        code = subprocess.run([operator / "verify.py", workspace, answer]).returncode
        reports = list((root / "swe-evaluation" / "logs" / "evaluation").glob(f"*/ansa/{INSTANCE}/report.json"))
        scored = {}
        if len(reports) == 1:
            scored = json.loads(reports[0].read_text()).get(INSTANCE, {})
        outcomes = scored.get("tests_status", {}).get("FAIL_TO_PASS", {})
        ok = (code == expected and scored.get("patch_successfully_applied") is True
              and scored.get("infra_failure") is False and outcomes.get(group) == cases)
        results[control] = {"exit": code, "expected": expected, "case": cases[0], "outcomes": outcomes, "ok": ok}
        print(f"Chart.js {control} control: {results[control]}", flush=True)
    (output / "results.json").write_text(json.dumps(results, indent=2) + "\n")
    return int(not all(r["ok"] for r in results.values()))


if __name__ == "__main__":
    sys.exit(run(*(Path(s).resolve() for s in sys.argv[1:])))
