import json
from pathlib import Path
from tempfile import TemporaryDirectory
import unittest

from run_evidence import open_frozen_plan


class FrozenRunTest(unittest.TestCase):
    def setUp(self):
        tmp = TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.run = Path(tmp.name) / "orchestration" / "parent-01" / ("ab" * 16)
        self.plan = {
            "schema_version": 1, "summary": "Week plan", "steps": [
                {"id": "offset", "task": "Correct Sunday", "access": "write",
                 "paths": ["src/week.py"], "depends_on": [], "acceptance": "Sunday groups correctly"}
            ], "command_checks": [{"argv": ["pytest", "-q"]}],
        }
        self.header = {
            "version": 1, "identity": {"parent_session": "parent-01", "run": self.run.name},
            "plan": json.dumps(self.plan), "budget": {"started_at": 1, "deadline": 50, "submissions": 8},
        }
        for folder in (self.run.parent.parent, self.run.parent, self.run, self.run / "ledger"):
            folder.mkdir(mode=0o700)
        self.init = self.run / "ledger" / "init.json"
        self.init.write_text(json.dumps(self.header))
        self.init.chmod(0o600)

    def view(self, name):
        folder = self.run / name
        if not folder.parent.exists():
            folder.parent.mkdir(mode=0o700)
        folder.mkdir(mode=0o700)
        (folder / "approved-plan.json").write_text(json.dumps(self.plan))
        (folder / "ansa-candidate.diff").write_text("@@ -1 +1 @@\n-old\n+new\n")
        for file in folder.iterdir():
            file.chmod(0o600)
        return folder / "approved-plan.json"

    def test_standard_review_exposes_planned_checks_without_inventing_results(self):
        path = self.view("verification-1")
        state = open_frozen_plan(path, "parent-01")
        self.assertEqual(state["checks"], [("pytest -q", "specified only")])
        self.assertEqual(state["mode"], "Orchestrate")
        self.assertEqual(state["agents"], [])
        self.assertFalse(state["pr_eligible"])
        self.assertEqual(state["documents"][0]["diff"], "@@ -1 +1 @@\n-old\n+new\n")
        self.assertEqual(state["plan"], ["offset · write · Correct Sunday"])

    def test_protection_matches_the_native_reader(self):
        path = self.view("verification-0")
        path.chmod(0o644)
        with self.assertRaisesRegex(ValueError, "permissions"):
            open_frozen_plan(path)
        path.chmod(0o600)
        path.parent.chmod(0o755)
        with self.assertRaisesRegex(ValueError, "private"):
            open_frozen_plan(path)

    def test_plus_review_records_historical_outcomes_and_clarifications(self):
        path = self.view("plus-review-views/candidate-0")
        (path.parent.parent / "approved-request.json").write_text(json.dumps({
            "user_request": "Correct Sunday", "user_clarifications": ["Keep CSV"],
            "approved_clarification_decisions": [],
            "approved_task_results": {"offset": {"worker_session": "child", "native_tool_evidence": None,
                "result": {"status": "completed", "summary": "Offset changed", "findings": []}}},
        }))
        (path.parent.parent / "approved-request.json").chmod(0o600)
        state = open_frozen_plan(path)
        self.assertEqual(state["agents"], [("offset", "completed · Offset changed")])
        self.assertEqual(state["clarifications"], [("Keep CSV", "in reviewer input")])
        self.assertEqual(state["mode"], "Orchestrate+")
        self.assertFalse(state["can_clarify"])

    def test_rejects_cross_session_changed_snapshot_and_symlink(self):
        path = self.view("verification-0")
        with self.assertRaises(ValueError):
            open_frozen_plan(path, "other-parent")
        self.header["identity"]["parent_session"] = "other-parent"
        self.init.write_text(json.dumps(self.header))
        with self.assertRaises(ValueError):
            open_frozen_plan(path)
        self.header["identity"]["parent_session"] = "parent-01"
        self.init.write_text(json.dumps(self.header))
        path.write_text("{}")
        with self.assertRaises(ValueError):
            open_frozen_plan(path)
        path.rename(path.with_name("target"))
        path.symlink_to(path.with_name("target"))
        with self.assertRaises(ValueError):
            open_frozen_plan(path)


if __name__ == "__main__":
    unittest.main()
