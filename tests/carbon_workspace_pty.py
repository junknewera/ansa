"""Production Carbon workspace acceptance with synthetic homes and real PTYs."""
import argparse
import json
import os
from pathlib import Path
import re
import shutil
import signal
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


def start(binary, root, home, cwd, extra=None, argv_extra=()):
    shell = shutil.which("bash")
    assert shell, "fixture requires Bash prompt integration"
    screen = Pty(str(binary), root, home, cwd, "", True, size=(100, 32),
                 argv=["--offline", "--cwd", str(cwd), *argv_extra],
                 extra_env={"SHELL": shell, **(extra or {})})
    screen.vt = Terminal(100, 32)
    until(screen, lambda text: "ANSA" in text and "Tab фокус" in text and ready(screen), "first workspace frame")
    return screen


def escape(screen):
    screen.send(b"\x1b")
    for _ in range(8):
        screen.drain(.01)


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


def ready(screen):
    return "gpt-6.1-sol" in screen.vt.line(-2) and screen.vt.line(-4).startswith("›")


def local(screen, command):
    assert command.startswith("/") and "\n" not in command and "\r" not in command
    screen.send(command.rstrip() + "\r")


def reading(screen):
    return "История" in screen.vt.line(1)


def records(root):
    result = []
    for directory in (root / "home/.ansa/sessions").iterdir():
        if directory.is_dir() and not directory.is_symlink():
            result.extend(session_cases.decode({path.name: path.read_bytes() for path in directory.glob("part-*.log")}))
    return result


def shell_ready(screen):
    return "Esc к файлам" in visible(screen)


def inspector_first(screen):
    # At narrow sizes Files is stacked below the chat; its content row follows
    # the visible tabs and rule instead of occupying a fixed screen row.
    tabs = next(row for row in range(3, screen.vt.rows - 5)
                if "Файлы" in screen.vt.line(row) and "Source" in screen.vt.line(row))
    return screen.vt.line(tabs + 2)


def terminal(screen):
    local(screen, "/files")
    until(screen, lambda frame: "q терминал" in frame, "Files inspector")
    screen.send("q")
    until(screen, lambda frame: "Терминал ·" in frame and shell_ready(screen), "embedded shell ready")


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
    local(screen, "/agent CHILD_HISTORY_TASK")
    until(screen, lambda frame: "CHILD_HISTORY_TASK" in screen.vt.line(1) and completed(root, "CHILD_HISTORY_TASK") and ready(screen), "agent completed in titled child chat")
    screen.send("CHILD_DRAFT Ж")
    until(screen, lambda frame: screen.vt.line(-4) == "› CHILD_DRAFT Ж", "child draft")
    paint_quiet(screen)
    until(screen, lambda frame: ready(screen) and "CHILD_HISTORY_TASK" in screen.vt.line(1),
          "completed child frame before Ctrl+C")
    screen.send(b"\x03")
    until(screen, lambda frame: screen.vt.line(-4) == "›", "Ctrl+C cleared the child draft")
    screen.send("CHILD_DRAFT Ж")
    until(screen, lambda frame: screen.vt.line(-4) == "› CHILD_DRAFT Ж", "new child draft")
    mouse(screen, 64, 2, 5)
    until(screen, lambda frame: reading(screen), "child mouse reading")
    screen.send(b"\x1bg")
    for _ in range(8): screen.drain(.01)
    assert "CHILD_HISTORY_TASK" in screen.vt.line(1) and reading(screen), "Alt navigated out of child"
    escape(screen)
    until(screen, lambda frame: "CHILD_HISTORY_TASK" in screen.vt.line(1) and not reading(screen), "first child Esc returns latest")
    assert screen.vt.line(-4) == "› CHILD_DRAFT Ж"
    escape(screen)
    until(screen, lambda frame: "CHILD_HISTORY_TASK" not in screen.vt.line(1) and ready(screen), "second child Esc returns parent")


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
    left = tuple(tuple(row[:chat_width]) for row in screen.vt.cells[3:-5])
    at = len(screen.raw)
    screen.send("jjjjj")
    until(screen, lambda frame: first_source().startswith("SRC_005"), "right Source scrolled")
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
    assert len(headers) == 1 and "готово" in headers[0], "tool lifecycle was split into duplicate rows"
    assert "prepared" not in compact and "KnownResult" not in compact and "Аргументы" not in compact
    screen.send("TOOLS_DRAFT Ж")
    until(screen, lambda frame: screen.vt.line(-4) == "› TOOLS_DRAFT Ж", "tool draft")
    row = next(row for row in range(3, screen.vt.rows - 5) if screen.vt.line(row).startswith("> get_test_value"))
    click(screen, 0, row)
    until(screen, lambda frame: "Аргументы" in body(screen) and "Вывод" in body(screen), "tool row opened inline")
    expanded = body(screen)
    assert "value: OMH_FIXED_VALUE_42" in expanded and "ok: true" in expanded
    assert '{"ok"' not in expanded and "KnownResult" not in expanded, "details exposed the raw protocol instead of readable values"
    assert [row["invocation"]["arguments"] for row in records(root) if row["kind"] == "intent"] == [{}], "display projection changed the invocation arguments"
    assert len([line for line in expanded.splitlines() if re.match(r"[>v] get_test_value\b", line)]) == 1
    assert screen.vt.line(-4) == "› TOOLS_DRAFT Ж"
    row = next(row for row in range(3, screen.vt.rows - 5) if screen.vt.line(row).startswith("v get_test_value"))
    click(screen, 0, row)
    until(screen, lambda frame: "Аргументы" not in body(screen) and "> get_test_value" in body(screen), "tool row collapsed")

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
    until(screen, lambda frame: "Аргументы" in body(screen) and "Вывод" in body(screen), "Ctrl+T opened tool details")
    assert screen.vt.line(-4) == "› TOOLS_DRAFT Ж"
    before = journals(root)
    row = next(row for row in range(3, screen.vt.rows - 5) if "Весь вывод" in screen.vt.line(row))
    click(screen, screen.vt.line(row).index("Весь вывод"), row)
    until(screen, lambda frame: "value: OMH_FIXED_VALUE_42" in frame and "j/k строки" in frame, "complete readable output inspector")
    assert screen.vt.line(-4) == "› TOOLS_DRAFT Ж"
    output_before = body(screen)
    screen.send("Ж界😀")
    screen.send(b"\x1b[200~" + "Паста Ж界😀\n/exit".encode() + b"\x1b[201~")
    screen.send(b"\x7f\x08\x1b[3~")
    for _ in range(8): screen.drain(.01)
    assert screen.vt.line(-4) == "› TOOLS_DRAFT Ж", "read-only output accepted Unicode, paste or deletion into the draft"
    assert body(screen) == output_before and journals(root) == before, "read-only output input changed the view or canonical journal"
    escape(screen)
    until(screen, lambda frame: "j/k строки" not in frame and "get_test_value" in frame, "output inspector returned to chat")
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
    until(screen, lambda frame: model in screen.vt.line(-2) and "Сообщение агенту" not in screen.vt.line(-2), "compact status after resize")
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
    until(screen, lambda frame: "panel/" in frame and "l каталог" in frame, "split file listing")
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
    assert "INSPECTOR_NEVER_COPY" not in text and "Файлы" not in text and "\x1b" not in text
    assert screen.raw[at:].count(b"\x1b]52;c;") == 1


def branches(screen, root, cwd):
    screen.send("parent-marker\r")
    until(screen, lambda frame: "OMH_TOOL_OK:OMH_FIXED_VALUE_42" in frame, "parent completion")
    local(screen, "/fork")
    until(screen, lambda frame: "Ветка" in screen.vt.line(1), "fork conversation")
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
    until(screen, lambda frame: "Сессии проекта" in frame and "Выбрать разговор" in frame and "child-marker" in frame, "session picker with saved conversation titles")
    assert "parent-marker" in popup_text(screen) and "child-marker" in popup_text(screen)
    assert "side-marker" not in popup_text(screen), "side conversation leaked into the main/fork picker"
    assert not re.search(r"\bs_\d+_", screen.vt.line(1)), "conversation breadcrumb uses storage IDs as titles"
    escape(screen)


def files_and_terminal(screen, root, cwd):
    (cwd / "notes.md").write_text("# Fixture heading\n\n**content**\n")
    local(screen, "/files notes.md")
    until(screen, lambda frame: "только чтение" in frame and "Fixture heading" in frame, "file source")
    screen.send("p")
    until(screen, lambda frame: "Fixture heading" in frame and "**content**" not in frame, "Markdown preview")
    screen.send("d")
    until(screen, lambda frame: "Diff недоступен" in frame, "non-Git diff explicit")
    before = journals(root)
    screen.send("q")
    until(screen, lambda frame: "Терминал ·" in frame and shell_ready(screen), "file inspector opened embedded terminal")
    screen.send("ANSA_PTY_VALUE=CONSOLE_ONLY; export ANSA_PTY_VALUE; printf '%s:%s\\n' \"$ANSA_PTY_VALUE\" \"$$\" > terminal-state; printf CONSOLE_ONLY\r")
    until(screen, lambda frame: (cwd / "terminal-state").exists() and "CONSOLE_ONLY" in frame and shell_ready(screen), "manual shell output")
    state = (cwd / "terminal-state").read_text()
    escape(screen)
    until(screen, lambda frame: "q терминал" in frame and "Esc к файлам" not in frame, "prompt Esc returned to Files")
    screen.send("q")
    until(screen, lambda frame: shell_ready(screen), "same terminal reopened")
    screen.send("printf '%s:%s\\n' \"$ANSA_PTY_VALUE\" \"$$\" > terminal-state-again; false; printf ' STATUS:%s ' \"$?\"\r")
    until(screen, lambda frame: (cwd / "terminal-state-again").exists() and "STATUS:1" in frame and shell_ready(screen), "persistent shell environment and status")
    assert (cwd / "terminal-state-again").read_text() == state, "q created a second shell or lost its environment"
    terminal_copy_then_input(screen, root, cwd)
    escape(screen)
    until(screen, lambda frame: "q терминал" in frame and "Esc к файлам" not in frame, "terminal returned to files again")
    escape(screen)
    until(screen, lambda frame: "q терминал" not in frame, "Files returned to chat")
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
        until(screen, lambda frame: "q терминал" in frame and "Esc к файлам" not in frame, "Files after frozen terminal selection")
        screen.send("q")
        until(screen, lambda frame: after in frame and shell_ready(screen), "new child output becomes visible after reopening terminal")
        observation["visible_after_reopen"] = True
        raise AssertionError("terminal command executed, but copied selection hid new output until Files/reopen") from None
    observation["new_output_visible"] = True
    assert journals(root) == before, "manual terminal copying/input changed the canonical journal"


def editor_case(screen, root, cwd, cancel):
    (cwd / "notes.md").write_text("original\n")
    local(screen, "/files notes.md")
    until(screen, lambda frame: "только чтение" in frame, "editor source")
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
    until(screen, lambda frame: "ANSA" in frame and "только чтение" in frame, "editor terminal restored")
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
    until(screen, lambda frame: "только чтение" in frame, "early editor source")
    at = len(screen.raw)
    screen.send("e")
    screen.wait(b"\x1b[?1002h", since=at)
    until(screen, lambda frame: "только чтение" in frame and "ANSA" in frame, "early editor return")
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
    until(screen, lambda frame: "только чтение" in frame, "missing editor source")
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
    local(screen, "/exit")
    return "normal"


def terminal_interrupt_and_app_escape(screen, root, cwd):
    before = journals(root)
    terminal(screen)
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
    assert "Терминал ·" in visible(screen) and journals(root) == before
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
    until(screen, lambda frame: "q терминал" in frame and "Esc к файлам" not in frame, "prompt Escape returns to Files")
    escape(screen)
    until(screen, lambda frame: "q терминал" not in frame, "Files Escape returns to chat")
    screen.send("SURVIVED_TERMINAL")
    until(screen, lambda frame: screen.vt.line(-4) == "› SURVIVED_TERMINAL", "composer after foreground interrupt")
    assert journals(root) == before and not (cwd / "cancel-late").exists()


def click_files_tab(screen):
    visible(screen)
    row = next(row for row in range(3, screen.vt.rows - 5)
               if "Файлы" in screen.vt.line(row) and "Терминал" in screen.vt.line(row))
    click(screen, screen.vt.line(row).index("Файлы") + 1, row)
    until(screen, lambda frame: "q терминал" in frame and "Esc к файлам" not in frame, "Files tab after shell exit")


def terminal_eof_and_exit(screen, root, cwd):
    before = journals(root)
    terminal(screen)
    screen.send("printf '%s' \"$$\" > first-shell.pid\r")
    until(screen, lambda frame: (cwd / "first-shell.pid").exists() and shell_ready(screen), "first shell identity")
    screen.send(b"\x04")
    until(screen, lambda frame: "Терминал завершён" in frame, "Ctrl+D exits embedded shell only")
    assert screen.child.poll() is None and journals(root) == before
    click_files_tab(screen)
    screen.send("q")
    until(screen, lambda frame: shell_ready(screen), "q starts a new shell after explicit shell EOF")
    screen.send("printf '%s' \"$$\" > second-shell.pid; exit 7\r")
    until(screen, lambda frame: (cwd / "second-shell.pid").exists() and "Терминал завершён" in frame, "exit 7 leaves workspace alive")
    assert (cwd / "first-shell.pid").read_text() != (cwd / "second-shell.pid").read_text()
    click_files_tab(screen)
    escape(screen)
    until(screen, lambda frame: "q терминал" not in frame, "chat after embedded shell exits")
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
    local(screen, "/terminals")
    until(screen, lambda frame: "Терминал агента" in frame and "Агент ещё не открыл терминал" in frame,
          "managed terminal panel in the normal Carbon entry point")
    session, = (root / "home/.ansa/sessions").glob("*/terminal-host-v1/terminal-v1.json")
    escape(screen)
    until(screen, lambda frame: "Терминал агента" not in frame, "Esc returned to the chat")
    local(screen, "/exit")
    result = screen.finish()
    assert result["exit"] == 0 and result["termios_restored"]
    assert session.with_name("closed-v1.json").is_file(), "clean host closed with a durable seal"
    resumed = start(screen.child.args[0], root, root / "home", cwd, argv_extra=["--session", session.parents[1].name])
    screen.additional_screens = [("resumed", resumed)]
    local(resumed, "/plan")
    resumed.send("SEALED_PLAN\r")
    until(resumed, lambda frame: completed(root, "SEALED_PLAN"), "Plan admitted after a clean restart")
    local(resumed, "/exit")
    resumed.result = resumed.finish()
    assert resumed.result["exit"] == 0 and resumed.result["termios_restored"]
    return "normal"


def completion_and_help(screen, root, cwd):
    before = journals(root)
    screen.send("/mo")
    until(screen, lambda frame: "/model" in body(screen) and "Tab/→ дописать" in frame, "slash prefix suggestions")
    screen.send(b"\t")
    until(screen, lambda frame: screen.vt.line(-4) == "› /model" and "Для следующих сообщений" not in frame, "Tab completes without dispatch")
    assert journals(root) == before
    screen.send(b"\r")
    until(screen, lambda frame: "Для следующих сообщений" in frame, "Enter opens the completed command")
    escape(screen)
    until(screen, lambda frame: "Для следующих сообщений" not in frame, "model menu closed before typing another prefix")
    screen.send("/")
    until(screen, lambda frame: "/model" in body(screen) and "/effort" in body(screen), "all command suggestions")
    screen.send(b"\x1b[B\x1b[C")
    until(screen, lambda frame: screen.vt.line(-4) == "› /effort" and "По умолчанию" not in frame, "arrows choose and Right inserts only")
    assert journals(root) == before
    screen.send(b"\x15")
    screen.send(b"/exit" + b"\x1b[D" * 4 + b"\r")
    until(screen, lambda frame: screen.vt.line(-4) == "› /model" and "Для следующих сообщений" not in frame,
          "Enter at a partial token inserts the selected command")
    assert journals(root) == before
    screen.send(b"\x15")
    screen.send("Проверь /mo потом")
    screen.send(b"\x1b[D" * len(" потом"))
    until(screen, lambda frame: "/model" in body(screen) and "Tab/→ дописать" in frame, "completion at a middle word boundary")
    screen.send(b"\t")
    until(screen, lambda frame: screen.vt.line(-4) == "› Проверь /model потом", "middle completion preserves prefix and suffix")
    assert journals(root) == before
    screen.send(b"\x05\x15")
    screen.send("?")
    until(screen, lambda frame: "Команды ANSA" in frame, "empty composer question mark opens help")
    command_rows = [match for row in range(screen.vt.rows)
                    if (match := re.match(r"^\s*│(?:› |  )(/[a-z]+)\s+(.*?)\s*│", screen.vt.line(row)))]
    labels = [row[1] for row in command_rows]
    expected = {"/model", "/effort", "/thinking", "/theme", "/files", "/agents", "/sessions", "/new", "/fork", "/btw", "/plan", "/copy", "/resume", "/help", "/exit"}
    assert set(labels) == expected and len(labels) == len(expected), "help does not give each command its own row"
    assert all(not re.search(r"\s{2,}/[a-z]+", row[2]) for row in command_rows), "help packed another command column into a row"
    assert "/run" not in visible(screen), "removed /run remains advertised"
    screen.send(b"\x1b[B\r")
    until(screen, lambda frame: screen.vt.line(-4) == "› /effort" and "Команды ANSA" not in frame, "help arrows select and Enter only inserts")
    assert journals(root) == before
    screen.send(b"\x15")
    screen.send("text?")
    until(screen, lambda frame: screen.vt.line(-4) == "› text?" and "Команды ANSA" not in frame, "question mark inside text stays text")
    assert journals(root) == before
    screen.send(b"\x05\x15")
    # An ordinary whitespace delimiter is also valid for no-argument local
    # commands. It must never turn /exit or a mode switch into a model prompt.
    screen.send("/plan \r")
    until(screen, lambda frame: "План" in screen.vt.line(-2) and screen.vt.line(-4) == "›", "trailing-space /plan routed locally")
    screen.send("/plan \r")
    until(screen, lambda frame: "План" not in screen.vt.line(-2) and screen.vt.line(-4) == "›", "/plan returns to work")
    assert journals(root) == before, "whitespace in local commands dispatched canonical user turns"
    screen.send("/exit \r")
    return "normal"


def new_resume_and_exit(screen, root, cwd):
    assert screen.vt.line(0) == "ANSA" and screen.vt.line(1) == ""
    assert "готов" not in screen.vt.line(-2) and cwd.name in screen.vt.line(-2)
    screen.send("PREVIOUS_SESSION\r")
    until(screen, lambda frame: completed(root, "PREVIOUS_SESSION") and "OMH_TOOL_OK" in frame,
          "previous session committed")
    previous, = [path for path in (root / "home/.ansa/sessions").iterdir() if path.is_dir()]
    local(screen, "/new")
    until(screen, lambda frame: ready(screen) and "PREVIOUS_SESSION" not in frame
          and sum(row["kind"] == "init" for row in records(root)) == 2, "new conversation with an empty transcript")
    assert sum(row["kind"] == "accepted" for row in records(root)) == 1
    local(screen, "/resume")
    until(screen, lambda frame: "Сессии проекта" in frame and "PREVIOUS_SESSION" in popup_text(screen),
          "resume opened saved sessions on an idle chat")
    assert "resume_not_available" not in visible(screen)
    escape(screen)
    until(screen, lambda frame: "Сессии проекта" not in frame, "session picker closed")
    local(screen, f"/resume {previous.name}")
    until(screen, lambda frame: "PREVIOUS_SESSION" in screen.vt.line(1), "resume with an ID reopened that conversation")
    screen.send("Черновик Ж界")
    until(screen, lambda frame: screen.vt.line(-4) == "› Черновик Ж界", "draft before interrupt")
    screen.send(b"\x03")
    until(screen, lambda frame: screen.vt.line(-4) == "›", "interrupt clears the draft")
    assert "ещё раз для выхода" not in visible(screen)
    local(screen, "/status")
    until(screen, lambda frame: "ansa: completed" in frame, "explicit status before exit")
    screen.send(b"\x03")
    until(screen, lambda frame: "Ctrl+C ещё раз для выхода" in screen.vt.line(-1), "first empty interrupt confirms exit")
    screen.send("x")
    until(screen, lambda frame: screen.vt.line(-4) == "› x" and "ещё раз для выхода" not in frame,
          "typing disarms exit")
    screen.send(b"\x03")
    until(screen, lambda frame: screen.vt.line(-4) == "›", "draft cleared after disarming")
    screen.send(b"\x03")
    until(screen, lambda frame: "Ctrl+C ещё раз для выхода" in screen.vt.line(-1), "exit armed again")
    screen.send(b"\x03")
    return "normal"


def model_effort_pair(screen, root, cwd):
    before = journals(root)
    local(screen, "/model")
    until(screen, lambda frame: "Для следующих сообщений" in frame and "Enter выбрать effort" in frame, "model selection stage")
    screen.send(b"\r")
    until(screen, lambda frame: "По умолчанию" in frame and "Enter применить пару" in frame, "effort selection follows model")
    assert journals(root) == before, "choosing a model committed before effort confirmation"
    escape(screen)
    until(screen, lambda frame: "Enter выбрать effort" in frame, "effort Esc returns to model list")
    escape(screen)
    assert journals(root) == before, "cancelling the staged pair wrote runtime state"
    until(screen, lambda frame: "Enter выбрать effort" not in frame, "model picker cancelled before reopening")
    local(screen, "/model")
    until(screen, lambda frame: "Enter выбрать effort" in frame, "cached model picker reopens")
    screen.send(b"\r")
    until(screen, lambda frame: "Enter применить пару" in frame, "pair ready to apply")
    screen.send(b"\r")
    until(screen, lambda frame: sum(row["kind"] == "generation_selected" for row in records(root)) == 1 and ready(screen), "single durable combined selection")
    events = records(root)
    selected = [row for row in events if row["kind"] == "generation_selected"]
    assert len(selected) == 1 and selected[0]["effort"] is None
    assert not any(row["kind"] in {"accepted", "response", "intent"} for row in events), "model selection dispatched generation or tools"


def plan_agent_inherits(screen, root, cwd):
    parent, child = "PLAN_PARENT", "PLAN_AGENT"
    screen.send(parent + "\r")
    until(screen, lambda frame: completed(root, parent) and ready(screen), "parent title committed")
    local(screen, "/plan")
    until(screen, lambda frame: "План" in screen.vt.line(-2), "parent entered plan")
    screen.send("Запусти субагента: " + child + "\r")
    until(screen, lambda frame: completed(root, child) and parent in screen.vt.line(1)
          and f"Результат субагента «{child}»" in frame and ready(screen), "plan child returned its result")
    child_turn, = [row for row in records(root) if row['kind'] == 'accepted' and row['text'] == child]
    assert child_turn['mode'] == 'plan'


def session_agent_titles_restart(screen, root, cwd):
    parent, child, nested = "PARENT_TITLE", "AGENT_TITLE", "NESTED_TITLE"
    screen.send(parent + "\r")
    until(screen, lambda frame: completed(root, parent) and ready(screen), "parent title committed")
    screen.send("Запусти субагента: " + child + "\r")
    until(screen, lambda frame: completed(root, child) and parent in screen.vt.line(1)
          and f"Результат субагента «{child}»" in frame and ready(screen), "explicit request returned its child result to the parent")
    local(screen, "/agents")
    until(screen, lambda frame: "Агенты разговора" in frame and child in frame, "background child listed")
    screen.send(b"\r")
    until(screen, lambda frame: child in screen.vt.line(1) and ready(screen), "open completed child from agent tree")
    local(screen, "/agent " + nested)
    until(screen, lambda frame: completed(root, nested) and nested in screen.vt.line(1) and ready(screen), "nested agent own title committed")
    escape(screen)
    until(screen, lambda frame: child in screen.vt.line(1) and nested not in screen.vt.line(1), "nested agent parent")
    escape(screen)
    until(screen, lambda frame: parent in screen.vt.line(1) and child not in screen.vt.line(1), "agent returned to root conversation")
    local(screen, "/agents")
    until(screen, lambda frame: "Агенты разговора" in frame and child in frame and nested in frame, "completed parent-child agent tree")
    assert not re.search(r"\bs_\d+_", screen.vt.line(1)), "agent breadcrumb displays an ID instead of a title"
    escape(screen)
    until(screen, lambda frame: "Агенты разговора" not in frame and child not in screen.vt.line(1), "agent inspector closed before session command")
    local(screen, "/sessions")
    until(screen, lambda frame: "Сессии проекта" in frame and "Выбрать разговор" in frame and parent in frame, "human-readable root session picker")
    assert child not in popup_text(screen) and nested not in popup_text(screen), "agents leaked into conversation picker"
    escape(screen)
    until(screen, lambda frame: "Сессии проекта" not in frame and parent in screen.vt.line(1), "session picker closed before exit")
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
    until(restarted, lambda frame: "Агенты разговора" in frame and child in frame and nested in frame, "completed agent tree restored without resuming agents")
    assert "Заверш" in visible(restarted), "restarted completed agents lost their observed status"
    assert not re.search(r"\bs_\d+_", restarted.vt.line(1)) and journals(root) == before
    restarted.result = restarted.finish(signal.SIGTERM)
    assert restarted.result["exit"] == 143 and restarted.result["termios_restored"] and restarted.result["flags_restored"], restarted.result
    assert journals(root) == before, "viewing saved session/agent titles changed canonical journals"
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
             ("terminal-interrupt-app-escape", terminal_interrupt_and_app_escape),
             ("terminal-eof-exit", terminal_eof_and_exit),
             ("outer-eof-cleans-terminal", outer_eof_cleans_terminal),
             ("editor-success", lambda *a: editor_case(*a, False)),
             ("editor-cancel", lambda *a: editor_case(*a, True)),
             ("editor-early-exit", editor_early_exit),
             ("editor-stopped", lambda *a: editor_early_exit(*a, True)),
             ("editor-missing", editor_missing), ("normal-exit", normal_exit)]
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--case", action="append", choices=[name for name, _ in cases], dest="selected_cases")
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=False)
    if args.selected_cases:
        cases = [(name, case) for name, case in cases if name in args.selected_cases]
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
            extra = ["--native-terminals"] if name == "native-terminal-panel" else []
            screen = start(args.binary.resolve(), root, home, cwd, {"EDITOR": str(editor_path)}, argv_extra=extra)
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
