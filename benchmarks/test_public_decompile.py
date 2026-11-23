import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

import public_decompile as task

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "tests"))
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "tests/parity"))
from parity import session_cases


class DecompileTest(unittest.TestCase):
    def test_stdio_and_grader(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            spec = task.prepare(task.TASK_IDS[0], root)
            workspace = Path(spec["workspace"])
            server = json.loads(Path(spec["mcp_config"]).read_text())["servers"][task.SERVER]
            command = server["connection"]
            requests = [
                {"id": 1, "method": "initialize"},
                {"method": "notifications/initialized"},
                {"id": 2, "method": "tools/list"},
                {"id": 3, "method": "tools/call", "params": {"name": task.TOOL, "arguments": {}}},
            ]
            messages = "\n".join(json.dumps({"jsonrpc": "2.0", **r}) for r in requests)
            run = subprocess.run([command["command"], *command["args"]], cwd=workspace,
                                 input=messages, text=True, capture_output=True, check=True)
            init, catalog, result = map(json.loads, run.stdout.splitlines())
            self.assertEqual(init["result"]["protocolVersion"], "2025-11-25")
            self.assertEqual(catalog["result"]["tools"][0]["name"], task.TOOL)
            self.assertIn("strlen", result["result"]["content"][0]["text"])

            journals = root / "home/.ansa/sessions/s1"
            journals.mkdir(parents=True)
            part = journals / "part-00000000000000000000.log"
            call = {"operation": "1", "call": "c"}
            intent = {"kind": "intent", "turn": "t", "invocation": dict(call, name=task.RUNTIME_TOOL)}
            outcome = {"kind": "outcome", "turn": "t",
                       "result": dict(call, dispatched=True, execution=0, output=json.dumps(result["result"]))}
            part.write_bytes(session_cases.encode([intent, outcome]))
            reference = ('#include <string.h>\nchar *func0(char **s, int n) {char *v="";'
                         'for(int i=0;i<n;++i) if(strlen(s[i])>strlen(v)) v=s[i]; return v;}')
            candidate = workspace / "recovered.c"
            candidate.write_text(reference)
            argv = [spec["verifier"], str(workspace), str(root / "answer.json")]

            def grade():
                return subprocess.run(argv, cwd=workspace, capture_output=True).returncode

            self.assertEqual(grade(), 0)
            candidate.write_text(reference.replace(">strlen(v)", ">=strlen(v)"))
            self.assertEqual(grade(), 1, "upstream assertions must reject last-wins ties")
            candidate.write_text(reference)
            outcome["result"]["call"] = "another-call"
            part.write_bytes(session_cases.encode([intent, outcome]))
            self.assertEqual(grade(), 1, "unmatched journal outcomes must fail")


if __name__ == "__main__":
    unittest.main()
