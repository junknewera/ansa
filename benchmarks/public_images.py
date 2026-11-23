"""Prepare the pinned CharXiv and SWE-bench image tasks for an attachment-capable frontend."""

import hashlib
import json
from pathlib import Path
import shutil
import sys
import tarfile
import tempfile
from urllib.request import urlopen
import zipfile

TASK_IDS = ("charxiv-reasoning-0", "swebench-chartjs-8567")
CHARXIV = "7ebe88f78dee387691551f071abcb2b9e1a8025b"
FIGURES = "f441eb632fc62f6f777830a0f47619e6e86459b0"
SWE = "4e6662d51c48e475f7f346e4fa09a6f8b31fcaa5"
CHART = "91628c144944029505de093c43733961ab4f420f"
INSTANCE = "chartjs__Chart.js-8567"
MIB = 1024 * 1024
GOLD_IMAGES = {
    "test/fixtures/core.layouts/long-labels.png": "ef9195d553389ddd72131f41e6e78af1ca69b0086a1c4ceb4f122575dff2a3dd",
    "test/fixtures/core.layouts/refit-vertical-boxes.png": "eb7454fda525d7c8c04ae8449d293450711e24374bbbb46cf491954215925a72",
    "test/fixtures/scale.time/invalid-data.png": "4390f64f55ac403ddc12b049a57f018e5758c54239b0154be573e781966c1013",
}


def fetch(url, path, maximum, expected):
    digest = hashlib.sha256()
    total = 0
    with urlopen(url, timeout=90) as response, path.open("xb") as stream:
        while block := response.read(MIB):
            total += len(block)
            if total > maximum:
                raise ValueError(f"oversized download: {url}")
            stream.write(block)
            digest.update(block)
    if digest.hexdigest() != expected:
        raise ValueError(f"download digest mismatch: {url}")


def chart_workspace(archive, target):
    prefix = f"Chart.js-{CHART}"
    with tarfile.open(archive, "r:gz") as source:
        for member in source:
            path = Path(member.name)
            if path.parts[:1] != (prefix,) or ".." in path.parts:
                raise ValueError("invalid Chart.js source path")
            output = target.joinpath(*path.parts[1:])
            if member.isdir():
                output.mkdir(parents=True, exist_ok=True)
            elif member.isfile():
                with source.extractfile(member) as data, output.open("xb") as dest:
                    shutil.copyfileobj(data, dest)
                output.chmod(0o700 if member.mode & 0o111 else 0o600)
            else:
                raise ValueError("unexpected Chart.js source entry")


def configure_chart_evaluation(row):
    # The pinned image provides Chromium and a Firefox snap launcher without
    # its snap. The dev row grades exactly one Chrome visual regression.
    target = "/base/test/fixtures/core.layouts/long-labels.js"
    if row["FAIL_TO_PASS"] != [f"Chart.layouts auto {target} "] or row["PASS_TO_PASS"]:
        raise ValueError("unexpected Chart.js grading targets")
    script = row["eval_script"]
    command = "--single-run --coverage --grep --auto-watch false"
    if script.count(command) != 1 or script.count("npm install ;") != 1:
        raise ValueError("unexpected upstream Karma invocation")
    script = script.replace(command, "--single-run --coverage --grep core.layouts --browsers chrome --auto-watch false")
    # Upstream FAIL_ONLY infers a pass from absence. Emit actual spec results
    # so an unexecuted test or browser failure cannot be graded as a success.
    row["eval_script"] = script.replace("npm install ;", "sed -i 's/suppressPassed: true/suppressPassed: false/' "
                                        "./karma.conf.js ; npm install ;")


def prepare(task_id, root):
    if task_id not in TASK_IDS:
        raise ValueError(f"unknown image task: {task_id}")
    root = Path(root).resolve()
    workspace = root / "workspace"
    operator = root / "operator"
    workspace.mkdir()
    operator.mkdir(mode=0o700)
    grader = operator / "grade.py"
    shutil.copyfile(Path(__file__).with_name("image_grade.py"), grader)
    verifier = operator / "verify.py"
    verifier.write_text(f"#!{sys.executable}\nfrom grade import main\nraise SystemExit(main({task_id!r}))\n")
    verifier.chmod(0o700)
    controls = operator / "control.py"
    controls.write_text(f"#!{sys.executable}\nfrom grade import control\ncontrol({task_id!r})\n")
    controls.chmod(0o700)
    deps = [grader]
    with tempfile.TemporaryDirectory(dir=root) as tmp:
        cache = Path(tmp)
        if task_id == TASK_IDS[0]:
            raw = f"https://raw.githubusercontent.com/princeton-nlp/CharXiv/{CHARXIV}/data/reasoning_val.json"
            fetch(raw, cache / "questions", MIB,
                  "8bf9b9c466baed7510104c65dcbcb6b3ca36080123a609f8af3e32d573c85d19")
            record = json.loads((cache / "questions").read_bytes())["0"]
            source = f"https://github.com/princeton-nlp/CharXiv/blob/{CHARXIV}/data/reasoning_val.json"
            fetch(f"https://huggingface.co/datasets/princeton-nlp/CharXiv/resolve/{FIGURES}/images.zip?download=true",
                  cache / "images.zip", 140 * MIB,
                  "c4b151cef2aba35523764dd1091d719d64acf81a16904d72f31da2723efbd855")
            attachment = operator / "0.jpg"
            with zipfile.ZipFile(cache / "images.zip") as images:
                attachment.write_bytes(images.read("0.jpg"))
            reference = operator / "answer.json"
            reference.write_text(json.dumps(record["answer"]))
            deps.append(reference)
            prompt = record["query"] + "\nAnswer with only the model name."
            modes = ["work", "plan"]
        else:
            try:
                import pyarrow.parquet as parquet
            except ImportError as error:
                raise ValueError("SWE-bench preparation requires pyarrow in this Python environment") from error
            source = f"https://huggingface.co/datasets/SWE-bench/SWE-bench_Multimodal/tree/{SWE}"
            fetch(source.replace("/tree/", "/resolve/") + "/data/dev-00000-of-00001.parquet",
                  cache / "dev.parquet", 4 * MIB,
                  "714f4b815680608168d620c87c6b3420b3d4690d6656fc85b1da6511bfb55931")
            row, = parquet.read_table(cache / "dev.parquet", filters=[("instance_id", "=", INSTANCE)]).to_pylist()
            if row["base_commit"] != CHART or row["repo"] != "chartjs/Chart.js":
                raise ValueError("wrong SWE-bench instance")
            configure_chart_evaluation(row)
            url, = row["image_assets"]["problem_statement"]
            attachment = operator / "issue.png"
            fetch(url, attachment, MIB, "a58026f2281051f054403ea9ea1c75bf5be9c5ccbffdb1ee002d4c3c55cacdef")
            archive = operator / "base.tar.gz"
            fetch(f"https://codeload.github.com/chartjs/Chart.js/tar.gz/{CHART}", archive, 6 * MIB,
                  "713267d89c971cd633b3eb0016f335b2fb20767e03da59b83dc357a853c3b82e")
            chart_workspace(archive, workspace)
            # Pin the linux/amd64 image the upstream dev row names by a mutable tag.
            row["image"] = row["image"].removesuffix(":latest") + (
                "@sha256:30d2248c943b31c82be02223c04ce59b6138f0e3f23a2ff403628dd7b884c317")
            assets = row["image_assets"]["test_patch"]
            if {asset["path"] for asset in assets} != set(GOLD_IMAGES):
                raise ValueError("wrong set of upstream visual test baselines")
            for asset in assets:
                local = operator / Path(asset["path"]).name
                fetch(asset["url"], local, MIB, GOLD_IMAGES[asset["path"]])
                asset["url"] = local.as_uri()
                deps.append(local)
            gold = operator / "gold.patch"
            gold.write_text(row.pop("patch"))
            deps.append(gold)
            dataset = operator / "dataset.json"
            dataset.write_text(json.dumps([row]))
            preparation = operator / "public_images.py"
            shutil.copyfile(__file__, preparation)
            deps.extend([archive, dataset, preparation])
            prompt = row["problem_statement"].replace("\r\n", "\n").strip()
            modes = ["work", "orchestrate", "orchestrate+"]
    return {"id": task_id, "workspace": str(workspace), "verifier": str(verifier),
            "oracle_fixture": str(controls), "family": task_id, "category": "images",
            "split": "development" if task_id == TASK_IDS[0] else "holdout",
            "verifier_inputs": list(map(str, deps)), "source": source, "modes": modes,
            "turns": [prompt], "attachments": [{"turn": 0, "path": str(attachment)}]}
