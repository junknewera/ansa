"""Production Carbon workspace acceptance with synthetic homes and real PTYs."""
import argparse
import fcntl
import json
import os
from pathlib import Path
import re
import select
import shutil
import signal
import socket
import subprocess
import tempfile
import termios
import time

from cli_progress_pty import Terminal, copied, roots
from parity.frontend_io import Pty, environment, journals
from parity import session_cases


def visible(screen):
    screen.vt.update(screen.raw)
    return "\n".join(screen.vt.line(row) for row in range(screen.vt.rows))


def until(screen, predicate, label, timeout=6):
    deadline = time.monotonic() + timeout
    while not predicate(visible(screen)):
        assert screen.child.poll() is None and time.monotonic() < deadline, (label, visible(screen))
        screen.drain(.01)


def start(binary, root, home, cwd, extra=None, argv_extra=(), model="gpt-6.1-sol"):
    shell = os.environ.get("ANSA_TEST_BASH") or shutil.which("bash")
    assert shell, "fixture requires Bash prompt integration"
    screen = Pty(str(binary), root, home, cwd, "", True, size=(100, 32),
                 argv=["--offline", "--cwd", str(cwd), *argv_extra],
                 extra_env={"SHELL": shell, **(extra or {})})
    screen.vt = Terminal(100, 32)
    until(screen, lambda text: "ANSA" in text and "Tab mode" in text and ready(screen, model), "first workspace frame")
    return screen


def escape(screen):
    screen.send(b"\x1b")
    for _ in range(8):
        screen.drain(.01)


def activity_open(screen):
    return any(line.strip() == "Terminals" or line.strip().startswith("Terminals · Events ")
               for line in visible(screen).splitlines())


def open_activity(screen):
    screen.send(b"\x14")
    until(screen, lambda _: activity_open(screen), "Terminals Activity opened")


def close_activity(screen):
    escape(screen)
    until(screen, lambda _: not activity_open(screen), "Terminals Activity closed")


def select_result(screen, label):
    until(screen, lambda _: activity_open(screen), "select a result in Terminals Activity")
    screen.send("G")
    paint_quiet(screen)
    for _ in range(screen.vt.rows * 2):
        screen.send(b"\x1b[A")
        paint_quiet(screen)
        if any(line.lstrip().startswith("› ") and label in line for line in body(screen).splitlines()):
            return
    raise AssertionError((label, visible(screen)))


def body(screen):
    visible(screen)
    return "\n".join(screen.vt.line(row) for row in range(3, screen.vt.rows - 5))


def popup_text(screen):
    visible(screen)
    return "\n".join(line.split("│", 1)[1].rsplit("│", 1)[0]
                     for line in (screen.vt.line(row) for row in range(screen.vt.rows))
                     if line.count("│") >= 2)


def mouse(screen, button, column, row, release=False):
    screen.send(f"\x1b[<{button};{column + 1};{row + 1}{'m' if release else 'M'}")


def paint_quiet(screen):
    # Hit targets belong to a completed paint. Seeing an input row can precede
    # the trailing status writes, so wait for the retained frame to settle.
    deadline = time.monotonic() + 1
    quiet = 0
    while quiet < 3:
        before = len(screen.raw)
        screen.drain(.01)
        visible(screen)
        quiet = quiet + 1 if len(screen.raw) == before else 0
        assert screen.child.poll() is None and time.monotonic() < deadline, "click frame readiness"


def click(screen, column, row):
    paint_quiet(screen)
    mouse(screen, 0, column, row)
    mouse(screen, 0, column, row, release=True)


def completed(root, message):
    for directory in (root / "home/.ansa/sessions").iterdir():
        records = session_cases.decode({path.name: path.read_bytes() for path in directory.glob("part-*.log")})
        turns = {record["turn"] for record in records if record["kind"] == "accepted" and record["text"] == message}
        if any(record["kind"] == "done" and record["turn"] in turns for record in records):
            return True
    return False


def ready(screen, model="gpt-6.1-sol"):
    return model in screen.vt.line(-2) and screen.vt.line(-4).startswith("  ›")


def local(screen, command):
    assert command.startswith("/") and "\n" not in command and "\r" not in command
    screen.send(command.rstrip() + "\r")


def approve_helpers(screen, prompt):
    until(screen, lambda frame: ready(screen) and "Ctrl+C stop" not in screen.vt.line(-2),
          "current turn settled before proposing helpers")
    screen.send(prompt + "\r")
    until(screen, lambda frame: "Start" in frame and "Refine" in frame and "Cancel" in frame,
          "interactive orchestration approval")
    screen.send(b"\r")
    until(screen, lambda frame: "Сводка готова" in frame and ready(screen), "coordinator's synthesis")
    assert "Синтетический результат проверки" not in visible(screen), "helper report shown as a chat message"


def open_helper(screen, by_mouse=False):
    local(screen, "/agents")
    until(screen, lambda frame: "Conversation agents" in frame and helper_title(frame), "helper roster")
    if by_mouse:
        row = next(row for row in range(screen.vt.rows) if "└─ " in screen.vt.line(row) or "├─ " in screen.vt.line(row))
        click(screen, screen.vt.line(row).index("─ ") + 2, row)
    else:
        screen.send(b"\r")
    until(screen, lambda frame: "Agent transcript" in frame and "Assignment" in frame
          and "Синтетический результат проверки" in frame, "read completed helper in the parent session")


def helper_title(text):
    return "Найти файлы" in text or "Проверить, достаточно" in text


def reading(screen):
    return "History" in screen.vt.line(1)


def records(root):
    result = []
    for directory in (root / "home/.ansa/sessions").iterdir():
        if directory.is_dir() and not directory.is_symlink():
            result.extend(session_cases.decode({path.name: path.read_bytes() for path in directory.glob("part-*.log")}))
    return result


def shell_ready(screen):
    return "Esc files" in visible(screen)


def inspector_first(screen):
    # At narrow sizes Files is stacked below the chat; its content row follows
    # the visible tabs and rule instead of occupying a fixed screen row.
    tabs = next(row for row in range(3, screen.vt.rows - 5)
                if "Files" in screen.vt.line(row) and "Source" in screen.vt.line(row))
    return screen.vt.line(tabs + 2)


def terminal(screen):
    local(screen, "/files")
    until(screen, lambda frame: "q terminal" in frame, "Files inspector")
    screen.send("q")
    until(screen, lambda frame: "Terminal ·" in frame and shell_ready(screen), "embedded shell ready")


def seed_history(binary, root, cwd, count=20):
    session = None
    for index in range(count):
        argv = [str(binary), "--offline", "--cwd", str(cwd)]
        if session:
            argv += ["--session", session]
        # A tall first message forces traversal across canonical pages rather
        # than proving only a short local screen scroll.
        text = ("HISTORY_FIRST\n" + "\n" * 80 + "HISTORY_FIRST_END") if index == 0 else f"HISTORY_SEED_{index:02}"
        result = subprocess.run([*argv, text], env=environment(root / "home", root, True),
                                capture_output=True, timeout=6)
        assert result.returncode == 0 and result.stdout == b"OMH_TOOL_OK:OMH_FIXED_VALUE_42\n", result.returncode
        session = re.search(rb"ansa session ([a-zA-Z0-9_-]+) ", result.stderr)[1].decode()
    return session


def mouse_history_send_and_child_escape(screen, root, cwd):
    session = seed_history(screen.child.args[0], root, cwd)
    local(screen, f"/sessions {session}")
    until(screen, lambda frame: "HISTORY_SEED_19" in frame and ready(screen), "seeded tail")
    draft = "HISTORY_DRAFT Ж界😀 jkgG"
    screen.send(draft)
    until(screen, lambda frame: screen.vt.line(-4) == f"  › {draft}", "history draft")
    before = journals(root)
    escape(screen)
    until(screen, lambda frame: not reading(screen) and "HISTORY_SEED_19" in body(screen), "root Esc stays at latest")
    escape(screen)
    assert not reading(screen) and screen.vt.line(-4) == f"  › {draft}"
    for _ in range(180):
        mouse(screen, 64, 2, 5)
        for _ in range(3): screen.drain(.01)
        if "  > HISTORY_FIRST" in body(screen).splitlines(): break
    else:
        raise AssertionError("mouse cannot reach the first canonical message")
    assert reading(screen) and screen.vt.line(-4) == f"  › {draft}"
    anchor = body(screen)
    screen.send(b"\x1bg\x1bG\x1bk\x1bj\x1b\xd0\x96")
    for _ in range(8): screen.drain(.01)
    assert reading(screen) and body(screen) == anchor and screen.vt.line(-4) == f"  › {draft}", "Alt keys changed conversation or reading anchor"
    screen.send(" TYPED_WHILE_READING")
    until(screen, lambda frame: screen.vt.line(-4).endswith("TYPED_WHILE_READING"), "typing while reading")
    submitted = draft + " TYPED_WHILE_READING"
    assert body(screen) == anchor and journals(root) == before, "typing moved reading anchor or dispatched a turn"
    screen.send(b"\r")
    until(screen, lambda frame: not reading(screen) and "TYPED_WHILE_READING" in body(screen), "Enter returned to latest and showed the submitted message")
    until(screen, lambda frame: completed(root, submitted) and ready(screen), "send while reading completed")
    assert sum(row["kind"] == "accepted" and row["text"] == submitted for row in records(root)) == 1
    approve_helpers(screen, "Запусти субагента: CHILD_HISTORY_TASK")
    open_helper(screen, by_mouse=True)
    before = journals(root)
    screen.send("CHILD_DRAFT Ж")
    paint_quiet(screen)
    assert screen.vt.line(-4) == "  ›" and "Agent transcript" in visible(screen)
    mouse(screen, 64, 2, 5)
    screen.send(b"\x1bg")
    for _ in range(8): screen.drain(.01)
    assert "Agent transcript" in visible(screen) and journals(root) == before
    escape(screen)
    until(screen, lambda frame: "Conversation agents" in frame and ready(screen), "Esc restores roster")
    escape(screen)
    until(screen, lambda frame: "Conversation agents" not in frame and ready(screen), "Esc restores parent input")


def split_source_scroll(screen, root, cwd):
    marker = "CHAT_STABLE_MARKER"
    screen.send(f"{marker}\r")
    until(screen, lambda frame: completed(root, marker) and ready(screen), "stable chat completion")
    (cwd / "source.rs").write_text("".join(f"SRC_{index:03} let value = {index};\n" for index in range(100)))
    at = len(screen.raw)
    screen.resize(120, 32)
    screen.vt = Terminal(120, 32)
    screen.wait(b"\x1b[H\x1b[2J", since=at)
    local(screen, "/files source.rs")
    panel_width = max((screen.vt.cols - 1) * 2 // 5, 34)
    chat_width = screen.vt.cols - 1 - panel_width - 3
    right = chat_width + 3
    first_source = lambda: "".join(screen.vt.cells[6][right:]).rstrip()
    until(screen, lambda frame: marker in body(screen) and first_source().startswith("SRC_000"), "split Source ready")
    paint_quiet(screen)
    left = tuple(tuple(row[:chat_width]) for row in screen.vt.cells[3:-5])
    at = len(screen.raw)
    screen.send("jjjjj")
    until(screen, lambda frame: first_source().startswith("SRC_005"), "right Source scrolled")
    paint_quiet(screen)
    delta = screen.raw[at:]
    assert left == tuple(tuple(row[:chat_width]) for row in screen.vt.cells[3:-5]), "Source scroll moved the chat"
    assert b"\x1b[2K" not in delta and b"\x1b[2J" not in delta, "Source scroll cleared complete rows"
    assert marker.encode() not in delta, "Source scroll repainted stable chat text"

    # Mouse paging in the chat changes its reading state while the right pane
    # keeps its position. j/k/g/G then belong to the ordinary chat draft.
    click(screen, 2, 3)
    mouse(screen, 64, 2, 5)
    until(screen, lambda frame: reading(screen) and first_source().startswith("SRC_005"), "mouse targeted the chat")
    screen.send("FOCUS_DRAFT Ж界😀 jkgG")
    until(screen, lambda frame: screen.vt.line(-4) == "  › FOCUS_DRAFT Ж界😀 jkgG", "chat navigation letters remain literal")
    assert first_source().startswith("SRC_005"), "typing in chat scrolled Files"


def markdown_source_preview(screen, root, cwd):
    words = " ".join(f"WORD{index:04}" for index in range(800))
    lines = ["# PREVIEW_HEADING", "", "**bold** *italic* [LINK_LABEL](https://example.invalid/path)", "",
             "- list item Ж界😀", "  - nested item", "", "```rust",
             'let literal = "**stay** [code](not-a-link)";', "```", "",
             "WRAP_START" + "界" * 60 + "END_WRAP", "", words, "", "## PREVIEW_BOTTOM"]
    source = "\n".join(lines) + "\n"
    (cwd / "preview.md").write_text(source)
    before = journals(root)
    local(screen, "/files preview.md")
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
    until(screen, lambda frame: "WORD0000" in inspector_first(screen), "Preview source-line jump")
    initial = inspector_first(screen)
    screen.send(b"\x1b[6~")
    until(screen, lambda frame: "WORD" in inspector_first(screen) and inspector_first(screen) != initial, "Preview scrolled within one source paragraph")
    anchor = int(re.search(r"WORD(\d{4})", inspector_first(screen))[1])
    assert anchor > 0
    at = len(screen.raw)
    screen.resize(60, 24)
    screen.vt = Terminal(60, 24)
    screen.wait(b"\x1b[H\x1b[2J", since=at)
    until(screen, lambda frame: "WORD" in inspector_first(screen), "Preview reflow after resize")
    reflowed = int(re.search(r"WORD(\d{4})", inspector_first(screen))[1])
    assert reflowed <= anchor < reflowed + 7, ("resize lost paragraph reading position", anchor, reflowed)
    screen.send("G")
    until(screen, lambda frame: "PREVIEW_BOTTOM" in frame, "resized Preview bottom")
    screen.send("sg")
    until(screen, lambda frame: "# PREVIEW_HEADING" in frame and "**bold**" in frame, "Source remains original after Preview")
    assert (cwd / "preview.md").read_text() == source and journals(root) == before


def inline_tool_details(screen, root, cwd):
    screen.send("TOOLS_TASK\r")
    until(screen, lambda frame: completed(root, "TOOLS_TASK") and ready(screen)
          and "OMH_TOOL_OK" in frame, "completed tool turn has an assistant reply")
    assert "get_test_value" not in body(screen) and "Arguments" not in body(screen)
    screen.send("TOOLS_DRAFT Ж")
    until(screen, lambda frame: screen.vt.line(-4) == "  › TOOLS_DRAFT Ж", "tool draft")
    open_activity(screen)
    until(screen, lambda _: "get_test_value" in body(screen), "compact completed tool in Activity")
    compact = body(screen)
    headers = [line for line in compact.splitlines() if re.match(r"[>v] get_test_value\b", line)]
    assert len(headers) == 1 and "done" in headers[0], "tool lifecycle was split into duplicate rows"
    assert "prepared" not in compact and "KnownResult" not in compact and "Arguments" not in compact
    row = next(row for row in range(3, screen.vt.rows - 5) if "> get_test_value" in screen.vt.line(row))
    click(screen, screen.vt.line(row).index("> get_test_value"), row)
    until(screen, lambda frame: "Arguments" in body(screen) and "Output" in body(screen), "tool row opened inline")
    expanded = body(screen)
    assert "value: OMH_FIXED_VALUE_42" in expanded and "ok: true" in expanded
    assert '{"ok"' not in expanded and "KnownResult" not in expanded, "details exposed the raw protocol instead of readable values"
    assert [row["invocation"]["arguments"] for row in records(root) if row["kind"] == "intent"] == [{}], "display projection changed the invocation arguments"
    assert len([line for line in expanded.splitlines() if re.match(r"(?:› )?[>v] get_test_value\b", line)]) == 1
    assert screen.vt.line(-4) == "  › TOOLS_DRAFT Ж"
    row = next(row for row in range(3, screen.vt.rows - 5) if "v get_test_value" in screen.vt.line(row))
    click(screen, screen.vt.line(row).index("v get_test_value"), row)
    until(screen, lambda frame: "Arguments" not in body(screen) and "> get_test_value" in body(screen), "tool row collapsed")

    # Dragging a tool row selects text rather than activating its click target.
    paint_quiet(screen)
    row = next(row for row in range(3, screen.vt.rows - 5) if "> get_test_value" in screen.vt.line(row))
    column = screen.vt.line(row).index("get_test_value")
    at = len(screen.raw)
    mouse(screen, 0, column, row)
    mouse(screen, 32, column + len("get_test_value") - 1, row)
    mouse(screen, 0, column + len("get_test_value") - 1, row, release=True)
    assert copied(screen, at) == "get_test_value", "tool-row drag did not copy the displayed text"
    screen.send(b"\x0f")
    until(screen, lambda frame: "Arguments" in body(screen) and "Output" in body(screen), "Ctrl+O opened tool details")
    assert screen.vt.line(-4) == "  › TOOLS_DRAFT Ж"
    before = journals(root)
    row = next(row for row in range(3, screen.vt.rows - 5) if "Full output" in screen.vt.line(row))
    click(screen, screen.vt.line(row).index("Full output"), row)
    until(screen, lambda frame: "value: OMH_FIXED_VALUE_42" in frame and "j/k lines" in frame, "complete readable output inspector")
    assert screen.vt.line(-4) == "  › TOOLS_DRAFT Ж"
    output_before = body(screen)
    screen.send("Ж界😀")
    screen.send(b"\x1b[200~" + "Паста Ж界😀\n/exit".encode() + b"\x1b[201~")
    screen.send(b"\x7f\x08\x1b[3~")
    for _ in range(8): screen.drain(.01)
    assert screen.vt.line(-4) == "  › TOOLS_DRAFT Ж", "read-only output accepted Unicode, paste or deletion into the draft"
    assert body(screen) == output_before and journals(root) == before, "read-only output input changed the view or canonical journal"
    escape(screen)
    until(screen, lambda frame: activity_open(screen) and "get_test_value" in frame, "output inspector returned to Activity")
    close_activity(screen)
    assert "get_test_value" not in body(screen) and "value: OMH_FIXED_VALUE_42" not in body(screen)
    assert journals(root) == before and screen.vt.line(-4) == "  › TOOLS_DRAFT Ж"


def input_and_copy(screen, root, cwd):
    until(screen, lambda frame: "Ctrl+F Files" in frame and "Ctrl+T Terminals" in frame,
          "available shortcuts on the idle workspace")
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
    assert all(hint not in frame for hint in ["PgUp", "PgDn", "↑/↓"])
    screen.send("draft Ж界😀")
    until(screen, lambda frame: "draft Ж界😀" in frame, "draft")
    mouse(screen, 64, 2, 4)
    until(screen, lambda frame: reading(screen), "mouse reading")
    screen.resize(48, 16)
    screen.vt = Terminal(48, 16)
    until(screen, lambda frame: "draft Ж界😀" in frame, "narrow draft preserved")
    screen.resize(100, 32)
    screen.vt = Terminal(100, 32)
    escape(screen)
    until(screen, lambda frame: not reading(screen) and "draft Ж界😀" in frame, "return to composer")


def mouse_copy(screen, root, cwd):
    model = "gpt-6.1-sol"
    screen.send("MOUSE_COPY Ж界😀\r")
    until(screen, lambda frame: completed(root, "MOUSE_COPY Ж界😀") and ready(screen), "mouse-copy conversation content")
    paint_quiet(screen)
    row = next(row for row in range(3, screen.vt.rows - 5) if screen.vt.line(row).startswith("  > MOUSE_COPY"))
    before = journals(root)
    at = len(screen.raw)
    mouse(screen, 0, 4, row)
    mouse(screen, 32, 4 + len("MOUSE_COPY") - 1, row)
    screen.wait(b"\x1b[7mM", at)
    before_resize = len(screen.raw)
    screen.resize(80, 20)
    screen.vt = Terminal(80, 20)
    screen.wait(b"\x1b[7mMOUSE_COPY", before_resize)
    until(screen, lambda frame: model in screen.vt.line(-2) and "Message agent" not in screen.vt.line(-2), "compact status after resize")
    mouse(screen, 0, 4 + len("MOUSE_COPY") - 1, row, release=True)
    text = copied(screen, at)
    assert text == "MOUSE_COPY" and "\x1b" not in text, repr(text)
    assert journals(root) == before, "selection or resize changed the canonical conversation"
    until(screen, lambda frame: screen.vt.line(-2).split().count(model) == 1 and model not in screen.vt.line(0),
          "one model in the narrow footer")
    for _ in range(2):
        paint_quiet(screen)
        row = next(row for row in range(3, screen.vt.rows - 5) if screen.vt.line(row).startswith("  > MOUSE_COPY"))
        at = len(screen.raw)
        mouse(screen, 0, 4, row)
        mouse(screen, 32, 4 + len("MOUSE_COPY") - 1, row)
        mouse(screen, 0, 4 + len("MOUSE_COPY") - 1, row, release=True)
        assert copied(screen, at) == "MOUSE_COPY"
    assert journals(root) == before, "copies changed the canonical conversation"
    screen.send("AFTER_SELECT Ж\r")
    until(screen, lambda frame: not reading(screen) and "> AFTER_SELECT Ж" in body(screen), "Enter sent after repeated drag copies")
    until(screen, lambda frame: completed(root, "AFTER_SELECT Ж") and ready(screen), "post-selection message completed")
    assert sum(row["kind"] == "accepted" and row["text"] == "AFTER_SELECT Ж" for row in records(root)) == 1


def selection_scroll(screen, root, cwd):
    (cwd / "panel").mkdir()
    (cwd / "panel" / "notes.txt").write_text("INSPECTOR_NEVER_COPY\n")
    session = seed_history(screen.child.args[0], root, cwd, count=34)
    local(screen, f"/sessions {session}")
    until(screen, lambda frame: "HISTORY_SEED_33" in frame and ready(screen), "selectable canonical tail")
    screen.resize(126, 32)
    screen.vt = Terminal(126, 32)
    local(screen, "/files")
    until(screen, lambda frame: "panel/" in frame and "l directory" in frame, "split file listing")
    screen.send(b"\r")
    paint_quiet(screen)
    assert "panel/" in visible(screen), "Enter entered a directory"
    screen.send("l")
    until(screen, lambda frame: "notes.txt" in frame, "l entered the selected directory")
    screen.send("h")
    until(screen, lambda frame: "panel/" in frame, "h returned to the parent")
    escape(screen)
    local(screen, "/files panel/notes.txt")
    until(screen, lambda frame: "INSPECTOR_NEVER_COPY" in frame and "HISTORY_SEED_33" in frame, "split pane for selection")
    paint_quiet(screen)
    row = next(row for row in range(3, 27) if "HISTORY_SEED_33" in screen.vt.line(row))
    first = body(screen)
    at = len(screen.raw)
    mouse(screen, 0, len("  > HISTORY_SEED_33") - 1, row)
    mouse(screen, 32, 10, 3)
    until(screen, lambda frame: body(screen) != first and reading(screen), "drag at the top edge scrolls")
    for _ in range(240):
        mouse(screen, 64, 6, 5)
        screen.drain(.014)
        if "> HISTORY_FIRST" in body(screen):
            break
    else:
        raise AssertionError("selection did not scroll across a journal page")
    row = next(row for row in range(3, 27) if "> HISTORY_FIRST" in screen.vt.line(row))
    paint_quiet(screen)
    mouse(screen, 0, 4, row, release=True)
    text = copied(screen, at)
    assert "HISTORY_FIRST" in text and "HISTORY_SEED_33" in text, text[-500:]
    assert "INSPECTOR_NEVER_COPY" not in text and "Files" not in text and "\x1b" not in text
    assert screen.raw[at:].count(b"\x1b]52;c;") == 1
    before = journals(root)
    open_activity(screen)
    until(screen, lambda _: "get_test_value" in body(screen), "operation history in Terminals")
    assert "HISTORY_FIRST" not in body(screen) and "HISTORY_SEED_33" not in body(screen)
    for _ in range(8):
        screen.send(b"\x1b[5~")
        paint_quiet(screen)
        if "Terminals · Events " in body(screen):
            break
    else:
        raise AssertionError("Activity did not cross a journal page")
    assert "get_test_value" in body(screen), "canonical operations disappeared on the older page"
    screen.send("G")
    until(screen, lambda _: activity_open(screen) and "Terminals · Events " not in body(screen),
          "return to latest operation history")
    close_activity(screen)
    assert reading(screen) and journals(root) == before, "Activity paging changed chat reading state or journal"


def branches(screen, root, cwd):
    screen.send("parent-marker\r")
    until(screen, lambda frame: "OMH_TOOL_OK:OMH_FIXED_VALUE_42" in frame, "parent completion")
    local(screen, "/fork")
    until(screen, lambda frame: "Fork" in screen.vt.line(1), "fork conversation")
    assert "parent-marker" in visible(screen)
    screen.send("child-marker\r")
    until(screen, lambda frame: "child-marker" in screen.vt.line(1) and completed(root, "child-marker") and ready(screen), "fork completion")
    escape(screen)
    until(screen, lambda frame: "child-marker" not in screen.vt.line(1) and "parent-marker" in frame, "parent return")
    assert "child-marker" not in visible(screen)
    answer = "OMH_TOOL_OK:OMH_FIXED_VALUE_42"

    def wide_panes():
        lines = visible(screen).splitlines()
        return ("\n".join(line.partition("│")[0] for line in lines if "│" in line),
                "\n".join(line.partition("│")[2] for line in lines if "│" in line))

    def both_conversations_visible(_):
        main, side = wide_panes()
        return ("Main conversation" in main and "› Main chat" in main and "parent-marker" in main and answer in main and "/btw · read-only · Esc → main chat" in side
                and "side-marker" in side and answer in side and completed(root, "side-marker"))

    local(screen, "/btw side-marker")
    until(screen, both_conversations_visible, "parent and completed BTW side by side")
    assert "parent-marker" in screen.vt.line(1) and "side-marker" not in screen.vt.line(1)
    assert "side-marker" not in wide_panes()[0]
    row = next(row for row in range(screen.vt.rows) if "│ ›" in screen.vt.line(row))
    click(screen, screen.vt.line(row).index("│") + 4, row)
    screen.send("side-followup")
    until(screen, lambda _: "› side-followup" in wide_panes()[1], "draft in the BTW composer")
    screen.send(b"\r")
    def both_side_answers(_):
        # A restored parent answer may precede the two side turns in this pane.
        side = wide_panes()[1]
        _, first, rest = side.partition("> side-marker")
        first_reply, second, second_reply = rest.partition("> side-followup")
        return (first and second and first_reply.count(answer) == 1
                and second_reply.count(answer) == 1 and completed(root, "side-followup"))

    until(screen, both_side_answers, "second BTW answer")
    assert "side-followup" not in wide_panes()[0]
    side_sessions = []
    for session in (root / "home/.ansa/sessions").iterdir():
        info = json.loads((session / "info.json").read_text())
        if info.get("kind") == "btw":
            side_sessions.append(session)
    side, = side_sessions
    accepted = [record for record in session_cases.decode({
        path.name: path.read_bytes() for path in side.glob("part-*.log")
    }) if record["kind"] == "accepted"]
    assert [(record["text"], record["mode"]) for record in accepted] == [
        ("side-marker", "plan"), ("side-followup", "plan"),
    ]
    screen.resize(54, 30)
    screen.vt = Terminal(54, 30)
    until(screen, lambda frame: "/btw · read-only · Esc → main chat" in frame and "side-followup" in frame
          and "parent-marker" in frame and frame.count(answer) >= 2, "stacked BTW after terminal resize")
    escape(screen)
    until(screen, lambda frame: "/btw · read-only" not in frame and "parent-marker" in frame
          and ready(screen), "side parent return")
    assert "side-marker" not in visible(screen) and "side-followup" not in visible(screen)
    screen.resize(100, 32)
    screen.vt = Terminal(100, 32)
    until(screen, lambda _: ready(screen), "parent ready after terminal resize")
    local(screen, "/sessions")
    until(screen, lambda frame: "Project sessions" in frame and "Choose a conversation" in frame and "child-marker" in frame, "session picker with saved conversation titles")
    assert "parent-marker" in popup_text(screen) and "child-marker" in popup_text(screen)
    assert "side-marker" not in popup_text(screen), "side conversation leaked into the main/fork picker"
    assert not re.search(r"\bs_\d+_", screen.vt.line(1)), "conversation breadcrumb uses storage IDs as titles"
    escape(screen)


def files_and_terminal(screen, root, cwd):
    (cwd / "notes.md").write_text("# Fixture heading\n\n**content**\n")
    local(screen, "/files notes.md")
    until(screen, lambda frame: "read-only" in frame and "Fixture heading" in frame, "file source")
    screen.send("p")
    until(screen, lambda frame: "Fixture heading" in frame and "**content**" not in frame, "Markdown preview")
    screen.send("d")
    until(screen, lambda frame: "Diff unavailable" in frame, "non-Git diff explicit")
    before = journals(root)
    screen.send("q")
    until(screen, lambda frame: "Terminal ·" in frame and shell_ready(screen), "file inspector opened embedded terminal")
    screen.send("ANSA_PTY_VALUE=CONSOLE_ONLY; export ANSA_PTY_VALUE; printf '%s:%s\\n' \"$ANSA_PTY_VALUE\" \"$$\" > terminal-state; printf CONSOLE_ONLY\r")
    until(screen, lambda frame: (cwd / "terminal-state").exists() and "CONSOLE_ONLY" in frame and shell_ready(screen), "manual shell output")
    state = (cwd / "terminal-state").read_text()
    escape(screen)
    until(screen, lambda frame: "q terminal" in frame and "Esc files" not in frame, "prompt Esc returned to Files")
    screen.send("q")
    until(screen, lambda frame: shell_ready(screen), "same terminal reopened")
    screen.send("printf '%s:%s\\n' \"$ANSA_PTY_VALUE\" \"$$\" > terminal-state-again; false; printf ' STATUS:%s ' \"$?\"\r")
    until(screen, lambda frame: (cwd / "terminal-state-again").exists() and "STATUS:1" in frame and shell_ready(screen), "persistent shell environment and status")
    assert (cwd / "terminal-state-again").read_text() == state, "q created a second shell or lost its environment"
    terminal_copy_then_input(screen, root, cwd)
    escape(screen)
    until(screen, lambda frame: "q terminal" in frame and "Esc files" not in frame, "terminal returned to files again")
    escape(screen)
    until(screen, lambda frame: "q terminal" not in frame, "Files returned to chat")
    assert "CONSOLE_ONLY" not in body(screen), "manual terminal output leaked into model conversation"
    assert journals(root) == before, "manual shell activity entered the canonical journal"


def terminal_copy_then_input(screen, root, cwd):
    """New manual input must resume terminal painting after an OSC52 copy."""
    before = journals(root)
    canary, after = "PTY_OUTPUT_CANARY", "PTY_AFTER_COPY"
    # The complete canaries are absent from the echoed command text, so the VT
    # assertions prove child output rather than merely input echo.
    screen.send("printf '\\nPTY_%s_%s\\n' OUTPUT CANARY\r")
    until(screen, lambda frame: canary in frame and shell_ready(screen), "terminal output ready for selection")
    paint_quiet(screen)
    row = next(row for row in range(3, screen.vt.rows - 5) if canary in screen.vt.line(row))
    column = screen.vt.line(row).index(canary)
    at = len(screen.raw)
    mouse(screen, 0, column, row)
    mouse(screen, 32, column + len(canary) - 1, row)
    mouse(screen, 0, column + len(canary) - 1, row, release=True)
    assert copied(screen, at) == canary, "manual terminal selection did not copy its displayed output"
    screen.send("printf reached > terminal-copy-command; printf '\\nPTY_%s_%s\\n' AFTER COPY\r")
    until(screen, lambda frame: (cwd / "terminal-copy-command").exists()
          and (cwd / "terminal-copy-command").read_text() == "reached", "command after terminal selection executed")
    observation = {"label": "terminal-input-after-copy", "owned_marker": "terminal-copy-command",
                   "owned_marker_content": "reached", "new_output_visible": False}
    screen.observations.append(observation)
    try:
        until(screen, lambda frame: after in frame and shell_ready(screen), "terminal paints new output after copied selection", timeout=1.5)
    except AssertionError:
        # Preserve a concrete recovery proof before failing the regression.
        escape(screen)
        until(screen, lambda frame: "q terminal" in frame and "Esc files" not in frame, "Files after frozen terminal selection")
        screen.send("q")
        until(screen, lambda frame: after in frame and shell_ready(screen), "new child output becomes visible after reopening terminal")
        observation["visible_after_reopen"] = True
        raise AssertionError("terminal command executed, but copied selection hid new output until Files/reopen") from None
    observation["new_output_visible"] = True
    assert journals(root) == before, "manual terminal copying/input changed the canonical journal"


def editor_case(screen, root, cwd, cancel):
    (cwd / "notes.md").write_text("original\n")
    local(screen, "/files notes.md")
    until(screen, lambda frame: "read-only" in frame, "editor source")
    carbon_modes = termios.tcgetattr(screen.slave)
    screen.send("e")
    until(screen, lambda frame: "EDITOR_READY" in frame and "ANSA" in frame
          and "Editor · notes.md" in frame, "embedded editor visible")
    assert not screen.vt.alternate, "editor changed the outer alternate screen"
    assert termios.tcgetattr(screen.slave) == carbon_modes, "editor changed outer tty modes"
    if cancel:
        screen.send(b"\x03")
    else:
        screen.send(b"save\r")
    until(screen, lambda frame: "ANSA" in frame and "read-only" in frame, "file inspector restored")
    assert not screen.vt.alternate, "editor screen modes escaped pane"
    if not cancel:
        until(screen, lambda frame: "edited" in frame, "edited file reloaded")
        assert "edited" in (cwd / "notes.md").read_text()
    else:
        assert (cwd / "notes.md").read_text() == "original\n"
    assert os.tcgetpgrp(screen.master) == screen.child.pid, "editor owned the outer terminal"
    escape(screen)
    screen.send("after-editor")
    until(screen, lambda frame: "after-editor" in frame, "input after editor")


def editor_early_exit(screen, root, cwd, stopped=False):
    (cwd / "notes.md").write_text("original\n")
    local(screen, "/files notes.md")
    until(screen, lambda frame: "read-only" in frame, "early editor source")
    screen.send("e")
    marker = "editor_stopped" if stopped else "early exit contents"
    until(screen, lambda frame: "read-only" in frame and marker in frame, "early editor return")
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
    local(screen, "/files notes.md")
    until(screen, lambda frame: "read-only" in frame, "missing editor source")
    screen.send("e")
    until(screen, lambda frame: "Could not open editor" in frame, "missing editor diagnostic")
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
    local(screen, "/exit")
    return "normal"


def terminal_interrupt_and_app_escape(screen, root, cwd):
    before = journals(root)
    terminal(screen)
    screen.send("printf token > draft-result")
    until(screen, lambda frame: "draft-result" in frame and shell_ready(screen), "Bash has an unfinished command")
    screen.send(b"\x1b[D" * len("n > draft-result"))
    screen.send(b"\x04")
    escape(screen)
    until(screen, lambda frame: "q terminal" in frame and "Esc files" not in frame, "Escape leaves Bash prompt")
    assert not (cwd / "draft-result").exists(), "leaving the panel ran the command"
    screen.send("q")
    until(screen, lambda frame: "draft-result" in frame and shell_ready(screen), "Bash retains the command")
    screen.send("X\r")
    until(screen, lambda frame: (cwd / "draft-result").exists() and shell_ready(screen), "resumed command executes")
    assert (cwd / "draft-result").read_text() == "tokeX", "reopening the panel moved the cursor"
    screen.send("if true; then\r")
    until(screen, lambda frame: "if true; then" in frame and shell_ready(screen), "Bash continuation prompt")
    escape(screen)
    until(screen, lambda frame: "q terminal" in frame and "Esc files" not in frame, "Escape leaves continuation prompt")
    screen.send("q")
    until(screen, lambda frame: shell_ready(screen), "Bash retained the continuation prompt")
    screen.send("printf OK > draft-multiline; fi\r")
    until(screen, lambda frame: (cwd / "draft-multiline").exists() and shell_ready(screen), "resumed continuation executes")
    screen.send("printf %s 'first")
    screen.send(b"\x16\n")
    screen.send("second' > draft-quoted")
    until(screen, lambda frame: "draft-quoted" in frame and shell_ready(screen), "Bash retains a quoted line break")
    escape(screen)
    until(screen, lambda frame: "q terminal" in frame and "Esc files" not in frame, "Escape leaves quoted multiline input")
    screen.send("q")
    until(screen, lambda frame: "draft-quoted" in frame and shell_ready(screen), "Bash retains quoted multiline input")
    screen.send("\r")
    until(screen, lambda frame: (cwd / "draft-quoted").exists() and shell_ready(screen), "resumed quoted command executes")
    assert (cwd / "draft-quoted").read_text() == "first\nsecond"
    screen.send("printf ready > cancel-ready; sleep 30; printf bad > cancel-late\r")
    until(screen, lambda frame: (cwd / "cancel-ready").exists() and not shell_ready(screen), "real foreground process ready")
    screen.send(b"\x03")
    until(screen, lambda frame: shell_ready(screen), "Ctrl+C returned the same shell prompt")
    assert not (cwd / "cancel-late").exists(), "Ctrl+C did not terminate foreground work"
    screen.send("printf ready > app-ready; IFS= read -r -n 1 key; printf '%s' \"$key\" > app-key; printf APP_KEY_DONE\r")
    until(screen, lambda frame: (cwd / "app-ready").exists() and not shell_ready(screen), "foreground reader ready")
    escape(screen)
    until(screen, lambda frame: (cwd / "app-key").exists() and "APP_KEY_DONE" in frame and shell_ready(screen), "foreground app received Escape")
    assert (cwd / "app-key").read_bytes() == b"\x1b", "Escape navigated away instead of reaching the foreground reader"
    assert "Terminal ·" in visible(screen) and journals(root) == before
    assert os.tcgetpgrp(screen.master) == screen.child.pid, "manual shell stole the outer terminal foreground group"
    at = len(screen.raw)
    screen.resize(10, 7)
    screen.vt = Terminal(10, 7)
    until(screen, lambda frame: len(screen.raw) > at and "ansa" in frame.lower(), "tiny terminal inspector hidden after resize")
    screen.send(b"\tHIDDEN_SAFE")
    at = len(screen.raw)
    screen.resize(100, 32)
    screen.vt = Terminal(100, 32)
    screen.wait(b"\x1b[H\x1b[2J", since=at)
    until(screen, lambda frame: screen.vt.line(-4) == "  › HIDDEN_SAFE", "Tab at tiny size keeps input in composer")
    assert journals(root) == before, "hidden terminal input entered the model journal"
    screen.send(b"\x15")
    escape(screen)
    until(screen, lambda frame: "q terminal" in frame and "Esc files" not in frame, "prompt Escape returns to Files")
    escape(screen)
    until(screen, lambda frame: "q terminal" not in frame, "Files Escape returns to chat")
    screen.send("SURVIVED_TERMINAL")
    until(screen, lambda frame: screen.vt.line(-4) == "  › SURVIVED_TERMINAL", "composer after foreground interrupt")
    assert journals(root) == before and not (cwd / "cancel-late").exists()


def click_files_tab(screen):
    visible(screen)
    row = next(row for row in range(3, screen.vt.rows - 5)
               if "Files" in screen.vt.line(row) and "Terminal" in screen.vt.line(row))
    click(screen, screen.vt.line(row).index("Files") + 1, row)
    until(screen, lambda frame: "q terminal" in frame and "Esc files" not in frame, "Files tab after shell exit")


def terminal_eof_and_exit(screen, root, cwd):
    before = journals(root)
    terminal(screen)
    screen.send("printf '%s' \"$$\" > first-shell.pid\r")
    until(screen, lambda frame: (cwd / "first-shell.pid").exists() and shell_ready(screen), "first shell identity")
    screen.send(b"\x04")
    until(screen, lambda frame: "Terminal exited" in frame, "Ctrl+D exits embedded shell only")
    assert screen.child.poll() is None and journals(root) == before
    click_files_tab(screen)
    screen.send("q")
    until(screen, lambda frame: shell_ready(screen), "q starts a new shell after explicit shell EOF")
    screen.send("printf '%s' \"$$\" > second-shell.pid; exit 7\r")
    until(screen, lambda frame: (cwd / "second-shell.pid").exists() and "Terminal exited" in frame, "exit 7 leaves workspace alive")
    assert (cwd / "first-shell.pid").read_text() != (cwd / "second-shell.pid").read_text()
    click_files_tab(screen)
    escape(screen)
    until(screen, lambda frame: "q terminal" not in frame, "chat after embedded shell exits")
    screen.send("AFTER_SHELL_EOF")
    until(screen, lambda frame: screen.vt.line(-4) == "  › AFTER_SHELL_EOF", "shell EOF did not close or corrupt composer")
    assert journals(root) == before, "shell EOF/exit mutated the model journal"


def outer_eof_cleans_terminal(screen, root, cwd):
    before = journals(root)
    terminal(screen)
    screen.send("sleep 30 & child=$!; printf '%s' \"$child\" > eof-child.pid; wait \"$child\"; printf bad > eof-late\r")
    until(screen, lambda frame: (cwd / "eof-child.pid").exists() and not shell_ready(screen), "manual child execution barrier")
    pid = int((cwd / "eof-child.pid").read_text())
    click(screen, 2, 3)
    screen.send(b"\x04")
    result = screen.finish()
    assert result["exit"] == 0 and result["termios_restored"] and result["flags_restored"], result
    try:
        state = Path(f"/proc/{pid}/stat").read_text().split(") ", 1)[1].split()[0]
    except FileNotFoundError:
        pass
    else:
        assert state == "Z", "outer EOF left a live manual terminal child"
    assert not (cwd / "eof-late").exists(), "outer EOF let manual work continue"
    assert journals(root) == before and not any(row["kind"] == "cancel" for row in records(root)), "outer EOF fabricated canonical terminal actions"
    return "normal"


def native_terminal_panel(screen, root, cwd):
    home = root / "home"
    catalog = home / ".ansa/sessions"
    started_from_home = start(screen.child.args[0], root, home, home)
    screen.additional_screens = [("home-startup", started_from_home)]
    assert "Agent terminal" not in visible(started_from_home)
    local(started_from_home, "/exit")
    started_from_home.result = started_from_home.finish()
    assert started_from_home.result["exit"] == 0 and started_from_home.result["termios_restored"]
    screen.send("/term")
    until(screen, lambda frame: "/terminals" in body(screen), "native terminal command completion")
    screen.send(b"\t\r")
    until(screen, lambda _: activity_open(screen), "Activity works before the agent has opened a shell")
    row = next(row for row in range(screen.vt.rows) if screen.vt.line(row).strip() == "Activity  Shell")
    click(screen, screen.vt.line(row).index("Shell"), row)
    until(screen, lambda frame: "Agent terminal" in frame and "The agent has not opened a terminal yet" in frame,
          "managed terminal panel in the normal Carbon entry point")
    session, = catalog.glob("*/terminal-host-v1/terminal-v1.json")
    escape(screen)
    until(screen, lambda frame: "No activity" in frame and activity_open(screen), "Esc returned to Activity")
    close_activity(screen)
    screen.send("KEEP_FILES_DRAFT\x06")
    until(screen, lambda frame: "Files" in frame and "KEEP_FILES_DRAFT" in screen.vt.line(-4), "Ctrl+F opens files without submitting a draft")
    escape(screen)
    until(screen, lambda _: ready(screen), "Esc returns focus to draft")
    screen.send(b"\x15")
    screen.send("TERMINAL_DEMO\r")
    until(screen, lambda frame: completed(root, "TERMINAL_DEMO"), "offline terminal tool created a PTY")
    assert "terminal_create" not in body(screen) and "ANSA_TERMINAL_READY" not in body(screen)
    open_activity(screen)
    screen.send("s")
    until(screen, lambda frame: "Demo terminal" in frame and "ANSA_TERMINAL_READY" in body(screen),
          "Carbon renders live PTY output on the Shell tab")
    screen.send("a")
    until(screen, lambda frame: activity_open(screen) and "terminal_create" in frame, "return to recorded terminal operations")
    close_activity(screen)
    assert "ANSA_TERMINAL_READY" not in body(screen)
    local(screen, "/exit")
    result = screen.finish()
    assert result["exit"] == 0 and result["termios_restored"]
    assert session.with_name("closed-v1.json").is_file(), "clean host closed with a durable seal"
    resumed = start(screen.child.args[0], root, root / "home", cwd, argv_extra=["--session", session.parents[1].name])
    screen.additional_screens.append(("resumed", resumed))
    local(resumed, "/plan")
    resumed.send("SEALED_PLAN\r")
    until(resumed, lambda frame: completed(root, "SEALED_PLAN") and "OMH_TOOL_OK" in frame
          and "Plan" in resumed.vt.line(-2) and "Ctrl+C stop" not in resumed.vt.line(-2),
          "reopened Plan turn settled")
    local(resumed, "/plan")
    until(resumed, lambda frame: ready(resumed) and "Plan" not in resumed.vt.line(-2),
          "reopened conversation returned to Work")
    resumed.send("TERMINAL_DEMO\r")
    restarted_host = session.parents[1] / "terminal-host-v1-2"
    until(resumed, lambda frame: (restarted_host / "0001.json").is_file(),
          "reopened conversation can start a fresh shell")
    local(resumed, "/terminals Demo terminal")
    until(resumed, lambda frame: "ANSA_TERMINAL_READY" in body(resumed), "reopened named shell is live")
    escape(resumed)
    until(resumed, lambda _: activity_open(resumed), "named shell returns to Activity")
    close_activity(resumed)
    local(resumed, "/exit")
    resumed.result = resumed.finish()
    assert resumed.result["exit"] == 0 and resumed.result["termios_restored"]
    assert (restarted_host / "closed-v1.json").is_file()
    live_terminal_default(screen, root, cwd)
    return "normal"


def seed_siwc(home):
    auth = home / ".ansa/auth"
    auth.mkdir(parents=True, mode=0o700)
    (home / ".ansa").chmod(0o700)
    identity = dict(version=1, host_id="synthetic-host", client_id="synthetic-client",
                    subject="synthetic-subject")
    credentials = dict(identity, scope="openid profile email offline_access resource.invoke chatgpt.tokens.use.direct",
                       session="synthetic-session", expires_at=4102444800, earliest_refresh_at=0,
                       refresh_pending=False, access_token="synthetic-access", refresh_token="synthetic-refresh",
                       nonce="synthetic-nonce")
    for filename, value in (("registration.json", identity), ("credentials.json", credentials)):
        path = auth / filename
        path.write_text(json.dumps(value))
        path.chmod(0o600)
    return auth


def live_terminal_default(screen, root, cwd):
    home = root / "live-home"
    auth = seed_siwc(home)

    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        listener.listen(4)
        proxy = f"http://127.0.0.1:{listener.getsockname()[1]}"
        env = {key: proxy for key in ("http_proxy", "HTTP_PROXY", "https_proxy",
                                     "HTTPS_PROXY", "all_proxy", "ALL_PROXY")}
        env.update(no_proxy="", NO_PROXY="")
        options = ["--cwd", str(cwd), "--auth-dir", str(auth)]
        catalog = home / ".ansa/sessions"

        def launch(label, arguments):
            child = Pty(screen.child.args[0], root, home, cwd, "", True, size=(100, 32),
                        argv=[*options, *arguments], extra_env=env)
            child.vt = Terminal(100, 32)
            screen.additional_screens.append((label, child))
            return child

        for legacy_flag in (False, True):
            before = set(catalog.iterdir()) if catalog.exists() else set()
            child = launch(f"live-startup-{legacy_flag}", ["--native-terminals"] if legacy_flag else [])
            until(child, lambda _: ready(child), "ordinary Carbon startup with synthetic auth")
            local(child, "/exit")
            child.result = child.finish()
            assert child.result["exit"] == 0 and child.result["termios_restored"]
            session, = set(catalog.iterdir()) - before
            init, = session_cases.decode({p.name: p.read_bytes() for p in session.glob("part-*.log")})
            assert init["kind"] == "init", "idle Carbon submitted work"
            config = init["configuration"]
            tools = {tool["name"] for tool in config["tools"]["descriptors"]}
            assert "terminal_create" in tools and "run_command" not in tools
            instructions = config["epoch"]["instructions"]
            assert "Call terminal_list first" in instructions
            assert "lifetime_seconds=0 and idle_seconds=0" in instructions
        child = launch("live-restore", ["--session", session.name])
        until(child, lambda _: ready(child), "resume a managed conversation without flags")
        local(child, "/exit")
        child.result = child.finish()
        assert child.result["exit"] == 0 and child.result["termios_restored"]
        assert not select.select([listener], [], [], 0)[0], "provider connection during startup"


def completion_and_help(screen, root, cwd):
    before = journals(root)
    screen.send("/mo")
    until(screen, lambda frame: "/model" in body(screen) and "Tab/→ complete" in frame, "slash prefix suggestions")
    screen.send(b"\t")
    until(screen, lambda frame: screen.vt.line(-4) == "  › /model" and "Applies to future messages" not in frame, "Tab completes without dispatch")
    assert journals(root) == before
    screen.send(b"\r")
    until(screen, lambda frame: "Applies to future messages" in frame, "Enter opens the completed command")
    escape(screen)
    until(screen, lambda frame: "Applies to future messages" not in frame, "model menu closed before typing another prefix")
    screen.send("/")
    until(screen, lambda frame: "/model" in body(screen) and "/effort" in body(screen), "all command suggestions")
    screen.send(b"\x1b[B\x1b[C")
    until(screen, lambda frame: screen.vt.line(-4) == "  › /effort" and "Default" not in frame, "arrows choose and Right inserts only")
    assert journals(root) == before
    screen.send(b"\x15")
    screen.send(b"/exit" + b"\x1b[D" * 4 + b"\r")
    until(screen, lambda frame: screen.vt.line(-4) == "  › /model" and "Applies to future messages" not in frame,
          "Enter at a partial token inserts the selected command")
    assert journals(root) == before
    screen.send(b"\x15")
    screen.send("Проверь /mo потом")
    screen.send(b"\x1b[D" * len(" потом"))
    until(screen, lambda frame: "/model" in body(screen) and "Tab/→ complete" in frame, "completion at a middle word boundary")
    screen.send(b"\t")
    until(screen, lambda frame: screen.vt.line(-4) == "  › Проверь /model потом", "middle completion preserves prefix and suffix")
    assert journals(root) == before
    screen.send(b"\x05\x15")
    screen.send("?")
    until(screen, lambda frame: "ANSA commands" in frame, "empty composer question mark opens help")
    command_rows = [match for row in range(screen.vt.rows)
                    if (match := re.match(r"^\s*│(?:› |  )(/[a-z]+\+?)\s+(.*?)\s*│", screen.vt.line(row)))]
    labels = [row[1] for row in command_rows]
    expected = {"/model", "/effort", "/login", "/theme", "/files", "/agents", "/sessions",
                "/new", "/fork", "/btw", "/plan", "/copy", "/help", "/exit", "/autocompact",
                "/orchestrate", "/orchestrate+", "/terminals"}
    assert set(labels) == expected and len(labels) == len(expected), "help does not give each command its own row"
    assert all(not re.search(r"\s{2,}/[a-z]+", row[2]) for row in command_rows), "help packed another command column into a row"
    assert not {"/run", "/pr"} & set(labels), "a removed command remains advertised"
    screen.send(b"\x1b[B\r")
    until(screen, lambda frame: screen.vt.line(-4) == "  › /effort" and "ANSA commands" not in frame, "help arrows select and Enter only inserts")
    assert journals(root) == before
    screen.send(b"\x15")
    screen.send("text?")
    until(screen, lambda frame: screen.vt.line(-4) == "  › text?" and "ANSA commands" not in frame, "question mark inside text stays text")
    assert journals(root) == before
    screen.send(b"\x05\x15")
    # An ordinary whitespace delimiter is also valid for no-argument local
    # commands. It must never turn /exit or a mode switch into a model prompt.
    screen.send("/plan \r")
    until(screen, lambda frame: "Plan" in screen.vt.line(-2) and screen.vt.line(-4) == "  ›", "trailing-space /plan routed locally")
    screen.send("/plan \r")
    until(screen, lambda frame: "Plan" not in screen.vt.line(-2) and screen.vt.line(-4) == "  ›", "/plan returns to work")
    screen.send("/pr")
    until(screen, lambda frame: "Commands" not in frame and screen.vt.line(-4) == "  › /pr",
          "removed /pr is not suggested")
    screen.send(b" main\r")
    until(screen, lambda frame: "Ask the agent to open a pull request with gh" in frame,
          "obsolete /pr stays local without dispatch")
    assert journals(root) == before, "whitespace in local commands dispatched canonical user turns"
    screen.send("/exit \r")
    return "normal"


def new_resume_and_exit(screen, root, cwd):
    assert screen.vt.line(0) == "  ANSA" and screen.vt.line(1) == ""
    assert "ready" not in screen.vt.line(-2) and cwd.name in screen.vt.line(-2)
    local(screen, "/sessions")
    until(screen, lambda _: "No saved conversations" in popup_text(screen),
          "a new empty session is absent from history")
    escape(screen)
    prompt = "PREVIOUS_SESSION\n\n\n\nSECOND_PARAGRAPH"
    screen.send("\x1b[200~" + prompt + "\x1b[201~\r")
    until(screen, lambda frame: completed(root, prompt) and "OMH_TOOL_OK" in frame,
          "previous session committed")
    rows = [line.strip() for line in body(screen).splitlines()]
    first = rows.index("> PREVIOUS_SESSION")
    assert rows[first:first + 3] == ["> PREVIOUS_SESSION", "", "SECOND_PARAGRAPH"]
    previous, = [path for path in (root / "home/.ansa/sessions").iterdir() if path.is_dir()]
    local(screen, "/new")
    until(screen, lambda frame: ready(screen) and "PREVIOUS_SESSION" not in frame
          and sum(row["kind"] == "init" for row in records(root)) == 2, "new conversation with an empty transcript")
    assert sum(row["kind"] == "accepted" for row in records(root)) == 1
    empty_journal = journals(root)
    local(screen, "/new")
    until(screen, lambda frame: ready(screen) and "PREVIOUS_SESSION" not in frame,
          "repeated new stays on an empty conversation")
    local(screen, "/resume")
    until(screen, lambda frame: "Project sessions" in frame and "PREVIOUS_SESSION" in popup_text(screen),
          "resume opened saved sessions on an idle chat")
    assert "resume_not_available" not in visible(screen)
    assert "New conversation" not in popup_text(screen) and journals(root) == empty_journal
    escape(screen)
    until(screen, lambda frame: "Project sessions" not in frame, "session picker closed")
    before_search = journals(root)
    local(screen, "/sessions search previous_session")
    until(screen, lambda _: "Search project sessions" in popup_text(screen)
          and "PREVIOUS_SESSION" in popup_text(screen), "search found the original user message")
    screen.send(b"\r")
    until(screen, lambda _: "Saved conversation" in popup_text(screen)
          and "PREVIOUS_SESSION" in popup_text(screen), "source opened at the archived turn")
    escape(screen)
    until(screen, lambda _: "Saved conversation" not in popup_text(screen), "source reader closed")
    assert journals(root) == before_search, "search or source reading changed the session journal"
    incomplete = previous.parent / "incomplete_search_journal"
    incomplete.mkdir(mode=0o700)
    info = json.loads((previous / "info.json").read_text())
    info["id"] = incomplete.name
    (incomplete / "info.json").write_text(json.dumps(info))
    (incomplete / "info.json").chmod(0o600)
    local(screen, "/sessions search absent-search-marker")
    until(screen, lambda _: "Search incomplete" in popup_text(screen)
          and "some sessions unavailable" in popup_text(screen), "search reports missing journal with zero hits")
    escape(screen)
    until(screen, lambda _: "Search project sessions" not in popup_text(screen), "incomplete search closed")
    assert journals(root) == before_search, "search recovery wrote a journal part"
    local(screen, f"/resume {previous.name}")
    until(screen, lambda frame: "PREVIOUS_SESSION" in screen.vt.line(1), "resume with an ID reopened that conversation")
    screen.send("Черновик Ж界")
    until(screen, lambda frame: screen.vt.line(-4) == "  › Черновик Ж界", "draft before interrupt")
    screen.send(b"\x03")
    until(screen, lambda frame: screen.vt.line(-4) == "  ›", "interrupt clears the draft")
    assert "again to exit" not in visible(screen)
    local(screen, "/status")
    until(screen, lambda frame: "ansa: completed" in frame, "explicit status before exit")
    screen.send(b"\x03")
    until(screen, lambda frame: "Press Ctrl+C again to exit" in screen.vt.line(-1), "first empty interrupt confirms exit")
    screen.send("x")
    until(screen, lambda frame: screen.vt.line(-4) == "  › x" and "again to exit" not in frame,
          "typing disarms exit")
    screen.send(b"\x03")
    until(screen, lambda frame: screen.vt.line(-4) == "  ›", "draft cleared after disarming")
    screen.send(b"\x03")
    until(screen, lambda frame: "Press Ctrl+C again to exit" in screen.vt.line(-1), "exit armed again")
    screen.send(b"\x03")
    return "normal"


def model_effort_pair(screen, root, cwd):
    before = journals(root)
    local(screen, "/model")
    until(screen, lambda frame: "Applies to future messages" in frame and "Enter choose effort" in frame, "model selection stage")
    screen.send(b"\r")
    until(screen, lambda frame: "Default" in frame and "Enter apply" in frame, "effort selection follows model")
    assert journals(root) == before, "choosing a model committed before effort confirmation"
    escape(screen)
    until(screen, lambda frame: "Enter choose effort" in frame, "effort Esc returns to model list")
    escape(screen)
    assert journals(root) == before, "cancelling the staged pair wrote runtime state"
    until(screen, lambda frame: "Enter choose effort" not in frame, "model picker cancelled before reopening")
    local(screen, "/model")
    until(screen, lambda frame: "Enter choose effort" in frame, "cached model picker reopens")
    screen.send(b"\r")
    until(screen, lambda frame: "Enter apply" in frame, "pair ready to apply")
    screen.send(b"\r")
    until(screen, lambda frame: sum(row["kind"] == "generation_selected" for row in records(root)) == 1 and ready(screen), "single durable combined selection")
    events = records(root)
    selected = [row for row in events if row["kind"] == "generation_selected"]
    assert len(selected) == 1 and selected[0]["effort"] is None
    assert not any(row["kind"] in {"accepted", "response", "intent"} for row in events), "model selection dispatched generation or tools"
    home = root / "home"
    initial, = (home / ".ansa/sessions").iterdir()
    local(screen, "/exit")
    assert screen.finish()["exit"] == 0

    override = "fixture-other-model"
    (home / ".ansa/config.json").write_text(json.dumps({"schema_version": 1, "model": override}))
    restarted = start(screen.child.args[0], root, home, cwd)
    screen.additional_screens = [("saved-model", restarted)]
    local(restarted, "/exit")
    restarted.result = restarted.finish()
    assert restarted.result["exit"] == 0

    explicit = start(screen.child.args[0], root, home, cwd, argv_extra=["--model", override], model=override)
    screen.additional_screens.append(("explicit-model", explicit))
    local(explicit, f"/sessions {initial.name}")
    until(explicit, lambda frame: ready(explicit), "resumed session uses its own saved model")
    explicit.send("MODEL_INHERITANCE_FIXTURE\r")
    until(explicit, lambda _: completed(root, "MODEL_INHERITANCE_FIXTURE"),
          "create a turn before opening a distinct conversation")
    previous = set((home / ".ansa/sessions").iterdir())
    local(explicit, "/new")
    until(explicit, lambda frame: ready(explicit)
          and sum(row["kind"] == "init" for row in records(root)) == 4,
          "new conversation inherits the foreground model")
    created, = set((home / ".ansa/sessions").iterdir()) - previous
    init, = session_cases.decode({path.name: path.read_bytes() for path in created.glob("part-*.log")})
    assert init["configuration"]["binding"]["requested_model"] == selected[0]["model"]
    local(explicit, "/exit")
    explicit.result = explicit.finish()
    assert explicit.result["exit"] == 0
    return "normal"


def plan_agent_inherits(screen, root, cwd):
    parent, child = "PLAN_PARENT", "PLAN_AGENT"
    screen.send(parent + "\r")
    until(screen, lambda frame: completed(root, parent) and ready(screen)
          and "OMH_TOOL_OK:OMH_FIXED_VALUE_42" in frame, "parent reply rendered")
    local(screen, "/plan")
    until(screen, lambda frame: "Plan" in screen.vt.line(-2), "parent entered plan")
    before = records(root)
    screen.send("Запусти субагента: подготовка\r")
    until(screen, lambda frame: "Refine" in frame, "cancel preview")
    escape(screen)
    screen.send(b"\t")
    until(screen, lambda frame: "Orchestrate" in screen.vt.line(-2), "third Tab mode")
    screen.send("ORCHESTRATION_BLOCKED_WRITE_FIXTURE\r")
    until(screen, lambda frame: "A manual check is required before starting" in frame and "[Refine]" in frame,
          "write plan explains blocked approval before Enter")
    screen.send(b"\x1b[A\r")
    until(screen, lambda frame: "[Start unavailable]" in frame, "blocked write plan remains open")
    escape(screen)
    screen.send("ORCHESTRATION_LONG_PLAN_FIXTURE\r")
    until(screen, lambda frame: "Проверить перенос длинного плана" in frame, "long proposal")
    assert "VISIBLE_END_OF_STEP" not in visible(screen)
    screen.send(b"\x1b[6~")
    until(screen, lambda frame: "VISIBLE_END_OF_STEP" in frame, "full step accessible before approval")
    escape(screen)
    after_preview = records(root)
    assert len(journals(root)) == 1, "drafting a plan created a separate session"
    assert [row for row in after_preview if row["kind"] == "outcome"] == [
        row for row in before if row["kind"] == "outcome"
    ], "preview, scroll or cancel dispatched a tool"
    drafts = [row for row in after_preview if row.get("draft")]
    assert len(drafts) == 3 and drafts[0]["mode"] == "plan"
    assert all(not row["next"]["tools"]["descriptors"] for row in drafts)
    assert any(item.get("content") == parent for item in drafts[0]["next"]["input"])

    approve_helpers(screen, "Запусти субагента: " + child)
    accepted = [row for row in records(root) if row['kind'] == 'accepted']
    helpers = [row for row in accepted if row['command'] == 'assignment']
    assert len(helpers) == 2 and all(row['mode'] == 'plan' for row in helpers)
    assert sum(row.get('handoff') is True for row in accepted) == 1


def session_agent_titles_restart(screen, root, cwd):
    parent = "PARENT_TITLE " + "context " * 2800 + "EARLIEST_CONTEXT_END"
    screen.send("\x1b[200~" + parent + "\x1b[201~\r")
    until(screen, lambda frame: completed(root, parent) and ready(screen) and "OMH_TOOL_OK" in frame
          and "Ctrl+C stop" not in screen.vt.line(-2), "parent turn settled and title committed")
    screen.send(b"\t\t")
    until(screen, lambda frame: "Orchestrate" in screen.vt.line(-2), "third Tab mode")
    screen.send("Почему отчёт иногда пустой?\r")
    until(screen, lambda frame: "Refine" in frame, "plan from a vague request")
    before_refine = journals(root)
    screen.send(b"\x1b[B\r")
    until(screen, lambda frame: "Enter a clarification" in frame, "refinement returned to composer")
    assert journals(root) == before_refine, "refinement launched a helper"
    approve_helpers(screen, "Проверить формат входных данных")
    open_helper(screen)
    escape(screen)
    until(screen, lambda frame: "PARENT_TITLE" in screen.vt.line(1) and not helper_title(screen.vt.line(1)), "agent returned to root conversation")
    until(screen, lambda frame: "Conversation agents" in frame and "Найти файлы" in frame and "Проверить, достаточно" in frame, "Esc restored roster with both completed helpers")
    assert not re.search(r"\bs_\d+_", screen.vt.line(1)), "agent breadcrumb displays an ID instead of a title"
    escape(screen)
    until(screen, lambda frame: "Conversation agents" not in frame and not helper_title(screen.vt.line(1)), "agent inspector closed before session command")
    local(screen, "/sessions")
    until(screen, lambda frame: "Project sessions" in frame and "Choose a conversation" in frame and "PARENT_TITLE" in frame, "human-readable root session picker")
    assert not helper_title(popup_text(screen)), "agents leaked into conversation picker"
    escape(screen)
    until(screen, lambda frame: "Project sessions" not in frame and "PARENT_TITLE" in screen.vt.line(1), "session picker closed before exit")
    parent_id = next(json.loads((directory / "info.json").read_text())["id"]
                     for directory in (root / "home/.ansa/sessions").iterdir()
                     if any(event["kind"] == "accepted" and event["text"] == parent
                            for event in session_cases.decode({part.name: part.read_bytes() for part in directory.glob("part-*.log")})))
    before = journals(root)
    local(screen, "/exit")
    result = screen.finish()
    assert result["exit"] == 0 and result["termios_restored"] and result["flags_restored"], result
    restarted = start(screen.child.args[0], root, root / "home", cwd, argv_extra=["--session", parent_id])
    screen.additional_screens = [("restart", restarted)]
    until(restarted, lambda frame: "PARENT_TITLE" in restarted.vt.line(1) and ready(restarted), "saved parent title restored")
    local(restarted, "/agents")
    until(restarted, lambda frame: "Conversation agents" in frame and "Найти файлы" in frame and "Проверить, достаточно" in frame, "completed agent tree rebuilt from journals")
    assert "Completed" in visible(restarted), "restarted completed agents lost their observed status"
    assert not re.search(r"\bs_\d+_", restarted.vt.line(1)) and journals(root) == before
    escape(restarted)
    escape(restarted)
    until(restarted, lambda frame: "Conversation agents" not in frame and "Agent transcript" not in frame,
          "focus returned to conversation")
    followup = "Use the previous proposed plan in this conversation"
    restarted.send(followup + "\r")
    until(restarted, lambda frame: completed(root, followup) and ready(restarted),
          "ordinary Work turn uses the saved conversation after restart")
    accepted = [row for row in records(root) if row["kind"] == "accepted"]
    drafts = [row for row in accepted if row.get("draft")]
    assert len(drafts) == 2 and all(
        any(item.get("content") == parent for item in row["next"]["input"])
        for row in drafts
    ), "drafts lost the original long message"
    assert drafts[-1]["text"] == "Проверить формат входных данных"
    native_plan = lambda item: item.get("role") == "assistant" and "Разобрать запрос" in str(item.get("content"))
    assert any(map(native_plan, drafts[-1]["next"]["input"])), "refinement omitted the prior plan"
    last, = [row for row in accepted if row["text"] == followup]
    history = last["next"]["input"]
    assert sum(map(native_plan, history)) == 2 and any(
        "Сводка готова" in str(item.get("content")) for item in history if item.get("role") == "assistant"
    ), "ordinary Work lost the proposed plans or the synthesis"
    restarted.result = restarted.finish(signal.SIGTERM)
    assert restarted.result["exit"] == 143 and restarted.result["termios_restored"] and restarted.result["flags_restored"], restarted.result
    return "normal"


def orchestration_candidate(screen, root, cwd):
    def git(*args, path=cwd):
        return subprocess.check_output(["git", *args], cwd=path, text=True)

    git("init", "-q")
    report = cwd / "report.txt"
    report.write_text("committed version\n")
    check_marker = root / "check-ran"
    check = cwd / "check.sh"
    check.write_text(f'#!/bin/sh\nset -eu\ngrep -qx "after fixture" report.txt\nprintf x >> "{check_marker}"\nprintf "fixture-check-output\\n"\n')
    check.chmod(0o700)
    git("add", "report.txt", "check.sh")
    git("-c", "user.name=Fixture", "-c", "user.email=fixture@localhost", "commit", "-qm", "fixture")
    original = git("rev-parse", "HEAD").strip()
    report.write_text("before fixture\n")
    before = git("status", "--short")
    screen.send(b"\t\t")
    until(screen, lambda frame: "Orchestrate" in screen.vt.line(-2), "writer plan mode")
    screen.send("ORCHESTRATION_COMMAND_FIXTURE\r")
    until(screen, lambda frame: "Independent review" in frame or "independent review" in frame,
          "plan discloses verification and fixes")
    assert not (home := root / "home/.ansa/orchestration").exists()
    assert "./check.sh" in visible(screen) and not check_marker.exists()
    screen.send(b"\r")
    until(screen, lambda frame: "ansa-result готова" in frame, "candidate synthesis in the chat", timeout=15)
    assert all(label not in body(screen) for label in ("Verified changes", "Approved plan", "report.txt · +1 −1", "./check.sh · passed"))
    open_activity(screen)
    until(screen, lambda frame: "report.txt · +1 −1" in frame, "verified candidate in Activity")
    select_result(screen, "report.txt · +1 −1")
    screen.send(b"\r")
    until(screen, lambda frame: "Verified changes" in frame and "after fixture" in frame,
          "reviewed isolated candidate and diff")
    checkout, = home.glob("*/*/checkout")
    release = json.loads((checkout.parent / "release.json").read_text())
    assert release["candidate"] == git("rev-parse", "ansa-result", path=checkout).strip()
    assert git("rev-parse", "HEAD").strip() == original
    assert git("show", "ansa-result:report.txt", path=checkout) == "after fixture\n"
    assert report.read_text() == "before fixture\n"
    assert git("show", ":report.txt") == "committed version\n"
    assert git("status", "--short") == before
    assert check_marker.read_text() == "x"
    assert "project snapshot → ansa-result" in visible(screen)
    for cols, rows in [(80, 24), (120, 36)]:
        screen.resize(cols, rows)
        screen.vt = Terminal(cols, rows)
        until(screen, lambda frame: "report.txt · +1 −1" in frame, "candidate diff at terminal size")
        screen.send(b"f")
        until(screen, lambda frame: screen.vt.line(4).startswith("report.txt · +1 −1")
              and "after fixture" in frame, "wide diff with its actual comparison base")
        screen.send(b"f")
    screen.resize(100, 32)
    screen.vt = Terminal(100, 32)
    until(screen, lambda frame: "report.txt · +1 −1" in frame, "diff restored after resize")
    escape(screen)

    def candidate_excerpt(frame):
        rows = frame.splitlines()
        headings = [index for index, row in enumerate(rows) if "report.txt · +1 −1" in row]
        if len(headings) != 1 or headings[0] + 2 >= len(rows):
            return False
        start = headings[0]
        return ("- before fixture" in rows[start + 1]
                and "+ after fixture" in rows[start + 2])

    until(screen, candidate_excerpt, "Activity shows the actual change once below its summary")
    close_activity(screen)
    screen.send("review draft")
    until(screen, lambda _: "review draft" in screen.vt.line(-4), "draft in the chat")
    open_activity(screen)
    until(screen, lambda frame: "./check.sh · passed" in frame, "confirmed command in Activity")
    select_result(screen, "./check.sh · passed")
    screen.send(b"\r")
    until(screen, lambda frame: "fixture-check-output" in frame, "show actual command output")
    escape(screen)
    until(screen, lambda frame: "› ./check.sh · passed" in frame and "review draft" in screen.vt.line(-4),
          "same keyboard result and draft after Esc")
    close_activity(screen)
    screen.send(b"\x03")
    results = [event for event in records(root) if event["kind"] == "outcome"]
    assert len(results) == 2, "edit and independent diff review should each use a native tool"
    run = checkout.parent
    parent_id = run.parent.name
    until(screen, lambda frame: (run / "summary-admitted").is_file(), "durable parent handoff receipt")
    before_journals = journals(root)
    frozen = git("rev-parse", "ansa-result", path=checkout)
    local(screen, "/exit")
    result = screen.finish()
    assert result["exit"] == 0 and result["termios_restored"], result
    # Lose only the coordinator's derived acknowledgement. Native outcomes,
    # candidate identity and parent acceptance remain durable.
    (run / "summary-admitted").unlink()
    pending = run / "summary-admitted.pending"
    pending.touch()
    pending.chmod(0o600)
    restarted = start(screen.child.args[0], root, root / "home", cwd, argv_extra=["--session", parent_id])
    screen.additional_screens = [("restart", restarted)]
    assert "Approved plan" not in body(restarted)
    open_activity(restarted)
    until(restarted, lambda frame: "Approved plan" in frame, "plan link after restart")
    select_result(restarted, "Approved plan")
    restarted.send(b"\r")
    until(restarted, lambda frame: "Original request" in frame, "original question from approved run ledger")
    escape(restarted)
    until(restarted, lambda frame: activity_open(restarted) and "./check.sh · passed" in frame, "verified checks recovered from their receipts")
    select_result(restarted, "./check.sh · passed")
    restarted.send(b"\r")
    until(restarted, lambda frame: "fixture-check-output" in frame, "verified command output after restarting")
    escape(restarted)
    until(restarted, lambda _: activity_open(restarted), "restored output returns to Activity")
    close_activity(restarted)
    local(restarted, "/sessions")
    until(restarted, lambda frame: "Resume orchestration" in frame, "explicit continuation from original ledgers")
    restarted.send(b"\r")
    until(restarted, lambda frame: (run / "summary-admitted").is_file(), "restored durable handoff receipt", timeout=15)
    assert "Verified changes" not in body(restarted)
    open_activity(restarted)
    until(restarted, lambda frame: "report.txt · +1 −1" in frame, "reconciled exact original candidate")
    select_result(restarted, "report.txt · +1 −1")
    restarted.send(b"\r")
    until(restarted, lambda frame: "Verified changes" in frame and "after fixture" in frame, "reconciled candidate diff")
    assert git("rev-parse", "ansa-result", path=checkout) == frozen
    assert check_marker.read_text() == "x", "recovery ran the completed command again"
    assert journals(root) == before_journals, "reconciliation executed a second tool or repeated the handoff"
    restarted.result = restarted.finish(signal.SIGTERM)
    assert restarted.result["exit"] == 143 and restarted.result["termios_restored"], restarted.result
    return "normal"


def orchestration_review_resume(screen, root, cwd, exhausted=False):
    def git(*args, path=cwd):
        return subprocess.check_output(["git", *args], cwd=path, text=True).strip()

    def reviewer_records():
        for directory in (root / "home/.ansa/sessions").iterdir():
            rows = session_cases.decode({path.name: path.read_bytes()
                                         for path in directory.glob("part-*.log")})
            if any(row["kind"] == "accepted" and
                   row["text"].startswith("Независимо проверьте замороженный коммит")
                   for row in rows):
                yield directory, rows

    report = cwd / "report.txt"
    report.write_text("before fixture review-resume\n")
    check_marker = root / "check-ran"
    check = cwd / "check.sh"
    check.write_text('#!/bin/sh\nset -eu\ngrep -qx "after fixture review-resume" report.txt\n'
                     f'printf x >> "{check_marker}"\n')
    check.chmod(0o700)
    git("init", "-q")
    git("add", "report.txt", "check.sh")
    git("-c", "user.name=Fixture", "-c", "user.email=fixture@localhost", "commit", "-qm", "fixture")
    original = git("rev-parse", "HEAD")
    previous_status = git("status", "--short")
    screen.send(b"\t\t")
    until(screen, lambda _: "Orchestrate" in screen.vt.line(-2), "ordinary orchestration mode")
    screen.send("ORCHESTRATION_REVIEW_RESUME_FIXTURE\r")
    until(screen, lambda frame: "./check.sh" in frame and "Verify review-resume fixture" in frame,
          "review and command require approval")
    assert not check_marker.exists()
    screen.send(b"\r")
    store = root / "home/.ansa/orchestration"
    until(screen, lambda frame: "Agent paused" in frame and "resume with /sessions" in frame and any(
        row["kind"] == "pause" and row.get("diagnostic") == "tool_round_limit"
        for _, rows in reviewer_records() for row in rows),
        "read-only reviewer paused at the tool round limit", timeout=15)
    checkout, = store.glob("*/*/checkout")
    run = checkout.parent
    frozen = (run / "candidate-0").read_text()
    assert git("show", f"{frozen}:report.txt", path=checkout) == "after fixture review-resume"
    assert check_marker.read_text() == "x"
    (reviewer, paused), = reviewer_records()
    reads = ["ansa-candidate.diff", "approved-plan.json", "checkout/report.txt"]
    rounds = 1 if exhausted else 2
    assert [row["invocation"]["arguments"]["path"] for row in paused
            if row["kind"] == "intent"] == reads[:rounds]
    assert sum(row["kind"] == "outcome" for row in paused) == rounds
    assert not any(row["kind"] in ("cancel", "done") for row in paused)
    local(screen, "/exit")
    assert screen.finish()["exit"] == 0

    restarted = start(screen.child.args[0], root, root / "home", cwd,
                      argv_extra=["--session", run.parent.name])
    screen.additional_screens = [("restart", restarted)]
    local(restarted, "/sessions")
    until(restarted, lambda frame: "Resume orchestration" in frame, "saved run can resume")
    restarted.send(b"\r")
    if exhausted:
        expected = "Plan stopped (run_budget)"
        until(restarted, lambda frame: expected in frame or "run_step_unavailable" in frame,
              "explicit reviewer resume", timeout=15)
        assert expected in visible(restarted), visible(restarted)
        assert not (run / "release.json").exists()
        assert not (run / "summary-admitted").exists()
        assert subprocess.run(["git", "show-ref", "--verify", "--quiet", "refs/heads/ansa-result"],
                              cwd=checkout, check=False).returncode == 1
        before = journals(root)
        local(restarted, "/sessions")
        until(restarted, lambda frame: "Project sessions" in frame and "Choose a conversation" in frame,
              "session picker after reviewer exhaustion")
        assert "Resume orchestration" not in visible(restarted)
        escape(restarted)
        assert journals(root) == before, "Session picker continued an exhausted review"
    else:
        until(restarted, lambda _: (run / "summary-admitted").is_file(), "durable candidate acceptance", timeout=15)
        assert "Verified changes" not in body(restarted)
        open_activity(restarted)
        until(restarted, lambda frame: "Independent review · passed" in frame, "resumed verification in Activity")
        select_result(restarted, "report.txt · +1 −1")
        restarted.send(b"\r")
        until(restarted, lambda frame: "Verified changes" in frame and "after fixture review-resume" in frame, "resumed candidate diff")
        assert git("rev-parse", "ansa-result", path=checkout) == frozen
        assert json.loads((run / "release.json").read_text())["candidate"] == frozen
    assert git("rev-parse", "HEAD") == original and git("status", "--short") == previous_status
    assert report.read_text() == "before fixture review-resume\n"
    assert check_marker.read_text() == "x", "Resume repeated the required command"
    (resumed_reviewer, resumed), = reviewer_records()
    assert resumed_reviewer == reviewer and resumed[:len(paused)] == paused
    assert [row["kind"] for row in resumed[len(paused):] if row["kind"] in
            ("accepted", "resume", "cancel", "pause", "done")] == [
        "resume", "pause" if exhausted else "done",
    ]
    if exhausted:
        assert resumed[-1]["diagnostic"] == "tool_round_limit"
    assert [row["invocation"]["arguments"]["path"] for row in resumed[len(paused):]
            if row["kind"] == "intent"] == reads[rounds:2 * rounds]
    edit_intents = [row for row in records(root)
                    if row["kind"] == "intent" and row["invocation"]["name"] == "edit_file"]
    assert len(edit_intents) == 1, "Resume dispatched another edit"
    edit_operation = edit_intents[0]["invocation"]["operation"]
    edit_outcome, = [row["result"] for row in records(root)
                     if row["kind"] == "outcome" and row["result"]["operation"] == edit_operation]
    assert json.loads(edit_outcome["output"])["applied"]
    restarted.result = restarted.finish(signal.SIGTERM)
    assert restarted.result["exit"] == 143 and restarted.result["termios_restored"]
    return "normal"


def openrouter_orchestration(screen, root, cwd):
    until(screen, lambda frame: "deepseek/deepseek-v4.1-flash-1017" in frame
          and screen.vt.line(-4).startswith("  ›"), "pinned OpenRouter model")
    screen.send(b"\t\t")
    until(screen, lambda frame: "Orchestrate" in screen.vt.line(-2), "OpenRouter plan mode")
    held_child = root / "hold-orchestration-child"
    held_summary = root / "hold-orchestration-summary"
    held_child.touch()
    held_summary.touch()
    prompt = "summarize fixture " + ("details " * 70) + "END_OF_USER_REQUEST"
    screen.send(prompt + "\r")
    until(screen, lambda frame: "OPENROUTER_CARBON_PLAN" in frame and "Start" in frame, "validated strict plan")
    assert not (root / "home/.ansa/orchestration").exists()
    assert "Follow-up recommendation" in visible(screen)
    screen.send(b"\r")
    open_activity(screen)
    until(screen, lambda frame: "0/1 steps done" in frame, "read-only plan starts despite a post-report recommendation")
    close_activity(screen)
    assert "Work plan" not in body(screen) and "0/1 steps done" not in body(screen)
    local(screen, "/effort low")
    until(screen, lambda frame: "Wait for orchestration to finish before switching models" in frame,
          "the orchestration generation remains fixed while a helper runs")
    screen.send(b"\x7f" * len("/effort low"))
    screen.send("keep draft")
    until(screen, lambda frame: "keep draft" in screen.vt.line(-4), "draft retained while helper runs")
    assert "Draft · send after the task finishes" not in visible(screen)
    open_activity(screen)
    held_child.unlink()
    until(screen, lambda frame: "Coordinator is writing the summary" in frame
          and "Draft · send after the task finishes" in frame, "summary phase comes from coordinator state")
    close_activity(screen)
    assert "Coordinator is writing the summary" not in body(screen)
    held_summary.unlink()
    until(screen, lambda frame: "OPENROUTER_CARBON_DONE" in frame, "coordinator summary", timeout=15)
    assert "Approved plan" not in body(screen)
    assert "summarize fixture" in visible(screen) and "END_OF_USER_REQUEST" in visible(screen) and "keep draft" in screen.vt.line(-4)
    assert not any(row.get("text") == "keep draft" for row in records(root))
    open_activity(screen)
    until(screen, lambda frame: "Approved plan" in frame, "approved plan in Activity after completion")
    select_result(screen, "Approved plan")
    screen.send(b"\r")
    until(screen, lambda frame: "OPENROUTER_CARBON_PLAN" in frame, "read approved plan from its journal")
    escape(screen)
    until(screen, lambda _: activity_open(screen), "approved plan returns to Activity")
    close_activity(screen)
    screen.send(b"\x03")
    local(screen, "/exit")
    return "normal"


def configure_plus_roles(screen, pending=False):
    if not pending:
        until(screen, lambda _: ready(screen, "gpt-6.2-sol"), "SIWC adapter starts Carbon")
        local(screen, "/orchestrate+")
    until(screen, lambda frame: "Save role models" in frame, "all three role selectors")
    for role, model, model_offset, effort in [
        ("Strong", "gpt-6.2-sol", 0, "high"),
        ("Medium", "gpt-6.1-sol", 1, "medium"),
        ("Weak", "gpt-6.1-luna", 2, "low"),
    ]:
        screen.send(b"\r")
        until(screen, lambda frame: f"{role} · model" in frame, f"{role} model chooser")
        screen.send(b"\x1b[B" * model_offset + b"\r")
        until(screen, lambda frame: f"{role} · {model}" in frame and effort in frame,
              f"{role} model's supported effort")
        screen.send(b"\x1b[B\r")
        until(screen, lambda frame: "Save role models" in frame and f"{model} · {effort}" in frame,
              f"{role} selection")
    screen.send(b"\r")
    if not pending:
        until(screen, lambda frame: "Orchestrate+ role models saved" in frame, "durable role profile")


def siwc_orchestrate_plus(screen, root, cwd):
    def git(*args, path=cwd):
        return subprocess.check_output(["git", *args], cwd=path, text=True)

    report = cwd / "report.txt"
    report.write_text("before fixture\n")
    check = cwd / "check.sh"
    marker = root / "check-ran"
    check.write_text(f'#!/bin/sh\nset -eu\ngrep -q "^after fixture" report.txt\ncat report.txt >> "{marker}"\nprintf "fixture-check-output\\n"\n')
    check.chmod(0o700)
    git("init", "-q")
    git("add", "report.txt", "check.sh")
    git("-c", "user.name=Fixture", "-c", "user.email=fixture@localhost", "commit", "-qm", "fixture")
    original = git("rev-parse", "HEAD")
    configure_plus_roles(screen)
    local(screen, "/model gpt-6.1-sol")
    until(screen, lambda frame: "Default" in frame and "Enter apply" in frame, "default conversation effort")
    screen.send(b"\r")
    until(screen, lambda _: ready(screen, "gpt-6.1-sol"), "different conversation model active")
    prior = "PLUS_BASE_CONTEXT"
    screen.send(prior + "\r")
    until(screen, lambda frame: completed(root, prior) and "PLUS_CONTEXT_READY" in frame
          and "Ctrl+C stop" not in screen.vt.line(-2),
          "previous Work turn from a different model settled")
    prompt = "Exercise quick, fresh, and final review decisions"
    screen.send("/orchestrate+ " + prompt + "\r")
    until(screen, lambda frame: all(label in frame for label in ("PLUS_CARBON_PLAN", "Start", "Refine", "Cancel")),
          "approval shows plan and role models")
    frame = visible(screen)
    assert all(f"{model} · {effort}" in frame for model, effort in [
        ("gpt-6.2-sol", "high"), ("gpt-6.1-sol", "medium"), ("gpt-6.1-luna", "low")])
    assert "./check.sh" in frame and not marker.exists()
    ledgers = root / "home/.ansa/orchestration"
    assert not ledgers.exists()
    assert report.read_text() == "before fixture\n" and not marker.exists()
    assert len(journals(root)) == 1, "survey repair launched child sessions before approval"
    before_approval = [json.loads(line)["stage"] for line in (root / "plus-requests.jsonl").read_text().splitlines()]
    assert before_approval == ["prior-work", "survey-for-implementation", "strong-plan"], before_approval
    holds = {stage: root / f"hold-plus-{stage}"
             for stage in ("coordinate", "worker-rejected", "whole-review")}
    for hold in holds.values():
        hold.touch()

    def held(stage):
        trace = root / "plus-requests.jsonl"
        return f'"stage":"{stage}"' in trace.read_text() if trace.is_file() else False

    def running_role(detail, title):
        rows = visible(screen).splitlines()
        return any(detail in row and title in rows[index - 1] and "Running" in rows[index - 1]
                   for index, row in enumerate(rows) if index)

    screen.send(b"\r")
    until(screen, lambda _: held("coordinate") and body(screen).count(prompt) == 1,
          "repaired proposal leaves one visible user request in the main conversation")
    assert all(label not in body(screen) for label in ("Work plan", "0/2 steps done", "Coordinator · gpt-6.1-sol"))
    open_activity(screen)
    until(screen, lambda frame: all(label in frame for label in (
        "Proposed plan", "Work plan", "□ Set report to after fixture", "0/2 steps done",
        "Coordinator · gpt-6.1-sol", "Running · planning",
    )),
          "coordinator active and historical plan links loaded before the first step")
    assert body(screen).count("Proposed plan") == 1 and prompt not in body(screen)
    paint_quiet(screen)
    row = next(index for index, line in enumerate(visible(screen).splitlines())
               if "Coordinator · gpt-6.1-sol" in line)
    click(screen, screen.vt.line(row).index("Coordinator · gpt-6.1-sol"), row)
    until(screen, lambda frame: "Agent transcript" in frame and "Assignment" in frame,
          "clicking the model row in Activity opens the coordinator's transcript")
    escape(screen)
    until(screen, lambda frame: activity_open(screen) and "0/2 steps done" in frame,
          "Escape restores live Activity")
    select_result(screen, "Set report to after fixture")
    screen.send(b"\r")
    until(screen, lambda frame: "Agent transcript" in frame and "Assignment" in frame,
          "Enter opens the selected card in Activity")
    escape(screen)
    until(screen, lambda frame: activity_open(screen) and "0/2 steps done" in frame,
          "Escape restores the coordinator's live activity")
    row = next(index for index, line in enumerate(visible(screen).splitlines())
               if "0/2 steps done" in line)
    click(screen, screen.vt.line(row).index("0/2 steps done"), row)
    until(screen, lambda frame: "Conversation agents" in frame
          and running_role("Coordinator · planning · gpt-6.1-sol", "Set report to after fixture"),
          "clickable live activity opens the running coordinator")
    escape(screen)
    until(screen, lambda frame: activity_open(screen) and "Conversation agents" not in frame,
          "Escape returns from the roster to Activity")
    close_activity(screen)
    local(screen, "/agents")
    until(screen, lambda frame: "Conversation agents" in frame
          and running_role("Coordinator · planning · gpt-6.1-sol", "Set report to after fixture"),
          "/agents opens the same running coordinator")
    screen.send(b"\r")
    until(screen, lambda frame: "Agent transcript" in frame and "Assignment" in frame,
          "Enter opens the running coordinator's transcript")
    escape(screen)
    until(screen, lambda frame: "Conversation agents" in frame, "Escape returns to live roster")
    holds["coordinate"].unlink()

    worker = "Worker · gpt-6.1-luna"
    until(screen, lambda _: held("worker-rejected")
          and running_role(worker, "Set report to after fixture"),
          "already-open roster receives the newly created worker")
    row = next(index - 1 for index, line in enumerate(visible(screen).splitlines()) if worker in line)
    click(screen, screen.vt.line(row).index("Set report to after fixture"), row)
    until(screen, lambda frame: "Agent transcript" in frame and "Assignment" in frame,
          "mouse opens the running worker's transcript")
    escape(screen)
    until(screen, lambda frame: "Conversation agents" in frame, "Escape restores live roster")
    holds["worker-rejected"].unlink()

    until(screen, lambda frame: held("whole-review") and "Conversation agents" in frame
          and running_role("Strong reviewer · gpt-6.2-sol", "PLUS_CARBON_PLAN"),
          "already-open roster receives the running strong reviewer", timeout=35)
    escape(screen)
    if reading(screen):
        escape(screen)
    holds["whole-review"].unlink()
    def completed_plus_synthesis(frame):
        rows = records(root)
        turns = {event["turn"] for event in rows if event.get("handoff") is True}
        return ("PLUS_CARBON_DONE" in frame and "(response preview)" not in frame
                and "Ctrl+C stop" not in screen.vt.line(-2)
                and any(event["kind"] == "done" and event["turn"] in turns for event in rows))

    until(screen, completed_plus_synthesis, "durable Plus synthesis and idle composer", timeout=45)

    checkout, = ledgers.glob("*/*/checkout")
    assert git("show", "ansa-result:report.txt", path=checkout) == "after fixture reviewed\n"
    assert git("rev-parse", "HEAD") == original and report.read_text() == "before fixture\n"
    assert not git("status", "--short", "--untracked-files=no")
    assert marker.read_text() == "after fixture\nafter fixture reviewed\n"
    release = json.loads((checkout.parent / "release.json").read_text())
    assert release["candidate"] == git("rev-parse", "ansa-result", path=checkout).strip()
    assert git("diff", "--name-only", release["baseline"], release["candidate"], path=checkout) == "report.txt\n"
    local(screen, "/basic")
    until(screen, lambda _: ready(screen, "gpt-6.2-sol")
          and all("Orchestrate+" not in screen.vt.line(row) for row in (0, -2)),
          "return to Work in the same conversation")
    followup = "PLUS_WORK_FOLLOWUP"
    screen.send(followup + "\r")
    until(screen, lambda frame: completed(root, followup) and "PLUS_CONTINUITY_OK" in frame,
          "the strong model continues the original conversation")
    accepted = [row for row in records(root) if row["kind"] == "accepted"]
    first_draft, repair_draft = [row for row in accepted if row.get("draft")]
    assert first_draft["text"] == prompt
    summary_turn, = [row for row in accepted if row.get("handoff")]
    continued, = [row for row in accepted if row["text"] == followup]
    for row in (first_draft, repair_draft, summary_turn, continued):
        request = row["next"]
        assert request["binding"]["requested_model"] == "gpt-6.2-sol"
        assert request["effort"] == "high"
        assert any(item.get("content") == prior for item in request["input"])
    assert any("PLUS_CARBON_PLAN" in str(item.get("content")) for item in continued["next"]["input"]
               if item.get("role") == "assistant")
    trace = [json.loads(line)["stage"] for line in (root / "plus-requests.jsonl").read_text().splitlines()]
    assert trace == [
        "prior-work", "survey-for-implementation", "strong-plan", "coordinate", *["worker-rejected", "reject"] * 3,
        "escalate-quick", "quick-fix", "quick-verify", "coordinate",
        *["worker-rejected", "reject"] * 3, "escalate-fresh", "fresh-coordinate",
        "fresh-worker", "fresh-verify", "whole-review", "review-coordinate",
        "review-fix", "targeted-review", "summary", "later-work",
    ], trace
    reviewers = []
    for directory in (root / "home/.ansa/sessions").iterdir():
        if directory.is_dir() and not directory.is_symlink():
            rows = session_cases.decode({path.name: path.read_bytes() for path in directory.glob("part-*.log")})
            turns = [row["turn"] for row in rows if row["kind"] == "accepted" and
                     row["text"].startswith(("Perform the whole candidate's", "Recheck the preceding findings"))]
            if turns:
                reviewers.append(turns)
    assert reviewers == [["plus-review-0", "plus-review-1"]], reviewers
    local(screen, "/exit")
    return "normal"


def siwc_orchestrate_plus_blocked(screen, root, cwd, reject=False):
    (root / "orchestration-plus-blocked").touch()
    if reject:
        (root / "orchestration-plus-reject-summary").touch()
    configure_plus_roles(screen)
    screen.send(b"\t\t\t")
    until(screen, lambda _: "Orchestrate+" in screen.vt.line(-2), "orchestration mode")
    screen.send(b"Inspect Git status with the approved read scope\r")
    until(screen, lambda frame: "PLUS_BLOCKED_PLAN" in frame and "Start" in frame,
          "read-only plan approval")
    screen.send(b"\r")

    def handoff_event(kind):
        rows = records(root)
        turn = next((row["turn"] for row in rows if row.get("handoff") is True), None)
        return turn is not None and any(row["kind"] == kind and row.get("turn") == turn for row in rows)

    if reject:
        until(screen, lambda _: handoff_event("pause"), "failed synthesis pauses durably")
        screen.send(b"\x03")
        until(screen, lambda _: handoff_event("cancel"), "explicit cancel after synthesis failure")
    else:
        until(screen, lambda frame: handoff_event("done") and "PLUS_STOPPED" in frame,
              "parent answers why the approved run stopped")
    until(screen, lambda _: "Tab mode" in screen.vt.line(-2), "parent idle after stopped handoff")
    local(screen, "/basic")
    until(screen, lambda _: ready(screen, "gpt-6.2-sol")
          and "Orchestrate+" not in screen.vt.line(-2), "return to Work")
    followup = "PLUS_BLOCKED_FOLLOWUP"
    screen.send(followup + "\r")
    until(screen, lambda frame: completed(root, followup) and "PLUS_BLOCKED_CONTEXT_RETAINED" in frame,
          "normal Work turn receives the authoritative stop evidence")
    rows = records(root)
    handoff, = [row for row in rows if row.get("handoff") is True]
    evidence = json.loads(handoff["text"])
    assert evidence["orchestration_outcome"] == "stopped" and evidence["reported_status"] == "blocked"
    assert evidence["reason"] == "Approved read-only tools cannot run Git status"
    assert handoff["next"]["binding"]["requested_model"] == "gpt-6.2-sol"
    assert not any(row["kind"] == "outcome" for row in rows)
    run, = (root / "home/.ansa/orchestration").glob("*/*")
    assert not (run / "checkout").exists() and not (run / "release.json").exists()
    events = [json.loads(path.read_text())["body"]["event"] for path in sorted((run / "ledger").glob("event-*.json"))]
    stopped, = [event for event in events if event["event"] == "plus_stopped"]
    assert stopped["step"] == evidence["task_id"] == "status"
    assert stopped["phase"] == evidence["phase"] == "strong escalation"
    assert stopped["report"]["status"] == evidence["reported_status"]
    assert stopped["report"]["summary"] == evidence["reason"]
    receipt = run / "summary-admitted"
    assert receipt.read_text() == run.name
    if reject:
        # Simulate a lost delivery receipt. Resuming a stopped ledger must only
        # reconcile the original handoff, even after the user cancelled synthesis.
        before_journals = journals(root)
        receipt.unlink()
        local(screen, "/sessions")
        until(screen, lambda frame: "Resume orchestration" in frame and "Stopped at status" in frame,
              "stopped run is discoverable from its persisted ledger")
        screen.send(b"\r")
        until(screen, lambda _: receipt.exists(), "recover stopped handoff without restarting workers")
        assert receipt.read_text() == run.name and journals(root) == before_journals
    stages = [json.loads(line)["stage"] for line in (root / "plus-requests.jsonl").read_text().splitlines()]
    assert stages == [
        "strong-plan", "coordinate", *["worker-blocked", "verify-blocked"] * 3,
        "escalate-blocked", "stopped-summary", "later-work",
    ], stages
    local(screen, "/exit")
    return "normal"


def siwc_orchestrate_plus_implement(screen, root, cwd):
    (root / "orchestration-plus-implement").touch()
    report = cwd / "report.txt"
    report.write_text("before fixture\n")
    marker = root / "implementation-check-ran"
    check = cwd / "check.sh"
    check.write_text(f'#!/bin/sh\nset -eu\ngrep -q "^after fixture" report.txt\ntouch "{marker}"\n')
    check.chmod(0o700)
    until(screen, lambda _: ready(screen, "gpt-6.2-sol"), "SIWC adapter starts Carbon")
    local(screen, "/plan")
    until(screen, lambda _: "Plan" in screen.vt.line(-2), "start from read-only planning mode")
    prompt = "Change report.txt to after fixture and run the test\nInspect /files"
    screen.send("/orchestrate+ " + prompt + "\r")
    until(screen, lambda frame: "Save role models" in frame, "first use asks for role models")
    assert not any(row["kind"] == "accepted" for row in records(root))
    screen.send(b"\r")
    until(screen, lambda frame: "Strong · model" in frame, "open the model chooser")
    escape(screen)
    until(screen, lambda frame: "Save role models" in frame, "return to roles without losing the task")
    configure_plus_roles(screen, pending=True)
    until(screen, lambda frame: all(label in frame for label in (
        "MISLABELED_INVESTIGATION", "Start read only", "Implement",
    )), "the model mislabeled the requested change as an investigation")
    assert not (root / "home/.ansa/orchestration").exists()
    screen.send(b"\x1b[B\x1b[B")
    until(screen, lambda frame: "[Implement]" in frame, "Implement is selected")
    screen.send(b"\r")
    until(screen, lambda frame: all(label in frame for label in (
        "IMPLEMENT_SCOPED_CHANGE", "Change the fixture report", "report.txt", "./check.sh", "[Start]",
    )), "a scoped implementation with executable verification requires fresh approval")
    assert not (root / "home/.ansa/orchestration").exists()
    rows = records(root)
    drafts = [row for row in rows if row["kind"] == "accepted"]
    assert len(drafts) == 2 and all(row.get("draft") for row in drafts)
    assert drafts[0]["text"] == prompt and drafts[0]["mode"] == "basic"
    assert not any(row["kind"] in ("intent", "outcome") for row in rows)
    assert report.read_text() == "before fixture\n" and not marker.exists()
    stages = [json.loads(line)["stage"] for line in (root / "plus-requests.jsonl").read_text().splitlines()]
    assert stages == ["mislabeled-investigation", "implement-proposal"], stages
    escape(screen)
    until(screen, lambda frame: "[Start]" not in frame, "unapproved implementation is dismissed")
    local(screen, "/exit")
    return "normal"


def siwc_orchestration(screen, root, cwd):
    until(screen, lambda _: ready(screen, "gpt-6.2-sol"), "GPT ready without a network call")
    screen.send(b"\t\t")
    until(screen, lambda _: "Orchestrate" in screen.vt.line(-2), "GPT orchestration mode")
    for attempt, prompt in enumerate(("UNCONFIRMED_FORMAT", "UNCONFIRMED_FORMAT", "UNCONSTRAINED_FORMAT"), start=1):
        def paused_draft():
            rows = records(root)
            drafts = [row["turn"] for row in rows if row["kind"] == "accepted" and row.get("draft")]
            return len(drafts) == attempt and any(
                row["kind"] == "pause" and row["turn"] == drafts[-1]
                and row["diagnostic"] == "siwc_structured_format_unconfirmed"
                for row in rows
            )

        screen.send(prompt + "\r")
        until(screen, lambda frame: paused_draft()
              and "ChatGPT did not confirm the strict plan format" in frame
              and "Drafting a plan" not in frame
              and screen.vt.line(-4) == "  › " + prompt, "unconfirmed response rejected and draft restored")
        assert not (root / "home/.ansa/orchestration").exists()
        draft = [row for row in records(root) if row["kind"] == "accepted" and row["text"] == prompt][-1]
        until(screen, lambda _: [line.strip() for line in body(screen).splitlines()].count("> " + prompt) == 1,
              "one chat message even after a repeated planning attempt")
        assert not any(row["kind"] == "done" and row["turn"] == draft["turn"] for row in records(root))
        screen.send(b"\x03")
        until(screen, lambda _: screen.vt.line(-4) == "  ›", "clear failed draft")
        if attempt == 1:
            local(screen, "/sessions")
            until(screen, lambda _: "Resume current request" in popup_text(screen), "paused draft can be resumed")
            screen.send(b"\r")
            until(screen, lambda frame: sum(row["kind"] == "pause" and row["turn"] == draft["turn"]
                                            for row in records(root)) == 2
                  and "ChatGPT did not confirm the strict plan format" in frame
                  and "Drafting a plan" not in frame, "resumed draft can pause again without hiding the error")
        local(screen, "/cancel")
        until(screen, lambda _: "cancelled" in screen.vt.line(-2)
              and any(row["kind"] == "cancel" and row["turn"] == draft["turn"]
                      for row in records(root)), "cancel paused unconfirmed draft")
    screen.send(b"UNNAMED_READ_SCOPE\r")
    until(screen, lambda frame: "Name the read folder in your request" in frame
          and screen.vt.line(-4) == "  › UNNAMED_READ_SCOPE", "the model cannot select an unrelated folder")
    assert completed(root, "UNNAMED_READ_SCOPE"), "strict response is retained as an unapproved draft"
    screen.send(b"\x03")
    until(screen, lambda _: screen.vt.line(-4) == "  ›", "clear the unnamed folder request")
    screen.send(b"/orchestrate Inspect Workspace/pet\r")
    until(screen, lambda frame: "Read folder" in frame and "SIWC_CARBON_PLAN" in frame and "Start" in frame,
          "the read folder and plan are visible before approval")
    selected = str(root / "home/Workspace/pet")
    assert selected in visible(screen), "the approval screen must show the canonical read folder"
    assert not (root / "home/.ansa/orchestration").exists()
    screen.send(b"\r")
    until(screen, lambda frame: "SIWC_CARBON_DONE" in frame and ready(screen, "gpt-6.2-sol"),
          "strict GPT helpers ran their tools and the coordinator answered")
    assert "SIWC_HELPER_DONE" not in body(screen), "helper result leaked into the conversation"
    all_records = records(root)
    assert sum(row["kind"] == "outcome" for row in all_records) == 3
    assert len(list((root / "home/.ansa/sessions").iterdir())) == 3
    assert [row["configuration"]["epoch"]["exact_state"]["settings"]["cwd"]
            for row in all_records if row["kind"] == "init" and "final_format" in row["configuration"]] == [selected, selected]

    handoff, = (row["turn"] for row in records(root) if row.get("handoff") is True)
    until(screen, lambda _: any(row["kind"] == "done" and row["turn"] == handoff for row in records(root))
          and "Tab mode" in screen.vt.line(-2), "coordinator is idle before lock test")
    paint_quiet(screen)

    timed_out = "SYNTHETIC_PLAN_TIMEOUT"
    screen.send(timed_out + "\r")
    until(screen, lambda frame: "Plan paused" in body(screen) and "http_timeout" in frame
          and screen.vt.line(-4) == "  › " + timed_out, "a transport timeout is visible next to the request")
    timeout_turn, = (row["turn"] for row in records(root)
                     if row["kind"] == "accepted" and row["text"] == timed_out)
    assert [(row["kind"], row.get("diagnostic")) for row in records(root)
            if row.get("turn") == timeout_turn] == [("accepted", None), ("pause", "http_timeout")]
    screen.send(b"\x13\x03")
    until(screen, lambda _: screen.vt.line(-4) == "  ›", "Ctrl+S cannot freeze the paused composer")
    screen.send(b"AFTER_TIMEOUT\t")
    until(screen, lambda _: "Orchestrate+" in screen.vt.line(-2)
          and screen.vt.line(-4) == "  › AFTER_TIMEOUT", "Tab after a timeout preserves typed text")
    screen.send(b"\r")
    until(screen, lambda _: screen.vt.line(-4) == "  › AFTER_TIMEOUT"
          and "Plan paused" in body(screen), "Enter preserves a new draft while the prior plan can resume")
    assert not any(row.get("text") == "AFTER_TIMEOUT" for row in records(root))
    screen.send(b"\x03")
    until(screen, lambda _: screen.vt.line(-4) == "  ›", "clear the composer before a local command")
    local(screen, "/cancel")
    until(screen, lambda _: "cancelled" in screen.vt.line(-2)
          and any(row["kind"] == "cancel" and row["turn"] == timeout_turn
                  for row in records(root)), "discard the timed out plan explicitly")
    local(screen, "/basic")
    until(screen, lambda _: "Orchestrate" not in screen.vt.line(-2), "switch to work after timeout")
    screen.send(b"AFTER_TIMEOUT\r")
    until(screen, lambda frame: completed(root, "AFTER_TIMEOUT") and "SIWC_CARBON_DONE" in frame,
          "the next request is sent successfully")
    local(screen, "/orchestrate")
    until(screen, lambda _: "Orchestrate" in screen.vt.line(-2) and "Tab mode" in screen.vt.line(-2),
          "orchestration is idle before holding the auth lock")

    with (root / "home/.ansa/auth/lock").open("rb") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        screen.send(b"Cancel this draft\r")
        until(screen, lambda _: "Drafting a plan" in screen.vt.line(-1),
              "plan waits in the footer for the shared account lock")
        until(screen, lambda _: "Drafting a plan" in body(screen) and screen.vt.line(-4) == "  ›",
              "planning status is also visible below the request")
        screen.send(b"\x03")
        until(screen, lambda frame: "Drafting a plan" not in frame
              and screen.vt.line(-4) == "  › Cancel this draft", "cancel keeps the pending draft")
        def cancelled_draft():
            rows = records(root)
            request = next((row["turn"] for row in rows if row["kind"] == "accepted"
                            and row["text"] == "Cancel this draft"), None)
            return request and any(row["kind"] == "cancel" and row["turn"] == request for row in rows)

        until(screen, lambda _: cancelled_draft(), "the exact pending draft was cancelled durably")
        screen.send(b"\x03")
        until(screen, lambda _: screen.vt.line(-4) == "  ›", "clear the pending draft")
        before_login = journals(root)
        local(screen, "/login")
        until(screen, lambda _: "Continue in your browser" in popup_text(screen), "in-chat sign-in starts")
        screen.send(b"\x03")
        until(screen, lambda frame: "ChatGPT sign-in" not in frame, "Ctrl+C cancels sign-in")
        assert journals(root) == before_login
        local(screen, "/exit")
        assert screen.finish()["exit"] == 0, "exit waited for the locked account after cancelling"

    # Remove only the seeded fixture token, then use the actual startup command
    # with a synthetic provider that rejects OAuth discovery before opening a browser.
    (root / "home/.ansa/auth/credentials.json").unlink()
    before = journals(root)
    startup = Pty(screen.child.args[0], root, root / "home", cwd, "siwc-carbon", True)
    try:
        startup.wait("Enter /login to open your browser, or /exit to quit.")
        startup.send("/login\r")
        startup.wait("ChatGPT sign-in failed (fixture_unexpected_exchange)")
        startup.send("/exit\r")
        result = startup.finish()
        assert result["exit"] == 0 and result["termios_restored"] and result["flags_restored"]
        assert journals(root) == before, "startup sign-in did not create an empty conversation"
    finally:
        startup.close()
    return "normal"


def matugen_palette(screen, root, cwd):
    prefs = cwd.parent / ".ansa" / "ui.json"
    import struct
    import zlib

    def chunk(name, payload):
        return struct.pack(">I", len(payload)) + name + payload + struct.pack(">I", zlib.crc32(name + payload))

    (cwd / "palette.png").write_bytes(
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", 16, 16, 8, 2, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress((b"\0" + b"\x26\x7e\xb9" * 16) * 16))
        + chunk(b"IEND", b"")
    )

    def preview(source):
        local(screen, "/theme")
        until(screen, lambda _: "Generate with Matugen" in popup_text(screen), "Matugen in theme picker")
        screen.send(b"\x1b[B" * 12 + b"\r")
        until(screen, lambda _: "Image path or HEX color" in popup_text(screen), "Matugen source")
        screen.send(source + "\r")
        until(screen, lambda _: "Expressive" in popup_text(screen), "Matugen schemes")
        screen.send(b"\x1b[B" * 3 + b"\r")
        until(screen, lambda _: "Contrast from -100" in popup_text(screen), "Matugen contrast")
        screen.send(b"25\r")
        until(screen, lambda _: "Palette preview" in popup_text(screen), "Generated palette")

    preview("#c67ca9")
    assert not prefs.exists()
    escape(screen)
    assert not prefs.exists()
    preview("palette.png")
    screen.send(b"\r")
    until(screen, lambda frame: "Theme Matugen saved" in frame, "Saved Matugen palette")
    saved = json.loads(prefs.read_text())
    assert saved["theme"] == "matugen"
    assert saved["generated"]["colors"]["accent"].startswith("#")
    local(screen, "/exit")
    return "normal"


def active_plan_clarification(screen, root, cwd):
    (cwd / "first.txt").write_text("first\n")
    (cwd / "second.txt").write_text("second\n")
    screen.send(b"\t\t")
    until(screen, lambda frame: "Orchestrate" in screen.vt.line(-2), "orchestration mode")
    screen.send("ORCHESTRATION_CLARIFICATION_FIXTURE\r")
    until(screen, lambda frame: "Start" in frame and "Refine" in frame, "clarification plan")
    screen.send(b"\r")
    ledgers = root / "home/.ansa/orchestration"
    def events():
        return [json.loads(path.read_text())["body"]["event"] for path in sorted(ledgers.glob("*/*/ledger/event-*.json"))]

    until(screen, lambda _: any(event.get("event") == "post" and event["step"] == "first" for event in events()), "first step running")
    clarification = "Focus the comparison on the first line"
    screen.send(clarification + "\r")
    open_activity(screen)
    until(screen, lambda frame: "Coordinator receives the clarification" in frame, "clarification status in Activity")
    close_activity(screen)
    until(screen, lambda frame: "Оба помощника завершили проверку" in frame, "parent summary")
    assert "Coordinator receives the clarification" not in body(screen) and "Work plan" not in body(screen)
    records = events()
    accepted, = [i for i, event in enumerate(records) if event.get("event") == "clarification" and event["text"] == clarification]
    finished, = [i for i, event in enumerate(records) if event.get("event") == "finished" and event["step"] == "first"]
    decided, = [i for i, event in enumerate(records) if event.get("event") == "coordinator_decision"]
    second, = [i for i, event in enumerate(records) if event.get("event") == "bound" and event["step"] == "second"]
    assert accepted < finished < decided < second
    local(screen, "/exit")
    return "normal"


def main():
    cases = [("input-copy-resize", input_and_copy), ("mouse-copy-resize", mouse_copy),
             ("mouse-selection-scroll", selection_scroll),
             ("fork-btw", branches), ("files-terminal-persistence", files_and_terminal),
             ("mouse-history-send-child-escape", mouse_history_send_and_child_escape),
             ("split-source-scroll", split_source_scroll),
             ("markdown-source-preview", markdown_source_preview),
             ("inline-tool-details", inline_tool_details),
             ("slash-completion-help", completion_and_help),
             ("new-resume-interrupt", new_resume_and_exit),
             ("native-terminal-panel", native_terminal_panel),
             ("model-effort-pair", model_effort_pair),
             ("matugen-palette", matugen_palette),
             ("plan-agent-inherits", plan_agent_inherits),
             ("session-agent-titles-restart", session_agent_titles_restart),
             ("orchestration-candidate", orchestration_candidate),
             ("orchestration-review-resume", orchestration_review_resume),
             ("orchestration-review-resume-exhausted", lambda *a: orchestration_review_resume(*a, True)),
             ("active-plan-clarification", active_plan_clarification),
             ("terminal-interrupt-app-escape", terminal_interrupt_and_app_escape),
             ("terminal-eof-exit", terminal_eof_and_exit),
             ("outer-eof-cleans-terminal", outer_eof_cleans_terminal),
             ("editor-success", lambda *a: editor_case(*a, False)),
             ("editor-cancel", lambda *a: editor_case(*a, True)),
             ("editor-early-exit", editor_early_exit),
             ("editor-stopped", lambda *a: editor_early_exit(*a, True)),
             ("editor-missing", editor_missing), ("normal-exit", normal_exit),
             ("openrouter-orchestration", openrouter_orchestration),
             ("siwc-orchestration", siwc_orchestration),
             ("siwc-orchestrate-plus", siwc_orchestrate_plus),
             ("siwc-orchestrate-plus-implement", siwc_orchestrate_plus_implement),
             ("siwc-orchestrate-plus-blocked", siwc_orchestrate_plus_blocked),
             ("siwc-orchestrate-plus-blocked-rejected", lambda *args: siwc_orchestrate_plus_blocked(*args, reject=True))]
    transport_cases = {"openrouter-orchestration", "siwc-orchestration",
                       *(name for name, _ in cases if name.startswith("siwc-orchestrate-plus"))}
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--case", action="append", choices=[name for name, _ in cases], dest="selected_cases")
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=False)
    if args.selected_cases:
        cases = [(name, case) for name, case in cases if name in args.selected_cases]
    else:
        cases = [(name, case) for name, case in cases if name not in transport_cases]
    results = []
    for name, case in cases:
        started = time.monotonic()
        with tempfile.TemporaryDirectory(prefix="ansa-m4-", dir="/tmp") as directory:
            root = Path(directory)
            home, cwd = roots(root)
            (home / ".bashrc").write_text("PS1='fixture> '\nHISTFILE=/dev/null\n")
            editor = cwd / "fixture-editor"
            editor.write_text('#!/bin/sh\n[ "$1" = "--" ] && shift\nprintf "\\033[?1049h\\033[?25l\\033[31mEDITOR_READY\\n"\nread answer\nprintf "edited\\n" >> "$1"\n')
            if name == "editor-early-exit":
                editor.write_text('#!/bin/sh\nsleep 30 &\nprintf "%s\\n" "$!" > editor-child.pid\nprintf "early exit contents\\n" > "$2"\nexit 0\n')
            elif name == "editor-stopped":
                editor.write_text('#!/bin/sh\nkill -STOP $$\nprintf resumed > after-stop\n')
            editor.chmod(0o700)
            editor_path = cwd / "does-not-exist" if name == "editor-missing" else editor
            if name in transport_cases:
                home.chmod(0o700)
                cwd.chmod(0o700)
                if name.startswith("siwc-"):
                    seed_siwc(home)
                if name == "siwc-orchestration":
                    nested = home / "Workspace/pet/ansa"
                    nested.mkdir(parents=True)
                    (nested / "AGENTS.md").write_text("Follow the fixture instructions.\n" * 600 + "END_NESTED_INSTRUCTIONS\n")
                if name.startswith("siwc-orchestrate-plus"):
                    (root / "orchestration-plus").touch()
                adapter = "openrouter-carbon" if name.startswith("openrouter-") else "siwc-carbon"
                size = (110, 44) if name.startswith("siwc-orchestrate-plus") else (100, 32)
                screen = Pty(str(args.binary.resolve()), root, home, cwd, "", True, size=size,
                             argv=["--frontend-fixture", str(root), adapter])
                screen.vt = Terminal(*size)
            else:
                if name.startswith("orchestration-review-resume"):
                    directory = home / ".ansa"
                    directory.mkdir(mode=0o700, exist_ok=True)
                    config = directory / "config.json"
                    rounds = 1 if name.endswith("-exhausted") else 2
                    config.write_text(json.dumps({"schema_version": 1, "tool_rounds": rounds, "tool_calls": 4}))
                    config.chmod(0o600)
                screen = start(args.binary.resolve(), root, home, cwd, {"EDITOR": str(editor_path)})
            try:
                normal = case(screen, root, cwd) == "normal"
                result = screen.finish(None if normal else signal.SIGTERM)
                assert result["exit"] == (0 if normal else 143), result["exit"]
                assert result["termios_restored"] and result["flags_restored"], "terminal restoration"
                results.append({"case": name, "passed": True, "exit": result["exit"],
                                "termios_restored": result["termios_restored"], "flags_restored": result["flags_restored"],
                                "duration": time.monotonic() - started})
            except Exception as error:
                results.append({"case": name, "passed": False, "error": repr(error), "duration": time.monotonic() - started})
                if name == "siwc-orchestrate-plus":
                    store = home / ".ansa/orchestration"
                    events = {str(path.relative_to(store)): json.loads(path.read_text())
                              for path in sorted(store.glob("**/event-*.json"))}
                    marker = root / "check-ran"
                    diagnostic = {
                        "ledger_events": events,
                        "check_runs": marker.read_text().splitlines() if marker.is_file() else [],
                    }
                    try:
                        diagnostic["pauses"] = [row for row in records(root) if row["kind"] == "pause"]
                    except (AssertionError, OSError, ValueError) as capture_error:
                        diagnostic["pause_capture_error"] = repr(capture_error)
                    (args.output / (name + ".failure.json")).write_text(
                        json.dumps(diagnostic, ensure_ascii=False, indent=2) + "\n")
                raise
            finally:
                (args.output / (name + ".pty")).write_bytes(screen.raw)
                (args.output / (name + ".journals.json")).write_text(json.dumps(journals(root)) + "\n")
                (args.output / (name + ".screen.txt")).write_text(visible(screen))
                if (trace := root / "plus-requests.jsonl").is_file():
                    (args.output / (name + ".requests.jsonl")).write_bytes(trace.read_bytes())
                (args.output / (name + ".observations.json")).write_text(json.dumps(screen.observations, ensure_ascii=False, indent=2) + "\n")
                (args.output / "report.json").write_text(json.dumps(results, ensure_ascii=False, indent=2) + "\n")
                screen.close()
                for label, extra in getattr(screen, "additional_screens", []):
                    (args.output / (name + "-" + label + ".pty")).write_bytes(extra.raw)
                    (args.output / (name + "-" + label + ".screen.txt")).write_text(visible(extra))
                    (args.output / (name + "-" + label + ".terminal.json")).write_text(json.dumps(getattr(extra, "result", None)) + "\n")
                    extra.close()
    print(json.dumps({"passed": len(results), "cases": [result["case"] for result in results]}))


if __name__ == "__main__":
    main()
