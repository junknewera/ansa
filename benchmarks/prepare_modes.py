"""Prepare the development and holdout inputs for the matched-model mode comparison."""
import argparse
from pathlib import Path
import sys

from bench import load, write
from public_terminal import prepare as public_task

MODES = ["orchestrate", "orchestrate+"]
MODEL = {"model": "gpt-6.2-sol", "effort": "xhigh"}


def script(path, body):
    path.write_text(f"#!{sys.executable}\n{body}")
    path.chmod(0o700)


def fixture(root, name, split, filename, initial, positive, checks, prompt, clarification=None):
    workspace = root / "inputs"
    operator = root / "operator"
    workspace.mkdir(parents=True)
    operator.mkdir()
    (workspace / filename).write_text(initial)
    test_script = "#!/bin/sh\nset -eu\n" + checks
    (workspace / "test.sh").write_text(test_script)
    (workspace / "test.sh").chmod(0o700)
    operator_test = operator / "test.sh"
    operator_test.write_text(test_script + (
        f'test "$(cat {filename})" = {positive.strip()}\n' if clarification else ""))
    verifier = operator / "verify"
    script(verifier, f"""from pathlib import Path
import subprocess
import sys
work = Path(sys.argv[1])
raise SystemExit(subprocess.run(["/bin/sh", {str(operator_test)!r}], cwd=work, timeout=10).returncode)
""")
    control = operator / "control"
    script(control, f"""from pathlib import Path
import sys
work = Path(sys.argv[2])
(work / {filename!r}).write_text({positive!r} if sys.argv[1] == "pass" else {initial!r})
Path(sys.argv[3]).write_text("{{}}")
""")
    task = {"id": name, "family": name, "split": split,
            "category": "clarification" if clarification else "patch", "modes": MODES,
            "turns": [prompt + f" Only {filename} may be changed. Run ./test.sh to verify."],
            "workspace": str(workspace), "verifier": str(verifier),
            "oracle_fixture": str(control), "verifier_inputs": [str(operator_test)],
            "plan_requirements": {"paths": [filename], "check": "test.sh"}}
    if clarification:
        task["clarification"] = clarification
    return task


def fixtures(root):
    patch = (
        ("retry-delay", "development", "retry.py",
         "def delay(attempt):\n    return 30\n",
         "def delay(attempt):\n    return min(30, 2 ** attempt)\n",
         """python3 -B -c 'from retry import delay; assert [delay(n) for n in (0, 1, 2, 4, 5, 8)] == [1,2,4,16,30,30]'\n""",
         "Implement exponential retry delay. Attempts are zero-based and the delay is capped at 30 seconds."),
        ("cache-label", "holdout", "labels.py",
         "def label(key, region):\n    return key\n",
         "def label(key, region):\n    return region.strip().lower() + ':' + key.strip()\n",
         """python3 -B -c 'from labels import label; assert label(" AbC ", " US ") == "us:AbC"; assert label(" v ", "Eu ") == "eu:v"'\n""",
         "Prefix the trimmed cache key with the trimmed lowercase region and a colon. Preserve key case."),
    )
    for name, split, *args in patch:
        yield fixture(root / name, name, split, *args)
    for name, split, field, answer, alternative in (
        ("choose-region", "development", "region", "west", "east"),
        ("choose-retention", "holdout", "retention", "21", "7"),
    ):
        path = f"{field}.txt"
        question = f"Which {field} should I use?"
        prompt = (f"Implement the requested {field} setting in {path}. The allowed values are "
                  f"{answer} and {alternative}. The user has yet to choose. "
                  f"In the initial proposed plan ask exactly '{question}' and wait for my answer. "
                  "After my answer, update the plan and implement the chosen setting.")
        yield fixture(root / name, name, split, path, "unset\n", answer + "\n",
                      f'case "$(cat {path})" in {answer}|{alternative}) ;; *) exit 1 ;; esac\n', prompt,
                      {"question": question, "answer": f"Use {answer}."})


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--auth-dir", type=Path, required=True)
    args = parser.parse_args()
    root = args.output.resolve()
    root.mkdir(mode=0o700)
    tasks = list(fixtures(root))
    for name, target in (("count-dataset-tokens", "answer.txt"), ("mteb-retrieve", "result.txt")):
        directory = root / name
        directory.mkdir()
        task = public_task(name, directory)
        task.update(split="development", modes=MODES, plan_requirements={"paths": [target]})
        task["turns"][0] += (f" Only {target} may be changed. Include an executable check for the result.")
        if name == "mteb-retrieve":
            task["turns"][0] += (' Prefix the query with "为这个句子生成表示以用于检索相关文章：" '
                                 "before encoding; encode the documents without that prefix.")
        tasks.append(task)
    common = MODEL | {"binary": str(args.binary.resolve(strict=True)), "route": "siwc",
                      "auth_dir": str(args.auth_dir.resolve(strict=True))}
    arms = [common | {"id": "orch", "mode": "orchestrate"},
            common | {"id": "plus", "mode": "orchestrate+",
                      "roles": {role: MODEL for role in ("strong", "medium", "weak")}}]
    manifest = root / "manifest.json"
    spec = {"version": 1, "seed": "matched-orchestration-v1", "repetitions": 1,
            "arms": arms, "tasks": tasks, "comparison": {"left": "orch", "right": "plus",
                                                        "factor": "mode"},
            "budget": {"trial_seconds": 600, "verifier_seconds": 300, "total_seconds": 7200,
                       "tool_rounds": 32, "tool_calls": 64}}
    write(manifest, spec)
    load(manifest)
    holdout = root / "holdout-manifest.json"
    write(holdout, spec | {"repetitions": 3, "budget": spec["budget"] | {"total_seconds": 10800}})
    load(holdout)


if __name__ == "__main__":
    main()
