"""Exercise Cargo compiler selection against an isolated copy of the scaffold."""

import json
import os
from pathlib import Path
import shutil
import sys
import tempfile
import unittest
from unittest.mock import patch

import check_toolchain


class ToolchainTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="ansa-toolchain-test-")
        self.addCleanup(self.temporary.cleanup)
        self.workspace = Path(self.temporary.name) / "rust"
        source = Path(check_toolchain.__file__).resolve().parents[2] / "rust"
        for name in ("Cargo.toml", "Cargo.lock", "rust-toolchain.toml",
                     "crates/ansa/Cargo.toml", "crates/ansa/src/lib.rs",
                     "crates/ansa/examples/protocol_adapter.rs",
                     "crates/ansa/examples/auth_adapter.rs",
                     "crates/ansa/examples/session_adapter.rs",
                     "crates/ansa/examples/runtime_adapter.rs",
                     "crates/ansa/examples/tools_adapter.rs",
                     "crates/ansa/examples/frontend_adapter.rs",
                     "crates/ansa/src/bin/ansa.rs", "crates/ansa-linux/Cargo.toml",
                     "crates/ansa-linux/src/lib.rs"):
            target = self.workspace / name
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(source / name, target)

    def test_selected_pinned_compiler_passes(self):
        with patch.dict(os.environ, {"RUSTC": shutil.which("rustc")}):
            report = check_toolchain.check(self.workspace)
        self.assertEqual(report["status"], "PASS", report.get("diagnostic"))
        self.assertIn("rustc " + report["declared"]["channel"], report["cargo_rustc"]["output"])

    def test_invalid_environment_compiler_is_not_path_rustc(self):
        with patch.dict(os.environ, {"RUSTC": shutil.which("false")}):
            report = check_toolchain.check(self.workspace)
        self.assertEqual(report["status"], "FAIL")

    def test_invalid_cargo_config_compiler_is_not_path_rustc(self):
        config = self.workspace / ".cargo/config.toml"
        config.parent.mkdir()
        config.write_text("[build]\nrustc = " + json.dumps(shutil.which("false")) + "\n")
        # Let Cargo config select the compiler, even if the parent defines RUSTC.
        with patch.dict(os.environ):
            os.environ.pop("RUSTC", None)
            os.environ.pop("CARGO_BUILD_RUSTC", None)
            report = check_toolchain.check(self.workspace)
        self.assertEqual(report["status"], "FAIL")

    def test_workspace_wrapper_cannot_change_compiler_version(self):
        wrapper = Path(self.temporary.name) / "compiler-wrapper"
        wrapper.write_text(
            f"#!{sys.executable}\nimport os, sys\n"
            "if '--version' in sys.argv[2:]:\n"
            "    print('rustc 0.0.0 (synthetic compiler override)')\n"
            "else:\n"
            "    os.execvp(sys.argv[1], sys.argv[1:])\n"
        )
        wrapper.chmod(0o700)
        with patch.dict(os.environ, {"RUSTC_WORKSPACE_WRAPPER": str(wrapper)}):
            report = check_toolchain.check(self.workspace)
        self.assertEqual(report["status"], "FAIL")
        self.assertIn("Cargo-selected rustc", report["diagnostic"])


if __name__ == "__main__":
    unittest.main()
