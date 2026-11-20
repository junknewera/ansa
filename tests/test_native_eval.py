"""Synthetic acceptance for the offline native comparison entry point."""

import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

import native_eval as evaluation


class NativeEvalTests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)
        self.manifest = self.root / "manifest.json"
        self.receipts = self.root / "receipts"
        self.receipts.mkdir()
        prompt = b"Write the verified status.\n"
        (self.root / "prompt.txt").write_bytes(prompt)
        self.answer = b"Status: verified\n"
        budget = dict.fromkeys(evaluation.COUNTERS, 8)
        budget.update(dict.fromkeys(evaluation.TOKENS))
        self.spec = {
            "version": 1, "comparison": "orchestration", "evidence": "synthetic",
            "seed": "test-seed", "repetitions": 1,
            "provenance": {key: evaluation.sha(key.encode()) for key in evaluation.HASHES}
            | {"route": "synthetic", "model": "synthetic", "effort": "low"},
            "variants": {key: evaluation.sha(key.encode()) for key in ("N0", "N1", "N2")},
            "budget": budget,
            "total_budget": {key: value * 3 if value is not None else None
                             for key, value in budget.items()},
            "tasks": [{"id": "status", "split": "holdout",
                       "inputs": {"prompt.txt": evaluation.sha(prompt)},
                       "checks": {"report.txt": evaluation.sha(self.answer)}}],
        }

    def invoke(self, command):
        args = [sys.executable, "-B", evaluation.__file__, command, self.manifest]
        if command == "report":
            args.append(self.receipts)
        run = subprocess.run(args, capture_output=True, text=True, check=False)
        return run.returncode, json.loads(run.stdout) if run.stdout else run.stderr

    def plan(self):
        self.manifest.write_text(json.dumps(self.spec))
        code, plan = self.invoke("plan")
        self.assertEqual(code, 0, plan)
        return plan

    def capture(self, plan, arm, *, answer=None, **changes):
        run_id = next(row["run_id"] for row in plan["schedule"] if row["arm"] == arm)
        run = self.receipts / run_id
        (run / "artifacts").mkdir(parents=True)
        (run / "stages").mkdir()
        stages = []
        for role in evaluation.STAGES[arm]:
            session = None if role == "verification" else f"{arm}-{role}"
            raw = json.dumps({"role": role, "synthetic": True, "session": session}).encode()
            (run / "stages" / f"{role}.json").write_bytes(raw)
            stages.append({"role": role, "session": session, "sha256": evaluation.sha(raw)})
        receipt = {
            "manifest_sha256": plan["manifest_sha256"], "evaluator_sha256": plan["evaluator_sha256"],
            "run_id": run_id, "variant_sha256": self.spec["variants"][arm], "workspace": arm,
            "provenance": self.spec["provenance"], "outcome": "completed", "stages": stages,
            "usage": dict.fromkeys(evaluation.COUNTERS, 2) | dict.fromkeys(evaluation.TOKENS),
        }
        receipt.update(changes)
        (run / "receipt.json").write_text(json.dumps(receipt))
        if answer is not None:
            (run / "artifacts" / "report.txt").write_bytes(answer)
        return run

    def test_same_oracle_independently_grades_all_three_arms(self):
        plan = self.plan()
        self.assertEqual(self.invoke("plan"), (0, plan))
        self.capture(plan, "N0", answer=self.answer)
        self.capture(plan, "N1", answer=b"Wrong bytes despite completed receipt.\n")
        self.capture(plan, "N2")
        code, report = self.invoke("report")
        self.assertEqual(code, 0, report)
        self.assertEqual(report["evidence"], "synthetic")
        self.assertEqual({row["arm"]: row["status"] for row in report["schedule"]},
                         {"N0": "accepted", "N1": "rejected", "N2": "rejected"})
        self.assertIsNone(report["total_usage"]["reported_tokens"])
        self.assertIsNone(report["summary"]["N2"]["holdout"]["calls_per_accepted"])

    def test_preserves_infrastructure_error_timeout_and_missing_trial(self):
        plan = self.plan()
        self.capture(plan, "N0", outcome="infra_error", stages=[])
        self.capture(plan, "N1", outcome="timeout", stages=[])
        code, report = self.invoke("report")
        self.assertEqual(code, 1, report)
        self.assertEqual({row["arm"]: row["status"] for row in report["schedule"]},
                         {"N0": "infra_error", "N1": "timeout", "N2": "missing"})
        self.assertIsNone(report["total_usage"]["calls"])

    def test_rejects_wrong_model_missing_review_and_changed_stage(self):
        plan = self.plan()
        self.capture(plan, "N0", answer=self.answer,
                     provenance=self.spec["provenance"] | {"model": "other"})
        run = self.capture(plan, "N1", answer=self.answer)
        (run / "stages" / "planner.json").write_bytes(b"changed")
        self.capture(plan, "N2", answer=self.answer, stages=[])
        code, report = self.invoke("report")
        self.assertEqual(code, 1, report)
        self.assertEqual({row["arm"]: row["error"] for row in report["schedule"]},
                         {"N0": "receipt identity mismatch", "N1": "changed stage evidence",
                          "N2": "missing stage"})

    def test_reports_per_trial_and_aggregate_budget_failures(self):
        self.spec["budget"]["reported_tokens"] = 5
        self.spec["total_budget"]["calls"] = 4
        plan = self.plan()
        usage = dict.fromkeys(evaluation.COUNTERS, 2) | dict.fromkeys(evaluation.TOKENS)
        self.capture(plan, "N0", answer=self.answer)
        self.capture(plan, "N1", answer=self.answer, usage=usage | {"reported_tokens": 6})
        self.capture(plan, "N2", answer=self.answer, usage=usage | {"reported_tokens": 4})
        code, report = self.invoke("report")
        self.assertEqual(code, 1, report)
        self.assertEqual({row["arm"]: row["status"] for row in report["schedule"]},
                         {"N0": "budget_unproven", "N1": "budget_exceeded", "N2": "accepted"})
        self.assertEqual(report["total_budget"]["exceeded"], ["calls"])

    def test_rejects_shared_workspace_or_session(self):
        plan = self.plan()
        self.capture(plan, "N0", answer=self.answer, workspace="shared")
        self.capture(plan, "N1", answer=self.answer, workspace="shared")
        run = self.capture(plan, "N2", answer=self.answer)
        receipt_path = run / "receipt.json"
        receipt = json.loads(receipt_path.read_bytes())
        receipt["stages"][-1]["session"] = "N2-builder"
        receipt_path.write_text(json.dumps(receipt))
        code, report = self.invoke("report")
        self.assertEqual(code, 1, report)
        self.assertEqual({row["arm"]: row["error"] for row in report["schedule"]},
                         {"N0": "reused workspace identity", "N1": "reused workspace identity",
                          "N2": "reused session identity"})

    def test_plan_rejects_changed_frozen_input(self):
        self.plan()
        (self.root / "prompt.txt").write_bytes(b"changed")
        code, error = self.invoke("plan")
        self.assertEqual(code, 2)
        self.assertIn("changed fixture input", error)

    def test_profile_mode_leaves_baseline_eligible(self):
        self.spec["comparison"] = "profile"
        self.spec["variants"] = {key: evaluation.sha(key.encode())
                                 for key in ("baseline", "candidate")}
        plan = self.plan()
        self.capture(plan, "baseline", answer=self.answer)
        self.capture(plan, "candidate", answer=b"regression")
        code, report = self.invoke("report")
        self.assertEqual(code, 0, report)
        self.assertEqual(report["summary"]["baseline"]["holdout"]["accepted"], 1)
        self.assertEqual(report["summary"]["candidate"]["holdout"]["accepted"], 0)


if __name__ == "__main__":
    unittest.main()
