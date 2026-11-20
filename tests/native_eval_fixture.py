"""Capture an offline N0/N1/N2 comparison through Carbon, then grade the artifacts."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import subprocess
import sys
import tempfile
import time

from carbon_workspace_pty import completed, local, roots, start, until
from parity import session_cases
import native_eval as evaluation

PROMPT = b"ORCHESTRATION_COMMAND_FIXTURE\n"
INITIAL = b"before fixture\n"
EXPECTED = b"after fixture\n"
CHECK = b'#!/bin/sh\nset -eu\ngrep -qx "after fixture" report.txt\nprintf "fixture check passed\\n"\n'


def require(condition, message):
    evaluation.require(condition, message)


def git(path, *args, check=True):
    return subprocess.run(
        ["git", "-c", "core.hooksPath=/dev/null", *args], cwd=path,
        check=check, capture_output=True, timeout=5,
        env={"PATH": os.environ["PATH"], "GIT_CONFIG_NOSYSTEM": "1",
             "GIT_CONFIG_GLOBAL": "/dev/null", "GIT_AUTHOR_DATE": "2020-01-01T00:00:00Z",
             "GIT_COMMITTER_DATE": "2020-01-01T00:00:00Z"},
    )


def commit(path):
    git(path, "add", "report.txt", "check.sh")
    git(path, "-c", "user.name=Fixture", "-c", "user.email=fixture@localhost",
        "commit", "-qm", "offline comparison fixture")
    return git(path, "rev-parse", "HEAD").stdout.decode().strip()


def git_artifact(path, candidate, base_tree):
    require(re.fullmatch(r"(?:[0-9a-f]{40}|[0-9a-f]{64})", candidate), "invalid candidate name")
    parent_fields = git(path, "rev-list", "--parents", "-n", "1", candidate).stdout.split()
    require(len(parent_fields) == 2, "candidate must have a single parent")
    require(git(path, "rev-parse", f"{candidate}^1^{{tree}}").stdout.strip() == base_tree,
            "wrong candidate preimage")
    changed = git(path, "diff-tree", "--no-commit-id", "--name-only", "-r", candidate).stdout
    require(changed == b"report.txt\n", "candidate modified unexpected files")
    git(path, "diff", "--check", f"{candidate}^", candidate)
    return git(path, "show", f"{candidate}:report.txt").stdout


def traces(home):
    result = {}
    for directory in sorted((home / ".ansa/sessions").iterdir()):
        if directory.is_symlink() or not directory.is_dir():
            continue
        parts = {part.name: evaluation.read(part) for part in directory.glob("part-*.log")}
        if not parts:
            continue
        rows = evaluation.decode(session_cases.compact(session_cases.decode(parts)))
        require(rows and rows[0]["kind"] == "init", "missing native init")
        binding = rows[0]["configuration"]["binding"]
        require(binding["account"] == "offline-fixture" and
                binding["requested_model"] == binding["returned_model"] == "gpt-6.1-sol",
                "unexpected native account or model")
        result[rows[0]["session"]] = rows
    return result


def one_trace(sessions, prefix, operation):
    found = [(session, rows) for session, rows in sessions.items() if any(
        row["kind"] == "accepted" and row["text"].startswith(prefix) for row in rows)]
    require(len(found) == 1, f"expected one native {operation} session")
    session, rows = found[0]
    require(any(row["kind"] == "done" for row in rows), "native turn did not complete")
    intents = [row["invocation"] for row in rows if row["kind"] == "intent"]
    require(len(intents) == 1 and intents[0]["name"] == operation, "unexpected native invocation")
    outcomes = [row["result"] for row in rows if row["kind"] == "outcome"]
    require(len(outcomes) == 1 and outcomes[0]["operation"] == intents[0]["operation"] and
            outcomes[0]["execution"] == 0 and outcomes[0]["effects"] == 1,
            "native outcome does not match completed invocation")
    return session, rows


def command_evidence(run, candidate):
    spec_path, = (run / "check-jobs").glob("*/request.json")
    spec_raw = evaluation.read(spec_path)
    spec = evaluation.decode(spec_raw)
    result = evaluation.decode(evaluation.read(spec_path.with_name("result.json")))
    require(result["request_hash"] == evaluation.sha(spec_raw), "wrong host check receipt hash")
    require(spec["invocation"]["arguments"] == {"candidate": candidate, "argv": ["./check.sh"]},
            "wrong candidate in host check")
    output = evaluation.decode(result["output"])
    require(result["execution"] == 0 and result["effects"] == 1 and output["exit_code"] == 0
            and output["cleanup_complete"] and not output["timed_out"]
            and output["stdout"] == "fixture check passed\n", "host verification did not pass")
    return {"spec": spec, "result": result}


def capture(binary, directory, arm, receipt):
    started = time.monotonic()
    with tempfile.TemporaryDirectory(prefix="ansa-m4-", dir="/tmp") as temporary:
        root = Path(temporary)
        home, cwd = roots(root)
        (home / ".bashrc").write_text("PS1='fixture> '\nHISTFILE=/dev/null\n")
        (cwd / "report.txt").write_bytes(INITIAL)
        (cwd / "check.sh").write_bytes(CHECK)
        (cwd / "check.sh").chmod(0o700)
        git(cwd, "init", "-q")
        original = commit(cwd)
        base_tree = git(cwd, "rev-parse", "HEAD^{tree}").stdout.strip()
        screen = start(binary, root, home, cwd, {"ANSA_OFFLINE_EVAL_ARM": arm})
        try:
            if arm != "N0":
                screen.send(b"\t\t")
                until(screen, lambda _: "Orchestrate" in screen.vt.line(-2), "orchestration mode")
            screen.send(PROMPT.rstrip(b"\n") + b"\r")
            if arm == "N0":
                until(screen, lambda _: completed(root, PROMPT.decode().strip()), "ordinary Work turn")
            else:
                until(screen, lambda frame: "./check.sh" in frame and "Refine" in frame, "plan confirmation")
                screen.send(b"\r")
                marker = "offline_eval_verified" if arm == "N1" else "Verified changes"
                until(screen, lambda frame: marker in frame, "frozen candidate", timeout=15)
                if arm == "N2":
                    until(screen, lambda _: any((home / ".ansa/orchestration").glob(
                        "*/*/summary-admitted")), "durable review handoff")
            local(screen, "/exit")
            exit_result = screen.finish()
            require(exit_result["exit"] == 0 and exit_result["termios_restored"],
                    "native frontend did not exit cleanly")
        finally:
            screen.close()
            (directory / "screen.pty").write_bytes(screen.raw)

        sessions = traces(home)
        (directory / "journals.json").write_text(json.dumps(sessions, ensure_ascii=False))
        if arm == "N0":
            session, rows = one_trace(sessions, PROMPT.decode().strip(), "edit_file")
            candidate = commit(cwd)
            artifact = git_artifact(cwd, candidate, base_tree)
            stages = {"basic": (session, rows)}
        else:
            checkout, = (home / ".ansa/orchestration").glob("*/*/checkout")
            run = checkout.parent
            snapshot = evaluation.decode(evaluation.read(run / "snapshot.json"))
            require(snapshot["source_head"] == original, "snapshot used a different source commit")
            candidate = evaluation.read(run / "candidate-0").decode()
            artifact = git_artifact(checkout, candidate, base_tree)
            source_clean = git(cwd, "status", "--porcelain").stdout == b""
            require(source_clean and (cwd / "report.txt").read_bytes() == INITIAL,
                    "orchestration changed source checkout")
            approved = evaluation.decode(evaluation.read(run / "verification-0/approved-plan.json"))
            require(approved["command_checks"] == [{"argv": ["./check.sh"]}], "wrong approved plan")
            builder = one_trace(sessions, "Синтетическая правка отчёта", "edit_file")
            parent_id = run.parent.name
            parent_rows = sessions[parent_id]
            draft_at, = [index for index, row in enumerate(parent_rows)
                         if row["kind"] == "accepted" and row["text"] == PROMPT.decode().strip()]
            draft = []
            for row in parent_rows[draft_at + 1:]:
                if row["kind"] == "accepted":
                    break
                draft.append(row)
            require([row["kind"] for row in draft].count("response") == 1
                    and any(row["kind"] == "done" for row in draft)
                    and not any(row["kind"] in ("intent", "outcome") for row in draft),
                    "planner did not complete a native tool-free turn in the parent")
            stages = {"planner": (parent_id, {"plan": approved, "journal": draft}), "builder": builder,
                      "verification": (None, command_evidence(run, candidate))}
            if arm == "N1":
                require(not (run / "review-0").exists() and not (run / "release.json").exists()
                        and git(checkout, "show-ref", "--verify", "--quiet", "refs/heads/ansa-result",
                                check=False).returncode == 1, "N1 entered review or release")
            else:
                require(git(checkout, "rev-parse", "ansa-result").stdout.strip() == candidate.encode(),
                        "released branch differs from frozen candidate")
                reviewer, rows = one_trace(sessions, "Независимо проверьте замороженный коммит",
                                           "read_file")
                require(any(candidate in row.get("text", "") for row in rows),
                        "review targeted another candidate")
                stages["reviewer"] = reviewer, rows

        receipt["stages"] = []
        for role, (session, data) in stages.items():
            raw = json.dumps(data, ensure_ascii=False).encode()
            (directory / "stages" / f"{role}.json").write_bytes(raw)
            receipt["stages"].append({"role": role, "session": session, "sha256": evaluation.sha(raw)})
        (directory / "artifacts" / "report.txt").write_bytes(artifact)
        (directory / "candidate.json").write_text(json.dumps(
            {"base": original, "base_tree": base_tree.decode(), "candidate": candidate}))
        events = [event for rows in sessions.values() for event in rows]
        receipt["usage"] = {
            "calls": sum(event["kind"] == "response" for event in events),
            "tools": sum(event["kind"] == "outcome" for event in events),
            "retries": 0, "checks": int(arm != "N0"),
            "compacts": sum(event["kind"] == "compacted" for event in events),
            "wall_ms": int((time.monotonic() - started) * 1000),
            "reported_tokens": None, "estimated_tokens": None,
        }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=Path,
                        help="ansa compiled with frontend-test-adapter")
    parser.add_argument("--output", required=True, type=Path, help="fresh evidence directory")
    args = parser.parse_args()
    output = args.output.resolve()
    output.mkdir(mode=0o700)
    binary = args.binary.resolve()
    build = hashlib.sha256()
    with binary.open("rb") as stream:
        for block in iter(lambda: stream.read(64 * 1024), b""):
            build.update(block)
    source = Path(__file__).resolve().parents[1]
    (output / "prompt.txt").write_bytes(PROMPT)
    (output / "report-start.txt").write_bytes(INITIAL)
    (output / "check.sh").write_bytes(CHECK)
    manifest = output / "manifest.json"
    budget = dict.fromkeys(evaluation.COUNTERS, 12) | dict.fromkeys(evaluation.TOKENS)
    budget["wall_ms"] = 40_000
    spec = {
        "version": 1, "comparison": "orchestration", "evidence": "synthetic",
        "seed": "carbon-offline-fixture", "repetitions": 1,
        "provenance": {
            "source": evaluation.sha(git(source, "rev-parse", "HEAD").stdout),
            "build": build.hexdigest(), "route": "synthetic",
            "account_binding": evaluation.sha(b"offline-fixture"), "model": "gpt-6.1-sol",
            "effort": "default", "tools": build.hexdigest(), "policy": evaluation.sha(CHECK),
            "context": evaluation.sha(PROMPT), "environment": evaluation.sha(platform.platform().encode()),
        },
        "variants": {arm: evaluation.sha(",".join(evaluation.STAGES[arm]).encode())
                     for arm in evaluation.ARMS["orchestration"]},
        "budget": budget,
        "total_budget": {key: value * 3 if value is not None else None
                         for key, value in budget.items()},
        "tasks": [{"id": "status", "split": "development", "checks": {"report.txt": evaluation.sha(EXPECTED)},
                   "inputs": {"prompt.txt": evaluation.sha(PROMPT), "report-start.txt": evaluation.sha(INITIAL),
                              "check.sh": evaluation.sha(CHECK)}}],
    }
    manifest.write_text(json.dumps(spec, indent=2) + "\n")
    spec, manifest_hash, evaluator = evaluation.freeze(manifest)
    identity = {"manifest_sha256": manifest_hash, "evaluator_sha256": evaluator}
    receipts = output / "receipts"
    for row in evaluation.schedule(spec):
        directory = receipts / row["run_id"]
        (directory / "artifacts").mkdir(parents=True)
        (directory / "stages").mkdir()
        arm = row["arm"]
        receipt = identity | {
            "run_id": row["run_id"], "workspace": row["run_id"].replace(".", "-"),
            "variant_sha256": spec["variants"][arm], "provenance": spec["provenance"],
        }
        try:
            capture(binary, directory, arm, receipt)
        except (AssertionError, KeyError, OSError, ValueError, subprocess.SubprocessError) as error:
            (directory / "capture-error.txt").write_text(str(error))
        else:
            receipt["outcome"] = "completed"
            (directory / "receipt.json").write_text(json.dumps(receipt, indent=2) + "\n")
    command = ["report", str(manifest), str(receipts)]
    with (output / "report.json").open("wb") as result:
        status = subprocess.run([sys.executable, "-B", evaluation.__file__, *command], stdout=result,
                                timeout=10, check=False).returncode
    report = evaluation.decode(evaluation.read(output / "report.json"))
    return status or int(any(row["status"] != "accepted" for row in report["schedule"]))


if __name__ == "__main__":
    raise SystemExit(main())
