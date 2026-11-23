"""Build positive/negative operator-side artifacts for the published verifiers."""

import json
from pathlib import Path
import shutil
import subprocess
import sys

operator = Path(__file__).resolve().parent
control, work, answer = sys.argv[1], Path(sys.argv[2]), Path(sys.argv[3])
if control not in ("pass", "fail"):
    raise ValueError("expected pass or fail")
positive = control == "pass"


def git(*args):
    return subprocess.check_output(["git", "-c", "core.hooksPath=/dev/null",
                                   "-c", "user.name=ANSA", "-c", "user.email=fixture@localhost",
                                   *args], cwd=work, text=True).strip()


task = (operator / "task-id").read_text()
response = ""
if task == "intercode-bash-fs1-48":
    shutil.copytree(operator.parent / "inputs", work, dirs_exist_ok=True)
    response = (operator / "expected.txt").read_text()
    if not positive:
        # A model can leave a clean Git status after changing a read-only task.
        git("init", "-q")
        (work / "index.html").write_text("committed mutation")
        git("add", "-f", "--", ".")
        git("commit", "-qm", "Mutated inputs")
elif task == "count-dataset-tokens":
    (work / "answer.txt").write_text("79586" if positive else "179586")
elif task == "mteb-retrieve":
    (work / "result.txt").write_text("MTEB: Massive Text Embedding Benchmark\n" if positive else "terminal-bench")
elif task == "fix-git":
    git("init", "-q")
    git("commit", "--allow-empty", "-qm", "Benchmark inputs")
    subprocess.run([str(operator / "setup.py")], cwd=work, check=True)
    if positive:
        lost = git("rev-parse", "HEAD@{1}")
        git("merge", "--no-edit", "-X", "theirs", lost)
    else:
        # A content-only verifier would wrongly accept copying the two files.
        for source, target in (("about.md", "_includes/about.md"), ("default.html", "_layouts/default.html")):
            (work / target).write_bytes((operator / source).read_bytes())
else:
    raise ValueError(f"unknown task {task}")
answer.write_text(json.dumps({"answers": [response], "candidate": None, "plan": None}))
