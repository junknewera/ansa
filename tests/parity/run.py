#!/usr/bin/env python3
"""Small, offline M0 evidence capture. This is not a milestone scheduler."""

import argparse
from contextlib import ExitStack
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import platform
import signal
import subprocess
import sys
import tempfile
import time
import xml.etree.ElementTree as ET

ROOT = Path(__file__).resolve().parents[2]
MANIFEST = ROOT / "tests/parity/fixtures/manifest.json"


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def write_json(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n")


def platform_info():
    release = {}
    if Path("/etc/os-release").is_file():
        for line in Path("/etc/os-release").read_text().splitlines():
            key, _, value = line.partition("=")
            if key in ("ID", "VERSION_ID"):
                release[key] = value.strip('"')
    return {"system": platform.system(), "machine": platform.machine(),
            "kernel": platform.release(), "os_release": release}


def source_info():
    def git(*args):
        return subprocess.check_output(["git", "-C", str(ROOT), *args], timeout=15)
    paths = git("ls-files", "--cached", "--others", "--exclude-standard", "-z", "--",
                "CMakeLists.txt", "flake.nix", "flake.lock", "include", "src", "tests", "rust")
    # Only reviewed code/config/fixture types; never inspect environment/credential files.
    suffixes = {".cpp", ".hpp", ".rs", ".py", ".sh", ".cmake", ".nix", ".json", ".sse", ".toml", ".lock"}
    files = {}
    for name in sorted(set(paths.decode().split("\0")) - {""}):
        path = ROOT / name
        if (path.is_file() and not path.is_symlink()
                and not any(part == ".env" or part.startswith(".env.") for part in path.parts)
                and (path.suffix in suffixes or path.name == "CMakeLists.txt")):
            files[name] = sha(path)
    return {"head": git("rev-parse", "HEAD").decode().strip(),
            "worktree_status": git("status", "--short").decode(), "files": files}


def command(argv, cwd, output, label, timeout=60, input_file=None):
    """Keep exact output bytes; kill/reap our process group on a timeout."""
    stdout = output / (label + ".stdout")
    stderr = output / (label + ".stderr")
    result = {"argv": [str(a) for a in argv], "cwd": str(cwd), "exit_code": None,
              "timed_out": False}
    start = time.monotonic()
    with ExitStack() as resources:
        out = resources.enter_context(stdout.open("wb"))
        err = resources.enter_context(stderr.open("wb"))
        stdin = resources.enter_context(Path(input_file).open("rb")) if input_file else subprocess.DEVNULL
        try:
            child = subprocess.Popen(result["argv"], cwd=cwd, stdin=stdin,
                                     stdout=out, stderr=err, start_new_session=True)
            try:
                result["exit_code"] = child.wait(timeout=timeout)
            except subprocess.TimeoutExpired:
                result["timed_out"] = True
                try:
                    os.killpg(child.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                result["exit_code"] = child.wait()
        except OSError as error:
            err.write(str(error).encode())
    result["elapsed_seconds"] = round(time.monotonic() - start, 6)
    result["stdout"] = {"file": stdout.name, "sha256": sha(stdout)}
    result["stderr"] = {"file": stderr.name, "sha256": sha(stderr)}
    return result


def output_json(result, directory):
    try:
        return json.loads((directory / result["stdout"]["file"]).read_bytes())
    except (ValueError, OSError):
        return {}


def succeeded(result):
    return result["exit_code"] == 0 and not result["timed_out"]


def preflight(probe, output, manifest):
    with tempfile.TemporaryDirectory(prefix="ansa-m0-preflight-") as cwd:
        # Deliberately collect both, including when the first one fails.
        linux = command([probe], cwd, output, "linux-preflight")
        rust = command([sys.executable, ROOT / "tests/parity/check_toolchain.py"],
                       cwd, output, "toolchain-preflight", timeout=120)
    result = {"schema_version": 1, "platform": platform_info(),
              "linux": linux, "toolchain": rust,
              "probe_sha256": sha(probe) if Path(probe).is_file() else None}
    result["errors"] = preflight_errors(result, output, manifest)
    return result


def preflight_errors(report, directory, manifest):
    errors = []
    for name in ("linux", "toolchain"):
        if not succeeded(report[name]):
            errors.append(name + " preflight failed or unavailable")
    linux = output_json(report["linux"], directory) if succeeded(report["linux"]) else {}
    checks = linux.get("checks", [])
    ids = [item["id"] for item in checks]
    if (linux.get("schema_version") != 1 or len(ids) != len(set(ids))
            or set(ids) != set(manifest["linux_checks"])
            or any(item["status"] != "pass" for item in checks)):
        errors.append("Linux required capabilities did not all pass")
    rust = output_json(report["toolchain"], directory) if succeeded(report["toolchain"]) else {}
    if rust.get("schema_version") != 1 or rust.get("status") != "PASS":
        errors.append("pinned Rust toolchain was not verified")
    return errors


def build_info(build):
    fields = {"CMAKE_BUILD_TYPE", "BUILD_TESTING", "OMH_SANITIZERS", "OMH_MUTATION_CHILD",
              "CMAKE_CXX_COMPILER", "CMAKE_HOME_DIRECTORY", "CMAKE_GENERATOR"}
    cache = {}
    for line in (build / "CMakeCache.txt").read_text().splitlines():
        key, sep, value = line.partition("=")
        key = key.split(":")[0]
        if sep and key in fields:
            cache[key] = value
    binaries = {p.name: sha(p) for p in build.iterdir()
                if p.is_file() and (p.name in ("ansa", "ansa_preflight") or p.name.endswith("_tests"))}
    return {"directory": str(build), "cache": cache, "binaries": binaries}


def fixture_errors(manifest):
    return ["fixture digest differs: " + item["path"] for item in manifest["fixtures"]
            if sha(ROOT / item["path"]) != item["sha256"]]


def expected_ids(manifest, cache):
    ids = manifest["ctest_cases"]
    if cmake_enabled(cache, "OMH_MUTATION_CHILD"):
        ids = [name for name in ids if name not in manifest["mutation_cases"]]
    return ids


def cmake_enabled(cache, name):
    # Boolean keywords fold ASCII only; NOTFOUND and its suffix are case-sensitive.
    value = cache.get(name, "")
    false_keyword = value.isascii() and value.upper() in ("", "0", "OFF", "NO", "FALSE", "N", "IGNORE")
    return (not false_keyword
            and value != "NOTFOUND"
            and not value.endswith("-NOTFOUND"))


def sanitizer_errors(cache, options):
    errors = []
    enabled = cmake_enabled(cache, "OMH_SANITIZERS")
    if cmake_enabled(cache, "OMH_MUTATION_CHILD") and not enabled:
        errors.append("non-sanitized baseline must include both mutation tests")
    if enabled and (options.get("ASAN_OPTIONS") != "detect_leaks=1"
                    or options.get("UBSAN_OPTIONS") != "halt_on_error=1"):
        errors.append("required ASan/UBSan runtime options were not recorded")
    return errors


def junit_cases(path):
    cases = []
    for case in ET.parse(path).iter("testcase"):
        status = "passed"
        if case.find("skipped") is not None or case.get("status") in ("notrun", "disabled"):
            status = "skipped"
        elif case.find("failure") is not None or case.find("error") is not None:
            status = "failed"
        cases.append({"id": case.attrib["name"], "status": status,
                      "seconds": case.get("time")})
    return cases


def case_errors(actual, expected):
    ids = [case["id"] for case in actual]
    errors = []
    if not ids or len(ids) != len(set(ids)) or set(ids) != set(expected):
        errors.append("CTest case IDs missing, duplicate, or different from manifest")
    failed = [case["id"] for case in actual if case["status"] != "passed"]
    if failed:
        errors.append("CTest cases failed/skipped: " + ", ".join(failed))
    return errors


def cli_errors(commands, directory):
    errors = []
    for name in ("cli-self-check", "offline-roundtrip"):
        if not succeeded(commands[name]):
            errors.append(name + " command failed")
            continue
        data = output_json(commands[name], directory)
        if name == "cli-self-check":
            valid = data.get("project") == "ansa" and all(
                isinstance(data.get(key), str) and data[key]
                for key in ("target", "curl", "tls", "yyjson", "openssl"))
        else:
            valid = (data.get("status") == "passed" and data.get("mode") == "offline"
                     and data.get("tool_invocations") == 1 and data.get("native_replay") is True
                     and data.get("final_text") == "OMH_TOOL_OK:OMH_FIXED_VALUE_42")
        if not valid:
            errors.append(name + " independent output invariant failed")
    return errors


def capture(args, manifest):
    output, build = args.output.resolve(), args.cpp_build.resolve()
    output.mkdir(parents=True, exist_ok=False)
    report = {"schema_version": 1, "kind": "m0-cpp-capture",
              "created_at_utc": datetime.now(timezone.utc).isoformat(),
              "manifest_sha256": sha(MANIFEST), "source": source_info(),
              "platform": platform_info(), "build": build_info(build),
              "sanitizer_options": {key: os.environ.get(key) for key in ("ASAN_OPTIONS", "UBSAN_OPTIONS")}}
    report["preflight"] = preflight(args.probe or build / "ansa_preflight", output, manifest)
    commands = report["commands"] = {}
    with tempfile.TemporaryDirectory(prefix="ansa-m0-cli-") as cwd:
        for name, flag in (("cli-self-check", "--self-check"), ("offline-roundtrip", "--offline-demo")):
            commands[name] = command([build / "ansa", flag], cwd, output, name)
        commands["cmake-version"] = command(["cmake", "--version"], cwd, output, "cmake-version")
        commands["ctest-version"] = command(["ctest", "--version"], cwd, output, "ctest-version")
        compiler = report["build"]["cache"].get("CMAKE_CXX_COMPILER", "c++")
        commands["compiler-version"] = command([compiler, "--version"], cwd, output, "compiler-version")
        inventory = command(["ctest", "--test-dir", build, "--show-only=json-v1"],
                            cwd, output, "ctest-inventory")
        commands["ctest-inventory"] = inventory
        ctest = report["ctest"] = {"status": "NOT_RUN"}
        if args.run_ctest:
            commands["ctest-run"] = command(
                ["ctest", "--test-dir", build, "--output-on-failure", "--no-tests=error",
                 "--output-junit", output / "ctest-results.xml"], cwd, output, "ctest-run", timeout=1800)
            ctest["status"] = "EXECUTED"
            if (output / "ctest-results.xml").is_file():
                ctest["junit"] = {"file": "ctest-results.xml", "sha256": sha(output / "ctest-results.xml")}
                try:
                    ctest["cases"] = junit_cases(output / "ctest-results.xml")
                except (ET.ParseError, KeyError):
                    ctest["cases"] = []
    errors = report["preflight"]["errors"] + cli_errors(commands, output) + fixture_errors(manifest)
    errors += [name + " command failed" for name in ("cmake-version", "ctest-version", "compiler-version")
               if not succeeded(commands[name])]
    expected = expected_ids(manifest, report["build"]["cache"])
    if succeeded(inventory):
        ctest["registered_ids"] = [test["name"] for test in output_json(inventory, output).get("tests", [])]
        errors += case_errors([{"id": name, "status": "passed"} for name in ctest["registered_ids"]], expected)
    else:
        errors.append("CTest inventory unavailable")
    if args.run_ctest:
        errors += case_errors(ctest.get("cases", []), expected)
        if not succeeded(commands["ctest-run"]):
            errors.append("CTest command failed")
    report["errors"] = errors
    report["ctest"]["expected_ids"] = expected
    report["scope"] = "Recorded one C++ build. NOT_RUN CTest is pending; this is not an M0 GO."
    write_json(output / "capture.json", report)
    print(json.dumps({"capture": str(output / "capture.json"), "errors": errors,
                      "ctest": ctest["status"]}, indent=2))
    return int(bool(errors))


def check(directory, manifest):
    report = json.loads((directory / "capture.json").read_bytes())
    errors = fixture_errors(manifest)
    if report["schema_version"] != 1 or report["kind"] != "m0-cpp-capture":
        errors.append("unsupported evidence format")
    if report["manifest_sha256"] != sha(MANIFEST) or report["source"]["files"] != source_info()["files"]:
        errors.append("source/fixture/manifest snapshot differs from current checkout")
    build = report["build"]
    if build != build_info(Path(build["directory"])):
        errors.append("captured binaries/build configuration differ or are missing")
    probe = report["preflight"]
    if probe["probe_sha256"] != sha(Path(probe["linux"]["argv"][0])):
        errors.append("captured Linux probe binary differs")
    commands = {**report["commands"], **{k: report["preflight"][k] for k in ("linux", "toolchain")}}
    for name, result in commands.items():
        for stream in ("stdout", "stderr"):
            item = result[stream]
            if sha(directory / item["file"]) != item["sha256"]:
                errors.append(name + " " + stream + " digest mismatch")
        if not succeeded(result):
            errors.append(name + " failed or unavailable")
    errors += preflight_errors(report["preflight"], directory, manifest)
    errors += cli_errors(report["commands"], directory)
    cache = build["cache"]
    if (not cmake_enabled(cache, "BUILD_TESTING") or cache.get("CMAKE_BUILD_TYPE") not in ("Debug", "Release")
            or Path(cache.get("CMAKE_HOME_DIRECTORY", "")).resolve() != ROOT):
        errors.append("CMake source/profile/testing is not an eligible baseline")
    errors += sanitizer_errors(cache, report["sanitizer_options"])
    expected = expected_ids(manifest, cache)
    inventory = output_json(report["commands"]["ctest-inventory"], directory)
    errors += case_errors([{"id": test["name"], "status": "passed"} for test in inventory.get("tests", [])], expected)
    if report["ctest"]["status"] != "EXECUTED" or "ctest-run" not in commands:
        errors.append("CTest results pending; capture with --run-ctest")
    else:
        item = report["ctest"]["junit"]
        if sha(directory / item["file"]) != item["sha256"]:
            errors.append("JUnit digest mismatch")
        errors += case_errors(junit_cases(directory / item["file"]), expected)
    host = report["platform"]
    if host["system"] != "Linux" or host["machine"] != "aarch64" or host["os_release"].get("ID") != "nixos":
        errors.append("required native NixOS/aarch64 baseline is pending on this platform")
    result = {"milestone": "M0", "status": "FAIL" if errors else "PASS_FOR_THIS_CAPTURE",
              "scope": "One C++ build/profile only, never full M0 GO", "errors": errors,
              "remaining_manual_gates": ["X13s hardware identity; Debug/Release/sanitizer matrix",
                  "Nix builds and Rust build/fmt/clippy/test evidence", "preserved baseline executable/closure",
                  "independent review and PROJECT_STATE checkpoint"]}
    print(json.dumps(result, indent=2))
    return int(bool(errors))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="action", required=True)
    pre = sub.add_parser("preflight", help="record Linux and pinned-toolchain checks")
    pre.add_argument("--probe", type=Path, default=ROOT / "build-ansa/ansa_preflight")
    pre.add_argument("--output", type=Path, required=True)
    cap = sub.add_parser("capture", help="record safe CLI and optionally full CTest")
    cap.add_argument("--cpp-build", type=Path, required=True)
    cap.add_argument("--probe", type=Path)
    cap.add_argument("--output", type=Path, required=True)
    cap.add_argument("--run-ctest", action="store_true")
    chk = sub.add_parser("check", help="validate one native M0 capture, not close all M0 gates")
    chk.add_argument("--milestone", choices=["M0", "M1"], required=True)
    chk.add_argument("--evidence", type=Path, required=True)
    chk.add_argument("--fault-evidence", type=Path)
    for name in ("compare", "fault", "perf"):
        future = sub.add_parser(name, help="not implemented in M0 (always nonzero)")
        for option in ("suite", "cpp-build", "rust-target", "output"):
            future.add_argument("--" + option)
    args = parser.parse_args()
    if args.action in ("compare", "fault", "perf"):
        if args.action in ("compare", "fault") and args.suite == "risk":
            import risk
            return risk.capture(args)
        print(args.action + ": suite not implemented in M0", file=sys.stderr)
        return 2
    try:
        if args.action == "check" and args.milestone == "M1":
            import risk
            return risk.check(args.evidence.resolve(), args.fault_evidence)
        manifest = json.loads(MANIFEST.read_bytes())
        if args.action == "preflight":
            args.output.parent.mkdir(parents=True, exist_ok=True)
            if args.output.exists():
                raise FileExistsError("choose a new preflight output path")
            logs = args.output.with_suffix(args.output.suffix + ".logs")
            logs.mkdir(exist_ok=False)
            report = preflight(args.probe.resolve(), logs, manifest)
            for name in ("linux", "toolchain"):
                for stream in ("stdout", "stderr"):
                    report[name][stream]["file"] = str(logs.name / Path(report[name][stream]["file"]))
            write_json(args.output, report)
            print(json.dumps({"report": str(args.output), "errors": report["errors"]}, indent=2))
            return int(bool(report["errors"]))
        if args.action == "capture":
            if args.probe:
                args.probe = args.probe.resolve()
            return capture(args, manifest)
        return check(args.evidence.resolve(), manifest)
    except (OSError, ValueError, KeyError, TypeError, ET.ParseError, subprocess.SubprocessError) as error:
        print(json.dumps({"status": "FAIL", "diagnostic": str(error)}), file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
