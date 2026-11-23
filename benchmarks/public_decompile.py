#!/usr/bin/env python3
"""HumanEval-Decompile task 12: prepare, serve one MCP tool, and grade."""

import base64
import hashlib
import json
import os
from pathlib import Path
import shutil
import stat
import subprocess
import sys
import tempfile


TASK_IDS = ("humaneval-decompile-12-o2",)
HERE = Path(__file__).resolve()
FIXTURE = HERE.parent / "fixtures" / "humaneval12"
SOURCE = ("https://github.com/albertan017/LLM4Decompile/blob/"
          "85b364bf093eb2ee4f3687cfe38a203fca89f23e/legacy-test/decompile-eval-executable-gcc-obj.json")
OBJECT_SHA = "bc421cc9a074df3106e73441b609d5b1bab8e14ad809aa6db9429ebb4b53023f"
LISTING_SHA = "35776bf3762c0be3572c3da23c9287ac2b989cd8d208a0b80a50ac0124eb5ef9"
C_TEST_SHA = "831ddc9f294f661b273c127b100d0cb1e0eee0bd2ba0da3178bc3d5df289a294"
SERVER = "humaneval"
TOOL = "inspect_function"
RUNTIME_TOOL = "mcp_inspect_function_" + hashlib.sha256(f"{SERVER}/{TOOL}".encode()).hexdigest()[:16]
SIGNATURE = "char *func0(char **strings, int count);"


def checked(data, digest):
    if hashlib.sha256(data).hexdigest() != digest:
        raise ValueError("HumanEval-Decompile fixture digest mismatch")
    return data


def prepare(task_id, root):
    if task_id not in TASK_IDS:
        raise ValueError(f"unknown decompilation task: {task_id}")
    if not shutil.which("cc"):
        raise FileNotFoundError("native C compiler (cc) required to grade HumanEval-Decompile")
    root = root.resolve()
    workspace = root / "workspace"
    workspace.mkdir()
    binary = base64.b64decode((FIXTURE / "function.o.b64").read_bytes())
    (workspace / "function.o").write_bytes(checked(binary, OBJECT_SHA))
    checked((FIXTURE / "test.c").read_bytes(), C_TEST_SHA)
    checked((FIXTURE / "listing.txt").read_bytes(), LISTING_SHA)
    config = root / "mcp.json"
    config.write_text(json.dumps({"servers": {SERVER: {
        "connection": {"transport": "stdio", "command": sys.executable,
                       "args": ["-I", str(HERE), "--serve"]},
        "tools": [TOOL], "read_only": [TOOL],
    }}}))
    return {"id": task_id, "source": SOURCE + " (task_id=12, O2)",
            "category": "decompilation", "family": "humaneval-decompile-12", "split": "holdout",
            "oracle_fixture": str(FIXTURE / "control.py"),
            "turns": ["Use the inspect_function MCP tool to analyze func0 in function.o. "
                      "Write its behaviorally equivalent C implementation to recovered.c. "
                      f"The required declaration is {SIGNATURE}"],
            "modes": ["work"], "workspace": str(workspace), "verifier": str(HERE),
            "verifier_inputs": [str(FIXTURE / "test.c"), str(FIXTURE / "listing.txt")],
            "mcp_config": str(config)}


def listing_result():
    listing = checked((FIXTURE / "listing.txt").read_bytes(), LISTING_SHA).decode()
    return {"content": [{"type": "text", "text": f"SHA-256 {OBJECT_SHA}\n{listing}"}]}


def serve():
    # This one-object service also runs on ARM hosts without an x86 disassembler.
    for line in sys.stdin:
        message = json.loads(line)
        if "id" not in message:
            continue
        method = message.get("method")
        response = {"jsonrpc": "2.0", "id": message["id"]}
        if method == "initialize":
            response["result"] = {"protocolVersion": "2025-11-25", "capabilities": {"tools": {}},
                                  "serverInfo": {"name": SERVER, "version": "1"}}
        elif method == "tools/list":
            response["result"] = {"tools": [{"name": TOOL, "description": "Inspect func0 in function.o.",
                                             "inputSchema": {"type": "object", "properties": {},
                                                             "additionalProperties": False}}]}
        elif method == "tools/call" and message.get("params") == {"name": TOOL, "arguments": {}}:
            try:
                flags = os.O_RDONLY | os.O_NONBLOCK | os.O_NOFOLLOW
                with os.fdopen(os.open("function.o", flags), "rb") as stream:
                    if not stat.S_ISREG(os.fstat(stream.fileno()).st_mode):
                        raise ValueError("expected regular object")
                    checked(stream.read(1449), OBJECT_SHA)  # 1448 bytes plus EOF check
                response["result"] = listing_result()
            except (OSError, ValueError):
                response["error"] = {"code": -32602, "message": "pinned object unavailable"}
        else:
            response["error"] = {"code": -32601, "message": "unsupported method or tool"}
        print(json.dumps(response), flush=True)


def has_inspection(traces):
    expected = listing_result()
    for events in traces.values():
        pending = set()
        for event in events:
            if event["kind"] == "intent":
                call = event["invocation"]
                if call["name"] == RUNTIME_TOOL:
                    pending.add((event["turn"], call["operation"], call["call"]))
            elif event["kind"] == "outcome":
                call = event["result"]
                key = (event["turn"], call["operation"], call["call"])
                if key in pending:
                    pending.remove(key)
                    if call["dispatched"] and call["execution"] == 0 and json.loads(call["output"]) == expected:
                        return True
    return False


def grade(workspace, answer_path):
    sys.path.insert(0, str(HERE.parents[1] / "tests"))
    from carbon import journals

    if not has_inspection(journals(answer_path.parent / "home")):
        print("No successful inspect_function MCP outcome in this trial's journals.")
        return 1
    recovered = workspace / "recovered.c"
    if not recovered.exists() or not stat.S_ISREG(recovered.lstat().st_mode):
        print("Expected a regular recovered.c.")
        return 1
    checked((FIXTURE / "test.c").read_bytes(), C_TEST_SHA)
    with tempfile.TemporaryDirectory() as directory:
        build = Path(directory)
        (build / "signature.h").write_text(SIGNATURE + "\n")
        # Separate translation units keep candidate macros away from the assertions.
        compile_result = subprocess.run(["cc", "-std=c11", "-include", "signature.h",
                                         str(recovered.resolve()), str(FIXTURE / "test.c"), "-o", "check"],
                                        cwd=build)
        if compile_result.returncode:
            return 1
        try:
            passed = subprocess.run([build / "check"], cwd=build, timeout=5).returncode == 0
        except subprocess.TimeoutExpired:
            passed = False
        return 0 if passed else 1


if __name__ == "__main__":
    try:
        if sys.argv[1:] == ["--serve"]:
            serve()
        else:
            sys.exit(grade(*(Path(arg) for arg in sys.argv[1:])))
    except (OSError, ValueError, KeyError) as error:
        print(error, file=sys.stderr)
        sys.exit(2)
