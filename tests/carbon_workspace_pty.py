"""Production Carbon workspace acceptance with synthetic homes and real PTYs."""
import argparse
import json
import os
from pathlib import Path
import signal
import tempfile
import termios
import time

from cli_progress_pty import Terminal, copied, roots
from parity.frontend_io import Pty, journals
from parity import session_cases
# decode() uses the top-level protocol module. Reuse that exact type identity
# rather than loading a second NumberToken class through the namespace package.
protocol = session_cases.protocol


def visible(screen):
    screen.vt.update(screen.raw)
    return "\n".join(screen.vt.line(row) for row in range(screen.vt.rows))


def until(screen, predicate, label, timeout=6):
    deadline = time.monotonic() + timeout
    while not predicate(visible(screen)):
        assert screen.child.poll() is None and time.monotonic() < deadline, (label, visible(screen))
        screen.drain(.01)


def start(binary, root, home, cwd, extra=None):
    screen = Pty(str(binary), root, home, cwd, "", True, size=(100, 32),
                 argv=["--offline", "--cwd", str(cwd)], extra_env=extra)
    screen.vt = Terminal(100, 32)
    until(screen, lambda text: "ANSA" in text and "Tab focus" in text, "first workspace frame")
    return screen


def escape(screen):
    screen.send(b"\x1b")
    for _ in range(8):
        screen.drain(.01)


def input_and_copy(screen, root, cwd):
    before = journals(root)
    text = "Привет Ж界😀\n/exit is pasted text"
    screen.send(b"\x1b[200~" + text.encode() + b"\x1b[201~")
    until(screen, lambda frame: "/exit is pasted text" in frame, "multiline paste")
    assert journals(root) == before, "paste dispatched a command before Enter"
    screen.send(b"\r")
    until(screen, lambda frame: "OMH_TOOL_OK:OMH_FIXED_VALUE_42" in frame, "completed reply")
    at = len(screen.raw)
    screen.send(b"\x19")
    assert text in copied(screen, at)
    frame = visible(screen)
    assert all(hint not in frame for hint in ["Ctrl+F", "PgUp", "PgDn", "↑/↓"])
    screen.send("draft Ж界😀")
    until(screen, lambda frame: "draft Ж界😀" in frame, "draft")
    escape(screen)
    until(screen, lambda frame: "HISTORY" in frame, "Esc reading")
    screen.resize(48, 16)
    screen.vt = Terminal(48, 16)
    until(screen, lambda frame: "draft Ж界😀" in frame, "narrow draft preserved")
    screen.resize(100, 32)
    screen.vt = Terminal(100, 32)
    escape(screen)
    until(screen, lambda frame: "HISTORY" not in frame and "draft Ж界😀" in frame, "return to composer")


def mouse_copy(screen, root, cwd):
    at = len(screen.raw)
    screen.send(b"\x1b[<0;1;1M\x1b[<32;5;1M")
    screen.resize(70, 20)
    screen.vt = Terminal(70, 20)
    for _ in range(12):
        screen.drain(.01)
    screen.send(b"\x1b[<0;5;1m")
    text = copied(screen, at)
    assert text.startswith("ANSA"), repr(text)
    assert "\x1b" not in text
    screen.resize(100, 32)
    screen.vt = Terminal(100, 32)
    escape(screen)
    screen.send("still editable")
    until(screen, lambda frame: "still editable" in frame, "selection return")


def branches(screen, root, cwd):
    screen.send("parent-marker\r")
    until(screen, lambda frame: "OMH_TOOL_OK:OMH_FIXED_VALUE_42" in frame, "parent completion")
    screen.send("/fork\r")
    until(screen, lambda frame: "fork s_" in frame, "fork conversation")
    assert "parent-marker" in visible(screen)
    screen.send("child-marker\r")
    until(screen, lambda frame: "child-marker" in frame and "готов" in frame, "fork completion")
    escape(screen)
    until(screen, lambda frame: "fork s_" not in frame and "parent-marker" in frame, "parent return")
    assert "child-marker" not in visible(screen)
    screen.send("/btw side-marker\r")
    until(screen, lambda frame: "btw s_" in frame and "side-marker" in frame, "side conversation")
    escape(screen)
    until(screen, lambda frame: "btw s_" not in frame, "side parent return")
    assert "side-marker" not in visible(screen)
    screen.send("/sessions\r")
    until(screen, lambda frame: "Сессии проекта" in frame, "session picker")
    escape(screen)


def files_and_console(screen, root, cwd):
    (cwd / "notes.md").write_text("# Fixture heading\n\n**content**\n")
    screen.send("/files notes.md\r")
    until(screen, lambda frame: "READ ONLY" in frame and "Fixture heading" in frame, "file source")
    screen.send("p")
    until(screen, lambda frame: "Fixture heading" in frame and "**content**" not in frame, "Markdown preview")
    screen.send("d")
    until(screen, lambda frame: "Diff недоступен" in frame, "non-Git diff explicit")
    escape(screen)
    screen.send("/run printf CONSOLE_ONLY; exit 7\r")
    until(screen, lambda frame: "local console" in frame and "CONSOLE_ONLY" in frame and "exit 7" in frame, "console output")
    escape(screen)
    until(screen, lambda frame: "local console" not in frame, "console parent return")
    assert "CONSOLE_ONLY" not in visible(screen), "console output leaked into model conversation"


def editor_case(screen, root, cwd, cancel):
    (cwd / "notes.md").write_text("original\n")
    screen.send("/files notes.md\r")
    until(screen, lambda frame: "READ ONLY" in frame, "editor source")
    at = len(screen.raw)
    screen.send("e")
    screen.wait("EDITOR_READY", since=at)
    screen.vt.update(screen.raw)
    assert screen.vt.alternate and not screen.vt.cursor_visible and screen.vt.sgr == (31,), "editor mode fixture"
    assert termios.tcgetattr(screen.slave) == screen.original, "editor got noncanonical tty"
    if cancel:
        resumed = len(screen.raw)
        screen.send(b"\x03")
    else:
        resumed = len(screen.raw)
        screen.send(b"save\r")
    screen.wait(b"\x1b[?1002h", since=resumed)
    until(screen, lambda frame: "ANSA" in frame and "READ ONLY" in frame, "editor terminal restored")
    assert b"\x1b[?1049l\x1b[?25h\x1b[0m" in screen.raw[resumed:], "editor reset not delivered"
    assert not screen.vt.alternate and screen.vt.cursor_visible and not screen.vt.sgr, "editor screen modes not restored"
    if not cancel:
        until(screen, lambda frame: "edited" in frame, "edited file reloaded")
        assert "edited" in (cwd / "notes.md").read_text()
    else:
        assert (cwd / "notes.md").read_text() == "original\n"
    # The driver is outside the child's controlling session. Linux exposes
    # its peer's foreground group through the PTY master; slave TIOCGPGRP
    # correctly returns ENOTTY to this unrelated driver.
    assert os.tcgetpgrp(screen.master) == screen.child.pid, "foreground group not reclaimed"
    escape(screen)
    screen.send("after-editor")
    until(screen, lambda frame: "after-editor" in frame, "input after editor")


def editor_early_exit(screen, root, cwd, stopped=False):
    (cwd / "notes.md").write_text("original\n")
    screen.send("/files notes.md\r")
    until(screen, lambda frame: "READ ONLY" in frame, "early editor source")
    at = len(screen.raw)
    screen.send("e")
    screen.wait(b"\x1b[?1002h", since=at)
    until(screen, lambda frame: "READ ONLY" in frame and "ANSA" in frame, "early editor return")
    if stopped:
        assert not (cwd / "after-stop").exists(), "stopped editor was resumed"
    else:
        pidfile = cwd / "editor-child.pid"
        assert pidfile.exists(), "same-group child fixture never ran"
        pid = int(pidfile.read_text())
        deadline = time.monotonic() + 2
        while Path(f"/proc/{pid}/stat").exists():
            try:
                state = Path(f"/proc/{pid}/stat").read_text().split(") ", 1)[1].split()[0]
            except FileNotFoundError:
                break
            if state == "Z":
                break
            assert time.monotonic() < deadline, "editor left a live same-group child"
            screen.drain(.01)
    assert os.tcgetpgrp(screen.master) == screen.child.pid
    escape(screen)
    screen.send("parent-survived")
    until(screen, lambda frame: "parent-survived" in frame, "parent group survived")


def editor_missing(screen, root, cwd):
    (cwd / "notes.md").write_text("original\n")
    screen.send("/files notes.md\r")
    until(screen, lambda frame: "READ ONLY" in frame, "missing editor source")
    at = len(screen.raw)
    screen.send("e")
    screen.wait(b"\x1b[?1002h", since=at)
    until(screen, lambda frame: "Не удалось открыть редактор" in frame, "missing editor diagnostic")
    assert os.tcgetpgrp(screen.master) == screen.child.pid
    escape(screen)
    screen.send("restored-after-error")
    until(screen, lambda frame: "restored-after-error" in frame, "input after editor error")


def normal_exit(screen, root, cwd):
    screen.send("question\r")
    until(screen, lambda frame: "OMH_TOOL_OK:OMH_FIXED_VALUE_42" in frame, "normal completed reply")
    deadline = time.monotonic() + 1.1
    while time.monotonic() < deadline:
        screen.drain(.01)
    assert "command_queue_full" not in visible(screen), "internal polling reported a user error"
    screen.send("/exit\r")
    return "normal"


def eof_finishes_command(screen, root, cwd):
    screen.send("/run printf ready > eof-ready; sleep 0.5; printf EOF_DONE\r")
    until(screen, lambda frame: "local console" in frame, "EOF console")
    deadline = time.monotonic() + 4
    while not (cwd / "eof-ready").exists():
        assert time.monotonic() < deadline and screen.child.poll() is None
        screen.drain(.01)
    screen.send(b"\x04")
    deadline = time.monotonic() + 4
    while screen.child.poll() is None:
        assert time.monotonic() < deadline, "EOF did not drain accepted work"
        screen.drain(.01)
    completed = False
    for directory in (root / "home/.ansa/sessions").iterdir():
        records = session_cases.decode({path.name: path.read_bytes() for path in directory.glob("part-*.log")})
        assert not any(record["kind"] == "cancel" for record in records), "EOF wrote Cancel"
        for record in records:
            if record["kind"] == "outcome":
                result = record["result"]
                assert isinstance(result["execution"], protocol.NumberToken)
                assert isinstance(result["effects"], protocol.NumberToken)
                output = json.loads(result["output"])
                completed |= (protocol.structural(result["execution"]) == protocol.structural(0)
                              and protocol.structural(result["effects"]) == protocol.structural(1)
                              and result["dispatched"] is True and output.get("stdout") == "EOF_DONE"
                              and any(event["kind"] == "done" for event in records))
    assert completed, "EOF cancelled the accepted process"
    return "normal"


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=False)
    cases = [("input-copy-resize", input_and_copy), ("mouse-copy-resize", mouse_copy),
             ("fork-btw", branches), ("files-console", files_and_console),
             ("editor-success", lambda *a: editor_case(*a, False)),
             ("editor-cancel", lambda *a: editor_case(*a, True)),
             ("editor-early-exit", editor_early_exit),
             ("editor-stopped", lambda *a: editor_early_exit(*a, True)),
             ("editor-missing", editor_missing),
             ("normal-exit", normal_exit), ("eof-drains-command", eof_finishes_command)]
    results = []
    for name, case in cases:
        started = time.monotonic()
        with tempfile.TemporaryDirectory(prefix="ansa-m4-", dir="/tmp") as directory:
            root = Path(directory)
            home, cwd = roots(root)
            editor = cwd / "fixture-editor"
            editor.write_text('#!/bin/sh\n[ "$1" = "--" ] && shift\nprintf "\\033[?1049h\\033[?25l\\033[31mEDITOR_READY\\n"\nread answer\nprintf "edited\\n" >> "$1"\n')
            if name == "editor-early-exit":
                editor.write_text('#!/bin/sh\nsleep 30 &\nprintf "%s\\n" "$!" > editor-child.pid\nexit 0\n')
            elif name == "editor-stopped":
                editor.write_text('#!/bin/sh\nkill -STOP $$\nprintf resumed > after-stop\n')
            editor.chmod(0o700)
            editor_path = cwd / "does-not-exist" if name == "editor-missing" else editor
            screen = start(args.binary.resolve(), root, home, cwd, {"EDITOR": str(editor_path)})
            try:
                normal = case(screen, root, cwd) == "normal"
                result = screen.finish(None if normal else signal.SIGTERM)
                assert result["exit"] == (0 if normal else 143), result["exit"]
                assert result["termios_restored"] and result["flags_restored"], "terminal restoration"
                results.append({"case": name, "passed": True, "duration": time.monotonic() - started})
            except Exception as error:
                results.append({"case": name, "passed": False, "error": repr(error), "duration": time.monotonic() - started})
                raise
            finally:
                (args.output / (name + ".pty")).write_bytes(screen.raw)
                (args.output / (name + ".journals.json")).write_text(json.dumps(journals(root)) + "\n")
                (args.output / (name + ".screen.txt")).write_text(visible(screen))
                (args.output / "report.json").write_text(json.dumps(results, ensure_ascii=False, indent=2) + "\n")
                screen.close()
    print(json.dumps({"passed": len(results), "cases": [result["case"] for result in results]}))


if __name__ == "__main__":
    main()
