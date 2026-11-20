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


def select_result(screen, label):
    paint_quiet(screen)
    screen.send(b"\x07")
    for _ in range(screen.vt.rows):
        paint_quiet(screen)
        if any(line.startswith(f"› {label}") for line in visible(screen).splitlines()):
            return
        screen.send(b"\x1b[B")
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
    return model in screen.vt.line(-2) and screen.vt.line(-4).startswith("›")


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


def open_helper(screen):
    local(screen, "/agents")
    until(screen, lambda frame: "Conversation agents" in frame and helper_title(frame), "helper roster")
    screen.send(b"\r")
    until(screen, lambda frame: helper_title(screen.vt.line(1)) and ready(screen), "open helper archive")


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


def seed_history(binary, root, cwd):
    session = None
    for index in range(20):
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
    until(screen, lambda frame: screen.vt.line(-4) == f"› {draft}", "history draft")
    before = journals(root)
    escape(screen)
    until(screen, lambda frame: not reading(screen) and "HISTORY_SEED_19" in body(screen), "root Esc stays at latest")
    escape(screen)
    assert not reading(screen) and screen.vt.line(-4) == f"› {draft}"
    for _ in range(180):
        mouse(screen, 64, 2, 5)
        for _ in range(3): screen.drain(.01)
        if "> HISTORY_FIRST" in body(screen).splitlines(): break
    else:
        raise AssertionError("mouse cannot reach the first canonical message")
    assert reading(screen) and screen.vt.line(-4) == f"› {draft}"
    anchor = body(screen)
    screen.send(b"\x1bg\x1bG\x1bk\x1bj\x1b\xd0\x96")
    for _ in range(8): screen.drain(.01)
    assert reading(screen) and body(screen) == anchor and screen.vt.line(-4) == f"› {draft}", "Alt keys changed conversation or reading anchor"
    screen.send(" TYPED_WHILE_READING")
    until(screen, lambda frame: screen.vt.line(-4).endswith("TYPED_WHILE_READING"), "typing while reading")
    submitted = draft + " TYPED_WHILE_READING"
    assert body(screen) == anchor and journals(root) == before, "typing moved reading anchor or dispatched a turn"
    screen.send(b"\r")
    until(screen, lambda frame: not reading(screen) and "TYPED_WHILE_READING" in body(screen), "Enter returned to latest and showed the submitted message")
    until(screen, lambda frame: completed(root, submitted) and ready(screen), "send while reading completed")
    assert sum(row["kind"] == "accepted" and row["text"] == submitted for row in records(root)) == 1
    approve_helpers(screen, "Запусти субагента: CHILD_HISTORY_TASK")
    open_helper(screen)
    screen.send("CHILD_DRAFT Ж")
    until(screen, lambda frame: screen.vt.line(-4) == "› CHILD_DRAFT Ж", "child draft")
    paint_quiet(screen)
    until(screen, lambda frame: ready(screen) and helper_title(screen.vt.line(1)),
          "completed child frame before Ctrl+C")
    screen.send(b"\x03")
    until(screen, lambda frame: screen.vt.line(-4) == "›", "Ctrl+C cleared the child draft")
    screen.send("CHILD_DRAFT Ж")
    until(screen, lambda frame: screen.vt.line(-4) == "› CHILD_DRAFT Ж", "new child draft")
    mouse(screen, 64, 2, 5)
    screen.send(b"\x1bg")
    for _ in range(8): screen.drain(.01)
    assert helper_title(screen.vt.line(1)), "mouse or Alt navigated out of child"
    assert screen.vt.line(-4) == "› CHILD_DRAFT Ж"
    escape(screen)
    until(screen, lambda frame: not helper_title(screen.vt.line(1)) and ready(screen), "child Esc returns parent")


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
    until(screen, lambda frame: screen.vt.line(-4) == "› FOCUS_DRAFT Ж界😀 jkgG", "chat navigation letters remain literal")
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
    until(screen, lambda frame: completed(root, "TOOLS_TASK") and ready(screen) and "get_test_value" in body(screen), "compact completed tool")
    compact = body(screen)
    headers = [line for line in compact.splitlines() if re.match(r"[>v] get_test_value\b", line)]
    assert len(headers) == 1 and "done" in headers[0], "tool lifecycle was split into duplicate rows"
    assert "prepared" not in compact and "KnownResult" not in compact and "Arguments" not in compact
    screen.send("TOOLS_DRAFT Ж")
    until(screen, lambda frame: screen.vt.line(-4) == "› TOOLS_DRAFT Ж", "tool draft")
    row = next(row for row in range(3, screen.vt.rows - 5) if screen.vt.line(row).startswith("> get_test_value"))
    click(screen, 0, row)
    until(screen, lambda frame: "Arguments" in body(screen) and "Output" in body(screen), "tool row opened inline")
    expanded = body(screen)
    assert "value: OMH_FIXED_VALUE_42" in expanded and "ok: true" in expanded
    assert '{"ok"' not in expanded and "KnownResult" not in expanded, "details exposed the raw protocol instead of readable values"
    assert [row["invocation"]["arguments"] for row in records(root) if row["kind"] == "intent"] == [{}], "display projection changed the invocation arguments"
    assert len([line for line in expanded.splitlines() if re.match(r"[>v] get_test_value\b", line)]) == 1
    assert screen.vt.line(-4) == "› TOOLS_DRAFT Ж"
    row = next(row for row in range(3, screen.vt.rows - 5) if screen.vt.line(row).startswith("v get_test_value"))
    click(screen, 0, row)
    until(screen, lambda frame: "Arguments" not in body(screen) and "> get_test_value" in body(screen), "tool row collapsed")

    # Dragging a tool row selects text rather than activating its click target.
    paint_quiet(screen)
    row = next(row for row in range(3, screen.vt.rows - 5) if screen.vt.line(row).startswith("> get_test_value"))
    at = len(screen.raw)
    mouse(screen, 0, 2, row)
    mouse(screen, 32, 2 + len("get_test_value") - 1, row)
    mouse(screen, 0, 2 + len("get_test_value") - 1, row, release=True)
    assert copied(screen, at) == "get_test_value", "tool-row drag did not copy the displayed text"
    escape(screen)
    screen.send(b"\x14")
    until(screen, lambda frame: "Arguments" in body(screen) and "Output" in body(screen), "Ctrl+T opened tool details")
    assert screen.vt.line(-4) == "› TOOLS_DRAFT Ж"
    before = journals(root)
    row = next(row for row in range(3, screen.vt.rows - 5) if "Full output" in screen.vt.line(row))
    click(screen, screen.vt.line(row).index("Full output"), row)
    until(screen, lambda frame: "value: OMH_FIXED_VALUE_42" in frame and "j/k lines" in frame, "complete readable output inspector")
    assert screen.vt.line(-4) == "› TOOLS_DRAFT Ж"
    output_before = body(screen)
    screen.send("Ж界😀")
    screen.send(b"\x1b[200~" + "Паста Ж界😀\n/exit".encode() + b"\x1b[201~")
    screen.send(b"\x7f\x08\x1b[3~")
    for _ in range(8): screen.drain(.01)
    assert screen.vt.line(-4) == "› TOOLS_DRAFT Ж", "read-only output accepted Unicode, paste or deletion into the draft"
    assert body(screen) == output_before and journals(root) == before, "read-only output input changed the view or canonical journal"
    escape(screen)
    until(screen, lambda frame: "j/k lines" not in frame and "get_test_value" in frame, "output inspector returned to chat")
    assert journals(root) == before and screen.vt.line(-4) == "› TOOLS_DRAFT Ж"


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
    row = next(row for row in range(3, screen.vt.rows - 5) if screen.vt.line(row).startswith("> MOUSE_COPY"))
    before = journals(root)
    at = len(screen.raw)
    mouse(screen, 0, 2, row)
    mouse(screen, 32, 2 + len("MOUSE_COPY") - 1, row)
    screen.wait(b"\x1b[7mM", at)
    before_resize = len(screen.raw)
    screen.resize(80, 20)
    screen.vt = Terminal(80, 20)
    screen.wait(b"\x1b[7mMOUSE_COPY", before_resize)
    until(screen, lambda frame: model in screen.vt.line(-2) and "Message agent" not in screen.vt.line(-2), "compact status after resize")
    mouse(screen, 0, 2 + len("MOUSE_COPY") - 1, row, release=True)
    text = copied(screen, at)
    assert text == "MOUSE_COPY" and "\x1b" not in text, repr(text)
    assert journals(root) == before, "selection or resize changed the canonical conversation"
    until(screen, lambda frame: screen.vt.line(-2).split().count(model) == 1 and model not in screen.vt.line(0),
          "one model in the narrow footer")
    for _ in range(2):
        paint_quiet(screen)
        row = next(row for row in range(3, screen.vt.rows - 5) if screen.vt.line(row).startswith("> MOUSE_COPY"))
        at = len(screen.raw)
        mouse(screen, 0, 2, row)
        mouse(screen, 32, 2 + len("MOUSE_COPY") - 1, row)
        mouse(screen, 0, 2 + len("MOUSE_COPY") - 1, row, release=True)
        assert copied(screen, at) == "MOUSE_COPY"
    assert journals(root) == before, "copies changed the canonical conversation"
    screen.send("AFTER_SELECT Ж\r")
    until(screen, lambda frame: not reading(screen) and "> AFTER_SELECT Ж" in body(screen), "Enter sent after repeated drag copies")
    until(screen, lambda frame: completed(root, "AFTER_SELECT Ж") and ready(screen), "post-selection message completed")
    assert sum(row["kind"] == "accepted" and row["text"] == "AFTER_SELECT Ж" for row in records(root)) == 1


def selection_scroll(screen, root, cwd):
    (cwd / "panel").mkdir()
    (cwd / "panel" / "notes.txt").write_text("INSPECTOR_NEVER_COPY\n")
    session = seed_history(screen.child.args[0], root, cwd)
    local(screen, f"/sessions {session}")
    until(screen, lambda frame: "HISTORY_SEED_19" in frame and ready(screen), "selectable canonical tail")
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
    until(screen, lambda frame: "INSPECTOR_NEVER_COPY" in frame and "HISTORY_SEED_19" in frame, "split pane for selection")
    paint_quiet(screen)
    row = next(row for row in range(3, 27) if "HISTORY_SEED_19" in screen.vt.line(row))
    first = body(screen)
    at = len(screen.raw)
    mouse(screen, 0, len("> HISTORY_SEED_19") - 1, row)
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
    mouse(screen, 0, 2, row, release=True)
    text = copied(screen, at)
    assert "HISTORY_FIRST" in text and "HISTORY_SEED_19" in text, text[-500:]
    assert "INSPECTOR_NEVER_COPY" not in text and "Files" not in text and "\x1b" not in text
    assert screen.raw[at:].count(b"\x1b]52;c;") == 1


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
    local(screen, "/btw side-marker")
    until(screen, lambda frame: "side-marker" in screen.vt.line(1) and completed(root, "side-marker"), "side conversation")
    escape(screen)
    until(screen, lambda frame: "side-marker" not in screen.vt.line(1), "side parent return")
    assert "side-marker" not in visible(screen)
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
    screen.wait(b"\x1b[?1049l\x1b[?25h\x1b[0m", since=resumed)
    until(screen, lambda frame: "ANSA" in frame and "read-only" in frame, "editor terminal restored")
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
    local(screen, "/files notes.md")
    until(screen, lambda frame: "read-only" in frame, "early editor source")
    at = len(screen.raw)
    screen.send("e")
    screen.wait(b"\x1b[?1002h", since=at)
    until(screen, lambda frame: "read-only" in frame and "ANSA" in frame, "early editor return")
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
    at = len(screen.raw)
    screen.send("e")
    screen.wait(b"\x1b[?1002h", since=at)
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
    until(screen, lambda frame: screen.vt.line(-4) == "› HIDDEN_SAFE", "Tab at tiny size keeps input in composer")
    assert journals(root) == before, "hidden terminal input entered the model journal"
    screen.send(b"\x15")
    escape(screen)
    until(screen, lambda frame: "q terminal" in frame and "Esc files" not in frame, "prompt Escape returns to Files")
    escape(screen)
    until(screen, lambda frame: "q terminal" not in frame, "Files Escape returns to chat")
    screen.send("SURVIVED_TERMINAL")
    until(screen, lambda frame: screen.vt.line(-4) == "› SURVIVED_TERMINAL", "composer after foreground interrupt")
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
    until(screen, lambda frame: screen.vt.line(-4) == "› AFTER_SHELL_EOF", "shell EOF did not close or corrupt composer")
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
    until(screen, lambda frame: "Agent terminal" in frame and "The agent has not opened a terminal yet" in frame,
          "managed terminal panel in the normal Carbon entry point")
    session, = catalog.glob("*/terminal-host-v1/terminal-v1.json")
    escape(screen)
    until(screen, lambda frame: "Agent terminal" not in frame, "Esc returned to the chat")
    screen.send("TERMINAL_DEMO\r")
    until(screen, lambda frame: completed(root, "TERMINAL_DEMO"), "offline terminal tool created a PTY")
    local(screen, "/terminals")
    until(screen, lambda frame: "Demo terminal" in frame and "ANSA_TERMINAL_READY" in body(screen),
          "Carbon renders live PTY output")
    escape(screen)
    until(screen, lambda frame: "ANSA_TERMINAL_READY" not in body(screen), "Esc returned to the chat")
    local(screen, "/exit")
    result = screen.finish()
    assert result["exit"] == 0 and result["termios_restored"]
    assert session.with_name("closed-v1.json").is_file(), "clean host closed with a durable seal"
    resumed = start(screen.child.args[0], root, root / "home", cwd, argv_extra=["--session", session.parents[1].name])
    screen.additional_screens.append(("resumed", resumed))
    local(resumed, "/plan")
    resumed.send("SEALED_PLAN\r")
    until(resumed, lambda frame: completed(root, "SEALED_PLAN"), "Plan admitted after a clean restart")
    local(resumed, "/plan")
    resumed.send("TERMINAL_DEMO\r")
    restarted_host = session.parents[1] / "terminal-host-v1-2"
    until(resumed, lambda frame: (restarted_host / "0001.json").is_file(),
          "reopened conversation can start a fresh shell")
    local(resumed, "/terminals")
    until(resumed, lambda frame: "ANSA_TERMINAL_READY" in body(resumed), "reopened shell is live")
    escape(resumed)
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
    until(screen, lambda frame: screen.vt.line(-4) == "› /model" and "Applies to future messages" not in frame, "Tab completes without dispatch")
    assert journals(root) == before
    screen.send(b"\r")
    until(screen, lambda frame: "Applies to future messages" in frame, "Enter opens the completed command")
    escape(screen)
    until(screen, lambda frame: "Applies to future messages" not in frame, "model menu closed before typing another prefix")
    screen.send("/")
    until(screen, lambda frame: "/model" in body(screen) and "/effort" in body(screen), "all command suggestions")
    screen.send(b"\x1b[B\x1b[C")
    until(screen, lambda frame: screen.vt.line(-4) == "› /effort" and "Default" not in frame, "arrows choose and Right inserts only")
    assert journals(root) == before
    screen.send(b"\x15")
    screen.send(b"/exit" + b"\x1b[D" * 4 + b"\r")
    until(screen, lambda frame: screen.vt.line(-4) == "› /model" and "Applies to future messages" not in frame,
          "Enter at a partial token inserts the selected command")
    assert journals(root) == before
    screen.send(b"\x15")
    screen.send("Проверь /mo потом")
    screen.send(b"\x1b[D" * len(" потом"))
    until(screen, lambda frame: "/model" in body(screen) and "Tab/→ complete" in frame, "completion at a middle word boundary")
    screen.send(b"\t")
    until(screen, lambda frame: screen.vt.line(-4) == "› Проверь /model потом", "middle completion preserves prefix and suffix")
    assert journals(root) == before
    screen.send(b"\x05\x15")
    screen.send("?")
    until(screen, lambda frame: "ANSA commands" in frame, "empty composer question mark opens help")
    command_rows = [match for row in range(screen.vt.rows)
                    if (match := re.match(r"^\s*│(?:› |  )(/[a-z]+)\s+(.*?)\s*│", screen.vt.line(row)))]
    labels = [row[1] for row in command_rows]
    expected = {"/model", "/effort", "/thinking", "/theme", "/files", "/agents", "/sessions",
                "/new", "/fork", "/btw", "/plan", "/pr", "/copy", "/resume", "/help", "/exit", "/terminals"}
    assert set(labels) == expected and len(labels) == len(expected), "help does not give each command its own row"
    assert all(not re.search(r"\s{2,}/[a-z]+", row[2]) for row in command_rows), "help packed another command column into a row"
    assert "/run" not in visible(screen), "removed /run remains advertised"
    screen.send(b"\x1b[B\r")
    until(screen, lambda frame: screen.vt.line(-4) == "› /effort" and "ANSA commands" not in frame, "help arrows select and Enter only inserts")
    assert journals(root) == before
    screen.send(b"\x15")
    screen.send("text?")
    until(screen, lambda frame: screen.vt.line(-4) == "› text?" and "ANSA commands" not in frame, "question mark inside text stays text")
    assert journals(root) == before
    screen.send(b"\x05\x15")
    # An ordinary whitespace delimiter is also valid for no-argument local
    # commands. It must never turn /exit or a mode switch into a model prompt.
    screen.send("/plan \r")
    until(screen, lambda frame: "Plan" in screen.vt.line(-2) and screen.vt.line(-4) == "›", "trailing-space /plan routed locally")
    screen.send("/plan \r")
    until(screen, lambda frame: "Plan" not in screen.vt.line(-2) and screen.vt.line(-4) == "›", "/plan returns to work")
    local(screen, "/pr main")
    until(screen, lambda frame: "Opening a draft PR requires network access" in frame,
          "/pr stays local and refuses publishing offline")
    assert journals(root) == before, "whitespace in local commands dispatched canonical user turns"
    screen.send("/exit \r")
    return "normal"


def new_resume_and_exit(screen, root, cwd):
    assert screen.vt.line(0) == "ANSA" and screen.vt.line(1) == ""
    assert "ready" not in screen.vt.line(-2) and cwd.name in screen.vt.line(-2)
    screen.send("PREVIOUS_SESSION\r")
    until(screen, lambda frame: completed(root, "PREVIOUS_SESSION") and "OMH_TOOL_OK" in frame,
          "previous session committed")
    previous, = [path for path in (root / "home/.ansa/sessions").iterdir() if path.is_dir()]
    local(screen, "/new")
    until(screen, lambda frame: ready(screen) and "PREVIOUS_SESSION" not in frame
          and sum(row["kind"] == "init" for row in records(root)) == 2, "new conversation with an empty transcript")
    assert sum(row["kind"] == "accepted" for row in records(root)) == 1
    local(screen, "/resume")
    until(screen, lambda frame: "Project sessions" in frame and "PREVIOUS_SESSION" in popup_text(screen),
          "resume opened saved sessions on an idle chat")
    assert "resume_not_available" not in visible(screen)
    escape(screen)
    until(screen, lambda frame: "Project sessions" not in frame, "session picker closed")
    local(screen, f"/resume {previous.name}")
    until(screen, lambda frame: "PREVIOUS_SESSION" in screen.vt.line(1), "resume with an ID reopened that conversation")
    screen.send("Черновик Ж界")
    until(screen, lambda frame: screen.vt.line(-4) == "› Черновик Ж界", "draft before interrupt")
    screen.send(b"\x03")
    until(screen, lambda frame: screen.vt.line(-4) == "›", "interrupt clears the draft")
    assert "again to exit" not in visible(screen)
    local(screen, "/status")
    until(screen, lambda frame: "ansa: completed" in frame, "explicit status before exit")
    screen.send(b"\x03")
    until(screen, lambda frame: "Press Ctrl+C again to exit" in screen.vt.line(-1), "first empty interrupt confirms exit")
    screen.send("x")
    until(screen, lambda frame: screen.vt.line(-4) == "› x" and "again to exit" not in frame,
          "typing disarms exit")
    screen.send(b"\x03")
    until(screen, lambda frame: screen.vt.line(-4) == "›", "draft cleared after disarming")
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
    before = journals(root)
    screen.send("Запусти субагента: подготовка\r")
    until(screen, lambda frame: "Refine" in frame, "cancel preview")
    escape(screen)
    screen.send(b"\t")
    until(screen, lambda frame: "Orchestration" in screen.vt.line(-2), "third Tab mode")
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
    assert journals(root) == before, "preview, scroll or cancel executed a helper"
    approve_helpers(screen, "Запусти субагента: " + child)
    accepted = [row for row in records(root) if row['kind'] == 'accepted']
    helpers = [row for row in accepted if row['command'] == 'assignment']
    assert len(helpers) == 2 and all(row['mode'] == 'plan' for row in helpers)
    assert sum(row.get('handoff') is True for row in accepted) == 1


def session_agent_titles_restart(screen, root, cwd):
    parent = "PARENT_TITLE"
    screen.send(parent + "\r")
    until(screen, lambda frame: completed(root, parent) and ready(screen) and "OMH_TOOL_OK" in frame
          and "Ctrl+C stop" not in screen.vt.line(-2), "parent turn settled and title committed")
    screen.send(b"\t\t")
    until(screen, lambda frame: "Orchestration" in screen.vt.line(-2), "third Tab mode")
    screen.send("Почему отчёт иногда пустой?\r")
    until(screen, lambda frame: "Refine" in frame, "plan from a vague request")
    before_refine = journals(root)
    screen.send(b"\x1b[B\r")
    until(screen, lambda frame: "Enter a clarification" in frame, "refinement returned to composer")
    assert journals(root) == before_refine, "refinement launched a helper"
    approve_helpers(screen, "Проверить формат входных данных")
    open_helper(screen)
    escape(screen)
    until(screen, lambda frame: parent in screen.vt.line(1) and not helper_title(screen.vt.line(1)), "agent returned to root conversation")
    until(screen, lambda frame: "Conversation agents" in frame and "Найти файлы" in frame and "Проверить, достаточно" in frame, "Esc restored roster with both completed helpers")
    assert not re.search(r"\bs_\d+_", screen.vt.line(1)), "agent breadcrumb displays an ID instead of a title"
    escape(screen)
    until(screen, lambda frame: "Conversation agents" not in frame and not helper_title(screen.vt.line(1)), "agent inspector closed before session command")
    local(screen, "/sessions")
    until(screen, lambda frame: "Project sessions" in frame and "Choose a conversation" in frame and parent in frame, "human-readable root session picker")
    assert not helper_title(popup_text(screen)), "agents leaked into conversation picker"
    escape(screen)
    until(screen, lambda frame: "Project sessions" not in frame and parent in screen.vt.line(1), "session picker closed before exit")
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
    until(restarted, lambda frame: parent in restarted.vt.line(1) and ready(restarted), "saved parent title restored")
    local(restarted, "/agents")
    until(restarted, lambda frame: "Conversation agents" in frame and "Найти файлы" in frame and "Проверить, достаточно" in frame, "completed agent tree rebuilt from journals")
    assert "Completed" in visible(restarted), "restarted completed agents lost their observed status"
    assert not re.search(r"\bs_\d+_", restarted.vt.line(1)) and journals(root) == before
    restarted.result = restarted.finish(signal.SIGTERM)
    assert restarted.result["exit"] == 143 and restarted.result["termios_restored"] and restarted.result["flags_restored"], restarted.result
    assert journals(root) == before, "viewing saved session/agent titles changed canonical journals"
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
    report.write_text("before fixture\n")
    before = git("status", "--short")
    screen.send(b"\t\t")
    until(screen, lambda frame: "Orchestration" in screen.vt.line(-2), "writer plan mode")
    screen.send("ORCHESTRATION_COMMAND_FIXTURE\r")
    until(screen, lambda frame: "Independent review" in frame or "independent review" in frame,
          "plan discloses verification and fixes")
    assert not (home := root / "home/.ansa/orchestration").exists()
    assert "./check.sh" in visible(screen) and not check_marker.exists()
    screen.send(b"\r")
    until(screen, lambda frame: "ansa-result готова" in frame and "Verified changes" in frame,
          "reviewed isolated candidate and diff", timeout=15)
    checkout, = home.glob("*/*/checkout")
    assert git("show", "ansa-result:report.txt", path=checkout) == "after fixture\n"
    assert report.read_text() == "before fixture\n"
    assert git("show", ":report.txt") == "committed version\n"
    assert git("status", "--short") == before
    assert check_marker.read_text() == "x"
    assert "project snapshot → ansa-result" in visible(screen)
    screen.send(b"\t")
    for cols, rows in [(80, 24), (120, 36)]:
        screen.resize(cols, rows)
        screen.vt = Terminal(cols, rows)
        until(screen, lambda frame: "report.txt · +1 −1" in frame, "candidate diff at terminal size")
        screen.send(b"f")
        until(screen, lambda frame: any(line.startswith("report.txt · +1 −1") for line in frame.splitlines())
              and "after fixture" in frame, "wide diff with its actual comparison base")
        screen.send(b"f")
    screen.resize(100, 32)
    screen.vt = Terminal(100, 32)
    until(screen, lambda frame: "report.txt · +1 −1" in frame, "diff restored after resize")
    escape(screen)
    screen.send("review draft")
    until(screen, lambda frame: "./check.sh · passed" in frame, "confirmed command as chat result")
    select_result(screen, "./check.sh · passed")
    screen.send(b"\r")
    until(screen, lambda frame: "fixture-check-output" in frame, "show actual command output")
    escape(screen)
    until(screen, lambda frame: "› ./check.sh · passed" in frame and "review draft" in screen.vt.line(-4),
          "same keyboard result and draft after Esc")
    escape(screen)
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
    until(restarted, lambda frame: "Approved plan" in frame, "plan link after restart")
    select_result(restarted, "Approved plan")
    restarted.send(b"\r")
    until(restarted, lambda frame: "Original request" in frame, "original question from approved run ledger")
    escape(restarted)
    escape(restarted)
    until(restarted, lambda frame: "./check.sh · passed" in frame, "verified checks recovered from their receipts")
    select_result(restarted, "./check.sh · passed")
    restarted.send(b"\r")
    until(restarted, lambda frame: "fixture-check-output" in frame, "verified command output after restarting")
    escape(restarted)
    escape(restarted)
    local(restarted, "/sessions")
    until(restarted, lambda frame: "Resume orchestration" in frame, "explicit continuation from original ledgers")
    restarted.send(b"\r")
    until(restarted, lambda frame: "Verified changes" in frame, "reconciled exact original candidate", timeout=15)
    until(restarted, lambda frame: (run / "summary-admitted").is_file(), "restored durable handoff receipt")
    assert git("rev-parse", "ansa-result", path=checkout) == frozen
    assert check_marker.read_text() == "x", "recovery ran the completed command again"
    assert journals(root) == before_journals, "reconciliation executed a second tool or repeated the handoff"
    restarted.result = restarted.finish(signal.SIGTERM)
    assert restarted.result["exit"] == 143 and restarted.result["termios_restored"], restarted.result
    return "normal"


def openrouter_orchestration(screen, root, cwd):
    until(screen, lambda frame: "deepseek/deepseek-v4.1-flash-1017" in frame
          and screen.vt.line(-4).startswith("›"), "pinned OpenRouter model")
    screen.send(b"\t\t")
    until(screen, lambda frame: "Orchestration" in screen.vt.line(-2), "OpenRouter plan mode")
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
    until(screen, lambda frame: "0/1 agents" in frame, "read-only plan starts despite a post-report recommendation")
    screen.send("keep draft\r")
    until(screen, lambda frame: "Draft · send after the task finishes" in frame and "keep draft" in screen.vt.line(-4),
          "draft retained while helper runs")
    held_child.unlink()
    until(screen, lambda frame: "Coordinator is writing the summary" in frame, "summary phase comes from coordinator state")
    held_summary.unlink()
    until(screen, lambda frame: "OPENROUTER_CARBON_DONE" in frame, "coordinator summary", timeout=15)
    until(screen, lambda frame: "Coordinator is writing the summary" not in frame and "Approved plan" in frame,
          "completed coordination leaves original request and plan in history")
    assert "summarize fixture" in visible(screen) and "END_OF_USER_REQUEST" in visible(screen) and "keep draft" in screen.vt.line(-4)
    assert not any(row.get("text") == "keep draft" for row in records(root))
    select_result(screen, "Approved plan")
    screen.send(b"\r")
    until(screen, lambda frame: "OPENROUTER_CARBON_PLAN" in frame, "read approved plan from its journal")
    escape(screen)
    escape(screen)
    screen.send(b"\x03")
    local(screen, "/exit")
    return "normal"


def siwc_orchestration(screen, root, cwd):
    until(screen, lambda _: ready(screen, "gpt-6.2-sol"), "GPT ready without a network call")
    screen.send(b"\t\t")
    until(screen, lambda _: "Orchestration" in screen.vt.line(-2), "GPT orchestration mode")
    for prompt in ("UNCONFIRMED_FORMAT", "UNCONSTRAINED_FORMAT"):
        screen.send(prompt + "\r")
        until(screen, lambda frame: "ChatGPT did not confirm the strict plan format" in frame
              and screen.vt.line(-4) == "› " + prompt, "unconfirmed response rejected and draft restored")
        assert not (root / "home/.ansa/orchestration").exists()
        assert [row["kind"] for row in records(root)] == ["init"]
        screen.send(b"\x03")
        until(screen, lambda _: screen.vt.line(-4) == "›", "failed draft can be edited")
    screen.send(b"UNNAMED_READ_SCOPE\r")
    until(screen, lambda frame: "Name the read folder in your request" in frame
          and screen.vt.line(-4) == "› UNNAMED_READ_SCOPE", "the model cannot select an unrelated folder")
    assert [row["kind"] for row in records(root)] == ["init"]
    screen.send(b"\x03")
    until(screen, lambda _: screen.vt.line(-4) == "›", "clear the unnamed folder request")
    screen.send(b"Inspect Workspace/pet\r")
    until(screen, lambda frame: "Read folder" in frame and "SIWC_CARBON_PLAN" in frame and "Start" in frame,
          "the read folder and plan are visible before approval")
    selected = str(root / "home/Workspace/pet")
    assert f"Read folder · {selected}" in visible(screen), "the approval screen must show the canonical read folder"
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

    with (root / "home/.ansa/auth/lock").open("rb") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        screen.send(b"Cancel this draft\r")
        until(screen, lambda frame: "Drafting a plan" in frame, "plan waits for the shared account lock")
        time.sleep(0.1)
        screen.send(b"\x03")
        until(screen, lambda frame: "Drafting a plan" not in frame
              and screen.vt.line(-4) == "› Cancel this draft", "cancel keeps the pending draft")
        screen.send(b"\x03")
        until(screen, lambda _: screen.vt.line(-4) == "›", "clear the pending draft")
        local(screen, "/exit")
        assert screen.finish()["exit"] == 0, "exit waited for the locked account after cancelling"
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
             ("plan-agent-inherits", plan_agent_inherits),
             ("session-agent-titles-restart", session_agent_titles_restart),
             ("orchestration-candidate", orchestration_candidate),
             ("terminal-interrupt-app-escape", terminal_interrupt_and_app_escape),
             ("terminal-eof-exit", terminal_eof_and_exit),
             ("outer-eof-cleans-terminal", outer_eof_cleans_terminal),
             ("editor-success", lambda *a: editor_case(*a, False)),
             ("editor-cancel", lambda *a: editor_case(*a, True)),
             ("editor-early-exit", editor_early_exit),
             ("editor-stopped", lambda *a: editor_early_exit(*a, True)),
             ("editor-missing", editor_missing), ("normal-exit", normal_exit),
             ("openrouter-orchestration", openrouter_orchestration),
             ("siwc-orchestration", siwc_orchestration)]
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--case", action="append", choices=[name for name, _ in cases], dest="selected_cases")
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=False)
    if args.selected_cases:
        cases = [(name, case) for name, case in cases if name in args.selected_cases]
    else:
        cases = [(name, case) for name, case in cases if name not in ("openrouter-orchestration", "siwc-orchestration")]
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
                editor.write_text('#!/bin/sh\nsleep 30 &\nprintf "%s\\n" "$!" > editor-child.pid\nexit 0\n')
            elif name == "editor-stopped":
                editor.write_text('#!/bin/sh\nkill -STOP $$\nprintf resumed > after-stop\n')
            editor.chmod(0o700)
            editor_path = cwd / "does-not-exist" if name == "editor-missing" else editor
            if name in ("openrouter-orchestration", "siwc-orchestration"):
                home.chmod(0o700)
                cwd.chmod(0o700)
                if name == "siwc-orchestration":
                    seed_siwc(home)
                    nested = home / "Workspace/pet/ansa"
                    nested.mkdir(parents=True)
                    (nested / "AGENTS.md").write_text("Follow the fixture instructions.\n" * 600 + "END_NESTED_INSTRUCTIONS\n")
                adapter = name.replace("-orchestration", "-carbon")
                screen = Pty(str(args.binary.resolve()), root, home, cwd, "", True, size=(100, 32),
                             argv=["--frontend-fixture", str(root), adapter])
                screen.vt = Terminal(100, 32)
            else:
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
                raise
            finally:
                (args.output / (name + ".pty")).write_bytes(screen.raw)
                (args.output / (name + ".journals.json")).write_text(json.dumps(journals(root)) + "\n")
                (args.output / (name + ".screen.txt")).write_text(visible(screen))
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
