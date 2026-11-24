"""Exercise paged search and byte-offset reads in Carbon Plan with a synthetic provider."""
import argparse
import json
import os
from pathlib import Path
import shutil
import tempfile

from carbon_workspace_pty import (
    Pty, Terminal, body, completed, local, ready, records, roots, seed_siwc, until,
)

MODEL = "gpt-6.2-sol"
SRC = Path(__file__).resolve().parents[1] / "rust/crates/ansa/src/frontend/controller/workbench.rs"
NEEDLE = b"LINKER_FAILURE_SENTINEL: missing library libfixture"


def run(adapter):
    with tempfile.TemporaryDirectory(prefix="ansa-m4-", dir="/tmp") as name:
        root = Path(name)
        home, cwd = roots(root)
        home.chmod(0o700)
        cwd.chmod(0o700)
        seed_siwc(home)
        (root / "paged-search").touch(mode=0o600)
        shutil.copyfile(SRC, cwd / "workbench.rs")
        source_offset = SRC.read_bytes().index(b"fn side_chat_parent")
        assert source_offset > 65_536

        chunk = b"compiling fixture target\n" * 40_000
        with (cwd / "build.log").open("wb") as log:
            for _ in range(17):
                log.write(chunk)
            tail_offset = log.tell()
            log.write(NEEDLE + b"\n")
        assert tail_offset > 16 * 1024 * 1024

        env = dict(HOME=str(home), XDG_STATE_HOME=str(root / "xdg"),
                   PATH=os.environ.get("PATH", os.defpath), TERM="xterm", LC_ALL="C", NO_COLOR="1")
        screen = Pty(str(adapter), root, home, cwd, "", True, size=(120, 38), base_env=env,
                     argv=["--frontend-fixture", str(root), "siwc-carbon"])
        screen.vt = Terminal(120, 38)
        try:
            until(screen, lambda _: ready(screen, MODEL), "ready to open Plan")
            local(screen, "/plan")
            until(screen, lambda _: screen.vt.line(-2).strip().startswith("Plan ·"), "ordinary Plan")
            first = "Find the definition of side_chat_parent in workbench.rs"
            screen.send(first + "\r")
            until(screen, lambda frame: completed(root, first) and "FOUND: fn side_chat_parent" in frame
                  and "Ctrl+C stop" not in screen.vt.line(-2), "find the real function and read it", timeout=15)
            screen.send("Find the last linker failure in @build")
            until(screen, lambda _: "build.log" in body(screen), "Carbon suggests the log path")
            screen.send(b"\t")
            until(screen, lambda _: "@build.log" in screen.vt.line(-4), "Carbon inserts the log path")
            screen.send(b"\r")
            until(screen, lambda frame: "FOUND: " + NEEDLE.decode() in frame
                  and "Ctrl+C stop" not in screen.vt.line(-2), "find and read the log tail", timeout=25)
            local(screen, "/exit")
            finish = screen.finish()
            assert finish["exit"] == 0 and finish["termios_restored"] and finish["flags_restored"]
        finally:
            screen.close()

        journal = records(root)
        requests = [event for event in journal if event["kind"] == "accepted"]
        assert len(requests) == 2 and all(event["mode"] == "plan" for event in requests)
        assert requests[1]["text"].strip() == "Find the last linker failure in @build.log"
        intents = [event["invocation"] for event in journal if event["kind"] == "intent"]
        reads = [tool for tool in intents if tool["name"] == "read_file"]
        assert [int(tool["arguments"]["offset"].lexeme) for tool in reads] == [source_offset, tail_offset]
        results = {event["result"]["operation"]: json.loads(event["result"]["output"])
                   for event in journal if event["kind"] == "outcome"}
        pages = [tool for tool in intents if tool["name"] == "search_files"
                 and tool["arguments"]["query"] == "LINKER_FAILURE_SENTINEL"]
        assert len(pages) >= 2
        for previous, following in zip(pages, pages[1:]):
            page = results[previous["operation"]]
            assert page["matches"] == [] and page["next_cursor"] == following["arguments"]["cursor"]
        assert results[reads[-1]["operation"]]["content"].encode().startswith(NEEDLE)
        print(f"PASS: Plan found source at {source_offset} and log failure at {tail_offset} using {len(pages)} search pages")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True, help="compiled frontend_adapter")
    run(parser.parse_args().binary.resolve(strict=True))
