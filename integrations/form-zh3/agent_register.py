"""Native role ledger for Form Ж-3; selection inspects the supplied status only."""
import tkinter as tk
from tkinter import font, ttk


INK = "#241d22"
PAPER = "#e5d8a4"
LINING = "#d7dfab"
CARBON = "#80235b"
BRASS = "#74600e"
GREEN = "#285e56"
RED = "#9d2718"


def _motif(status):
    # Only the leading state is classified; archive summaries may describe
    # unrelated failures or approvals later in the same string.
    state = status.partition(" · ")[0].lower().strip()
    if state.startswith(("effect_unknown", "unknown", "interrupted", "cancelled", "failed")):
        return "broken", RED
    if state == "final review accepted":
        return "double", GREEN
    if state.startswith("step ") and state.endswith(" accepted"):
        return "accepted", GREEN
    if state == "final review pending" or state.startswith(("pending", "waiting", "blocked")):
        return "sleeve", BRASS
    if state == "patch returned":
        return "returned", CARBON
    if state.startswith("reviewing "):
        return "split", CARBON
    if state.startswith(("reading ", "working", "running", "executing")):
        return "clamp", BRASS
    return "entry", GREEN


def _fitted(text, face, width):
    if width <= 6:
        return ""
    # The complete incoming text remains in the selectable detail strip.
    text = " ".join(text[:500].split())
    if face.measure(text) <= width:
        return text
    limit = width - face.measure("…")
    low, high = 0, len(text)
    while low < high:
        mid = (low + high + 1) // 2
        if face.measure(text[:mid]) <= limit:
            low = mid
        else:
            high = mid - 1
    return text[:low] + "…"


class AgentRegister(tk.Frame):
    def __init__(self, parent):
        super().__init__(parent, bg=PAPER, bd=0)
        installed = {family.lower(): family for family in font.families(self)}
        def available(*names):
            return next((installed[name] for name in names if name in installed), names[-1])

        family = available("arial narrow", "liberation sans narrow", "dejavu sans condensed",
                           "liberation sans", "dejavu sans", "nimbus sans l", "helvetica")
        self.role_face = font.Font(root=self, family=family, size=-15, weight="bold")
        self.action_face = font.Font(root=self, family=family, size=-13)
        self.code_face = font.Font(root=self, family=available("liberation mono", "dejavu sans mono",
                                                               "nimbus mono l", "courier"),
                                   size=-11, weight="bold")
        self.title_face = font.Font(root=self, family=available("times new roman", "liberation serif",
                                                                "nimbus roman no9 l", "times"),
                                    size=-17, weight="bold", slant="italic")
        self.row_height = max(self.role_face.metrics("linespace") + 6, 24)
        self.rows, self.step, self.archive = (), "", False
        self.selected, self.expanded = 0, False
        self.note_text = None
        self.columnconfigure(0, weight=1)
        self.rowconfigure(1, weight=1)

        header = tk.Frame(self, bg="#bcac7e", relief="ridge", bd=1)
        header.grid(row=0, column=0, columnspan=2, sticky="ew")
        tk.Label(header, text="Agent register", anchor="w", font=self.title_face,
                 bg="#bcac7e", fg=INK, padx=6).pack(side="left")
        self.help = tk.Label(header, text="", font=self.code_face, fg="#e5d8a4",
                             bg="#514352", padx=6, pady=1)
        self.help.pack(side="right", padx=2)

        self.body = tk.Canvas(self, bg=PAPER, width=1, height=self.row_height * 4,
                              takefocus=1, highlightthickness=1,
                              highlightbackground="#72596e", highlightcolor=CARBON,
                              yscrollincrement=self.row_height)
        self.body.grid(row=1, column=0, sticky="nsew")
        self.scroll = ttk.Scrollbar(self, command=self.body.yview)
        self.scroll.grid(row=1, column=1, sticky="ns")
        self.body.configure(yscrollcommand=self._scrollbar)
        self.body.bind("<Configure>", lambda _e: self._paint())
        self.body.bind("<Button-1>", self._click)
        for key, movement in (("Up", -1), ("Down", 1), ("Home", "first"), ("End", "last")):
            self.body.bind(f"<{key}>", lambda _e, move=movement: self._move(move))
        for key in ("Return", "space"):
            self.body.bind(f"<{key}>", lambda _e: self._toggle())
        self.body.bind("<Escape>", lambda _e: self._close())
        self.body.bind("<Button-4>", lambda _e: self.body.yview_scroll(-1, "units"))
        self.body.bind("<Button-5>", lambda _e: self.body.yview_scroll(1, "units"))

        self.inset = tk.Frame(self, bg="#e9d7de", bd=1, relief="sunken")
        self.inset.columnconfigure(0, weight=1)
        self.origin = tk.Label(self.inset, font=self.code_face, anchor="w", padx=4,
                               bg="#d7b4c2", fg="#54233e")
        self.origin.grid(row=0, column=0, sticky="ew")
        tk.Button(self.inset, text="×", font=self.code_face, bd=0, padx=5, pady=0,
                  command=self._close, bg="#d7b4c2", fg=INK).grid(row=0, column=1)
        self.note = tk.Text(self.inset, font=self.action_face, bd=0, bg="#f5ebd9", fg=INK,
                            height=2, width=1, wrap="word", padx=4, state="disabled")
        self.note.grid(row=1, column=0, sticky="nsew")
        scroller = ttk.Scrollbar(self.inset, command=self.note.yview)
        scroller.grid(row=1, column=1, sticky="ns")
        self.note.configure(yscrollcommand=scroller.set)
        self.note.bind("<Escape>", lambda _e: self._close())

    def show(self, rows, step="", archive=False):
        rows = tuple((str(role), str(status)) for role, status in rows)
        if (rows, step, archive) == (self.rows, self.step, self.archive) and self.help.cget("text"):
            return
        selected_role = self.rows[self.selected][0] if self.rows else None
        previous_roles = [role for role, _status in rows]
        if selected_role in previous_roles and archive == self.archive:
            self.selected = previous_roles.index(selected_role)
        else:
            self.selected, self.expanded = 0, False
        self.rows, self.step, self.archive = rows, step, archive
        self.help.configure(text=("FROZEN" if archive else "Enter / detail") if rows else "NO ROWS")
        self.body.configure(height=self.row_height * min(4, max(2, len(rows))))
        self._paint()
        self._detail()

    def _scrollbar(self, start, end):
        self.scroll.set(start, end)
        if float(start) <= 0 and float(end) >= 1:
            self.scroll.grid_remove()
        else:
            self.scroll.grid()

    def _draw_mark(self, motif, color, x, y):
        c = self.body
        if motif == "sleeve":
            c.create_rectangle(x, y-5, x+18, y+5, outline=color)
            c.create_line(x+4, y-4, x+4, y+4, fill=color)
        elif motif in ("accepted", "double"):
            c.create_rectangle(x+1, y-6, x+18, y+6, outline=color, width=2)
            if motif == "double":
                c.create_rectangle(x+4, y-3, x+15, y+3, outline=color)
            else:
                c.create_line(x+4, y+1, x+7, y+3, x+14, y-3, fill=color, width=2)
        elif motif == "split":
            c.create_line(x+8, y-6, x, y-6, x, y+3, x+7, y+3, fill=color, width=2)
            c.create_line(x+12, y-3, x+19, y-3, x+19, y+6, x+11, y+6, fill=color, width=2)
        elif motif == "clamp":
            c.create_polygon(x, y+6, x+5, y-6, x+15, y-6, x+13, y-2,
                             x+8, y-2, x+5, y+6, fill="#bb9d27", outline=color)
        elif motif == "returned":
            c.create_rectangle(x+5, y-6, x+17, y+3, outline=color)
            c.create_line(x+16, y+6, x+1, y+6, x+5, y+2, fill=color, width=2)
        elif motif == "broken":
            c.create_line(x, y+5, x+6, y-3, x+9, y+1, fill=color, width=2)
            c.create_line(x+12, y-1, x+15, y+3, x+20, y-5, fill=color, width=2)
        else:
            c.create_line(x+1, y+3, x+17, y+3, fill=color, width=2)
            c.create_line(x+1, y, x+12, y, fill=color)

    def _paint(self):
        c = self.body
        c.delete("all")
        width = max(1, c.winfo_width() - 2)
        if not self.rows:
            c.configure(scrollregion=(0, 0, width, 0))
            c.create_text(8, 5, anchor="nw", width=max(45, width-16), font=self.action_face,
                          fill=INK, text=self.step or "No role records available in this view.")
            return
        height = self.row_height
        c.configure(scrollregion=(0, 0, width, len(self.rows)*height))
        role_end = max(82, min(158, int(width*.43)))
        for index, (role, status) in enumerate(self.rows):
            top = index * height
            mid = top + height // 2
            selected = index == self.selected
            side = role.strip().lower().startswith("/btw")
            paper = ("#fff1cd" if selected else "#e7d5dc" if side else
                     (PAPER if index % 2 else LINING))
            c.create_rectangle(0, top, width, top+height, width=0, fill=paper)
            c.create_line(0, top+height-1, width, top+height-1, fill="#a29778")
            c.create_line(role_end, top, role_end, top+height, fill="#bcaa89")
            if selected:
                c.create_rectangle(0, top, 4, top+height-1, width=0, fill=CARBON)
            motif, color = _motif(status)
            self._draw_mark(motif, color, 9, mid)
            side_label = "SIDE" if side and role_end >= 110 else ""
            c.create_text(34, mid, text=_fitted(role, self.role_face, role_end-(69 if side_label else 36)),
                          font=self.role_face, fill=INK, anchor="w")
            remaining = width - role_end - 10
            c.create_text(role_end+6, mid, anchor="w", font=self.action_face, fill=INK,
                          text=_fitted(status, self.action_face, remaining))
            if side_label:
                c.create_text(role_end-4, mid, anchor="e", font=self.code_face, fill=CARBON, text=side_label)

    def _detail(self):
        if not self.expanded or not self.rows:
            self.inset.grid_remove()
            return
        role, status = self.rows[self.selected]
        side = role.strip().lower().startswith("/btw")
        origin = "Side chat · outside run" if side else self.step or "Current run"
        if self.archive:
            origin = "Saved reviewer input"
        self.origin.configure(text=f"{role}   /   {origin}")
        if self.note_text != (role, status):
            self.note_text = (role, status)
            self.note.configure(state="normal")
            self.note.delete("1.0", "end")
            self.note.insert("1.0", status)
            self.note.configure(state="disabled")
        self.inset.grid(row=2, column=0, columnspan=2, sticky="ew")

    def _close(self):
        self.expanded = False
        self._detail()
        self.body.focus_set()
        return "break"

    def _toggle(self):
        if self.rows:
            self.expanded = not self.expanded
            self._detail()
        return "break"

    def _click(self, event):
        self.body.focus_set()
        index = int(self.body.canvasy(event.y) // self.row_height)
        if 0 <= index < len(self.rows):
            self.expanded = not self.expanded or index != self.selected
            self.selected = index
            self._paint()
            self._detail()

    def _move(self, movement):
        if self.rows:
            last = len(self.rows) - 1
            self.selected = (0 if movement == "first" else last if movement == "last" else
                             max(0, min(last, self.selected + movement)))
            height = self.row_height
            top = self.body.canvasy(0)
            bottom = top + self.body.winfo_height()
            start = self.selected * height
            if start < top or start + height > bottom:
                position = start if start < top else start + height - self.body.winfo_height()
                self.body.yview_moveto(max(0, position) / (len(self.rows)*height))
            self._paint()
            self._detail()
        return "break"
