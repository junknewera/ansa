#!/usr/bin/env bash
# Reproduce the public benchmark acceptance on Linux. No provider credentials.
set -euo pipefail
if (( $# > 1 )); then
  echo "Usage: $0 [new-output-directory]" >&2
  exit 2
fi
repo=$(cd "$(dirname "$0")/.." && pwd)
if (( $# )); then
  mkdir -m 700 -- "$1"
  evidence=$(cd "$1" && pwd)
else
  evidence=$(mktemp -d "${TMPDIR:-/tmp}/ansa-public-check.XXXXXXXX")
fi
echo "Evidence: $evidence"
cd "$repo"
docker info --format 'Docker {{.ServerVersion}} {{.OSType}}/{{.Architecture}}'
python=${PYTHON:-python3}
harness="$evidence/swebench"
revision=$("$python" -Bc 'import sys; sys.path.insert(0,"benchmarks"); from image_grade import HARNESS; print(HARNESS)')
git init -q "$harness"
git -C "$harness" fetch --depth 1 https://github.com/SWE-bench/SWE-bench.git "$revision"
git -C "$harness" checkout -q --detach FETCH_HEAD
test "$(git -C "$harness" rev-parse HEAD)" = "$revision"
if command -v uv >/dev/null; then
  uv=$(command -v uv)
else
  "$python" -m venv "$evidence/tools"
  "$evidence/tools/bin/python" -m pip install --disable-pip-version-check uv==0.13.9
  uv="$evidence/tools/bin/uv"
fi
"$uv" sync --project "$harness" --locked --no-dev --no-install-project --python "$python"
python="$harness/.venv/bin/python"
uri=$("$python" -c 'from pathlib import Path; import sys; print(Path(sys.argv[1]).as_uri())' "$harness")
"$uv" pip install --no-deps --python "$python" "swebench @ git+$uri@$revision"

# Run Cargo in rust/ so rustup selects the repository toolchain.
(
  cd rust
  cargo build --locked -p ansa --bin ansa --example frontend_adapter --features frontend-test-adapter
  cargo metadata --no-deps --format-version 1 > "$evidence/cargo.json"
)
target=$("$python" -c 'import json,sys;print(json.load(open(sys.argv[1]))["target_directory"])' "$evidence/cargo.json")
"$python" -B benchmarks/prepare_public.py --output "$evidence/tasks" --binary "$target/debug/ansa" --model offline
"$python" -B benchmarks/check_public_controls.py "$evidence/tasks/manifest.json" "$evidence/controls"
"$python" -B benchmarks/image_transport_smoke.py --manifest "$evidence/tasks/manifest.json" \
  --binary "$target/debug/examples/frontend_adapter" --output "$evidence/wire"
echo "Verified 8 public tasks. Reports: $evidence/controls/results.json and $evidence/wire/result.json"
