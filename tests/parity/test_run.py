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

    def test_cmake_true_aliases_cannot_bypass_sanitizer_options(self):
        manifest = json.loads(RUN.MANIFEST.read_bytes())
        options = {"ASAN_OPTIONS": "detect_leaks=1", "UBSAN_OPTIONS": "halt_on_error=1"}
        for value in ("ON", "TRUE", "YES", "Y", "1", "2", "true", "notfound", "probe-notfound", "falſe", "ıgnore"):
            with self.subTest(value=value):
                cache = {"OMH_SANITIZERS": value, "OMH_MUTATION_CHILD": "OFF"}
                self.assertTrue(RUN.sanitizer_errors(cache, {}))
                self.assertTrue(RUN.sanitizer_errors(cache, {"ASAN_OPTIONS": "detect_leaks=1"}))
                self.assertEqual(RUN.sanitizer_errors(cache, options), [])
                self.assertEqual(len(RUN.expected_ids(manifest, cache)), 87)
                cache["OMH_MUTATION_CHILD"] = value
                self.assertEqual(len(RUN.expected_ids(manifest, cache)), 85)
                cache["OMH_SANITIZERS"] = "OFF"
                self.assertTrue(RUN.sanitizer_errors(cache, options))

    def test_cmake_false_aliases_keep_both_mutation_tests(self):
        manifest = json.loads(RUN.MANIFEST.read_bytes())
        for value in ("OFF", "FALSE", "NO", "N", "0", "", "IGNORE", "NOTFOUND", "probe-NOTFOUND", "false"):
            with self.subTest(value=value):
                cache = {"OMH_SANITIZERS": value, "OMH_MUTATION_CHILD": value}
                self.assertEqual(RUN.sanitizer_errors(cache, {}), [])
                self.assertEqual(len(RUN.expected_ids(manifest, cache)), 87)

    def test_boolean_values_match_actual_cmake_if(self):
        values = ("ON", "off", "YES", "false", "1", "0", "2", "0.0", "-0", "",
                  "IGNORE", "ignore", "NOTFOUND", "notfound", "probe-NOTFOUND", "probe-notfound", "falſe", "ıgnore")
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)
            output = path / "values.txt"
            script = path / "booleans.cmake"
            script.write_text("\n".join(
                f'set(PROBE [=[{value}]=])\nif(PROBE)\n'
                f'  file(APPEND "{output}" "1\\n")\nelse()\n'
                f'  file(APPEND "{output}" "0\\n")\nendif()'
                for value in values
            ))
            subprocess.run(["cmake", "-P", script], check=True, capture_output=True)
            actual = [line == "1" for line in output.read_text().splitlines()]
        self.assertEqual([RUN.cmake_enabled({"flag": value}, "flag") for value in values], actual)


if __name__ == "__main__":
    unittest.main()
