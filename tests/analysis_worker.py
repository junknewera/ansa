"""Fixed worker against an inherited hostile rc and bounded exact CSV data."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile


def run(output):
    output.mkdir(parents=True, exist_ok=False)
    worker = Path(__file__).resolve().parents[1] / "rust/crates/ansa/src/tools/analysis/worker.py"
    with tempfile.TemporaryDirectory(prefix="ansa-analysis-worker-", dir="/tmp") as directory:
        root = Path(directory)
        hostile = root / "hostile-rc"
        hostile.write_text("savefig.dpi: 10000\ntext.usetex: True\nfigure.figsize: 1000, 1000\n")
        (root / "worker.py").write_bytes(worker.read_bytes())
        (root / "input.csv").write_text("date,value\n2026-01-01,0.1\n2026-01-02,0.2\n2026-02-02,-0.05\n2026-02-03,,\n".replace("2026-02-03,,", "2026-02-03,"))
        (root / "parameters.json").write_text(json.dumps(dict(date_column="date", value_column="value")))
        child = subprocess.run([sys.executable, "-I", "-B", str(root / "worker.py")], cwd=root,
                               env={"PATH":os.environ["PATH"], "HOME":str(root), "MATPLOTLIBRC":str(hostile)},
                               capture_output=True, timeout=15)
        (output / "stdout").write_bytes(child.stdout)
        (output / "stderr").write_bytes(child.stderr)
        assert child.returncode == 0, child.stderr.decode(errors="replace")
        proof = json.loads(child.stdout)
        assert proof["summary"] == dict(rows=4, missing=1, months=2, total="0.25")
        for name, known in proof["outputs"].items():
            contents = (root / name).read_bytes()
            assert known == dict(size_bytes=len(contents), sha256=hashlib.sha256(contents).hexdigest())
        for name, known in proof["inputs"].items():
            assert known == hashlib.sha256((root / name).read_bytes()).hexdigest()
        from PIL import Image
        with Image.open(root / "chart.png") as image:
            assert image.size == (1000, 500) and image.n_frames == 1
            image.load()
        (output / "chart.png").write_bytes((root / "chart.png").read_bytes())
    result = dict(passed=3, cases=["hostile-MATPLOTLIBRC-ignored", "fixed-1000x500-decoded-PNG", "exact-decimal-and-input-output-manifest"])
    (output / "report.json").write_text(json.dumps(result, indent=2))
    print(json.dumps(result))


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--output", required=True, type=Path)
    run(parser.parse_args().output)
