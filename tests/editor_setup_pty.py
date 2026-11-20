"""Exercise a real editor in the Carbon inspector and its PTY shutdown."""
import argparse
import json
import os
from pathlib import Path
import shlex
import shutil
import signal
import tempfile
import termios
import time

from carbon_workspace_pty import local, mouse, paint_quiet, start, until, visible
from cli_progress_pty import Terminal, roots
from parity.frontend_io import journals


def resize(screen, columns, rows):
    screen.vt = Terminal(columns, rows)
    screen.vt.offset = len(screen.raw)
    screen.resize(columns, rows)


def editor_dimensions(screen, geometry):
    paint_quiet(screen)
    screen.send(f":call writefile([&columns . 'x' . &lines], '{geometry}')\r")
    deadline = time.monotonic() + 3
    while not geometry.exists():
        assert time.monotonic() < deadline, visible(screen)
        screen.drain(.01)
    dimensions = tuple(map(int, geometry.read_text().strip().split("x")))
    geometry.unlink()
    return dimensions


def vim_in_panel(screen, root, cwd):
    resize(screen, 128, 36)
    screen.send("editor-chat-context\r")
    until(screen, lambda text: "OMH_TOOL_OK" in text and "editor-chat-context" in text,
          "settled chat before editing")
    before = journals(root)
    local(screen, "/files notes.md")
    until(screen, lambda text: "read-only" in text and "fixture document" in text,
          "source inspector")
    termios_in_carbon = termios.tcgetattr(screen.slave)
    screen.send("e")
    until(screen, lambda text: "Editor · notes.md" in text and ":w save" in text,
          "editor opens in inspector")
    geometry = cwd / "geometry"
    assert editor_dimensions(screen, geometry) == (50, 24), visible(screen)

    resize(screen, 104, 32)
    until(screen, lambda text: "Editor · notes.md" in text and "editor-chat-context" in text,
          "editor and chat remain visible after resize")
    assert editor_dimensions(screen, geometry) == (41, 20), visible(screen)

    # Clicking chat must not route the next keys to Carbon while Vim is open.
    mouse(screen, 0, 2, 5)
    mouse(screen, 0, 2, 5, release=True)
    screen.send(b"GoVIM_WRITTEN")
    until(screen, lambda text: "VIM_WRITTEN" in text and "-- INSERT --" in text,
          "Vim insert mode after clicking chat")
    screen.send(b"\x03")
    until(screen, lambda text: "-- INSERT --" not in text, "Vim received Ctrl+C")
    screen.send(b"oESC_TO_NORMAL\x1b")
    # A separate send exercises the input decoder's lone Escape timeout.
    until(screen, lambda text: "ESC_TO_NORMAL" in text and "-- INSERT --" not in text,
          "Vim returned to normal mode after Escape")
    screen.send(b":wq\r")
    until(screen, lambda text: "read-only" in text and "VIM_WRITTEN" in text,
          "source reload after Vim saves and exits")
    source = (cwd / "notes.md").read_text()
    assert source == "fixture document\nVIM_WRITTEN\nESC_TO_NORMAL\n", source
    assert termios.tcgetattr(screen.slave) == termios_in_carbon
    assert os.tcgetpgrp(screen.master) == screen.child.pid
    assert journals(root) == before, "editor input entered the chat journal"

    # Changing an already-previewed path to a symlink must fail validation
    # before the editor gets another opportunity to open it.
    moved = cwd / "moved.md"
    notes = cwd / "notes.md"
    notes.rename(moved)
    notes.symlink_to(moved)
    screen.send("e")
    until(screen, lambda text: "Could not open editor: workspace_file_access" in text,
          "source revalidated before editor spawn")
    assert moved.read_text() == source
    return screen.finish(signal.SIGTERM)


def blocked_output_shutdown(screen, root, cwd):
    local(screen, "/files notes.md")
    until(screen, lambda text: "read-only" in text and "fixture document" in text,
          "source inspector")
    paint_quiet(screen)
    termios.tcflow(screen.slave, termios.TCOOFF)
    try:
        screen.send("e")
        pid_path = cwd / "editor.pid"
        deadline = time.monotonic() + 3
        while not pid_path.exists():
            assert time.monotonic() < deadline, "private PTY could not start under outer flow control"
            screen.drain(.01)
        pid = int(pid_path.read_text())
        interrupted = time.monotonic()
        screen.child.send_signal(signal.SIGTERM)
        while screen.child.poll() is None:
            assert time.monotonic() - interrupted < 2, "editor shutdown blocked on outer output"
            screen.drain(.005)
        assert not Path(f"/proc/{pid}").exists(), "editor was not reaped"
    finally:
        termios.tcflow(screen.slave, termios.TCOON)
    return screen.finish()


def exercise(binary, output, name, editor_factory, run):
    started = time.monotonic()
    result = {"case": name, "passed": False}
    with tempfile.TemporaryDirectory(prefix="ansa-m4-", dir="/tmp") as directory:
        root = Path(directory)
        home, cwd = roots(root)
        (cwd / "notes.md").write_text("fixture document\n")
        screen = None
        try:
            screen = start(binary, root, home, cwd, {"VISUAL": editor_factory(cwd)})
            finished = run(screen, root, cwd)
            assert finished["exit"] == 143, finished["exit"]
            assert finished["termios_restored"] and finished["flags_restored"], "stdio restoration"
            result.update(passed=True, exit=finished["exit"], termios_restored=True, flags_restored=True)
        except Exception as error:
            result["error"] = repr(error)
            raise
        finally:
            result["duration_seconds"] = time.monotonic() - started
            if screen is not None:
                screen.drain(0)
                (output / (name + ".pty")).write_bytes(screen.raw)
                (output / (name + ".screen.txt")).write_text(visible(screen))
                screen.close()
            (output / (name + ".json")).write_text(json.dumps(result, indent=2) + "\n")
    return result


def vim_command(_cwd):
    editor = shutil.which("nvim") or shutil.which("vim")
    assert editor, "editor acceptance requires Neovim or Vim"
    return f"{shlex.quote(editor)} -u NONE -i NONE -n"


def sleeping_editor(cwd):
    editor = cwd / "fixture-editor"
    editor.write_text(f'#!/bin/sh\nprintf "%s\\n" "$$" > {shlex.quote(str(cwd / "editor.pid"))}\nexec sleep 30\n')
    editor.chmod(0o700)
    return str(editor)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=False)
    binary = args.binary.resolve()
    results = [exercise(binary, args.output, "vim-in-narrow-inspector", vim_command, vim_in_panel),
               exercise(binary, args.output, "editor-blocked-output-sigterm", sleeping_editor, blocked_output_shutdown)]
    (args.output / "report.json").write_text(json.dumps(results, indent=2) + "\n")
    print(json.dumps({"passed": len(results), "cases": [item["case"] for item in results]}))


if __name__ == "__main__":
    main()
