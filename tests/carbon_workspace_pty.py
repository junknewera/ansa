"""Production Carbon workspace acceptance with synthetic homes and real PTYs."""
import argparse
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import tempfile
import termios
import time

from cli_progress_pty import Terminal, copied, roots
from parity.frontend_io import Pty, environment, journals
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


def body(screen):
    visible(screen)
    return "\n".join(screen.vt.line(row) for row in range(3, screen.vt.rows - 5))


def mouse(screen, button, column, row, release=False):
    screen.send(f"\x1b[<{button};{column + 1};{row + 1}{'m' if release else 'M'}")


def click(screen, column, row):
    mouse(screen, 0, column, row)
    mouse(screen, 0, column, row, release=True)


def completed(root, message):
    for directory in (root / "home/.ansa/sessions").iterdir():
        records = session_cases.decode({path.name: path.read_bytes() for path in directory.glob("part-*.log")})
        turns = {record["turn"] for record in records if record["kind"] == "accepted" and record["text"] == message}
        if any(record["kind"] == "done" and record["turn"] in turns for record in records):
            return True
    return False


def ready(screen):
    return "готов" in screen.vt.line(-2)


def seed_history(binary, root, cwd):
    session = None
    for index in range(20):
        argv = [str(binary), "--offline", "--cwd", str(cwd)]
        if session:
            argv += ["--session", session]
        # The first page is taller than the tail. Clamping a pending `g` against
        # the old page would land in the middle of this long first message.
        text = ("HISTORY_FIRST\n" + "first-page line\n" * 80) if index == 0 else f"HISTORY_SEED_{index:02}"
        result = subprocess.run([*argv, text], env=environment(root / "home", root, True),
                                capture_output=True, timeout=6)
        assert result.returncode == 0 and result.stdout == b"OMH_TOOL_OK:OMH_FIXED_VALUE_42\n", result.returncode
        session = re.search(rb"ansa session ([a-zA-Z0-9_-]+) ", result.stderr)[1].decode()
    return session


def history_g_and_cancel(screen, root, cwd):
    session = seed_history(screen.child.args[0], root, cwd)
    screen.send(f"/sessions {session}\r")
    until(screen, lambda frame: "HISTORY_SEED_19" in frame and ready(screen), "seeded tail")
    draft = "HISTORY_DRAFT Ж界😀"
    screen.send(draft)
    until(screen, lambda frame: screen.vt.line(-4) == f"› {draft}", "history draft")
    before = journals(root)
    escape(screen)
    until(screen, lambda frame: "HISTORY" in screen.vt.line(1), "history before g")
    screen.send("g")
    until(screen, lambda frame: screen.vt.line(3).startswith("> HISTORY_FIRST"), "g loaded the first page at its top")
    assert screen.vt.line(-4) == f"› {draft}", "g edited the draft"
    screen.send("G")
    until(screen, lambda frame: "HISTORY_SEED_19" in body(screen) and "HISTORY" not in screen.vt.line(1), "G restored the tail")
    assert screen.vt.line(-4) == f"› {draft}" and journals(root) == before, "history navigation changed input or journal"

    escape(screen)
    until(screen, lambda frame: "HISTORY" in screen.vt.line(1), "reading before superseding g")
    # NUL is ignored by the editor and resolves standalone Esc immediately.
    # Both navigation commands reach the owner in one input batch, before it
    # can consume the requested timeline reply.
    screen.send(b"g\x1b\x00")
    until(screen, lambda frame: "HISTORY" not in screen.vt.line(1) and "HISTORY_SEED_19" in body(screen), "Esc superseded pending g")
    deadline = time.monotonic() + .7
    while time.monotonic() < deadline:
        screen.drain(.01)
    assert "HISTORY" not in visible(screen).splitlines()[1] and "HISTORY_SEED_19" in body(screen), "late first-page reply overrode Esc"
    assert screen.vt.line(-4) == f"› {draft}"
    escape(screen)
    until(screen, lambda frame: "HISTORY" in screen.vt.line(1), "reading before g and PgUp")
    at = len(screen.raw)
    screen.send(b"g\x1b[5~")
    until(screen, lambda frame: len(screen.raw) > at and "HISTORY" in screen.vt.line(1), "PgUp superseded pending g without overflow")
    assert screen.vt.line(-4) == f"› {draft}" and screen.child.poll() is None
    screen.send("G")
    until(screen, lambda frame: "HISTORY" not in screen.vt.line(1) and "HISTORY_SEED_19" in body(screen), "tail after superseded jumps")
    assert screen.vt.line(-4) == f"› {draft}" and journals(root) == before

    screen.send(b"\x15/agent CHILD_HISTORY_TASK\r")
    until(screen, lambda frame: "agent s_" in screen.vt.line(1) and completed(root, "CHILD_HISTORY_TASK") and ready(screen), "agent completed in child chat")
    screen.send("CHILD_DRAFT Ж")
    until(screen, lambda frame: screen.vt.line(-4) == "› CHILD_DRAFT Ж", "child draft")
    # The offline tool is immediate. Exercise SIGINT in this child without
    # depending on a race with its brief reasoning/tool step.
    screen.send(b"\x03")
    until(screen, lambda frame: "Нет активного шага" in frame, "child handled Ctrl+C")
    screen.send(b"\x1b[5~")
    until(screen, lambda frame: "HISTORY" in screen.vt.line(1), "child history")
    screen.send("g")
    until(screen, lambda frame: screen.vt.line(3).startswith("> HISTORY_FIRST"), "child g reached inherited first page")
    assert "agent s_" in screen.vt.line(1) and screen.vt.line(-4) == "› CHILD_DRAFT Ж"
    screen.send("G")
    until(screen, lambda frame: "CHILD_HISTORY_TASK" in body(screen) and "HISTORY" not in screen.vt.line(1), "child G restored its tail")
    assert screen.vt.line(-4) == "› CHILD_DRAFT Ж"
    escape(screen)
    until(screen, lambda frame: "agent s_" not in screen.vt.line(1) and ready(screen), "Esc returned from child")

    # A local console gives Ctrl+C a real running tool to cancel. Its ready
    # file is an execution barrier, rather than a guessed delay.
    screen.send("/run printf ready > cancel-ready; sleep 30; printf bad > cancel-late\r")
    until(screen, lambda frame: "local console" in frame and (cwd / "cancel-ready").exists(), "console tool executing")
    screen.send("CANCEL_DRAFT Ж")
    until(screen, lambda frame: screen.vt.line(-4) == "› CANCEL_DRAFT Ж", "cancel draft")
    screen.send(b"\x03")
    until(screen, lambda frame: "отменено" in screen.vt.line(-2) and "завершение" not in screen.vt.line(-2), "active console cancellation settled")
    screen.send(b"\x1b[5~")
    until(screen, lambda frame: "HISTORY" in screen.vt.line(1), "cancelled console history")
    screen.send("g")
    until(screen, lambda frame: screen.vt.line(3).startswith("> printf ready"), "g in cancelled child")
    screen.send("G")
    until(screen, lambda frame: "HISTORY" not in screen.vt.line(1), "G in cancelled child")
    assert screen.vt.line(-4) == "› CANCEL_DRAFT Ж" and not (cwd / "cancel-late").exists()


def split_source_scroll(screen, root, cwd):
    marker = "CHAT_STABLE_MARKER"
    screen.send(f"{marker}\r")
    until(screen, lambda frame: completed(root, marker) and ready(screen), "stable chat completion")
    (cwd / "source.rs").write_text("".join(f"SRC_{index:03} let value = {index};\n" for index in range(100)))
    at = len(screen.raw)
    screen.resize(120, 32)
    screen.vt = Terminal(120, 32)
    screen.wait(b"\x1b[H\x1b[2J", since=at)
    screen.send("/files source.rs\r")
    panel_width = max((screen.vt.cols - 1) * 2 // 5, 34)
    chat_width = screen.vt.cols - 1 - panel_width - 3
    right = chat_width + 3
    first_source = lambda: "".join(screen.vt.cells[6][right:]).rstrip()
    until(screen, lambda frame: marker in body(screen) and first_source().startswith("SRC_000"), "split Source ready")
    left = tuple(tuple(row[:chat_width]) for row in screen.vt.cells[3:-5])
    at = len(screen.raw)
    screen.send("jjjjj")
    until(screen, lambda frame: first_source().startswith("SRC_005"), "right Source scrolled")
    delta = screen.raw[at:]
    assert left == tuple(tuple(row[:chat_width]) for row in screen.vt.cells[3:-5]), "Source scroll moved the chat"
    assert b"\x1b[2K" not in delta and b"\x1b[2J" not in delta, "Source scroll cleared complete rows"
    assert marker.encode() not in delta, "Source scroll repainted stable chat text"

    # A click inside the chat must change focus before g/G. The right pane
    # stays at its current file position while the chat changes reading mode.
    click(screen, 2, 3)
    screen.send("g")
    until(screen, lambda frame: "HISTORY" in screen.vt.line(1) and screen.vt.line(3).startswith(f"> {marker}") and first_source().startswith("SRC_005"), "g targeted the clicked chat")
    screen.send("G")
    until(screen, lambda frame: "HISTORY" not in screen.vt.line(1) and first_source().startswith("SRC_005"), "G kept the inspector position")
    screen.send("FOCUS_DRAFT Ж界😀")
    until(screen, lambda frame: screen.vt.line(-4) == "› FOCUS_DRAFT Ж界😀", "chat input after focus click")


def markdown_source_preview(screen, root, cwd):
    words = " ".join(f"WORD{index:04}" for index in range(800))
    lines = ["# PREVIEW_HEADING", "", "**bold** *italic* [LINK_LABEL](https://example.invalid/path)", "",
             "- list item Ж界😀", "  - nested item", "", "```rust",
             'let literal = "**stay** [code](not-a-link)";', "```", "",
             "WRAP_START" + "界" * 60 + "END_WRAP", "", words, "", "## PREVIEW_BOTTOM"]
    source = "\n".join(lines) + "\n"
    (cwd / "preview.md").write_text(source)
    before = journals(root)
    screen.send("/files preview.md\r")
    until(screen, lambda frame: "# PREVIEW_HEADING" in frame and "**bold**" in frame, "Markdown Source")
    screen.send("p")
    until(screen, lambda frame: "PREVIEW_HEADING" in frame and "# PREVIEW_HEADING" not in frame and "END_WRAP" in frame, "styled wrapped Markdown Preview")
    text = body(screen)
    assert "LINK_LABEL" in text and "[LINK_LABEL](" not in text and "https://example.invalid" not in text
    assert "**bold**" not in text and "*italic*" not in text
    assert '**stay** [code](not-a-link)' in text, "Preview changed code literals"
    assert "list item Ж界😀" in text and "nested item" in text and "WRAP_START" in text
    screen.send("G")
    until(screen, lambda frame: "PREVIEW_BOTTOM" in frame, "Preview bottom is reachable")
    screen.send(f":{lines.index(words) + 1}\r")
    until(screen, lambda frame: "WORD0000" in screen.vt.line(6), "Preview source-line jump")
    initial = screen.vt.line(6)
    screen.send(b"\x1b[6~")
    until(screen, lambda frame: "WORD" in screen.vt.line(6) and screen.vt.line(6) != initial, "Preview scrolled within one source paragraph")
    anchor = int(re.search(r"WORD(\d{4})", screen.vt.line(6))[1])
    assert anchor > 0
    at = len(screen.raw)
    screen.resize(60, 24)
    screen.vt = Terminal(60, 24)
    screen.wait(b"\x1b[H\x1b[2J", since=at)
    until(screen, lambda frame: "WORD" in screen.vt.line(6), "Preview reflow after resize")
    reflowed = int(re.search(r"WORD(\d{4})", screen.vt.line(6))[1])
    assert reflowed <= anchor < reflowed + 7, ("resize lost paragraph reading position", anchor, reflowed)
    screen.send("G")
    until(screen, lambda frame: "PREVIEW_BOTTOM" in frame, "resized Preview bottom")
    screen.send("sg")
    until(screen, lambda frame: "# PREVIEW_HEADING" in frame and "**bold**" in frame, "Source remains original after Preview")
    assert (cwd / "preview.md").read_text() == source and journals(root) == before


def inline_tool_details(screen, root, cwd):
    screen.send("TOOLS_TASK\r")
    until(screen, lambda frame: completed(root, "TOOLS_TASK") and ready(screen) and "get_test_value" in body(screen), "compact completed tool")
    compact = body(screen)
    headers = [line for line in compact.splitlines() if re.match(r"[>v] get_test_value\b", line)]
    assert len(headers) == 1 and "готово" in headers[0], "tool lifecycle was split into duplicate rows"
    assert "prepared" not in compact and "KnownResult" not in compact and "Аргументы" not in compact
    screen.send("TOOLS_DRAFT Ж")
    until(screen, lambda frame: screen.vt.line(-4) == "› TOOLS_DRAFT Ж", "tool draft")
    row = next(row for row in range(3, screen.vt.rows - 5) if screen.vt.line(row).startswith("> get_test_value"))
    click(screen, 0, row)
    until(screen, lambda frame: "Аргументы" in body(screen) and "Вывод" in body(screen), "tool row opened inline")
    expanded = body(screen)
    assert "OMH_FIXED_VALUE_42" in expanded and "{}" in expanded
    assert len([line for line in expanded.splitlines() if re.match(r"[>v] get_test_value\b", line)]) == 1
    assert screen.vt.line(-4) == "› TOOLS_DRAFT Ж"
    row = next(row for row in range(3, screen.vt.rows - 5) if screen.vt.line(row).startswith("v get_test_value"))
    click(screen, 0, row)
    until(screen, lambda frame: "Аргументы" not in body(screen) and "> get_test_value" in body(screen), "tool row collapsed")

    # Dragging a tool row selects text rather than activating its click target.
    row = next(row for row in range(3, screen.vt.rows - 5) if screen.vt.line(row).startswith("> get_test_value"))
    at = len(screen.raw)
    mouse(screen, 0, 2, row)
    mouse(screen, 32, 2 + len("get_test_value") - 1, row)
    mouse(screen, 0, 2 + len("get_test_value") - 1, row, release=True)
    assert copied(screen, at) == "get_test_value", "tool-row drag did not copy the displayed text"
    escape(screen)
    screen.send(b"\x14")
    until(screen, lambda frame: "Аргументы" in body(screen) and "Вывод" in body(screen), "Ctrl+T opened tool details")
    assert screen.vt.line(-4) == "› TOOLS_DRAFT Ж"


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
             ("history-g-cancel", history_g_and_cancel),
             ("split-source-scroll", split_source_scroll),
             ("markdown-source-preview", markdown_source_preview),
             ("inline-tool-details", inline_tool_details),
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
