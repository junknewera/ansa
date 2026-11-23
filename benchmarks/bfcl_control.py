"""Check a prepared BFCL adapter via its configured stdio server and journal verifier."""

import json
from pathlib import Path
import subprocess
import sys

from bfcl_runtime import gold_call, local_name
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "tests/parity"))
import session_cases


def make_control(operator, control, answers):
    if control not in {"pass", "fail"}:
        raise ValueError("expected pass or fail oracle control")
    scenario = json.loads((operator / "scenario.json").read_text())
    gold = json.loads((operator / "gold.json").read_text())
    config = json.loads((operator.parent / "mcp.json").read_text())["servers"]["bfcl"]["connection"]
    parts = answers.parent / "home/.ansa/sessions/bfcl-test"
    parts.mkdir(parents=True)
    answers.write_text(json.dumps({"answers": ["Done.", "Moved.", "Found.", "How many lines?", "Done."]}))
    events = []
    with subprocess.Popen([config["command"], *config["args"]], stdin=subprocess.PIPE,
                          stdout=subprocess.PIPE, text=True) as server:
        seq = 0

        def rpc(method, params):
            nonlocal seq
            seq += 1
            server.stdin.write(json.dumps({"jsonrpc": "2.0", "id": seq, "method": method,
                                           "params": params}) + "\n")
            server.stdin.flush()
            response = json.loads(server.stdout.readline())
            if response["id"] != seq:
                raise AssertionError("wrong RPC id")
            return response["result"]

        rpc("initialize", {"protocolVersion": "2025-11-25"})
        catalog = rpc("tools/list", {})["tools"]
        if [tool["name"] for tool in catalog] != [t["name"] for t in scenario["tools"]]:
            raise AssertionError("BFCL catalog differs")
        # An invalid write into a directory raises inside the pinned simulator.
        # Exercise recovery before the first observed turn.
        rejected = rpc("tools/call", {"name": "echo", "arguments": {"content": "test", "file_name": "workspace"}})
        if rejected.get("isError") is not True:
            raise AssertionError("BFCL bad write did not produce a tool error")
        pwd = rpc("tools/call", {"name": "pwd", "arguments": {}})
        if json.loads(pwd["content"][0]["text"]) != {"current_working_directory": "/alex"}:
            raise AssertionError("BFCL changed state after a rejected write")
        for index, (turn, required) in enumerate(zip(scenario["turns"], gold, strict=True)):
            events.append({"kind": "accepted", "turn": str(index), "text": turn})
            for expression in required:
                name, args = gold_call(expression)
                if control == "fail" and name == "tail":
                    args["lines"] = 10
                reply = rpc("tools/call", {"name": name, "arguments": args})
                ids = {"call": str(seq), "operation": f"op-{seq}"}
                events.append({"kind": "intent", "turn": str(index), "invocation": ids | {
                    "name": local_name(name), "arguments": {"arguments_json": json.dumps(args)}}})
                events.append({"kind": "outcome", "turn": str(index), "result": ids | {
                    "dispatched": True, "execution": 0, "output": json.dumps(reply)}})
        server.stdin.close()
    (parts / "part-00000000000000000000.log").write_bytes(session_cases.encode(events))
