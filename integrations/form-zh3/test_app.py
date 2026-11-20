"""Native Tk integration checks. Run under X11 or Xvfb; headless discovery is supported."""

import errno
import fcntl
import json
import os
from pathlib import Path
import pty
import shutil
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import termios
import time
import unittest
from unittest.mock import patch

try:
    import tkinter as tk
except ImportError:
    tk = None

if tk is not None:
    from app import Form, arguments
    from agent_register import AgentRegister


def body(widget):
    return widget.get("1.0", "end-1c")


def descendants(widget):
    for child in widget.winfo_children():
        yield child
        yield from descendants(child)


@unittest.skipIf(tk is None, "Tkinter is unavailable")
class FormTests(unittest.TestCase):
    def setUp(self):
        try:
            self.root = tk.Tk()
        except tk.TclError as exc:
            if "display" in str(exc).lower():
                self.skipTest(f"X display unavailable: {exc}")
            raise
        self.form = None
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.addCleanup(self.close_window)

    def close_window(self):
        if self.form is not None:
            self.form.close()
        else:
            self.root.destroy()

    def open(self, *options):
        args = arguments(["--cwd", self.tmp.name, *options])
        self.form = Form(self.root, args)
        self.root.update()
        return self.form

    def wait_for(self, predicate):
        deadline = time.monotonic() + 3
        while time.monotonic() < deadline:
            self.root.update()
            if predicate():
                return
            time.sleep(.01)
        self.fail("The Tk event loop did not render the expected update")

    @unittest.skipUnless(os.environ.get("ANSA_TEST_BIN"), "set ANSA_TEST_BIN to a built Carbon executable")
    def test_real_carbon_pty_to_native_viewer(self):
        binary = Path(os.environ["ANSA_TEST_BIN"]).resolve(strict=True)
        base = Path(self.tmp.name)
        home, project, endpoint = (base / name for name in ("home", "project", "endpoint"))
        for directory in (home, project, endpoint):
            directory.mkdir(mode=0o700)
        probe_path = base / "probe.sock"
        try:
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as probe:
                probe.bind(str(probe_path))
                probe.listen(1)
        except OSError as exc:
            if exc.errno in (errno.EACCES, errno.EPERM, errno.EAFNOSUPPORT, errno.ENOSYS):
                self.skipTest(f"Named AF_UNIX sockets unavailable: {exc}")
            raise
        probe_path.unlink()

        # Give Carbon only its own empty home and XDG directories. Launch a
        # helper interpreter to acquire the controlling PTY before exec, so
        # Python never runs a preexec_fn in a fork of this Tk process.
        env = {"HOME": str(home), "PATH": os.environ.get("PATH", os.defpath),
               "TERM": "xterm-256color", "LC_ALL": "C", "SHELL": shutil.which("sh") or "/bin/sh"}
        for variable in ("XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_STATE_HOME", "XDG_CACHE_HOME", "XDG_RUNTIME_DIR"):
            location = base / variable.lower()
            location.mkdir(mode=0o700)
            env[variable] = str(location)
        master, slave = pty.openpty()
        self.addCleanup(os.close, master)
        try:
            fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 32, 100, 0, 0))
            os.set_blocking(master, False)
            launch = "import fcntl, os, sys, termios; fcntl.ioctl(0, termios.TIOCSCTTY, 0); os.execv(sys.argv[1], sys.argv[1:])"
            child = subprocess.Popen(
                [sys.executable, "-I", "-c", launch, str(binary), "--offline", "--cwd", str(project),
                 "--workspace-dir", str(endpoint)], stdin=slave, stdout=slave, stderr=slave,
                start_new_session=True, cwd=project, env=env,
            )
        finally:
            os.close(slave)

        def stop_child():
            if child.poll() is None:
                os.killpg(child.pid, signal.SIGTERM)
                try:
                    child.wait(timeout=2)
                except subprocess.TimeoutExpired:
                    os.killpg(child.pid, signal.SIGKILL)
                    child.wait(timeout=2)
        self.addCleanup(stop_child)
        tty_tail = bytearray()

        def runtime_until(predicate, label):
            deadline = time.monotonic() + 10
            while time.monotonic() < deadline:
                self.root.update()
                try:
                    tty_tail.extend(os.read(master, 65536))
                    del tty_tail[:-2048]
                except BlockingIOError:
                    pass
                except OSError as exc:
                    if exc.errno != errno.EIO:
                        raise
                if predicate():
                    return
                self.assertIsNone(child.poll(), f"Carbon exited during {label}: {bytes(tty_tail)!r}")
                time.sleep(.01)
            self.fail(f"Timed out during {label}: {bytes(tty_tail)!r}")

        runtime_until(lambda: (endpoint / "workspace.sock").is_socket(), "workspace startup")
        self.form = form = Form(self.root, arguments(["--cwd", str(project), "--workspace-dir", str(endpoint)]))
        runtime_until(lambda: form.current_identity and form.snapshot.get("phase") not in (None, "loading"),
                      "native viewer attachment")
        origin = form.current_identity
        prompt, marker = "FORM_ZH3_CONFIRM_ORIGIN", "OMH_TOOL_OK:OMH_FIXED_VALUE_42"
        os.write(master, (prompt + "\r").encode())
        runtime_until(lambda: any(e.get("role") == "assistant" and e.get("text") == marker
                                  for e in form.history) and marker in body(form.messages),
                      "committed reply on the Form")
        self.assertEqual(form.task["text"], prompt)
        self.assertEqual(form.link_state, "connected")

        # /btw keeps the main chat visible. Only its read-only side session
        # should record this prompt; the attached Form stays on the parent.
        catalog = home / ".ansa" / "sessions"
        original_dirs = set(catalog.iterdir())
        other_prompt = "FORM_ZH3_ONLY_IN_SIDE_SESSION"
        os.write(master, ("/btw " + other_prompt + "\r").encode())

        def other_session_received():
            for folder in set(catalog.iterdir()) - original_dirs:
                metadata = folder / "info.json"
                if metadata.is_file():
                    info = json.loads(metadata.read_text())
                    if info.get("kind") == "btw" and info.get("parent") == origin[0]:
                        return any(other_prompt.encode() in part.read_bytes() for part in folder.glob("part-*.log"))
            return False

        runtime_until(other_session_received, "side conversation")
        self.assertEqual(form.current_identity, origin)
        self.assertEqual(form.session_box.get(0, "end"), (origin[0],))
        self.assertIn(marker, body(form.messages))
        self.assertNotIn(other_prompt, body(form.messages))
        stop_child()
        self.wait_for(lambda: form.link_state != "connected")
        self.assertIn("disconnected", form.stamp.get().lower())
        self.assertIn(marker, body(form.messages))

    def test_demo_records_then_delivers_before_final_review_and_pr_preview(self):
        form = self.open("--demo")
        self.assertEqual(str(form.pr["state"]), "disabled")
        form.entry.insert(0, "Keep the historical report")
        form.record_btn.invoke()
        self.assertEqual(form.command.get(), "")
        self.assertIn("Keep the historical report", body(form.messages))
        self.assertIn("RECORDED", form.stamp.get().upper())

        form.next.invoke()
        self.assertIn("DELIVERED", form.stamp.get().upper())
        self.assertIn("running", body(form.checks).lower())
        self.assertEqual(str(form.record_btn["state"]), "disabled")
        form.next.invoke()
        self.assertIn("passed", body(form.checks).lower())
        self.assertEqual(dict(form.workers.rows)["Strong"], "reviewing code and security")
        form.workers.body.focus_force()
        form.workers.body.event_generate("<Home>")
        form.workers.body.event_generate("<Down>")
        form.workers.body.event_generate("<Down>")
        form.workers.body.event_generate("<Return>")
        self.root.update()
        self.assertIn("reviewing code and security", body(form.workers.note))
        form.workers.note.event_generate("<Escape>")
        self.root.update()
        self.assertFalse(form.workers.expanded)
        self.assertEqual(str(form.pr["state"]), "disabled")

        form.next.invoke()
        self.assertEqual(str(form.next["state"]), "disabled")
        self.assertEqual(str(form.pr["state"]), "normal")
        self.assertIn("accepted", form.release.get().lower())
        form.pr.invoke()
        self.root.update()
        previews = [win for win in self.root.winfo_children() if isinstance(win, tk.Toplevel)]
        self.assertEqual(len(previews), 1)
        self.assertIn("draft", previews[0].title().lower())

    def test_inspector_file_selection_tabs_and_evidence_drawer(self):
        panel = self.open("--demo").inspector
        self.assertEqual(panel.file_name.get(), "src/week.py")
        panel.buttons["Diff"].invoke()
        self.assertIn("weekday()", body(panel.after))
        panel.footnote.invoke()
        self.assertIsNotNone(panel.drawer)
        self.assertTrue(panel.drawer.winfo_exists())

        panel.file_menu.current(1)
        panel.file_menu.event_generate("<<ComboboxSelected>>")
        self.root.update()
        self.assertEqual(panel.file_name.get(), "tests/test_week.py")
        self.assertIsNone(panel.drawer)
        panel.buttons["Source"].invoke()
        self.assertIn("test_sunday", body(panel.page))
        panel.footnote.invoke()
        self.assertIsNotNone(panel.drawer)
        panel.buttons["Markdown Preview"].invoke()
        self.assertIsNone(panel.drawer)
        self.assertEqual(panel.tab, "Markdown Preview")

    def test_btw_opens_one_side_pane_and_escape_restores_the_main_file(self):
        form = self.open("--demo")
        panel = form.inspector
        panel.file_menu.current(1)
        panel.file_menu.event_generate("<<ComboboxSelected>>")
        form.entry.insert(0, "Unsent main draft")
        original_history = body(form.messages)
        sessions = form.session_box.get(0, "end")
        index = next(i for i, name in enumerate(sessions) if name.startswith("/btw"))
        form.session_box.selection_set(index)
        form.session_box.event_generate("<<ListboxSelect>>")
        self.root.update()
        first = form.aside
        self.assertIsNotNone(first)
        self.assertTrue(first.winfo_ismapped())
        self.assertIn("spec/week_spec.md", body(form.aside_log))
        form.session_box.event_generate("<<ListboxSelect>>")
        self.root.update()
        self.assertIs(form.aside, first)
        self.assertFalse(any(isinstance(widget, tk.Toplevel) for widget in descendants(self.root)))
        self.assertEqual(body(form.messages), original_history)
        self.assertEqual(form.command.get(), "Unsent main draft")
        form.aside_log.focus_force()
        form.aside_log.event_generate("<Escape>")
        self.root.update()
        self.assertIsNone(form.aside)
        self.assertFalse(first.winfo_exists())
        self.assertEqual(panel.file_name.get(), "tests/test_week.py")
        self.assertEqual(form.command.get(), "Unsent main draft")

    def test_viewer_poll_preserves_history_on_loss_and_resets_on_new_incarnation(self):
        # test_workspace covers the socket. Here messages enter via the actual Tk
        # polling callback so a disconnected client never starts a shell or model.
        batches = []
        with patch("app.WorkspaceFeed", autospec=True) as factory:
            view = factory.return_value
            view.poll.side_effect = lambda: batches.pop(0) if batches else []
            form = self.open("--workspace-dir", self.tmp.name)
            factory.assert_called_once_with(Path(self.tmp.name))
            view.start.assert_called_once()
            identity = {"session": "s_test", "incarnation": "i_1"}
            batches.append([
                ("connection", dict(identity, status="connected", capabilities=["snapshot", "history"])),
                ("snapshot", dict(identity, type="snapshot", through=3, version=1,
                                  state=dict(phase="completed", durable=True, busy=False,
                                             mode="basic", text="Confirmed answer", preview=None))),
                ("history", dict(identity, entries=[
                    dict(role="user", text="Inspect the report", index=0, state="committed"),
                    dict(role="assistant", text="Confirmed answer", index=1, state="committed"),
                ], total=2, begin=0, end=2)),
            ])
            self.wait_for(lambda: "Confirmed answer" in body(form.messages))
            self.assertEqual(form.task["text"], "Inspect the report")
            self.assertIn("completed", body(form.plan))
            self.assertIsNone(form.runtime_argv)

            # The same parent session may inspect an archived run without replacing
            # its active conversation. The archive reader has separate parsing tests.
            review = dict(title="Prior review", plan=["Check the previous candidate"],
                          agents=[("offset", "completed · old result")], checks=[], clarifications=[],
                          messages=["Read from the approved plan"], release="Historical snapshot",
                          documents=[])
            with patch("app.filedialog.askopenfilename", return_value="approved-plan.json"), \
                    patch("app.open_frozen_plan", return_value=review) as read:
                form.pick_review()
                self.wait_for(lambda: form.inspection_window is not None)
                read.assert_called_once_with("approved-plan.json", "s_test")
            archive = form.inspection_window
            saved_table = next(w for w in descendants(archive) if isinstance(w, AgentRegister))
            self.assertTrue(saved_table.archive)
            self.assertEqual(saved_table.rows, (("offset", "completed · old result"),))
            self.assertIn("Confirmed answer", body(form.messages))
            self.assertNotIn("old result", body(form.messages))
            archive.focus_force()
            archive.event_generate("<Escape>")
            self.root.update()
            self.assertFalse(archive.winfo_exists())

            batches.append([("connection", {"status": "disconnected", "detail": "Workspace closed"})])
            self.wait_for(lambda: "disconnected" in form.status.get().lower())
            self.assertIn("Confirmed answer", body(form.messages))
            batches.append([("connection", dict(identity, incarnation="i_2", status="connected"))])
            self.wait_for(lambda: form.current_identity == ("s_test", "i_2"))
            self.assertNotIn("Confirmed answer", body(form.messages))
            self.assertNotEqual(form.task["text"], "Inspect the report")


if __name__ == "__main__":
    unittest.main()
