"""Project shell, Carbon and editor PTYs, each owned by an embedded xterm."""

from __future__ import annotations

import os
from pathlib import Path
import shutil
import subprocess
import threading
import tkinter as tk
from collections.abc import Callable
from dataclasses import dataclass
from typing import Any

BG = "#19121c"
FG = "#e5d8a4"


@dataclass
class _Terminal:
    host: tk.Frame
    process: subprocess.Popen
    child: Any = None
    size: tuple[int, int] | None = None
    focus_requested: bool = True


def _ignore_vanished_window(error, request):
    # A child can exit between our geometry/focus request and the X server.
    return True


def _reap(process):
    try:
        process.wait(timeout=2)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait()


class TerminalDeck(tk.Frame):
    """Pack once. Each view starts only on request and survives switching."""

    def __init__(self, parent, project: Path, runtime_argv: list[str] | None):
        super().__init__(parent, bg=BG, borderwidth=0, highlightthickness=0)
        self.project = Path(project)
        self.runtime_argv = tuple(runtime_argv or ())
        self.active: str | None = None
        self.on_runtime_exit: Callable[[int], None] | None = None
        self.editor_path: Path | None = None
        self.on_editor_exit: Callable[[Path, int], None] | None = None
        self._terminals: dict[str, _Terminal] = {}
        self._closed = False
        self._timer = None
        self._display = None
        self._notice = tk.Label(self, text="", bg=BG, fg=FG, padx=8, pady=4,
                                justify="left", anchor="nw", font="TkFixedFont")
        self.bind("<Destroy>", self._destroyed, add="+")

    def _destroyed(self, event):
        if event.widget is self:
            self.close()

    def _message(self, text):
        self._notice.configure(text=text)
        self._notice.place(x=0, y=0, relwidth=1, relheight=1)
        self._notice.lift()

    def show_runtime(self):
        return self._show("runtime", self.runtime_argv)

    def show_shell(self):
        return self._show("shell", (os.environ.get("SHELL") or "/bin/sh",))

    def show_editor(self, path: Path | None = None):
        """Open the selected regular project file, or return to the active editor."""
        if self._closed:
            return False
        project = self.project.absolute()
        target = project / path if path is not None else None
        editor = self._terminals.get("editor")
        if editor and editor.process.poll() is None:
            if not self._show("editor", ()):
                return False
            if target is None or target == self.editor_path:
                return True
            text = f"Открыт {self.editor_path.name}. Завершите редактирование через :wq или :q."
            self._notice.configure(text=text)
            self._notice.place(x=0, y=0, relwidth=1)
            self._notice.lift()
            inset = self._notice.winfo_reqheight()
            editor.host.place(x=0, y=inset, relwidth=1, relheight=1, height=-inset)
            return False
        unavailable = "Выберите обычный файл проекта для редактирования."
        try:
            relative = target.relative_to(project) if target is not None else None
            if (not relative or ".." in relative.parts or not target.is_file()
                    or target.resolve(strict=True) != project.resolve(strict=True) / relative):
                return self._show("editor", (), unavailable)
        except (OSError, ValueError, RuntimeError):
            return self._show("editor", (), unavailable)
        binary = shutil.which("nvim") or shutil.which("vim")
        if not binary:
            return self._show("editor", (), "Для редактирования установите Neovim или Vim.")
        if self._show("editor", (binary, "--", str(target))):
            self.editor_path = target
            return True
        return False

    def _show(self, kind, argv, unavailable="Демонстрационная запись. Для Carbon откройте проект с runtime."):
        if self._closed:
            return False
        self.active = kind
        for terminal in self._terminals.values():
            terminal.focus_requested = False
            terminal.host.place_forget()
        terminal = self._terminals.get(kind)
        if terminal and terminal.process.poll() is not None:
            self._finished(kind, terminal)
            if self._closed:
                return False
            terminal = None
        if terminal is None and not argv:
            self._message(unavailable)
            return False
        if terminal is None:
            xterm = shutil.which("xterm")
            if self.tk.call("tk", "windowingsystem") != "x11" or not xterm:
                self._message("Для терминала нужны X11 или XWayland и программа xterm.")
                return False
            if self._display is None:
                try:
                    from Xlib import display, error
                except ImportError:
                    self._message("Для встроенного терминала установите python-xlib.")
                    return False
                try:
                    self._display = display.Display(self.winfo_screen())
                    self.tk.createfilehandler(self._display.fileno(), tk.READABLE, self._clicks)
                except (error.DisplayError, OSError) as exc:
                    self._message(f"Не удалось подключиться к X11: {exc}")
                    return False
            host = tk.Frame(self, bg=BG, borderwidth=0, highlightthickness=0,
                            takefocus=True)
            host.place(x=0, y=0, relwidth=1, relheight=1)
            host.update_idletasks()
            command = [
                xterm, "-display", self.winfo_screen(), "-into", str(host.winfo_id()),
                "-tn", "xterm-256color", "-u8", "-fa", "monospace", "-fs", "10",
                "-bg", BG, "-fg", FG, "-e", *argv,
            ]
            try:
                process = subprocess.Popen(command, cwd=self.project, start_new_session=True,
                                           stdin=subprocess.DEVNULL)
            except (ValueError, OSError) as exc:
                host.destroy()
                self._message(f"Не удалось запустить терминал: {exc}")
                return False
            terminal = _Terminal(host, process)
            self._terminals[kind] = terminal
        self._notice.place_forget()
        terminal.host.place(x=0, y=0, relwidth=1, relheight=1, height=0)
        terminal.host.lift()
        terminal.host.focus_set()
        terminal.focus_requested = True
        if self._timer is None:
            self._timer = self.after_idle(self._poll)
        return True

    def _fit(self, terminal):
        # xterm reparents an existing top-level X window. Tk's own -container
        # tracks CreateNotify, so it misses that child's size and keyboard focus.
        from Xlib import X, error

        host = terminal.host
        if not host.winfo_ismapped():
            return
        try:
            if terminal.child is None:
                parent = self._display.create_resource_object("window", host.winfo_id())
                children = parent.query_tree().children
                if not children:
                    return
                # This host contains exactly one foreign child and no Tk controls.
                terminal.child = children[0]
                # Native xterm receives the replayed click, including selection
                # and mouse reporting. Only transfer focus on an actual click.
                parent.grab_button(1, X.AnyModifier, False, X.ButtonPressMask,
                                   X.GrabModeSync, X.GrabModeAsync, X.NONE, X.NONE,
                                   onerror=_ignore_vanished_window)
            size = (max(1, host.winfo_width()), max(1, host.winfo_height()))
            if size != terminal.size:
                terminal.child.configure(x=0, y=0, width=size[0], height=size[1],
                                         border_width=0, onerror=_ignore_vanished_window)
                terminal.size = size
            if terminal.focus_requested and terminal.child.get_attributes().map_state == X.IsViewable:
                focus = self._display.get_input_focus().focus
                our_focus = str(self.tk.call("focus")) == str(host)
                if focus == X.PointerRoot:
                    # A focus-follows-pointer desktop may have another application
                    # under the pointer by the time the new xterm becomes visible.
                    top = self.winfo_toplevel()
                    under_pointer = self._display.screen().root.query_pointer().child
                    our_focus = getattr(under_pointer, "id", None) == int(top.wm_frame(), 0)
                if our_focus or focus == X.NONE:
                    terminal.child.set_input_focus(X.RevertToParent, X.CurrentTime,
                                                   onerror=_ignore_vanished_window)
                terminal.focus_requested = False
            self._display.flush()
        except (error.BadWindow, error.BadDrawable):
            terminal.child = None
            terminal.size = None

    def _clicks(self, *_):
        from Xlib import X

        while self._display.pending_events():
            event = self._display.next_event()
            if event.type != X.ButtonPress:
                continue
            terminal = self._terminals.get(self.active)
            try:
                if terminal and terminal.child and event.window.id == terminal.host.winfo_id():
                    terminal.host.focus_set()
                    terminal.child.set_input_focus(X.RevertToParent, X.CurrentTime,
                                                   onerror=_ignore_vanished_window)
            finally:
                self._display.allow_events(X.ReplayPointer, event.time)
                self._display.flush()

    def _finished(self, kind, terminal):
        del self._terminals[kind]
        terminal.host.destroy()
        code = terminal.process.returncode
        if self.active == kind:
            self._message(f"Терминал завершён (код {code}). Выберите вкладку для нового запуска.")
        if kind == "editor":
            path, self.editor_path = self.editor_path, None
            if self.on_editor_exit and path is not None:
                self.on_editor_exit(path, code)
        if kind == "runtime" and self.on_runtime_exit:
            self.on_runtime_exit(code)

    def _poll(self):
        self._timer = None
        # Other X requests can have moved input from the socket to Xlib's queue.
        self._clicks()
        for kind, terminal in list(self._terminals.items()):
            if self._closed:
                return
            if terminal.process.poll() is not None:
                self._finished(kind, terminal)
            elif self.active == kind:
                self._fit(terminal)
        if self._terminals and not self._closed:
            self._timer = self.after(120, self._poll)

    def close(self):
        if self._closed:
            return
        self._closed = True
        if self._timer is not None:
            self.after_cancel(self._timer)
            self._timer = None
        for terminal in self._terminals.values():
            if terminal.process.poll() is None:
                terminal.process.terminate()
                # Reap even after Tk's mainloop has already stopped.
                threading.Thread(target=_reap, args=(terminal.process,), daemon=False).start()
        self._terminals.clear()
        self.editor_path = None
        if self._display is not None:
            self.tk.deletefilehandler(self._display.fileno())
            self._display.close()
            self._display = None
