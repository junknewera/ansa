"""Grade a selected image task; exit 2 when the upstream evaluator cannot run."""

import hashlib
import importlib.metadata
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import uuid

HARNESS = "02e7a74ffd0b707aab73d203fe87bdc7c76afc8e"


def chart_patch(archive, workspace, scratch):
    from public_images import chart_workspace

    base = scratch / "base"
    chart_workspace(archive, base)
    repo = scratch / "index.git"
    subprocess.run(["git", "init", "--quiet", "--bare", str(repo)], check=True)

    def git(tree, *args):
        return subprocess.check_output(["git", "-c", "core.bare=false", "-c", "core.hooksPath=/dev/null",
                                        "--git-dir", str(repo), "--work-tree", str(tree), *args], cwd=tree)

    # The candidate may have changed its own Git history. Compare its actual tree
    # against the pinned archive, including staged, unstaged and new source files.
    git(base, "add", "--force", "--all", ".")
    original = git(base, "write-tree").decode().strip()
    git(workspace, "read-tree", "--empty")
    git(workspace, "read-tree", original)
    git(workspace, "add", "--all", ".")
    return git(workspace, "diff", "--cached", "--no-ext-diff", "--binary", original).decode()


def swe_grade(operator, workspace, output):
    from public_images import INSTANCE

    with tempfile.TemporaryDirectory() as scratch:
        patch = chart_patch(operator / "base.tar.gz", workspace, Path(scratch))
    if not patch:
        return 1
    package = importlib.metadata.distribution("swebench")
    provenance = json.loads(package.read_text("direct_url.json") or "{}")
    if provenance.get("vcs_info", {}).get("commit_id") != HARNESS:
        raise ValueError(f"install the SWE-bench verifier from commit {HARNESS}")
    # The harness writes both logs and reports under cwd. Keep per-attempt evidence
    # outside the agent's workspace, with fresh container names for concurrent trials.
    output.mkdir()
    pred = output / "predictions.jsonl"
    pred.write_text(json.dumps({"instance_id": INSTANCE, "model_name_or_path": "ansa", "model_patch": patch}) + "\n")
    run_id = uuid.uuid4().hex
    (output / "container-name").write_text(f"sweb.eval.{INSTANCE.lower()}.{run_id}\n")
    subprocess.run([sys.executable, "-I", "-m", "swebench.harness.run_evaluation",
                    "--dataset_name", str(operator / "dataset.json"), "--split", "dev",
                    "--instance_ids", INSTANCE, "--predictions_path", str(pred), "--run_id", run_id,
                    "--max_workers", "1", "--timeout", "240"], cwd=output, check=True)
    report_file = output / "logs" / "evaluation" / run_id / "ansa" / INSTANCE / "report.json"
    return report_score(report_file, INSTANCE)


def report_score(report_file, instance):
    from public_images import GOLD_IMAGES

    for path, expected in GOLD_IMAGES.items():
        staged = report_file.parent / "image_assets" / path.replace("/", "__")
        if hashlib.sha256(staged.read_bytes()).hexdigest() != expected:
            raise ValueError(f"missing or changed visual test baseline: {path}")
    report = json.loads(report_file.read_bytes())[instance]
    if report["infra_failure"] or "tests_status" not in report:
        raise ValueError("upstream evaluator did not produce test results")
    return 0 if report["resolved"] is True else 1


def control(task_id):
    outcome, workspace, answer_file = sys.argv[1:]
    if outcome not in {"pass", "fail"}:
        raise ValueError("expected pass or fail")
    operator = Path(__file__).resolve().parent
    if task_id == "charxiv-reasoning-0":
        expected = json.loads((operator / "answer.json").read_bytes())
        text = expected if outcome == "pass" else "Not " + expected
    else:
        from public_images import chart_workspace

        workspace = Path(workspace).resolve()
        chart_workspace(operator / "base.tar.gz", workspace)
        if outcome == "pass":
            subprocess.run(["git", "apply", "--binary", str(operator / "gold.patch")], cwd=workspace, check=True)
        else:
            # Exercise the actual regression tests on the unfixed source.
            with (workspace / "README.md").open("a") as readme:
                readme.write("\nUnrelated change.\n")
        text = ""
    Path(answer_file).write_text(json.dumps({"answers": [text]}))


def main(task_id):
    operator = Path(__file__).resolve().parent
    workspace, answers = map(Path, sys.argv[1:])
    try:
        if task_id == "charxiv-reasoning-0":
            expected = json.loads((operator / "answer.json").read_bytes())
            response, = json.loads(answers.read_bytes())["answers"]
            return int(response.strip().rstrip(".").casefold() != expected.casefold())
        return swe_grade(operator, workspace.resolve(), answers.resolve().parent / "swe-evaluation")
    except (ImportError, OSError, KeyError, ValueError, subprocess.SubprocessError) as error:
        print(f"image verifier: {error}", file=sys.stderr)
        return 2
