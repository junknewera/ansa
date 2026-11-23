#!/usr/bin/env python3
"""Install the known-pass or last-tie-wins mutant in an isolated calibration run."""

import json
from pathlib import Path
import sys

BENCH = Path(__file__).resolve().parents[2]
sys.path[:0] = [str(BENCH), str(BENCH.parent / "tests"), str(BENCH.parent / "tests/parity")]
from parity import session_cases
from public_decompile import RUNTIME_TOOL, listing_result

REFERENCE = """#include <string.h>

char *func0(char **strings, int count) {
    char *result = "";
    for (int i = 0; i < count; i++) {
        if (strlen(strings[i]) > strlen(result))
            result = strings[i];
    }
    return result;
}
"""


def main(control, workspace, answer):
    if control not in ("pass", "fail"):
        raise ValueError("expected pass or fail control")
    workspace.joinpath("recovered.c").write_text(
        REFERENCE if control == "pass" else REFERENCE.replace(" > ", " >= "))
    answer.write_text(json.dumps({"answers": [], "plan": None, "candidate": None}))
    # The grader derives tool evidence only from trial journals. Create the same
    # successful synthetic outcome for both controls to isolate the C verdict.
    directory = answer.parent / "home/.ansa/sessions/control"
    directory.mkdir(parents=True)
    call = {"operation": "1", "call": "c"}
    intent = {"kind": "intent", "turn": "t", "invocation": dict(call, name=RUNTIME_TOOL)}
    outcome = {"kind": "outcome", "turn": "t", "result": dict(
        call, dispatched=True, execution=0, output=json.dumps(listing_result()))}
    (directory / "part-00000000000000000000.log").write_bytes(session_cases.encode([intent, outcome]))


if __name__ == "__main__":
    main(sys.argv[1], Path(sys.argv[2]), Path(sys.argv[3]))
