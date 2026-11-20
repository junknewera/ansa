"""Native X11 regression tests for embedded xterm, PTY input and resize."""
import json
import os
from pathlib import Path
import shutil
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

try:
    import tkinter as tk
    from Xlib import X, XK, display
    from Xlib.ext import xtest
    from terminals import TerminalDeck
    HAVE_X11 = True
except ImportError:
    HAVE_X11 = False


@unittest.skipUnless(HAVE_X11 and os.environ.get("DISPLAY") and shutil.which("xterm"),
                     "requires Tk, python-xlib, X11 and xterm")
class TerminalTest(unittest.TestCase):
    def setUp(self):
        self.root = tk.Tk()
        self.root.geometry("820x270")
        self.root.update()
        self.root.focus_force()
        self.project = tempfile.TemporaryDirectory()
        self.addCleanup(self.project.cleanup)
        self.addCleanup(self.root.destroy)
        self.x = display.Display()
        self.addCleanup(self.x.close)

    def until(self, predicate):
        deadline = time.monotonic() + 6
        while time.monotonic() < deadline:
            self.root.update()
            if predicate():
                return
            time.sleep(0.025)
        self.fail("timed out waiting for the embedded terminal")

    def press(self, key, shift=False):
        code = self.x.keysym_to_keycode(XK.string_to_keysym(key))
        modifier = self.x.keysym_to_keycode(XK.string_to_keysym("Shift_L"))
        if shift:
            xtest.fake_input(self.x, X.KeyPress, modifier)
        xtest.fake_input(self.x, X.KeyPress, code)
        xtest.fake_input(self.x, X.KeyRelease, code)
        if shift:
            xtest.fake_input(self.x, X.KeyRelease, modifier)
        self.x.sync()

    def type_line(self, text):
        for key in (*text, "Return"):
            self.press(key)

    def pointer(self, widget, click=False):
        xtest.fake_input(self.x, X.MotionNotify, root=self.x.screen().root,
                         x=widget.winfo_rootx() + 12, y=widget.winfo_rooty() + 8)
        if click:
            xtest.fake_input(self.x, X.ButtonPress, 1)
            xtest.fake_input(self.x, X.ButtonRelease, 1)
        self.x.sync()

    def test_pty_input_resize_and_session_switch(self):
        project = Path(self.project.name)
        probe = project / "probe.py"
        probe.write_text(
            "import json,os,sys\nfrom pathlib import Path\n"
            "Path('argv').write_text(json.dumps([os.isatty(0),sys.argv[1:]]))\n"
            "while True:\n"
            " line = input()\n"
            " if line=='quit': break\n"
            " Path('line.tmp').write_text(json.dumps([line,list(os.get_terminal_size())]))\n"
            " Path('line.tmp').replace('line')\n"
        )
        arguments = ["argument with spaces", "$(literal)", "--openrouter"]
        draft = tk.Entry(self.root)
        draft.pack(fill="x")
        deck = TerminalDeck(self.root, project, [sys.executable, str(probe), *arguments])
        deck.pack(fill="both", expand=True)
        self.addCleanup(deck.close)
        exited = []
        deck.on_runtime_exit = exited.append
        self.assertEqual(deck._terminals, {})
        self.assertTrue(deck.show_runtime())
        runtime = deck._terminals["runtime"]
        self.until(lambda: (project / "argv").is_file() and runtime.child and
                   self.x.get_input_focus().focus.id == runtime.child.id)
        self.assertEqual(json.loads((project / "argv").read_text()), [True, arguments])
        self.pointer(draft, click=True)
        self.until(lambda: self.root.focus_get() is draft)
        self.pointer(runtime.host, click=True)
        self.until(lambda: self.x.get_input_focus().focus.id == runtime.child.id)
        self.pointer(draft)
        self.type_line("before")
        self.until(lambda: (project / "line").is_file())
        line, size_before = json.loads((project / "line").read_text())
        self.assertEqual(line, "before")
        self.assertEqual(draft.get(), "")
        with patch.dict(os.environ, SHELL="/bin/sh"):
            self.assertTrue(deck.show_shell())
        shell = deck._terminals["shell"]
        self.until(lambda: shell.child and self.x.get_input_focus().focus.id == shell.child.id)
        self.assertIsNone(runtime.process.poll())
        self.root.geometry("520x170")
        self.root.update()
        self.assertTrue(deck.show_runtime())
        self.until(lambda: self.x.get_input_focus().focus.id == runtime.child.id and
                   runtime.child.get_geometry().width == runtime.host.winfo_width())
        self.type_line("after")
        self.until(lambda: json.loads((project / "line").read_text())[0] == "after")
        _, size_after = json.loads((project / "line").read_text())
        self.assertLess(size_after[0], size_before[0])
        self.assertLess(size_after[1], size_before[1])
        self.assertIsNone(shell.process.poll())
        self.type_line("quit")
        self.until(lambda: bool(exited))
        self.assertEqual(exited, [0])
        deck.close()
        self.until(lambda: shell.process.poll() is not None)

    @unittest.skipUnless(shutil.which("nvim") or shutil.which("vim"), "requires Vim or Neovim")
    def test_editor_retains_dirty_buffer_and_saves_selected_file(self):
        project = Path(self.project.name)
        first, second = project / "- first.txt", project / "other.txt"
        first.write_text("first\n")
        second.write_text("second\n")
        deck = TerminalDeck(self.root, project, None)
        deck.pack(fill="both", expand=True)
        self.addCleanup(deck.close)
        exited = []
        deck.on_editor_exit = lambda path, code: exited.append((path, code))
        self.assertFalse(deck.show_editor())
        self.assertFalse(deck.show_editor(Path(__file__).resolve()))
        with patch.dict(os.environ, VIMINIT="autocmd VimEnter * silent write! ready"):
            self.assertTrue(deck.show_editor(first.name))
        editor = deck._terminals["editor"]
        self.until(lambda: (project / "ready").exists() and editor.child and
                   getattr(self.x.get_input_focus().focus, "id", None) == editor.child.id)
        for key in ["i", "a", "b", "c", "Escape"]:
            self.press(key)
        self.assertFalse(deck.show_editor(second))
        self.assertIs(deck._terminals["editor"], editor)
        self.assertEqual(deck.editor_path, first)
        self.assertIn(":wq", deck._notice.cget("text"))
        self.assertTrue(deck.show_shell())
        self.assertTrue(deck.show_editor())
        self.assertTrue(deck.show_editor(first))
        self.until(lambda: getattr(self.x.get_input_focus().focus, "id", None) == editor.child.id)
        self.press("z", shift=True)
        self.press("z", shift=True)
        self.until(lambda: bool(exited))
        self.assertEqual(exited, [(first, 0)])
        self.assertEqual(first.read_text(), "abcfirst\n")
        self.assertEqual(second.read_text(), "second\n")
        self.assertIsNone(deck.editor_path)

    def test_demo_and_missing_executable_are_inert(self):
        deck = TerminalDeck(self.root, Path(self.project.name), None)
        deck.pack()
        self.addCleanup(deck.close)
        self.assertFalse(deck.show_runtime())
        with patch("terminals.shutil.which", return_value=None):
            self.assertFalse(deck.show_shell())
        self.assertEqual(deck._terminals, {})


if __name__ == "__main__":
    unittest.main()
