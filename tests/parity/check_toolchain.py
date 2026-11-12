#!/usr/bin/env python3
"""Check declared Rust pins against the tools actually running this workspace."""

import argparse
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tomllib


def check(workspace: Path) -> dict:
    report = {"schema_version": 1, "status": "FAIL", "workspace": str(workspace)}
    tool_environment = dict(os.environ, RUSTUP_AUTO_INSTALL="0")
    try:
        manifest = tomllib.loads((workspace / "Cargo.toml").read_text())
        toolchain = tomllib.loads((workspace / "rust-toolchain.toml").read_text())
        package = manifest["workspace"]["package"]
        channel = toolchain["toolchain"]["channel"]
        report["declared"] = {
            "channel": channel,
            "rust_version": package["rust-version"],
            "edition": package["edition"],
        }
        if not re.fullmatch(r"\d+\.\d+\.\d+", channel):
            raise ValueError("toolchain channel must be an exact stable version")
        if package["rust-version"] != channel or package["edition"] != "2024":
            raise ValueError("workspace MSRV/edition disagrees with toolchain policy")

        versions = {}
        for tool in ("rustc", "cargo"):
            executable = shutil.which(tool)
            if executable is None:
                raise ValueError(f"missing executable: {tool}")
            output = subprocess.run(
                [executable, "--version"], cwd=workspace, check=True,
                capture_output=True, text=True, timeout=30, env=tool_environment,
            ).stdout.strip()
            report[tool] = {"path": str(Path(executable).resolve()), "output": output}
            match = re.match(rf"{tool} (\d+\.\d+\.\d+)(?:\s|$)", output)
            if match is None:
                raise ValueError(f"unrecognized stable version: {tool}")
            versions[tool] = match[1]
        if versions["rustc"] != channel:
            raise ValueError("running rustc does not match rust-toolchain.toml")
        # Cargo's patch version may differ within the same Rust toolchain release.
        if versions["cargo"].split(".")[:2] != channel.split(".")[:2]:
            raise ValueError("running Cargo is from a different Rust release series")

        metadata = subprocess.run(
            ["cargo", "metadata", "--offline", "--locked", "--no-deps", "--format-version=1"],
            cwd=workspace, check=True, capture_output=True, text=True, timeout=30,
            env=tool_environment,
        )
        data = json.loads(metadata.stdout)
        members = set(data["workspace_members"])
        packages = [p for p in data["packages"] if p["id"] in members]
        if not packages:
            raise ValueError("Cargo reported no workspace packages")
        report["packages"] = [
            {"name": p["name"], "edition": p["edition"], "rust_version": p["rust_version"]}
            for p in packages
        ]
        if any(p["edition"] != "2024" or p["rust_version"] != channel for p in packages):
            raise ValueError("Cargo package metadata disagrees with workspace pins")
        report["status"] = "PASS"
    except (OSError, ValueError, KeyError, subprocess.SubprocessError) as error:
        report["diagnostic"] = str(error)
    return report


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--workspace", type=Path, default=Path(__file__).resolve().parents[2] / "rust"
    )
    report = check(parser.parse_args().workspace.resolve())
    print(json.dumps(report, indent=2, sort_keys=True))
    return 0 if report["status"] == "PASS" else 1


if __name__ == "__main__":
    raise SystemExit(main())
