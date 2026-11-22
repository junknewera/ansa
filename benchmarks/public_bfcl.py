"""Prepare the pinned five-turn BFCL filesystem scenario for Carbon Work."""

import hashlib
import json
from pathlib import Path
import sys
from urllib.request import urlopen

TASK_IDS = ("multi_turn_miss_param_1",)
REVISION = "6ea57973c7a6097fd7c5915698c54c17c5b1b6c8"
PREFIX = "berkeley-function-call-leaderboard/bfcl_eval"
PROJECT = f"https://github.com/ShishirPatil/gorilla/blob/{REVISION}/{PREFIX}"
RAW = f"https://raw.githubusercontent.com/ShishirPatil/gorilla/{REVISION}/{PREFIX}"
DATA = "data/BFCL_v4_multi_turn_miss_param.json"
GOLD = "data/possible_answer/BFCL_v4_multi_turn_miss_param.json"
DOC = "data/multi_turn_func_doc/gorilla_file_system.json"
CODE = "eval_checker/multi_turn_eval/func_source_code"
HASHES = {
    DATA: "f0c66dda3795f5f53e3e1c0cc8ba0246b6761c8f58bdba8317203bf451ab8838",
    GOLD: "59c442901779e2c31c33abcd566d032e03736e5ad8069de2fe05489873046ecf",
    DOC: "c4c1b741c71e2a17c97a5dc9c4a91d89978c4eaece56494d14272d5df6c650e9",
    f"{CODE}/gorilla_file_system.py": "84173750930b09aaa31b90547c9bd79ce75714682608b35e78a9c1ea259faf2b",
    f"{CODE}/long_context.py": "5c9a5710115609c779e5b2ccb43f6ad58271ed96f2ea7a797edb0d65c385aa38",
}


def fetch(name):
    with urlopen(f"{RAW}/{name}", timeout=30) as response:
        data = response.read(1024 * 1024)
    if hashlib.sha256(data).hexdigest() != HASHES[name]:
        raise ValueError(f"BFCL content hash differs: {name}")
    return data


def selected(data, task_id):
    row, = [row for row in map(json.loads, data.splitlines()) if row["id"] == task_id]
    return row


def prepare(task_id, root):
    if task_id not in TASK_IDS:
        raise ValueError(f"unknown BFCL task: {task_id}")
    root = Path(root).resolve()
    operator = root / "operator"
    operator.mkdir()
    inputs = root / "inputs"
    inputs.mkdir()
    assets = {path: fetch(path) for path in HASHES}
    source = selected(assets[DATA], task_id)
    turns = [messages[0]["content"] for messages in source["question"]]
    tools = []
    for tool in map(json.loads, assets[DOC].splitlines()):
        if tool["name"] in source["excluded_function"]:
            continue
        schema = tool["parameters"] | {"type": "object", "additionalProperties": False}
        tools.append({"name": tool["name"], "description": tool["description"], "inputSchema": schema})
    fixture = {"turns": turns, "initial": source["initial_config"]["GorillaFileSystem"], "tools": tools}
    (operator / "scenario.json").write_text(json.dumps(fixture))
    (operator / "gold.json").write_text(json.dumps(selected(assets[GOLD], task_id)["ground_truth"]))
    module_dir = operator / "bfcl_eval" / CODE
    module_dir.mkdir(parents=True)
    for name in ("gorilla_file_system.py", "long_context.py"):
        (module_dir / name).write_bytes(assets[f"{CODE}/{name}"])
    # Make these regular packages so an installed BFCL release cannot shadow the pin.
    while module_dir != operator:
        (module_dir / "__init__.py").touch()
        module_dir = module_dir.parent

    # Preserve the original user messages. Workspace instructions identify their virtual machine.
    (inputs / "AGENTS.md").write_text(
        "The user's filesystem is hosted by the GorillaFileSystem MCP tools. "
        "Use these tools for file operations in this conversation.\n"
    )
    here = Path(__file__).resolve().parent
    runtime = here / "bfcl_runtime.py"
    config = root / "mcp.json"
    config.write_text(json.dumps({"servers": {"bfcl": {
        "connection": {"transport": "stdio", "command": sys.executable,
                       "args": ["-IB", str(runtime), str(operator)]},
        "tools": [tool["name"] for tool in tools],
    }}}))
    verifier = operator / "verify"
    control = operator / "control"
    launch = (f"#!{sys.executable} -IB\nimport sys\nfrom pathlib import Path\n"
              f"sys.path.insert(0, {str(here)!r})\n")
    verifier.write_text(launch + "from bfcl_runtime import verify\n"
                        "sys.exit(verify(Path(__file__).parent, Path(sys.argv[2])))\n")
    control.write_text(launch + "from bfcl_control import make_control\n"
                       "make_control(Path(__file__).parent, sys.argv[1], Path(sys.argv[3]))\n")
    verifier.chmod(0o700)
    control.chmod(0o700)
    tests = here.parent / "tests"
    deps = [runtime, here / "bfcl_control.py", config, here / "carbon.py", tests / "carbon_workspace_pty.py", tests / "cli_progress_pty.py",
            *(tests / "parity" / f for f in ("frontend_io.py", "session_cases.py", "protocol.py", "protocol_network.py", "run.py"))]
    deps += [p for p in operator.rglob("*") if p.is_file() and p not in (verifier, control)]
    return {"id": task_id, "category": "dialogue", "family": "bfcl-alex-filesystem", "split": "holdout",
            "oracle_fixture": str(control), "modes": ["work"], "workspace": str(inputs), "turns": turns,
            "source": f"{PROJECT}/{DATA}", "mcp_config": str(config), "verifier": str(verifier),
            "verifier_inputs": [str(p) for p in sorted(deps)]}
