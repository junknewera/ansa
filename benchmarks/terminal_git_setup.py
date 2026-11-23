"""Install Terminal-Bench's detached-HEAD history in a fresh trial checkout."""

import os
from pathlib import Path
import shutil
import subprocess

BASE = "d7d3e4ba9350f634d92d39ace2b471433ee57d50"


def restore(workspace, operator):
    env = {**os.environ, "GIT_AUTHOR_DATE": "2020-01-01T00:00:00Z",
           "GIT_COMMITTER_DATE": "2020-01-01T00:00:00Z", "GIT_CONFIG_GLOBAL": os.devnull,
           "GIT_CONFIG_NOSYSTEM": "1"}
    env.update(GIT_AUTHOR_NAME="Test User", GIT_AUTHOR_EMAIL="test@example.com",
               GIT_COMMITTER_NAME="Test User", GIT_COMMITTER_EMAIL="test@example.com")

    def git(*args):
        return subprocess.check_output(["git", "-c", "core.hooksPath=/dev/null",
                                       *args], cwd=workspace, env=env, text=True).strip()
    git("fetch", "--no-write-fetch-head", str(operator / "history.bundle"), "refs/heads/fixture-master")
    git("checkout", "-B", "master", BASE)
    git("checkout", "--detach", BASE + "^")
    for name, directory in (("about.md", "_includes"), ("default.html", "_layouts")):
        shutil.copyfile(operator / name, workspace / directory / name)
    git("add", "-A")
    git("commit", "-m", "Move to Stanford")
    lost = git("rev-parse", "HEAD")
    git("checkout", "master")
    return lost


if __name__ == "__main__":
    restore(Path.cwd(), Path(__file__).resolve().parent)
