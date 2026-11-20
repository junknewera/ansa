"""Plan bounded native comparisons and grade frozen artifacts offline.

Use `plan MANIFEST` before capture and `report MANIFEST RECEIPTS` after it.
The trusted capture host writes one <run_id>/receipt.json per planned run plus
its stages/<role>.json and artifacts/<filename>. Keep the oracle manifest and
the evaluator outside the solver's workspace. This tool verifies the archived
identity and checks the actual output bytes. It neither runs model calls nor
authenticates untrusted capture hosts. The Carbon synthetic example is in
native_eval_fixture.py.
"""

import argparse
import hashlib
import itertools
import json
from pathlib import Path
import re
import stat
import sys

ARMS = {"orchestration": ("N0", "N1", "N2"), "profile": ("baseline", "candidate")}
STAGES = {
    "N0": ("basic",),
    "N1": ("planner", "builder", "verification"),
    "N2": ("planner", "builder", "verification", "reviewer"),
    "baseline": ("profile",),
    "candidate": ("profile",),
}
COUNTERS = ("calls", "tools", "retries", "checks", "compacts", "wall_ms")
TOKENS = ("reported_tokens", "estimated_tokens")
METRICS = (*COUNTERS, *TOKENS)
OUTCOMES = ("completed", "failed", "blocked", "timeout", "cancelled", "infra_error")
HASHES = ("source", "build", "account_binding", "tools", "policy", "context", "environment")
PROVENANCE = (*HASHES, "route", "model", "effort")
MAX_BYTES = 256 * 1024
MAX_RUNS = 64


def require(condition, message):
    if not condition:
        raise ValueError(message)


def fields(obj, names, label):
    require(isinstance(obj, dict) and obj.keys() == set(names), f"invalid {label} fields")


def label(value):
    return isinstance(value, str) and re.fullmatch(r"[A-Za-z0-9_-]{1,64}", value)


def sha(data):
    return hashlib.sha256(data).hexdigest()


def hashed(value):
    return isinstance(value, str) and re.fullmatch(r"[a-f0-9]{64}", value)


def metrics(values):
    fields(values, METRICS, "metric")
    for key, value in values.items():
        require((key in TOKENS and value is None) or
                (type(value) is int and 0 <= value <= 2**53 - 1), f"invalid {key}")


def fixture_path(root, name):
    require(isinstance(name, str) and len(name) <= 256, "invalid filename")
    for part in name.split("/"):
        require(re.fullmatch(r"[A-Za-z0-9_-][A-Za-z0-9_.-]*", part), "invalid path component")
        root /= part
        require(not root.is_symlink(), "symlink in fixture path")
    return root


def read(path):
    require(stat.S_ISREG(path.lstat().st_mode), "input must be a regular file")
    with path.open("rb") as stream:
        data = stream.read(MAX_BYTES + 1)
    require(len(data) <= MAX_BYTES, "input exceeds byte limit")
    return data


def decode(data):
    def no_duplicates(pairs):
        result = dict(pairs)
        require(len(result) == len(pairs), "duplicate JSON key")
        return result

    return json.loads(data, object_pairs_hook=no_duplicates)


def freeze(path):
    raw = read(path)
    spec = decode(raw)
    fields(spec, ("version", "comparison", "evidence", "seed", "repetitions",
                  "provenance", "variants", "budget", "total_budget", "tasks"), "manifest")
    require(type(spec["version"]) is int and spec["version"] == 1, "unsupported version")
    require(spec["comparison"] in tuple(ARMS), "unknown comparison")
    arms = ARMS[spec["comparison"]]
    require(spec["evidence"] in ("synthetic", "captured"), "unknown evidence type")
    require(label(spec["seed"]), "invalid seed")
    repeats = spec["repetitions"]
    require(type(repeats) is int and repeats > 0, "invalid repetitions")
    provenance = spec["provenance"]
    fields(provenance, PROVENANCE, "provenance")
    require(all(hashed(provenance[key]) for key in HASHES), "invalid provenance hash")
    require(all(isinstance(provenance[key], str) and
                re.fullmatch(r"[A-Za-z0-9_.-]{1,80}", provenance[key])
                for key in ("model", "effort")), "invalid model or effort")
    require(provenance["route"] == ("synthetic" if spec["evidence"] == "synthetic" else "siwc"),
            "route/evidence mismatch")
    variants = spec["variants"]
    fields(variants, arms, "variant")
    require(all(hashed(value) for value in variants.values()) and
            len(set(variants.values())) == len(arms), "invalid variant hashes")
    for name in ("budget", "total_budget"):
        metrics(spec[name])
        require(spec[name]["wall_ms"] > 0, "wall budget must be positive")
    tasks = spec["tasks"]
    require(isinstance(tasks, list) and 0 < len(tasks) * len(arms) * repeats <= MAX_RUNS,
            "trial count outside bounds")
    ids = set()
    for task in tasks:
        fields(task, ("id", "split", "inputs", "checks"), "task")
        require(label(task["id"]) and task["id"] not in ids, "invalid or duplicate task id")
        ids.add(task["id"])
        require(task["split"] in ("development", "holdout"), "invalid split")
        for group in ("inputs", "checks"):
            filenames = task[group]
            require(isinstance(filenames, dict) and 0 < len(filenames) <= 16,
                    f"invalid {group}")
            for filename, wanted in filenames.items():
                require(hashed(wanted), f"invalid {group} hash")
                artifact = fixture_path(path.parent, filename)
                if group == "inputs":
                    require(sha(read(artifact)) == wanted, "changed fixture input")
    evaluator = sha(read(Path(__file__)))
    return spec, sha(raw), evaluator


def schedule(spec):
    rows = []
    for task, repeat, arm in itertools.product(
        spec["tasks"], range(1, spec["repetitions"] + 1), ARMS[spec["comparison"]]
    ):
        rows.append({"run_id": f'{task["id"]}.{repeat}.{arm}', "task": task["id"],
                     "split": task["split"], "arm": arm})
    return sorted(rows, key=lambda row: sha(f'{spec["seed"]}/{row["run_id"]}'.encode()))


def budget_status(usage, limits):
    return {
        "exceeded": [key for key in METRICS if limits[key] is not None and
                     usage[key] is not None and usage[key] > limits[key]],
        "unknown": [key for key in METRICS if limits[key] is not None and usage[key] is None],
    }


def grade(root, row, spec, identity, checks):
    run_id = row["run_id"]
    raw = read(fixture_path(root, f"{run_id}/receipt.json"))
    row["receipt_sha256"] = sha(raw)
    receipt = decode(raw)
    fields(receipt, (*identity, "run_id", "variant_sha256", "provenance",
                     "stages", "workspace", "outcome", "usage"), "receipt")
    require(all(receipt[key] == value for key, value in identity.items()) and
            receipt["run_id"] == run_id and
            receipt["variant_sha256"] == spec["variants"][row["arm"]] and
            receipt["provenance"] == spec["provenance"], "receipt identity mismatch")
    require(label(receipt["workspace"]), "invalid workspace identity")
    require(receipt["outcome"] in OUTCOMES, "invalid recorded outcome")
    metrics(receipt["usage"])
    row.update({key: receipt[key] for key in ("workspace", "outcome", "usage")})
    stages = receipt["stages"]
    expected = STAGES[row["arm"]]
    require(isinstance(stages, list) and len(stages) <= len(expected), "invalid stage inventory")
    require(receipt["outcome"] != "completed" or len(stages) == len(expected), "missing stage")
    sessions = []
    row["sessions"] = sessions
    for stage, role in zip(stages, expected):
        fields(stage, ("role", "session", "sha256"), "stage")
        require(stage["role"] == role and hashed(stage["sha256"]), "invalid stage identity")
        if role == "verification":
            require(stage["session"] is None, "verification must be host-owned")
        else:
            require(label(stage["session"]), "invalid session identity")
            sessions.append(stage["session"])
        journal = read(fixture_path(root, f"{run_id}/stages/{role}.json"))
        require(journal and sha(journal) == stage["sha256"], "changed stage evidence")
    row["stages"] = stages
    budget = budget_status(receipt["usage"], spec["budget"])
    row["budget"] = budget
    if budget["exceeded"]:
        row["status"] = "budget_exceeded"
    elif budget["unknown"]:
        row["status"] = "budget_unproven"
    elif receipt["outcome"] != "completed":
        row["status"] = receipt["outcome"]
    else:
        observed = {}
        for filename, wanted in checks.items():
            artifact = fixture_path(root, f"{run_id}/artifacts/{filename}")
            try:
                actual = sha(read(artifact))
            except FileNotFoundError:
                actual = None
            observed[filename] = {"expected": wanted, "actual": actual}
        row["checks"] = observed
        accepted = all(check["actual"] == check["expected"] for check in observed.values())
        row["status"] = "accepted" if accepted else "rejected"


def totals(rows):
    result = {}
    for key in METRICS:
        measured = [row.get("usage", {}).get(key) for row in rows]
        result[key] = None if None in measured else sum(measured)
    return result


def report(spec, identity, root):
    require(root.is_dir() and not root.is_symlink(), "invalid evidence directory")
    rows = schedule(spec)
    tasks = {task["id"]: task for task in spec["tasks"]}
    for row in rows:
        try:
            grade(root, row, spec, identity, tasks[row["task"]]["checks"])
        except FileNotFoundError:
            row["status"] = "missing"
        except (ValueError, TypeError, OSError, RecursionError) as error:
            row.update(status="invalid", error=str(error))
    sessions = [session for row in rows for session in row.get("sessions", [])]
    workspaces = [row["workspace"] for row in rows if "workspace" in row]
    for row in rows:
        if any(sessions.count(session) > 1 for session in row.get("sessions", [])):
            row.update(status="invalid", error="reused session identity")
        if "workspace" in row and workspaces.count(row["workspace"]) > 1:
            row.update(status="invalid", error="reused workspace identity")
    summary = {}
    for arm in ARMS[spec["comparison"]]:
        summary[arm] = {}
        for split in sorted({row["split"] for row in rows}):
            batch = [row for row in rows if row["arm"] == arm and row["split"] == split]
            accepted = sum(row["status"] == "accepted" for row in batch)
            usage = totals(batch)
            calls = usage["calls"]
            summary[arm][split] = {
                "planned": len(batch), "accepted": accepted, "usage": usage,
                "calls_per_accepted": calls / accepted if calls is not None and accepted else None,
            }
    total_usage = totals(rows)
    budget = budget_status(total_usage, spec["total_budget"])
    return {"schedule": rows, "summary": summary, "total_usage": total_usage,
            "total_budget": budget,
            "complete": not any(budget.values()) and
            all(row["status"] in ("accepted", "rejected") for row in rows)}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    plan = commands.add_parser("plan", help="verify fixture bytes and print the run order")
    plan.add_argument("manifest", type=Path)
    evaluate = commands.add_parser("report", help="grade previously captured trials")
    evaluate.add_argument("manifest", type=Path)
    evaluate.add_argument("receipts", type=Path)
    args = parser.parse_args()
    try:
        spec, manifest, evaluator = freeze(args.manifest)
        identity = {"manifest_sha256": manifest, "evaluator_sha256": evaluator}
        result = {"schedule": schedule(spec)} if args.command == "plan" else (
            report(spec, identity, args.receipts)
        )
        result.update(identity, comparison=spec["comparison"], evidence=spec["evidence"],
                      provenance=spec["provenance"], budget_limits=spec["budget"],
                      total_budget_limits=spec["total_budget"], variants=spec["variants"])
        print(json.dumps(result, indent=2, sort_keys=True))
        return 0 if args.command == "plan" or result["complete"] else 1
    except (ValueError, TypeError, OSError, RecursionError) as error:
        print(str(error), file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
