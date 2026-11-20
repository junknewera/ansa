import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest import mock

import project_files as files


class ProjectFilesTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.project = self.root / "project"
        self.project.mkdir()

    def git(self, *args):
        return subprocess.run(["git", "-c", "user.name=Test", "-c", "user.email=test@example.invalid",
                               *args], cwd=self.project, capture_output=True, check=True).stdout

    def init(self):
        self.git("init", "-q")
        self.git("commit", "-qm", "base", "--allow-empty")

    def test_staged_and_worktree_are_compared_to_the_same_head(self):
        self.init()
        directory = self.project / "nested"
        directory.mkdir()
        path = directory / "w[i]ld.py"
        path.write_bytes(b"one\r\nbase")
        self.git("add", "--all")
        self.git("commit", "-qm", "base file")
        path.write_bytes(b"one\r\nstaged")
        self.git("add", "--all")
        path.write_bytes(b"one\r\nwork\n")
        doc = files.load_document(directory, path)
        self.assertEqual(doc["source"], "one\r\nwork\n")
        self.assertIn("-base\n\\ No newline at end of file\n+work", doc["diff"])
        self.assertNotIn("staged", doc["diff"])
        self.assertEqual(doc["comparison"], "HEAD → working file")

    def test_untracked_and_staged_addition_and_clean(self):
        self.init()
        path = self.project / "-new"
        path.write_text("new\n")
        self.assertEqual(files.load_document(self.project, path)["comparison"], "Untracked file")
        self.git("add", "--", path.name)
        self.assertEqual(files.load_document(self.project, path)["comparison"], "Staged addition")
        self.git("commit", "-qm", "new")
        self.assertEqual(files.load_document(self.project, path)["diff"], "Working file matches HEAD.")

    def test_source_kept_for_absent_repo_unborn_head_or_missing_git(self):
        path = self.project / "note.md"
        path.write_text("source")
        self.assertIn("No Git repository", files.load_document(self.project, path)["diff"])
        self.git("init", "-q")
        self.assertIn("no HEAD commit", files.load_document(self.project, path)["diff"])
        with mock.patch.dict(os.environ, {"PATH": str(self.root)}):
            doc = files.load_document(self.project, path)
        self.assertEqual(doc["source"], "source")
        self.assertIn("Git executable", doc["diff"])

    def test_symlink_escape_non_regular_and_size(self):
        (self.root / "outside").write_text("secret")
        (self.project / "link").symlink_to(self.root / "outside")
        (self.project / "dir").symlink_to(self.root)
        os.mkfifo(self.project / "fifo")
        for path in ("../outside", "link", "dir/outside", "fifo"):
            with self.subTest(path=path), self.assertRaises(ValueError):
                files.load_document(self.project, Path(path))
        path = self.project / "too_big"
        path.write_bytes(b"a" * (files.FILE_LIMIT + 1))
        with self.assertRaisesRegex(ValueError, "256 KiB"):
            files.load_document(self.project, path)
        self.init()
        self.git("add", path.name)
        self.git("commit", "-qm", "large")
        path.write_text("now tiny")
        doc = files.load_document(self.project, path)
        self.assertIn("HEAD blob exceeds", doc["diff"])
        self.assertEqual(doc["source"], "now tiny")

    def test_missing_blob_never_becomes_an_untracked_file(self):
        self.init()
        path = self.project / "file"
        path.write_text("tracked\n")
        self.git("add", "file")
        self.git("commit", "-qm", "file")
        oid = self.git("rev-parse", "HEAD:file").decode().strip()
        (self.project / ".git" / "objects" / oid[:2] / oid[2:]).unlink()
        doc = files.load_document(self.project, path)
        self.assertEqual(doc["comparison"], "Diff unavailable")
        self.assertIn("cat-file", doc["diff"])

    def test_binary_encoding_and_timeout(self):
        path = self.project / "binary"
        for data in [b"a\0b", b"\xff"]:
            path.write_bytes(data)
            with self.assertRaises(ValueError):
                files.load_document(self.project, path)
        path.write_text("check")
        with mock.patch.object(files, "GIT_TIMEOUT", -1):
            doc = files.load_document(self.project, path)
        self.assertEqual(doc["comparison"], "Diff unavailable")
        self.assertIn("timed out", doc["diff"])


if __name__ == "__main__":
    unittest.main()
