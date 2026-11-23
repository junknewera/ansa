"""Offline regression tests for grading an image-based source patch."""

import hashlib
import io
import json
from pathlib import Path
import tarfile
import tempfile
import unittest
from unittest.mock import patch

from image_grade import chart_patch, report_score
from public_images import CHART, INSTANCE, chart_workspace


class ImageTaskTests(unittest.TestCase):
    def test_diff_against_archive_includes_new_files_and_deletions(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            archive = root / "base.tar.gz"
            with tarfile.open(archive, "w:gz") as tar:
                info = tarfile.TarInfo(f"Chart.js-{CHART}")
                info.type = tarfile.DIRTYPE
                tar.addfile(info)
                info = tarfile.TarInfo(f"Chart.js-{CHART}/old")
                info.size = 4
                tar.addfile(info, io.BytesIO(b"old\n"))
            workspace = root / "candidate"
            chart_workspace(archive, workspace)
            (workspace / "old").unlink()
            (workspace / "new").write_text("new\n")
            delta = chart_patch(archive, workspace, root)
            self.assertIn("new file mode", delta)
            self.assertIn("deleted file mode", delta)

    def test_requires_verified_upstream_test_images_and_results(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            staged = root / "image_assets"
            staged.mkdir()
            report = root / "report.json"
            report.write_text(json.dumps({INSTANCE: {"infra_failure": False, "resolved": True, "tests_status": {}}}))
            with patch("public_images.GOLD_IMAGES", {"test/ref.png": hashlib.sha256(b"sample").hexdigest()}):
                with self.assertRaises(FileNotFoundError):
                    report_score(report, INSTANCE)
                asset = staged / "test__ref.png"
                asset.write_bytes(b"wrong")
                with self.assertRaisesRegex(ValueError, "visual test baseline"):
                    report_score(report, INSTANCE)
                asset.write_bytes(b"sample")
                log = root / "test_output.txt"
                log.write_text("irrelevant test passed")
                with self.assertRaisesRegex(ValueError, "visual assertion"):
                    report_score(report, INSTANCE)
                target = "/base/test/fixtures/core.layouts/long-labels.js"
                log.write_text(f"    ✓ {target}\n")
                self.assertEqual(report_score(report, INSTANCE), 0)
                report.write_text(json.dumps({INSTANCE: {"infra_failure": False, "resolved": False, "tests_status": {}}}))
                log.write_text(f"    ✗ {target}\n")
                self.assertEqual(report_score(report, INSTANCE), 1)
                report.write_text(json.dumps({INSTANCE: {"infra_failure": True, "resolved": False}}))
                with self.assertRaisesRegex(ValueError, "test results"):
                    report_score(report, INSTANCE)


if __name__ == "__main__":
    unittest.main()
