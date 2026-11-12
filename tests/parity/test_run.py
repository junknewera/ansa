"""Regressions for false-green M0 evidence, using only synthetic files."""

import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location("parity_run", Path(__file__).with_name("run.py"))
RUN = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(RUN)


class EvidenceTests(unittest.TestCase):
    def test_future_commands_cannot_pass(self):
        for name in ("compare", "fault", "perf"):
            result = subprocess.run([sys.executable, str(Path(RUN.__file__)), name],
                                    capture_output=True, text=True, check=False)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("not implemented", result.stderr)

    def test_junit_skipped_failed_duplicate_and_missing_are_not_pass(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "results.xml"
            for body in ('', '<testcase name="a"><skipped/></testcase>',
                         '<testcase name="a"><failure/></testcase>',
                         '<testcase name="a" status="notrun"/>',
                         '<testcase name="a"/><testcase name="a"/>'):
                path.write_text('<testsuite>' + body + '</testsuite>')
                self.assertTrue(RUN.case_errors(RUN.junit_cases(path), ["a"]))
            path.write_text('<testsuite><testcase name="a"/></testsuite>')
            self.assertEqual(RUN.case_errors(RUN.junit_cases(path), ["a"]), [])

    def test_linux_failure_does_not_hide_missing_toolchain(self):
        with tempfile.TemporaryDirectory() as directory:
            with patch.object(RUN, "command", return_value={"exit_code": 1, "timed_out": False}) as run:
                report = RUN.preflight(Path(directory) / "missing", Path(directory), {"linux_checks": ["proc_children"]})
            self.assertEqual(run.call_count, 2)
            self.assertIn("linux preflight failed or unavailable", report["errors"])
            self.assertIn("toolchain preflight failed or unavailable", report["errors"])

    def test_cli_success_exit_does_not_replace_semantic_assertion(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)
            (path / "stdout").write_text(json.dumps({"status": "passed", "mode": "offline", "tool_invocations": 2}))
            result = {"exit_code": 0, "timed_out": False, "stdout": {"file": "stdout"}}
            errors = RUN.cli_errors({"cli-self-check": result, "offline-roundtrip": result}, path)
            self.assertEqual(len(errors), 2)

    def test_mutants_only_excluded_in_declared_child_inventory(self):
        manifest = json.loads(RUN.MANIFEST.read_bytes())
        full = RUN.expected_ids(manifest, {"OMH_MUTATION_CHILD": "OFF"})
        child = RUN.expected_ids(manifest, {"OMH_MUTATION_CHILD": "ON"})
        self.assertEqual(len(full), 87)
        self.assertEqual(len(child), 85)
        self.assertEqual(set(full) - set(child), {"mutation-drop", "mutation-reorder"})


if __name__ == "__main__":
    unittest.main()
