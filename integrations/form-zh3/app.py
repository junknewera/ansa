#!/usr/bin/env python3
"""Form Ж-3, an optional native desktop companion for ANSA Carbon."""
import argparse
import queue
import tempfile
import threading
from pathlib import Path
import tkinter as tk
from tkinter import filedialog, messagebox, ttk

from agent_register import AgentRegister
from demo import Demo
from inspector import Inspector
from project_files import load_document
from run_evidence import open_frozen_plan
from terminals import TerminalDeck
from workspace import WorkspaceFeed


STOCK = "#dbd8ac"
LINEN = "#beb388"
TEAL = "#326c6b"
PLUM = "#39103a"
INK = "#271c27"
CARBON = "#9e226a"
MUSTARD = "#d6a82c"
YELLOW = "#f4de49"
SANS = ("DejaVu Sans", 9)
BOLD = ("DejaVu Sans", 9, "bold")
MONO = ("DejaVu Sans Mono", 10)
SERIF = ("DejaVu Serif", 12, "bold italic")


def section(parent, title, color, **grid):
    box = tk.Frame(parent, bg=STOCK, bd=2, relief="ridge")
    box.grid(sticky="nsew", **grid)
    tk.Label(box, text=title, bg=color, fg="white", anchor="w", font=BOLD,
             padx=6, pady=2, relief="raised", bd=1).pack(fill="x")
    return box


def action(parent, label, command, color=STOCK):
    return tk.Button(parent, text=label, command=command, bg=color, fg=INK,
                     activebackground="#fff0a5", font=SANS, padx=5, pady=1,
                     bd=2, relief="raised", cursor="hand2")


def field(parent, height=2, font=SANS):
    holder = tk.Frame(parent, bg=STOCK)
    holder.pack(fill="both", expand=True)
    out = tk.Text(holder, height=height, width=1, wrap="word", bg="#eee9cf",
                  fg=INK, bd=1, relief="sunken", font=font, padx=5, pady=3,
                  state="disabled", highlightthickness=0)
    scroll = ttk.Scrollbar(holder, command=out.yview)
    out.configure(yscrollcommand=scroll.set)
    scroll.pack(side="right", fill="y")
    out.pack(side="left", fill="both", expand=True)
    return out


def write(widget, value):
    """Changing runtime counters must not reset an operator's reading position."""
    if getattr(widget, "_last_value", None) == value:
        return
    position = widget.yview()
    widget.configure(state="normal")
    widget.delete("1.0", "end")
    widget.insert("1.0", value)
    widget.configure(state="disabled")
    if position and position[1] < 0.999:
        widget.yview_moveto(position[0])
    widget._last_value = value


def list_lines(items):
    return "\n".join(f"{a}   ·   {b}" for a, b in items)


class Form:
    def __init__(self, root, args):
        self.root, self.args = root, args
        self.project = args.cwd.resolve()
        self.demo = Demo() if args.demo else None
        self.endpoint = None
        self.temp = None
        self.feed = None
        self.current_identity = None
        self.files = {}
        self.link_state = "waiting"
        self.exit_code = None
        self.inspection_window = None
        self.aside = None
        self.aside_log = None
        self.terminal_expanded = False
        self.file_results = queue.SimpleQueue()
        self.reading = False
        self.closed = False
        self.history = []
        self.snapshot = {}
        self.status = tk.StringVar(value="Opening inspection report")
        self.source = tk.StringVar()
        self.phase = tk.StringVar()
        self.stamp = tk.StringVar()
        self.release = tk.StringVar()
        self.focus = tk.StringVar(value="Form Ж-3")
        self.command = tk.StringVar()
        self.runtime_argv = None
        if not self.demo:
            if args.terminal_only:
                self.runtime_argv = [args.ansa, "--cwd", str(self.project)]
            elif args.workspace_dir:
                self.endpoint = args.workspace_dir.absolute()
            else:
                self.temp = tempfile.TemporaryDirectory(prefix="ansa-zh3-")
                self.endpoint = Path(self.temp.name)
                self.runtime_argv = [args.ansa, "--cwd", str(self.project),
                                     "--workspace-dir", str(self.endpoint)]
            if self.runtime_argv and args.offline:
                self.runtime_argv.append("--offline")
        root.title("ANSA — Malocclusion Inspection Report, Form Ж-3")
        root.configure(bg=LINEN)
        root.geometry(f"{min(1550, int(root.winfo_screenwidth() * .94))}x"
                      f"{min(930, int(root.winfo_screenheight() * .9))}")
        root.minsize(740, 540)
        self._header()
        self._body()
        self._bottom()
        if self.demo:
            self.render_demo()
            self.show_terminal("record")
        else:
            self.render_live()
            if args.terminal_only:
                self.status.set("Terminal-only mode · Carbon owns the conversation")
            if self.endpoint:
                self.connect_feed()
            if self.runtime_argv:
                root.after_idle(lambda: self.show_terminal("runtime"))
            else:
                self.show_terminal("record")
        root.bind("<Control-f>", self.pick_file)
        root.bind("<Control-t>", self.terminal_menu)
        root.protocol("WM_DELETE_WINDOW", self.request_close)
        self.poll_id = root.after(120, self.poll)

    def _header(self):
        bar = tk.Frame(self.root, bg="#d0cbd1", bd=2, relief="raised")
        bar.pack(fill="x")
        tk.Label(bar, text=" ANSA ", bg="#443748", fg="white", font=BOLD).pack(side="left", fill="y")
        self.nameplate = tk.Label(bar, textvariable=self.phase, bg=YELLOW, font=BOLD,
                                  fg=INK, padx=6, bd=3, relief="sunken")
        self.nameplate.pack(side="left", fill="y")
        tk.Label(bar, text="Malocclusion Inspection Report, Form Ж-3", anchor="w",
                 bg="#e5dca4", fg=INK, font=("DejaVu Serif", 16, "bold italic"),
                 padx=7).pack(side="left", fill="both", expand=True)
        tk.Label(bar, textvariable=self.source, bg="#e6b5cd", fg=CARBON, relief="ridge",
                 bd=2, font=BOLD, padx=6).pack(side="right", fill="y")

    def _body(self):
        self.main = tk.Frame(self.root, bg=LINEN)
        self.main.pack(fill="both", expand=True)
        for col, weight in enumerate((13, 37, 50)):
            self.main.columnconfigure(col, weight=weight, uniform="page")
        self.main.rowconfigure(0, weight=7, uniform="rows")
        self.main.rowconfigure(1, weight=3, uniform="rows")
        land = section(self.main, "Sessions / Land register", TEAL, row=0, column=0, rowspan=2)
        self.session_box = tk.Listbox(land, height=5, width=1, bg="#eeebcb", fg="#222d68",
                                      exportselection=False, font=SANS, bd=1, selectmode="browse")
        self.session_box.pack(fill="both", expand=True)
        self.session_box.bind("<<ListboxSelect>>", self.choose_session)
        tk.Label(land, text="Registry / coordinates", bg="#897757", fg="white",
                 anchor="w", font=BOLD).pack(fill="x")
        tk.Label(land, text="Land     shared\nShell     easement\nNotes     disputed",
                 anchor="nw", justify="left", font=SANS, bg=STOCK, fg=INK,
                 bd=2, relief="groove", padx=3, pady=5).pack(fill="x")
        action(land, "Open file…", self.pick_file).pack(fill="x")
        if not self.demo:
            action(land, "Frozen review…", self.pick_review).pack(fill="x")
            if self.endpoint:
                action(land, "Reconnect viewer", self.connect_feed).pack(fill="x")
        self.edit_button = action(land, "Edit current file · e", self.edit_current)
        self.edit_button.pack(fill="x")
        self.catalog_menu = tk.Menubutton(land, text="Result catalog ▾", relief="raised",
                                         bd=2, font=SANS, bg=STOCK, state="disabled")
        self.catalog = tk.Menu(self.catalog_menu, tearoff=False)
        self.catalog_menu.configure(menu=self.catalog)
        self.catalog_menu.pack(fill="x")

        register = section(self.main, "Task / Approved plan", TEAL, row=0, column=1)
        self.task = tk.Label(register, bg="#e8dbac", fg=INK, anchor="w", font=SERIF,
                             justify="left", padx=5)
        self.task.pack(fill="x")
        register.bind("<Configure>", lambda e: self.task.configure(wraplength=max(80, e.width - 12)))
        self.plan = field(register, font=("DejaVu Serif", 11))
        self.plan.tag_configure("step", foreground="#735415", font=("DejaVu Sans Mono", 10, "bold"))
        self.plan.tag_configure("current", background="#d4c082", foreground="#37201d")
        self.workers = AgentRegister(register)
        self.workers.pack(fill="both", expand=True)
        self.inspector = Inspector(self.main)
        self.inspector.grid(row=0, column=2, sticky="nsew")
        for widget in (self.inspector.before, self.inspector.after, self.inspector.page, self.inspector.gasket):
            widget.bind("<KeyPress-e>", self.edit_current)

        ledger = section(self.main, "Message ledger / Clarification inbox", "#695088", row=1, column=1)
        receipt = tk.Frame(ledger, bg="#c9bb99", bd=1, relief="sunken")
        receipt.pack(fill="x")
        self.stamped = tk.Label(receipt, textvariable=self.stamp, bg="#ecd3ca", fg="#92225d",
                                font=("DejaVu Serif", 13, "bold italic"), padx=7,
                                anchor="w", relief="ridge", bd=3)
        self.stamped.pack(side="left")
        tk.Label(receipt, text="INCOMING  /  COPY II", font=("DejaVu Sans Mono", 8),
                 bg="#c9bb99", fg="#523733").pack(side="right")
        self.messages = field(ledger)
        entrybar = tk.Frame(ledger, bg=STOCK)
        entrybar.pack(fill="x")
        if self.demo:
            self.entry = tk.Entry(entrybar, textvariable=self.command, font=SANS, width=1)
            self.entry.pack(side="left", expand=True, fill="x")
            self.entry.bind("<Return>", self.record)
            self.record_btn = action(entrybar, "Record", self.record)
            self.record_btn.pack(side="right")
        elif self.runtime_argv:
            action(entrybar, "Write in Carbon ↙", lambda: self.show_terminal("runtime")).pack(fill="x")
        else:
            tk.Label(entrybar, text="Viewer · input belongs to the attached Carbon",
                     font=SANS, anchor="w", bg=STOCK).pack(fill="x")

        results = section(self.main, "Check results / Release office", "#846319", row=1, column=2)
        self.checks = field(results, font=("DejaVu Sans Mono", 11, "bold"))
        self.checks.configure(bg="#e4dfa9", fg="#46342b", relief="ridge", bd=3)
        tk.Label(results, textvariable=self.release, bg=STOCK, fg=INK,
                 anchor="w", padx=6, font=BOLD).pack(fill="x")
        controls = tk.Frame(results, bg=STOCK)
        controls.pack(fill="x")
        if self.demo:
            self.next = action(controls, "Next demo phase ▸", self.advance, "#e7cf66")
            self.next.pack(side="left")
            action(controls, "Replay", self.restart).pack(side="left")
            self.pr = action(controls, "Preview draft PR", self.preview_pr)
            self.pr.pack(side="right")
        elif self.runtime_argv:
            action(controls, "Review / publish in Carbon", lambda: self.show_terminal("runtime")).pack(side="left")

    def _bottom(self):
        strip = tk.Frame(self.root, bg="#aaa0a2", bd=2, relief="raised")
        strip.pack(fill="x")
        self.tab_buttons = {}
        self.terminal_selector = tk.Menu(self.root, tearoff=False)
        tabs = [("record", "Specimen shell" if self.demo else "Connection log")]
        if self.runtime_argv:
            tabs.append(("runtime", "Carbon / agent terminals"))
        tabs.append(("shell", "Open local shell"))
        for key, name in tabs:
            b = action(strip, name, lambda k=key: self.show_terminal(k))
            b.pack(side="left")
            self.tab_buttons[key] = b
        self.zoom = action(strip, "Expand terminal ↕", self.toggle_dock)
        self.zoom.pack(side="left")
        tk.Label(strip, textvariable=self.focus, bg="#e0c873", font=SANS, anchor="e").pack(side="right")
        self.dock = tk.Frame(self.root, bg=PLUM, height=158)
        self.dock.pack(fill="x")
        self.dock.pack_propagate(False)
        self.dock_mark = tk.Label(self.dock, text="P T Y", relief="ridge", bd=3,
                                  bg="#a49343", fg=PLUM, font=("DejaVu Serif", 10, "bold italic"),
                                  wraplength=12)
        self.dock_mark.pack(side="left", fill="y")
        self.deck = TerminalDeck(self.dock, self.project, self.runtime_argv)
        self.deck.on_runtime_exit = self.runtime_exit
        self.deck.on_editor_exit = self.editor_exit
        self.record_text = tk.Text(self.dock, height=3, width=1, bg="#28132c", fg="#f8e8e1",
                                   bd=2, relief="sunken", font=MONO, padx=7, pady=4,
                                   wrap="word", state="disabled")
        tk.Label(self.root, textvariable=self.status, anchor="w", bg="#d6cea6",
                 fg=INK, font=SANS, padx=4, bd=1, relief="sunken").pack(fill="x")

    def sessions(self, names):
        if tuple(names) != self.session_box.get(0, "end"):
            self.session_box.delete(0, "end")
            for name in names:
                self.session_box.insert("end", name)

    def choose_session(self, _event):
        choice = self.session_box.curselection()
        if not choice:
            return
        name = self.session_box.get(choice[0])
        if self.demo:
            if name.startswith("/btw"):
                self.open_aside()
                return
            details = {"weeklog": self.demo.state()["title"],
                       "fork": "Independent conversation · same project · no messages in this specimen",
                       "History": "\n".join(self.demo.state()["messages"])}
            self.detail(name, details.get(name, name))
        elif self.runtime_argv:
            self.show_terminal("runtime")

    def open_aside(self):
        if self.aside is None:
            self.inspector._close_evidence()
            self.aside = section(self.main, "/btw · Side conversation", "#763a79", row=0, column=2)
            self.aside_log = field(self.aside, font=SERIF)
            action(self.aside, "Esc · Return to inspection", self.close_aside).pack(fill="x")
            self.aside.bind("<Escape>", self.close_aside)
            self.aside_log.bind("<Escape>", self.close_aside)
            self.aside_log.master.bind("<Escape>", self.close_aside)
            for widget in self.aside.winfo_children():
                widget.bind("<Escape>", self.close_aside)
        self.update_aside()
        self.aside.lift()
        self.aside_log.focus_set()
        self.status.set("Demo /btw · read-only side conversation · Esc returns to the form")

    def update_aside(self):
        if self.aside is not None:
            status = dict(self.demo.state()["agents"])["/btw"]
            write(self.aside_log, "weeklog   /   /btw\n\nWhere does the week specification define the start "
                  "of a week?\n\nspec/week_spec.md\n" + status)

    def close_aside(self, _event=None):
        if self.aside is not None:
            self.aside.destroy()
            self.aside = self.aside_log = None
            self.entry.focus_set()
            self.status.set("Demo · focus returned to the main form")
        return "break"

    def detail(self, title, text):
        win = tk.Toplevel(self.root)
        win.title(title)
        win.geometry("560x270")
        win.transient(self.root)
        write(field(win, font=MONO), text)
        action(win, "Close", win.destroy).pack()
        win.bind("<Escape>", lambda _e: win.destroy())

    def render_demo(self):
        s = self.demo.state()
        self.phase.set(s["mode"])
        self.source.set("DEMO · weeklog")
        self.task.configure(text=s["title"])
        write(self.plan, s["step"] + "\n" + "\n".join(f"{i}. {v}" for i, v in enumerate(s["plan"], 1)))
        self.plan.tag_add("step", "1.0", "1.end")
        line = {"local_review": 3, "tests": 4, "final_review": 4}.get(s["phase"])
        self.plan.tag_remove("current", "1.0", "end")
        if line:
            self.plan.tag_add("current", f"{line}.0", f"{line}.end")
        self.workers.show(s["agents"], step=s["step"])
        self.sessions(s["sessions"])
        self.stamp.set(f"№ {16 + len(s['clarifications'])}   {s['clarifications'][-1][1].upper()}")
        queue_rows = [f"#{i}   {m}   [{st}]" for i, (m, st) in enumerate(s["clarifications"], 17)]
        write(self.messages, "\n".join(s["messages"] + queue_rows))
        write(self.checks, list_lines(s["checks"]))
        self.release.set(s["release"])
        self.next.configure(state="normal" if s["can_advance"] else "disabled")
        self.pr.configure(state="normal" if s["pr_eligible"] else "disabled")
        self.record_btn.configure(state="normal" if s["can_clarify"] else "disabled")
        self.entry.configure(state="normal" if s["can_clarify"] else "disabled")
        if not self.files:
            self.files = {d["name"]: d for d in s["documents"]}
            self.inspector.set_documents(list(self.files.values()))
        self.status.set("Demo replay · local fixture · release office Ж-3")
        write(self.record_text, "Recorded specimen · user shell in weeklog\n$ git status --short --branch\n## main...origin/main\n$")

    def advance(self):
        self.demo.advance()
        self.render_demo()
        self.update_aside()

    def restart(self):
        self.close_aside()
        self.demo.restart()
        self.files.clear()
        self.command.set("")
        self.render_demo()

    def record(self, _event=None):
        if self.demo.clarify(self.command.get()):
            self.command.set("")
            self.render_demo()
        return "break"

    def preview_pr(self):
        if self.demo.state()["pr_eligible"]:
            self.detail("Demo · Draft PR", "weeklog → main\n\nFix Sunday week grouping\n"
                        "Two files · 2 tests passed · final review accepted\n\nLocal preview only.")

    def render_live(self):
        s = self.snapshot
        self.phase.set(str(s.get("input_mode") or s.get("mode") or "Carbon"))
        self.source.set(("TERMINAL" if self.args.terminal_only else "ATTACHED" if self.args.workspace_dir
                         else "CARBON") + " · " + self.project.name)
        user_entries = [e for e in self.history if e.get("role") == "user"]
        self.task.configure(text=user_entries[-1]["text"][:180] if user_entries else "Current conversation")
        write(self.plan, f"Runtime phase   {s.get('phase', 'Waiting for workspace')}\n"
              f"Journal durable   {s.get('durable', '—')}")
        self.workers.show([], step="Open /agents in Carbon for current assignments, or load a frozen review.")
        self.sessions([self.current_identity[0]] if self.current_identity else [])
        entries = [f"{e.get('role', '')}   {e.get('text', '')}" for e in self.history]
        preview = s.get("preview")
        if isinstance(preview, dict) and preview.get("state") == "preview" and preview.get("text"):
            entries.append("Preview   " + preview["text"])
        self.stamp.set("Last received · disconnected" if self.current_identity and self.link_state != "connected"
                       else "Origin session · confirmed history" if self.current_identity else "Awaiting origin session")
        if self.args.terminal_only:
            write(self.plan, "Carbon is open in the terminal below.")
            self.stamp.set("Terminal input / output")
        write(self.messages, "\n\n".join(entries))
        write(self.checks, "Open the approved plan in Carbon to inspect candidate checks.")
        self.release.set("Review the accepted candidate in Carbon · publish via gh")

    def receive(self, kind, data):
        if kind == "connection":
            status = data.get("status", "")
            self.link_state = status
            if self.exit_code is None:
                self.status.set(f"Workspace {status} · {data.get('detail', '')}")
            if status != "connected":
                self.catalog_menu.configure(state="disabled")
                self.render_live()
            write(self.record_text, f"{self.endpoint}\n{self.status.get()}")
            if status == "connected":
                identity = (data.get("session"), data.get("incarnation"))
                if identity != self.current_identity:
                    self.current_identity = identity
                    self.history, self.snapshot = [], {}
                    self.catalog.delete(0, "end")
                    self.catalog_menu.configure(state="disabled")
                    self.files.clear()
                    self.inspector.set_documents([])
                    self.render_live()
        elif kind == "snapshot":
            self.snapshot = data.get("state", {})
            self.render_live()
        elif kind == "history":
            self.history = data.get("entries", [])
            self.render_live()
        elif kind == "artifacts":
            self.catalog.delete(0, "end")
            for item in data.get("documents", []):
                key = item.get("document_id")
                if key:
                    self.catalog.add_command(label=item.get("path") or key,
                                             command=lambda k=key: self.feed.read_document(k))
            for item in data.get("artifacts", []):
                key, rev = item.get("artifact_id"), item.get("revision")
                if key and rev:
                    supported = item.get("mime") in ("text/markdown", "text/csv")
                    self.catalog.add_command(label=item.get("title") or key,
                                             state="normal" if supported else "disabled",
                                             command=lambda k=key, r=rev: self.feed.read_artifact(k, r))
            self.catalog_menu.configure(state="normal" if self.catalog.index("end") is not None else "disabled")
        elif kind == "document":
            self.install_document(data)
        elif kind == "notice":
            self.status.set(data["detail"])

    def install_document(self, data):
        key = data.get("key", data["name"])
        if key not in self.files and len(self.files) >= 24:
            del self.files[next(iter(self.files))]
        for old in [k for k, value in self.files.items() if value["name"] == data["name"] and k != key]:
            del self.files[old]
        self.files[key] = data
        self.inspector.set_documents(list(self.files.values()))
        self.inspector.select_file(data["name"])

    def pick_file(self, _event=None):
        if self.reading:
            return "break"
        path = filedialog.askopenfilename(parent=self.root, title="Inspect project file",
                                          initialdir=self.project)
        if path:
            self.read_project_file(Path(path))
        return "break"

    def read_project_file(self, path):
        self.reading = True
        self.status.set("Reading selected project file…")

        def read():
            try:
                document = load_document(self.project, path)
                document["local_path"] = str(path.absolute())
                self.file_results.put((document, ""))
            except (ValueError, OSError) as exc:
                self.file_results.put((None, str(exc)))
        threading.Thread(target=read, daemon=True).start()

    def edit_current(self, _event=None):
        path = self.inspector.document.get("local_path")
        if not path:
            self.status.set("Open a local project file to edit it. Review copies are read only.")
            return "break"
        if "editor" not in self.tab_buttons:
            button = action(self.zoom.master, "File editor", lambda: self.show_terminal("editor"))
            button.pack(side="left", before=self.zoom)
            self.tab_buttons["editor"] = button
        return self.show_terminal("editor", Path(path))

    def show_terminal(self, key, path=None):
        if key == "runtime" and not self.runtime_argv:
            self.status.set("The attached Carbon owns its terminal." if not self.demo
                            else "This is an in-memory replay. Launch without --demo to open Carbon.")
            return "break"
        self.record_text.pack_forget()
        self.deck.pack_forget()
        if key == "record":
            self.record_text.pack(fill="both", expand=True)
        else:
            self.deck.pack(fill="both", expand=True)
            self.dock.update_idletasks()
            if key == "editor":
                started = self.deck.show_editor(path)
            elif key == "runtime":
                started = self.deck.show_runtime()
            else:
                started = self.deck.show_shell()
            if key == "runtime" and started and self.exit_code is not None:
                self.exit_code = None
                self.status.set("Carbon restarted · new session")
                if self.endpoint:
                    self.connect_feed()
        for label, button in self.tab_buttons.items():
            button.configure(relief="sunken" if label == key else "raised")
        self.focus.set({"runtime": "Carbon owns input", "shell": "User owns local shell", "editor": "Vim owns input",
                        "record": "Read only"}[key] if key == "record" or started else "See terminal notice")
        return "break"

    def editor_exit(self, path, status):
        if self.closed:
            return
        self.status.set(f"Editor exited ({status}) · {path}")
        if self.inspector.document.get("local_path") == str(path) and not self.reading:
            self.read_project_file(path)

    def runtime_exit(self, status):
        if not self.closed:
            self.exit_code = status
            self.status.set(f"Carbon exited ({status}) · select its terminal tab to start a new session")

    def connect_feed(self):
        if self.feed is not None:
            self.feed.close()
        self.feed = WorkspaceFeed(self.endpoint)
        self.feed.start()

    def terminal_menu(self, _event=None):
        menu = self.terminal_selector
        menu.delete(0, "end")
        for key, button in self.tab_buttons.items():
            menu.add_command(label=button["text"], command=lambda name=key: self.show_terminal(name))
        try:
            menu.tk_popup(self.zoom.winfo_rootx(), self.zoom.winfo_rooty())
        finally:
            menu.grab_release()
        return "break"

    def toggle_dock(self):
        self.terminal_expanded = not self.terminal_expanded
        self.dock.configure(height=max(220, int(self.root.winfo_height() * .56))
                            if self.terminal_expanded else 158)
        self.zoom.configure(text="Restore form ↕" if self.terminal_expanded else "Expand terminal ↕")

    def pick_review(self):
        if self.reading:
            return
        filename = filedialog.askopenfilename(parent=self.root, title="Open frozen reviewer input",
                                              filetypes=[("Approved plan", "approved-plan.json")])
        if filename:
            self.reading = True
            identity = self.current_identity
            def load():
                try:
                    view = open_frozen_plan(filename, identity[0] if identity else None)
                    self.file_results.put(({"archive": view, "origin": identity}, ""))
                except (OSError, ValueError) as exc:
                    self.file_results.put((None, str(exc)))
            threading.Thread(target=load, daemon=True).start()

    def show_review(self, view):
        if self.inspection_window is not None and self.inspection_window.winfo_exists():
            self.inspection_window.destroy()
        win = self.inspection_window = tk.Toplevel(self.root)
        win.title("Ж-3 · Frozen reviewer input")
        win.geometry("950x650")
        win.columnconfigure((0, 1), weight=1, uniform="dossier")
        win.rowconfigure(0, weight=1)
        left = section(win, "Frozen reviewer input / archived evidence", TEAL, row=0, column=0)
        write(field(left, font=SERIF), view["title"] + "\n\n" + "\n".join(view["plan"]))
        register = AgentRegister(left)
        register.pack(fill="both", expand=True)
        register.show(view["agents"], step="Recorded task results", archive=True)
        write(field(left), "\n".join(view["messages"]) + "\n\n" + list_lines(view["checks"] + view["clarifications"]))
        tk.Label(left, text=view["release"], bg=STOCK).pack(fill="x")
        panel = Inspector(win)
        panel.grid(row=0, column=1, sticky="nsew")
        panel.set_documents(view["documents"])
        win.bind("<Escape>", lambda _e: win.destroy())

    def poll(self):
        if self.closed:
            return
        if self.feed:
            for kind, data in self.feed.poll():
                self.receive(kind, data)
        try:
            doc, error = self.file_results.get_nowait()
        except queue.Empty:
            pass
        else:
            self.reading = False
            if doc and "archive" in doc:
                if doc["origin"] == self.current_identity:
                    self.show_review(doc["archive"])
                else:
                    self.status.set("Session changed while reading the review")
            elif doc:
                self.install_document(doc)
                self.status.set(f"Project file · {doc['name']}")
            else:
                self.status.set(error)
        self.poll_id = self.root.after(120, self.poll)

    def request_close(self):
        if self.deck.editor_path and not messagebox.askokcancel(
                "Close ANSA", "A file editor is open. Closing this window ends it and may discard unsaved edits.",
                parent=self.root):
            return
        self.close()

    def close(self):
        self.closed = True
        self.root.after_cancel(self.poll_id)
        if self.feed:
            self.feed.close()
        self.deck.close()
        self.root.destroy()
        if self.temp:
            self.temp.cleanup()


def arguments(argv=None):
    cli = argparse.ArgumentParser(description=__doc__)
    modes = cli.add_mutually_exclusive_group()
    modes.add_argument("--demo", action="store_true", help="open the offline weeklog specimen")
    modes.add_argument("--workspace-dir", type=Path, help="attach as viewer to an existing Carbon workspace")
    modes.add_argument("--terminal-only", action="store_true", help="run Carbon in the form without its viewer socket")
    cli.add_argument("--cwd", type=Path, default=Path.cwd(), help="project for the shell and file inspector")
    cli.add_argument("--offline", action="store_true", help="launch Carbon with its local offline provider")
    cli.add_argument("--ansa", default="ansa", help="path to the ANSA executable")
    args = cli.parse_args(argv)
    if args.offline and (args.demo or args.workspace_dir):
        cli.error("--offline applies to a newly launched Carbon")
    if not args.cwd.is_dir():
        cli.error("--cwd must be a directory")
    return args


if __name__ == "__main__":
    options = arguments()
    root = tk.Tk()
    form = Form(root, options)
    try:
        root.mainloop()
    finally:
        if not form.closed:
            form.close()
