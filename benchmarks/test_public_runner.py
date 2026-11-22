"""Offline regression checks for public task input preparation and unsupported rows."""

import contextlib
import io
import json
from pathlib import Path
import tempfile
import time
import unittest
from unittest.mock import patch

from bench import load, main, run_trial, write
from carbon import TrialStopped, check_run_status


class PublicInputs(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name)
        self.manifest = self.root / "manifest.json"
        (self.root / "inputs").mkdir()
        self.task = {"id": "case", "turns": ["hello"], "verifier": "/bin/true",
                     "workspace": "inputs", "modes": ["work"]}
        self.arm = {"id": "base", "model": "fixture", "binary": "/bin/true",
                    "route": "offline", "mode": "work"}
        self.spec = {"version": 1, "seed": "demo", "repetitions": 1, "arms": [self.arm],
                     "tasks": [self.task], "budget": {"tool_calls": 2, "tool_rounds": 1,
                     "trial_seconds": 3, "verifier_seconds": 2, "total_seconds": 5}}

    def freeze(self):
        write(self.manifest, self.spec)
        return load(self.manifest)[:3]

    def test_orchestration_reports_terminal_status_before_timeout(self):
        for status, expected in (("Plan stopped", "failed"),
                                 ("Plan stopped at the approved scope", "failed"),
                                 ("Plan stopped (orchestration_check_effect_unknown)", "infra_error")):
            with self.subTest(status=status), self.assertRaises(TrialStopped) as stopped:
                check_run_status("  " + status)
            self.assertEqual((stopped.exception.status, str(stopped.exception)), (expected, status))
        check_run_status("Ctrl+F Files")

    def test_unsupported_image_is_visible_and_never_dispatched(self):
        image = self.root / "plot.png"
        image.write_bytes(b"fixture")
        self.task["attachments"] = [{"turn": 0, "path": str(image)}]
        self.freeze()
        output = self.root / "out"
        with patch("bench.run_trial", side_effect=AssertionError), contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(main([str(self.manifest), "--output", str(output)]), 1)
        trial, = json.loads((output / "report.json").read_text())["trials"]
        self.assertEqual((trial["status"], trial["mode"], trial["model"]), ("unsupported", "work", "fixture"))

    def test_git_history_and_task_mcp_are_ready_before_capture(self):
        setup = self.root / "seed.sh"
        setup.write_text("#!/bin/sh\nset -eu\ngit branch recovered HEAD\n")
        setup.chmod(0o700)
        config = self.root / "mcp.json"
        config.write_text('{"servers":[]}\n')
        self.task.update(git_setup="seed.sh", mcp_config="mcp.json")
        arms, tasks, budget = self.freeze()
        def inspect(binary, root, home, workspace, env, arm, turns, deadline, git, head, fixture_args):
            self.assertEqual(git(workspace, "rev-parse", "recovered").strip(), head)
            self.assertEqual((home / ".ansa/mcp.json").read_bytes(), config.read_bytes())
            raise TrialStopped("pass", "fixture inspected")
        trial = self.root / "trial"
        trial.mkdir()
        with patch("bench.capture", side_effect=inspect):
            result = run_trial(trial, arms["base"], tasks["case"], budget, time.monotonic() + 5)
        self.assertEqual(result["status"], "pass", result)

    def test_conflicting_mcp_and_invalid_turn_are_rejected(self):
        (self.root / "mcp.json").write_text("{}")
        self.task["mcp_config"] = self.arm["mcp_config"] = "mcp.json"
        with self.assertRaisesRegex(ValueError, "either task or arm"):
            self.freeze()
        del self.arm["mcp_config"]
        self.task["attachments"] = [{"turn": 1, "path": "mcp.json"}]
        with self.assertRaisesRegex(ValueError, "integer"):
            self.freeze()


if __name__ == "__main__":
    unittest.main()
