"""Prepare the public Terminal-Bench log summarization task for ANSA."""

import argparse
from pathlib import Path
import subprocess
import sys
from urllib.request import urlopen

from bench import write

REVISION = "2fd12b88aafdd04a52c298e3940bcb189f9766d6"
TASK = f"https://github.com/harbor-framework/terminal-bench-2/tree/{REVISION}/log-summary-date-ranges"
RAW = TASK.replace("github.com", "raw.githubusercontent.com").replace("/tree/", "/")


def upstream(path):
    with urlopen(f"{RAW}/{path}", timeout=30) as response:
        data = response.read(256 * 1024 + 1)
    if len(data) > 256 * 1024:
        raise ValueError("upstream file exceeds download bound")
    return data


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--auth-dir", type=Path, required=True)
    parser.add_argument("--route", choices=("siwc", "openrouter"), required=True)
    parser.add_argument("--model", required=True)
    args = parser.parse_args()
    output = args.output.resolve()
    output.mkdir(mode=0o700)
    inputs = output / "inputs"
    inputs.mkdir()
    operator = output / "operator"
    operator.mkdir(mode=0o700)

    files = ("instruction.md", "environment/log_generator_deterministic.py", "tests/test_outputs.py")
    instruction, generator, tests = [upstream(path) for path in files]
    for name, data in zip(("instruction.md", "generator.py", "upstream_tests.py"),
                          (instruction, generator, tests)):
        (operator / name).write_bytes(data)
    # The Harbor task mounts its workspace at /app. The local runner uses a fresh cwd.
    prompt = instruction.decode().replace("/app/", "./").strip()
    original = 'output_dir = "/app/logs"'
    generator = generator.decode()
    if generator.count(original) != 1:
        raise ValueError("upstream log generator no longer has the expected output path")
    local_generator = operator / "local_generator.py"
    local_generator.write_text(generator.replace(original, 'output_dir = "logs"'))
    subprocess.run([sys.executable, "-B", str(local_generator)], cwd=inputs, check=True, timeout=60)

    verifier = operator / "verify.py"
    verifier.write_text(f"#!{sys.executable}\n" + """import runpy
from pathlib import Path
import sys

tests = runpy.run_path(str(Path(__file__).with_name("upstream_tests.py")))
try:
    for name in ("test_summary_file_exists", "test_summary_structure_and_counts"):
        check = tests[name]
        check.__globals__["OUTPUT_FILE"] = Path(sys.argv[1]) / "summary.csv"
        check()
except (AssertionError, FileNotFoundError, UnicodeError) as error:
    print(error, file=sys.stderr)
    sys.exit(1)
""")
    verifier.chmod(0o700)

    arm = {"binary": str(args.binary.resolve()), "route": args.route, "model": args.model,
           "mode": "work", "auth_dir": str(args.auth_dir.resolve())}
    (operator / "candidate.md").write_text("Before finishing, verify the output against the user's requirements.\n")
    write(output / "manifest.json", {
        "version": 1, "seed": "terminal-bench-logs", "repetitions": 1,
        "arms": [arm | {"id": "baseline"}, arm | {"id": "candidate", "guidance": "operator/candidate.md"}],
        "budget": {"trial_seconds": 300, "verifier_seconds": 15, "total_seconds": 630,
                   "tool_rounds": 16, "tool_calls": 32},
        "tasks": [{"id": "log-summary-date-ranges", "source": TASK,
                   "modes": ["work", "orchestrate", "orchestrate+"],
                   "turns": [prompt], "workspace": "inputs", "verifier": "operator/verify.py",
                   "verifier_inputs": ["operator/upstream_tests.py"]}],
    })


if __name__ == "__main__":
    main()
