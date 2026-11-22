"""Prepare pinned InterCode and Terminal-Bench tasks for the Carbon runner."""

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
from urllib.request import urlopen

from terminal_git_setup import BASE as SITE_REV, restore
from terminal_grade import intercode_files

TASK_IDS = ("intercode-bash-fs1-48", "fix-git", "count-dataset-tokens", "mteb-retrieve")
TERMINAL = "harbor-framework/terminal-bench-2"
TERMINAL_REV = "2fd12b88aafdd04a52c298e3940bcb189f9766d6"
INTERCODE = "princeton-nlp/intercode"
INTERCODE_REV = "c3e46d827cfc9d4c704ec078f7abf9f41e3191d8"
SITE = "https://github.com/AI-Harness-Evals/personal-site.git"
DATASET_REV = "a82400884621626d41bef89b7604f8054e7e00e0"
TOKENIZER_REV = "989aa7980e4cf806f80c7fef2b1adb7bc71aa306"


def fetch(url, target, limit=1024 * 1024):
    target.parent.mkdir(exist_ok=True, parents=True)
    with urlopen(url, timeout=90) as response, target.open("xb") as stream:
        size = 0
        while chunk := response.read(64 * 1024):
            size += len(chunk)
            if size > limit:
                raise ValueError(f"download exceeds bound: {url}")
            stream.write(chunk)


def github(repo, revision, path, target):
    fetch(f"https://raw.githubusercontent.com/{repo}/{revision}/{path}", target)


def git(repo, *args):
    subprocess.run(["git", "-c", "core.hooksPath=/dev/null", "-C", str(repo), *args],
                   check=True, timeout=180)


def install_script(source, dest):
    dest.write_text(f"#!{sys.executable}\n" + Path(__file__).with_name(source).read_text())
    dest.chmod(0o700)


def prepare(task_id, root):
    if task_id not in TASK_IDS:
        raise ValueError(f"unknown terminal task: {task_id}")
    root = Path(root).resolve(strict=True)
    inputs, operator = root / "inputs", root / "operator"
    inputs.mkdir()
    operator.mkdir(mode=0o700)
    verifier = operator / "verify.py"
    install_script("terminal_grade.py", verifier)
    task = {"id": task_id, "modes": ["work", "orchestrate", "orchestrate+"],
            "workspace": str(inputs), "verifier": str(verifier)}
    if task_id == "intercode-bash-fs1-48":
        catalog = operator / "catalog.json"
        github(INTERCODE, INTERCODE_REV, "data/nl2bash/nl2bash_fs_1.json", catalog)
        row = json.loads(catalog.read_text())[48]
        # Upstream setup writes into /testbed and /index.html. Relocate both
        # inside the fresh input tree before running the pinned script.
        generator = operator / "upstream_setup.sh"
        github(INTERCODE, INTERCODE_REV, "docker/bash_scripts/setup_nl2b_fs_1.sh", generator)
        script = generator.read_text().replace("/testbed", "./testbed").replace("/index.html", "./index.html")
        subprocess.run(["bash", "-eu"], input=script, text=True, cwd=inputs, check=True, timeout=30)
        gold = row["gold"].replace("/testbed", "./testbed")
        expected = subprocess.check_output(["bash", "-o", "pipefail", "-c", gold], cwd=inputs,
                                           text=True, env=dict(os.environ, LC_ALL="C"))
        (operator / "baseline.json").write_text(json.dumps(intercode_files(inputs), sort_keys=True))
        (operator / "expected.txt").write_text(expected)
        prompt = row["query"].replace("/testbed", "./testbed")
        prompt += " Return just the five output lines with block counts and paths."
        task["modes"] = ["work", "plan"]
        task["source"] = f"https://github.com/{INTERCODE}/blob/{INTERCODE_REV}/data/nl2bash/nl2bash_fs_1.json"
    else:
        task["source"] = f"https://github.com/{TERMINAL}/tree/{TERMINAL_REV}/{task_id}"
        for path, target in [("instruction.md", "instruction.md"), ("tests/test_outputs.py", "author_tests.py")]:
            github(TERMINAL, TERMINAL_REV, f"{task_id}/{path}", operator / target)
        prompt = (operator / "instruction.md").read_text().strip().replace("/app/", "./")
        if task_id == "fix-git":
            task["modes"] = ["work"]
            for name in ("about.md", "default.html"):
                github(TERMINAL, TERMINAL_REV, f"fix-git/environment/resources/patch_files/{name}", operator / name)
            with tempfile.TemporaryDirectory() as tmp:
                # The upstream site was removed. Pin its original commit in a mirror.
                git(tmp, "init", "-q")
                git(tmp, "fetch", "--no-tags", SITE, SITE_REV)
                git(tmp, "branch", "fixture-master", SITE_REV)
                git(tmp, "bundle", "create", str(operator / "history.bundle"), "refs/heads/fixture-master")
                lost = restore(Path(tmp), operator)
                (operator / "lost-commit").write_text(lost)
            setup = operator / "setup.py"
            install_script("terminal_git_setup.py", setup)
            task["git_setup"] = str(setup)
        elif task_id == "mteb-retrieve":
            github(TERMINAL, TERMINAL_REV, f"{task_id}/environment/task-deps/data.txt", inputs / "data.txt")
            prompt = prompt.replace("You have the mteb package at version 1.36.8 installed.",
                                    "Use mteb version 1.36.8 if needed.")
        else:
            # Keep the original README, both parquet splits and tokenizer in
            # the snapshot so mutable Hub aliases cannot change a trial.
            resources = (
                ("https://huggingface.co/datasets/ryanmarten/OpenThoughts-1k-sample",
                 DATASET_REV, "dataset", ["README.md", "data/train-00000-of-00001.parquet",
                                          "metadata/train-00000-of-00001.parquet"]),
                ("https://huggingface.co/Qwen/Qwen2.5-1.5B-Instruct", TOKENIZER_REV, "tokenizer",
                 ["config.json", "tokenizer.json", "tokenizer_config.json", "merges.txt", "vocab.json"]),
            )
            for base, rev, local, files in resources:
                for file in files:
                    fetch(f"{base}/resolve/{rev}/{file}", inputs / local / file, limit=32 * 1024 * 1024)
            prompt += f" Use the local ./dataset ({DATASET_REV}) and ./tokenizer ({TOKENIZER_REV}) snapshots."
    (operator / "task-id").write_text(task_id)
    task["verifier_inputs"] = [str(p) for p in sorted(operator.iterdir()) if p != verifier]
    control = operator / "controls.py"
    install_script("terminal_controls.py", control)
    categories = {"intercode-bash-fs1-48": "linux", "fix-git": "git",
                  "count-dataset-tokens": "tokenization", "mteb-retrieve": "retrieval"}
    task.update(oracle_fixture=str(control), family=task_id, category=categories[task_id],
                split="holdout" if task_id in ("fix-git", "mteb-retrieve") else "development")
    task["turns"] = [prompt]
    return task
