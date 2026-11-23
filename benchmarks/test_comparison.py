"""Offline strict A/B examples and regressions. Run with python -B benchmarks/test_comparison.py."""

import contextlib
import copy
import io
import json
from pathlib import Path
import shutil
import sys
import tempfile
import unittest
from unittest.mock import patch

import bench
import comparison


class ComparisonTest(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)
        (self.root / "inputs").mkdir()
        (self.root / "candidate.md").write_text("Check the output file.")
        self.executable("verify", """import json, sys
from pathlib import Path
answer = json.loads(Path(sys.argv[2]).read_text())
sys.exit(int(answer["answers"] != ["42"]))
""")
        self.executable("fixture", """import json, sys
from pathlib import Path
expected, workspace, answer = sys.argv[1:]
Path(answer).write_text(json.dumps({"answers": ["42" if expected == "pass" else "41"]}))
""")
        arm = {"binary": sys.executable, "route": "offline", "model": "fixture", "mode": "work"}
        task = {"turns": ["What is the answer?"], "workspace": "inputs", "verifier": "verify",
                "oracle_fixture": "fixture", "modes": ["work"], "category": "example"}
        self.spec = {"version": 1, "repetitions": 1, "seed": "example",
                     "arms": [arm | {"id": "a"}, arm | {"id": "b", "guidance": "candidate.md"}],
                     "budget": {"total_seconds": 30, "trial_seconds": 5, "verifier_seconds": 5,
                                "tool_calls": 2, "tool_rounds": 2},
                     "tasks": [task | {"id": "one", "family": "one", "split": "development"},
                               task | {"id": "two", "family": "two", "split": "holdout"}],
                     "comparison": {"left": "a", "right": "b", "factor": "guidance"}}

    def executable(self, name, content):
        path = self.root / name
        path.write_text(f"#!{sys.executable}\n" + content)
        path.chmod(0o700)

    def manifest(self, spec):
        path = self.root / "manifest.json"
        bench.write(path, spec)
        return path

    def test_split_one_factor_and_frozen_controls(self):
        for mutation, message in (
                (lambda s: s["arms"][0].update(model="different"), "alone to differ"),
                (lambda s: s["tasks"][1].update(family="one"), "leaks across"),
                (lambda s: s["tasks"][1].pop("oracle_fixture"), "positive/negative"),
                (lambda s: s["tasks"][1].update(attachments=[{"turn": 0, "path": "candidate.md"}]),
                 "each split needs a supported task")):
            spec = copy.deepcopy(self.spec)
            mutation(spec)
            with self.assertRaisesRegex(ValueError, message):
                bench.load(self.manifest(spec))

    def test_unsupported_images_remain_ungraded_without_oracle_fixture(self):
        image = self.spec["tasks"][0] | {"id": "image", "family": "figure",
                                         "attachments": [{"turn": 0, "path": "candidate.md"}]}
        del image["oracle_fixture"]
        self.spec["tasks"].append(image)
        out = self.root / "images"
        with patch("bench.run_trial", return_value={"status": "pass"}):
            self.assertEqual(bench.main([str(self.manifest(self.spec)), "--output", str(out)]), 0)
        report = json.loads((out / "report.json").read_text())
        self.assertEqual(report["paired"], {"a:b": {"ungraded": 1, "both_pass": 1}})
        self.assertFalse((out / "controls/image").exists())

    def test_controls_and_heldout_scoring(self):
        path = self.manifest(self.spec)

        def run(root, arm, task, *_):
            self.assertEqual(task["id"], "two")
            return {"status": "pass" if arm["id"] == "b" else "fail", "wall_seconds": 1,
                    "usage": {"admitted_responses": 1, "dispatched_tools": 0, "reported_tokens": None}}

        out = self.root / "run"
        with patch("bench.run_trial", side_effect=run) as trial:
            self.assertEqual(bench.main([str(path), "--split", "holdout", "--output", str(out)]), 0)
            self.assertEqual(trial.call_count, 2)
        self.assertEqual(json.loads((out / "controls/two/receipt.json").read_text()), {"pass": 0, "fail": 1})
        result = json.loads((out / "report.json").read_text())
        self.assertEqual(result["split"], "holdout")
        self.assertEqual(result["comparison"]["by_category"], {"example": {"right_only": 1}})
        self.assertEqual(result["comparison"]["metrics"]["b"]["reported_tokens"],
                         {"sum": None, "trials_reporting": 0, "per_solved_task": None})
        self.assertEqual(result["comparison"]["coverage"]["a"]["scheduled_tasks"], ["two"])

    def test_frozen_plan_refuses_mutated_asset(self):
        manifest = self.manifest(self.spec)
        plan = self.root / "plan.json"
        with plan.open("w") as stream, contextlib.redirect_stdout(stream):
            self.assertEqual(bench.main([str(manifest)]), 0)
        self.assertEqual(json.loads(plan.read_text())["split"], "development")
        (self.root / "candidate.md").write_text("mutated guidance")
        with contextlib.redirect_stderr(io.StringIO()), patch("bench.run_trial") as trial:
            self.assertEqual(bench.main([str(manifest), "--frozen-plan", str(plan),
                                         "--output", str(self.root / "runs")]), 2)
            trial.assert_not_called()
            self.assertFalse((self.root / "runs").exists())

    def test_mode_comparison_locks_plus_roles(self):
        spec = self.spec
        spec["comparison"]["factor"] = "mode"
        same = {"model": "fixture", "effort": "xhigh"}
        a, b = spec["arms"]
        b.pop("guidance")
        for mode in ("orchestrate", "propose"):
            with self.subTest(mode=mode):
                a.update(same, mode=mode)
                b.update(same, mode=mode + "+",
                         roles={k: same.copy() for k in ("strong", "medium", "weak")})
                for task in spec["tasks"]:
                    task["modes"] = [mode, mode + "+"]
                self.assertEqual(len(bench.load(self.manifest(spec))[3]), 4)
                b["roles"]["weak"]["effort"] = "high"
                with self.assertRaisesRegex(ValueError, "roles must match"):
                    bench.load(self.manifest(spec))

    def test_trace_counts_failed_connect_and_retry_separately_from_sent(self):
        path = self.root / "trace.jsonl"
        data = [{"kind": "http", "id": "g", "seconds": .1, "error": "http_connect"},
                {"kind": "http_sent", "id": "g"},
                {"kind": "http", "id": "g", "seconds": .2, "error": None},
                {"kind": "generation", "id": "g", "session": "s", "seconds": .4, "error": None},
                {"kind": "tool", "operation": "op", "seconds": .1}]
        path.write_text("".join(json.dumps(row) + "\n" for row in data))
        traces = {"s": [{"kind": "init"}, {"kind": "outcome",
                       "result": {"operation": "op", "dispatched": True}}]}
        outcome = comparison.trace_metrics(path, traces)
        self.assertEqual([outcome[k] for k in ("http_attempts", "http_sent", "http_retries", "http_errors")],
                         [2, 1, 1, 1])
        self.assertEqual(outcome["by_role"]["lead"]["tool_seconds"], .1)
        data.insert(0, {"kind": "connect_retry", "id": "g"})
        path.write_text("".join(json.dumps(row) + "\n" for row in data))
        outcome = comparison.trace_metrics(path, traces)
        for counts in (outcome, outcome["by_role"]["lead"]):
            self.assertEqual([counts[k] for k in ("http_attempts", "http_sent", "http_retries",
                                                   "http_errors", "connect_retries")], [3, 1, 2, 2, 1])

    def test_plan_grading_checks_scope_and_user_approval(self):
        plan = {"steps": [{"access": "write", "paths": ["./answer.txt"]}],
                "command_checks": [{"argv": ["./test.sh"]}]}
        requirements = {"paths": ["answer.txt"], "check": "test.sh"}
        self.assertEqual(comparison.grade_plan(plan, requirements, True)["status"], "pass")
        echoed = plan | {"command_checks": [{"argv": ["echo", "test.sh"]}]}
        self.assertFalse(comparison.grade_plan(echoed, requirements, True)["criteria"]["expected_check"])
        plan["steps"][0]["paths"].append("extra.txt")
        failed = comparison.grade_plan(plan, requirements, True)
        self.assertEqual(failed["status"], "fail")
        self.assertFalse(failed["criteria"]["expected_write_paths"])

    def test_frozen_plan_refuses_changed_pty_helper(self):
        manifest = self.manifest(self.spec)
        plan = self.root / "plan.json"
        checkout = self.root / "checkout"
        for path in bench.CARBON_HELPERS:
            target = checkout / path
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(bench.PROJECT / path, target)
        with patch("bench.PROJECT", checkout):
            with plan.open("w") as stream, contextlib.redirect_stdout(stream):
                self.assertEqual(bench.main([str(manifest)]), 0)
            helper = checkout / "tests/parity/frontend_io.py"
            source = helper.read_text()
            self.assertTrue("'xterm'" in source)
            helper.write_text(source.replace("'xterm'", "'xterm-color'", 1))
            args = [str(manifest), "--frozen-plan", str(plan), "--output", str(self.root / "runs")]
            with contextlib.redirect_stderr(io.StringIO()) as errors, patch("bench.run_trial") as trial:
                self.assertEqual(bench.main(args), 2)
                trial.assert_not_called()
            self.assertIn("tests/parity/frontend_io.py", errors.getvalue())
            self.assertFalse((self.root / "runs").exists())

    def test_broken_oracle_prevents_dispatch(self):
        self.executable("verify", "raise SystemExit(0)\n")
        with patch("bench.run_trial") as trial:
            self.assertEqual(bench.main([str(self.manifest(self.spec)), "--output", str(self.root / "bad")]), 2)
            trial.assert_not_called()

    def test_stages_require_response_or_compaction(self):
        def init(state):
            return {"kind": "init", "configuration": {"epoch": {"exact_state": state}}}

        def response(state):
            return {"kind": "response", "response": {"request": {"epoch": {"exact_state": state}}}}

        review = init({"review_skill_sha256": "c" * 64, "orchestration_parent": "p"})
        traces = {"p": [init({}), response({"purpose": "orchestration_plan"})],
                  "started": [review], "worker": [init({}), response({})],
                  "review": [review, response({}), {"kind": "compacted"}]}
        self.assertEqual(comparison.stages(traces), {"planner": ["p"], "reviewer": ["review"], "compaction": ["review"]})
        self.spec["comparison"]["stage"] = "reviewer"
        out = self.root / "missing"
        with patch("bench.run_trial", return_value={"status": "pass"}):
            self.assertEqual(bench.main([str(self.manifest(self.spec)), "--output", str(out)]), 1)
        result = json.loads((out / "report.json").read_text())
        self.assertEqual(result["paired"], {"a:b": {"stage_missing": 1}})


if __name__ == "__main__":
    unittest.main()
