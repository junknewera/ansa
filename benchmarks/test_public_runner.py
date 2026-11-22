"""Offline regression checks for public task input preparation and unsupported rows."""

import base64
import contextlib
import hashlib
import io
import json
from pathlib import Path
import tempfile
import time
import unittest
from unittest.mock import patch

from bench import load, main, run_trial, write
from carbon import TrialStopped, check_run_status, completed_turn, prepare_turn_images, verify_turn_images


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

    def test_provider_capability_or_transport_errors_do_not_score_as_losses(self):
        for reason, status in (("siwc_image_unsupported", "unsupported"),
                               ("openrouter_image_unsupported", "unsupported"),
                               ("provider_usage_limit", "infra_error"), ("http_timeout", "infra_error"),
                               ("tool_turn_limit", "failed")):
            events = [{"kind": "accepted"}, {"kind": "pause", "diagnostic": reason}]
            with self.subTest(reason=reason), self.assertRaises(TrialStopped) as stopped:
                completed_turn(events, lambda _: True)
            self.assertEqual(stopped.exception.status, status)

    def test_image_schedule_binds_capability_to_each_arm(self):
        f = self.root / "plot.png"
        f.write_bytes(b"PNG")
        self.task["attachments"] = [{"turn": 0, "path": str(f)}]
        self.task["modes"] = ["work", "plan", "planner"]
        self.spec["arms"].extend([
            self.arm | {"id": "legacy", "route": "siwc", "auth_dir": "inputs"},
            self.arm | {"id": "siwc", "route": "siwc", "auth_dir": "inputs", "image_input": True},
            self.arm | {"id": "plan", "route": "openrouter", "auth_dir": "inputs",
                        "mode": "plan", "image_input": True},
            self.arm | {"id": "planner", "route": "siwc", "auth_dir": "inputs",
                        "mode": "planner", "image_input": True},
            self.arm | {"id": "orch", "route": "siwc", "auth_dir": "inputs",
                        "mode": "orchestrate", "image_input": True}])
        self.freeze()
        rows = {row["arm"]: row for row in load(self.manifest)[3]}
        self.assertEqual({name for name, row in rows.items() if "unsupported" not in row}, {"siwc", "plan"})
        self.assertIn("this task", rows["orch"]["unsupported"])
        self.assertEqual(rows["plan"]["route"], "openrouter")
        self.assertEqual(rows["legacy"]["model"], "fixture")
        self.spec["arms"][1]["image_input"] = 1
        with self.assertRaisesRegex(ValueError, "boolean"):
            self.freeze()

    def test_frozen_images_match_order_and_exact_provider_input(self):
        data = b"\xff\xd8\xff" + b"jpeg-bytes"
        file = self.root / "public source.jpg"
        file.write_bytes(data)
        sha = hashlib.sha256(data).hexdigest()
        attachment = {"turn": 1, "path": str(file), "sha256": sha}
        empty, images = prepare_turn_images(self.root, [attachment], ["hello", "describe"])
        self.assertFalse(empty)
        self.assertEqual(images[0]["path"].read_bytes(), data)
        self.assertNotEqual(images[0]["path"], file)
        url = "data:image/jpeg;base64," + base64.b64encode(data).decode()
        parts = [{"type": "input_text", "text": "describe"}, {"type": "input_image", "image_url": url}]
        history = [{"role": "user", "content": parts}, {"type": "function_call_output"}]
        response = {"response": {"request": {"input": history}}}
        verify_turn_images(images, "describe", {"image_sha256": [sha]}, response)
        with self.assertRaisesRegex(ValueError, "accepted image hashes"):
            verify_turn_images(images, "describe", {}, response)
        history.append({"role": "user", "content": "follow up"})
        verify_turn_images([], "follow up", {}, response)
        history.pop()
        parts[1]["image_url"] = url[:-4] + "AAAA"
        with self.assertRaisesRegex(ValueError, "differ"):
            verify_turn_images(images, "describe", {"image_sha256": [sha]}, response)
        sub = self.root / "second"
        sub.mkdir()
        file.write_bytes(b"another image")
        with self.assertRaisesRegex(ValueError, "image input changed"):
            prepare_turn_images(sub, [attachment], ["hello", "describe"])

    def test_git_history_and_task_mcp_are_ready_before_capture(self):
        setup = self.root / "seed.sh"
        setup.write_text("#!/bin/sh\nset -eu\ngit branch recovered HEAD\n")
        setup.chmod(0o700)
        config = self.root / "mcp.json"
        config.write_text('{"servers":[]}\n')
        self.task.update(git_setup="seed.sh", mcp_config="mcp.json")
        arms, tasks, budget = self.freeze()
        def inspect(binary, root, home, workspace, env, arm, turns, deadline, git, head, fixture_args, attachments=()):
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
