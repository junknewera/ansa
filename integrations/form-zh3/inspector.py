"""Native source, patch and evidence inspector for Form Ж-3."""
import json
import math
import random
import re
import tkinter as tk
from tkinter import ttk


PAPER = "#f4eadd"
INK = "#231c23"
PINK = "#ed8fba"
PURPLE = "#885070"
FONT = ("DejaVu Sans", 9)
MONO = ("DejaVu Sans Mono", 10)
TABS = ("Source", "Diff", "Markdown Preview")
HUNK = re.compile(r"^@@ -([0-9]{1,19})(?:,[0-9]+)? \+([0-9]{1,19})(?:,[0-9]+)? @@(.*)$")


def patch_hunks(patch):
    """Keep the original text and both line number spaces, including empty rows."""
    groups = []
    current = None
    for line in patch.split("\n"):
        match = HUNK.match(line)
        if match:
            old, new = int(match[1]), int(match[2])
            current = {"title": line, "rows": []}
            groups.append(current)
        elif line.startswith("diff --git "):
            current = None
        elif current is not None and line.startswith((" ", "-", "+")):
            kind = line[0]
            current["rows"].append((kind, old if kind != "+" else None,
                                    new if kind != "-" else None, line[1:]))
            old += kind != "+"
            new += kind != "-"
    return groups


def paper_text(parent, wrap="none"):
    frame = tk.Frame(parent, bg=PAPER, bd=1, relief="sunken")
    frame.rowconfigure(0, weight=1)
    frame.columnconfigure(0, weight=1)
    field = tk.Text(frame, height=1, width=1, font=MONO, bg=PAPER, fg=INK,
                    bd=0, padx=5, pady=2, wrap=wrap, highlightthickness=1,
                    highlightbackground=PAPER, highlightcolor=PURPLE, takefocus=True,
                    selectbackground="#63537a", selectforeground="white", state="disabled")
    scroll = ttk.Scrollbar(frame, orient="vertical", command=field.yview)
    field.configure(yscrollcommand=scroll.set)
    field.grid(row=0, column=0, sticky="nsew")
    scroll.grid(row=0, column=1, sticky="ns")
    if wrap == "none":
        # Keep the gutter stable: Tk computes horizontal fractions from the
        # visible lines, which themselves change when a scrollbar is hidden.
        pan = tk.Scrollbar(frame, orient="horizontal", command=field.xview,
                           width=7, bd=0, highlightthickness=0, takefocus=False)
        pan.grid(row=1, column=0, sticky="ew")
        field.configure(xscrollcommand=pan.set)
    field.tag_configure("minus", background="#ffdf99", foreground="#4e2622")
    field.tag_configure("plus", background="#cee49f", foreground="#183322")
    field.tag_configure("header", foreground="#59416a")
    field.tag_configure("heading", font=("DejaVu Serif", 14, "bold"), spacing1=4)
    field.tag_configure("code", font=MONO, background="#e5ddc9")
    field.tag_configure("quote", foreground="#575061", lmargin1=10, lmargin2=10)
    field.bind("<Button-1>", lambda event: field.focus_set(), add="+")
    return frame, field


def fill_text(field, entries):
    field.configure(state="normal")
    field.delete("1.0", "end")
    for content, tag in entries:
        field.insert("end", content, tag)
    field.configure(state="disabled")


def display_evidence(value):
    if value in (None, "", [], {}):
        return "No evidence is attached to this file."
    if isinstance(value, str):
        return value
    return json.dumps(value, ensure_ascii=False, indent=2, default=str)


class Inspector(tk.Frame):
    def __init__(self, parent, on_evidence=None):
        super().__init__(parent, bg="#bc927e", relief="ridge", bd=2)
        self.on_evidence = on_evidence
        self.documents = {}
        self.document = {}
        self.hunks = []
        self.hunk = 0
        self.raw = False
        self.tab = "Diff"
        self.drawer = None

        folder = tk.Frame(self, bg="#8b7468", bd=1, relief="raised")
        folder.pack(fill="x")
        tk.Label(folder, text="Files", fg="white", bg="#76675c", padx=8,
                 font=("DejaVu Sans", 9, "bold")).pack(side="left", fill="y")
        self.file_name = tk.StringVar(self)
        self.file_menu = ttk.Combobox(folder, textvariable=self.file_name, state="readonly",
                                     width=14, font=MONO, takefocus=True)
        self.file_menu.pack(side="left", fill="x", expand=True)
        self.file_menu.bind("<<ComboboxSelected>>", lambda event: self.select_file(self.file_name.get()))
        stripes = tk.Canvas(folder, width=37, height=15, bg="#ee8594", highlightthickness=0)
        stripes.pack(side="right", fill="y")
        for x in range(0, 37, 8):
            stripes.create_rectangle(x, 0, x + 3, 45, fill="#fbd9ce", outline="")

        bar = tk.Frame(self, bg="#d2c7ba")
        bar.pack(fill="x")
        self.buttons = {}
        for name in TABS:
            self.buttons[name] = tk.Button(bar, text=name, font=FONT, cursor="hand2",
                                           bg="#ded8cf", padx=7, pady=1, bd=2,
                                           command=lambda n=name: self.set_tab(n))
            self.buttons[name].pack(side="left")
        self.footnote = tk.Button(bar, text="³", bg="#f3d850", fg=INK, bd=2,
                                  font=("DejaVu Serif", 13, "bold"), pady=0, padx=6,
                                  command=self.toggle_evidence, takefocus=True)
        self.footnote.pack(side="right", fill="y")

        self.heading = tk.Label(self, font=("DejaVu Sans Mono", 10, "bold"), fg=INK,
                                bg="#e0c7dd", anchor="w", padx=6, bd=1, relief="sunken")
        self.heading.pack(fill="x")
        self.body = tk.Frame(self, bg=PAPER, bd=2, relief="groove")
        self.body.pack(expand=True, fill="both")
        self.body.columnconfigure(0, weight=1)
        self.body.rowconfigure(0, weight=1)

        self.sheet = tk.Frame(self.body, bg=PAPER)
        self.sheet.columnconfigure(0, weight=1)
        self.sheet.rowconfigure(0, weight=3)
        self.sheet.rowconfigure(2, weight=2)
        top, self.before = paper_text(self.sheet)
        top.grid(row=0, column=0, sticky="nsew")
        self.gasket = tk.Canvas(self.sheet, height=123, width=20, bg=PINK, highlightthickness=0,
                                cursor="hand2", takefocus=True)
        self.gasket.grid(row=1, column=0, sticky="ew")
        self.gasket.bind("<Configure>", self._draw_gasket)
        self.gasket.bind("<Button-1>", self._hit_gasket)
        self.gasket.bind("<Return>", lambda event: self.toggle_evidence())
        self.gasket.bind("<space>", lambda event: self.toggle_evidence())
        self.gasket.bind("<FocusIn>", lambda event: self.gasket.itemconfigure("focus", state="normal"))
        self.gasket.bind("<FocusOut>", lambda event: self.gasket.itemconfigure("focus", state="hidden"))
        bottom, self.after = paper_text(self.sheet)
        bottom.grid(row=2, column=0, sticky="nsew")
        self.rulers = tk.Canvas(self.sheet, height=68, width=20, highlightthickness=0)
        self.rulers.grid(row=3, column=0, sticky="ew")
        self.rulers.bind("<Configure>", self._draw_rulers)
        self.sheet.bind("<Configure>", self._scale_sheet)
        self.page_frame, self.page = paper_text(self.body)
        self.patch_tools = tk.Frame(self, bg="#cdc3ad")
        self.counter = tk.Label(self.patch_tools, bg="#cdc3ad", fg=INK, font=FONT)
        self.counter.pack(side="left", padx=4)
        self.prev = tk.Button(self.patch_tools, text="‹", font=FONT, bd=1, pady=0,
                              command=lambda: self._move_hunk(-1))
        self.next = tk.Button(self.patch_tools, text="›", font=FONT, bd=1, pady=0,
                              command=lambda: self._move_hunk(1))
        self.prev.pack(side="left")
        self.next.pack(side="left")
        self.raw_button = tk.Button(self.patch_tools, text="Full diff", font=FONT, bd=1, pady=0,
                                    command=self._toggle_raw)
        self.raw_button.pack(side="right")
        self.set_documents([])
        self.bind("<Configure>", self._position_drawer)
        for widget in (self.file_menu, self.gasket, self.before, self.after, self.page, *self.buttons.values(), self.footnote):
            widget.bind("<F3>", lambda event: self.toggle_evidence())

    def _scale_sheet(self, event):
        size = min(155, max(24, int(event.height * .36)))
        if event.height < 156:
            self.rulers.grid_remove()
            size = min(38, size)
        elif not self.rulers.winfo_manager():
            self.rulers.grid(row=3, column=0, sticky="ew")
        ruler = min(85, max(30, int(event.height * .18)))
        if int(self.gasket["height"]) != size:
            self.gasket.configure(height=size)
        if int(self.rulers["height"]) != ruler:
            self.rulers.configure(height=ruler)

    def set_documents(self, documents):
        files = {doc["name"]: dict(doc) for doc in documents if doc.get("name")}
        name = self.file_name.get()
        name = name if name in files else next(iter(files), "")
        if self.documents == files and self.file_menu.cget("values") and self.file_name.get() == name:
            return
        self.documents = files
        self.file_menu.configure(values=tuple(files))
        self.select_file(name)

    def select_file(self, name):
        doc = self.documents.get(name, {})
        if doc and doc == self.document and self.file_name.get() == name:
            return
        self._close_evidence()
        self.file_name.set(name if doc else "")
        self.document = doc
        self.hunks = patch_hunks(doc.get("diff") or "")
        self.hunk = 0
        self.raw = False
        self._render()

    def set_tab(self, tab):
        if tab not in TABS:
            raise ValueError("Unknown inspector tab: " + str(tab))
        self._close_evidence()
        self.tab = tab
        self._render()

    def _toggle_raw(self):
        self.raw = not self.raw
        self._render()

    def _move_hunk(self, offset):
        self.hunk = max(0, min(len(self.hunks) - 1, self.hunk + offset))
        self._render()

    def _render(self):
        doc = self.document
        for label, button in self.buttons.items():
            active = label == self.tab
            button.configure(relief="sunken" if active else "raised", bg=PINK if active else "#ddd8cf",
                             font=("DejaVu Sans", 9, "bold italic" if active else "normal"))
        self.sheet.grid_remove()
        self.page_frame.grid_remove()
        self.patch_tools.pack_forget()
        path = doc.get("name", "No file selected")
        is_diff = self.tab == "Diff"
        split = is_diff and self.hunks and not self.raw
        count = f"  —  hunk {self.hunk + 1}/{len(self.hunks)}" if split else ""
        comparison = doc.get("comparison") if is_diff else None
        self.heading.configure(text=path + count + (f"  ·  {comparison}" if comparison else ""))
        if is_diff and self.hunks:
            self.patch_tools.pack(fill="x", side="bottom", before=self.body)
            self.counter.configure(text=f"Hunk {self.hunk + 1} / {len(self.hunks)}")
            self.prev.configure(state="normal" if self.hunk > 0 and not self.raw else "disabled")
            self.next.configure(state="normal" if self.hunk + 1 < len(self.hunks) and not self.raw else "disabled")
            self.raw_button.configure(text="Gasket view" if self.raw else "Full diff")
        if split:
            self.sheet.grid(row=0, column=0, sticky="nsew")
            group = self.hunks[self.hunk]
            rows = group["rows"]
            changed = [i for i, row in enumerate(rows) if row[0] != " "]
            first = changed[0] if changed else 0
            last = changed[-1] if changed else len(rows) - 1
            before = [(group["title"] + "\n", "header")]
            after = []
            for i, (kind, old, new, line) in enumerate(rows):
                if old is not None and (i <= last or not changed):
                    before.append((f"{old:>4}  {kind}  {line}\n", "minus" if kind == "-" else ""))
                if new is not None and (i >= first or not changed):
                    after.append((f"{new:>4}  {kind}  {line}\n", "plus" if kind == "+" else ""))
            if not any(row[0] == "-" for row in rows) and not before[1:]:
                before.append(("     ∅  No old lines\n", "header"))
            if not after:
                after.append(("     ∅  No new lines\n", "header"))
            before[-1] = (before[-1][0].removesuffix("\n"), before[-1][1])
            after[-1] = (after[-1][0].removesuffix("\n"), after[-1][1])
            fill_text(self.before, before)
            fill_text(self.after, after)
            self.before.yview_moveto(1)
            self.after.yview_moveto(0)
            return
        self.page_frame.grid(row=0, column=0, sticky="nsew")
        self.page.configure(wrap="none", font=MONO)
        if not doc:
            entries = [("No documents in this workspace.", "header")]
        elif is_diff:
            patch = doc.get("diff") or ""
            entries = [(line, "plus" if line.startswith("+") else "minus" if line.startswith("-")
                        else "header" if line.startswith("@@") else "") for line in patch.splitlines(True)]
            if not entries:
                entries = [("No patch is available for this file.", "header")]
        elif self.tab == "Source":
            source = doc.get("source")
            entries = [(source, "")] if isinstance(source, str) else [("Source unavailable.", "header")]
        else:
            self.page.configure(wrap="word", font=("DejaVu Sans", 10))
            entries = self._markdown(doc)
        fill_text(self.page, entries)
        self.page.yview_moveto(0)

    @staticmethod
    def _markdown(doc):
        source = doc.get("source")
        if not isinstance(source, str):
            return [("Source unavailable.", "header")]
        markdown = (doc.get("format") or "").lower() in ("md", "markdown", "text/markdown")
        if not markdown and not doc.get("name", "").lower().endswith((".md", ".markdown")):
            return [("Markdown preview is available for Markdown files.", "header")]
        entries = []
        fence = ""
        for line in source.splitlines(keepends=True):
            markup = line.lstrip()
            if fence:
                if re.match(rf"^{re.escape(fence)}[{re.escape(fence[0])}]*\s*$", markup):
                    fence = ""
                else:
                    entries.append((line, "code"))
                continue
            opening = re.match(r"^(`{3,}|~{3,})", markup)
            if opening:
                fence = opening[1]
            elif re.match(r"^#{1,6}\s", markup):
                entries.append((markup.lstrip("#").lstrip(" "), "heading"))
            elif markup.startswith("> "):
                entries.append((markup[2:], "quote"))
            elif markup.startswith(("- ", "* ")):
                entries.append(("• " + markup[2:], ""))
            else:
                entries.append((line, ""))
        return entries

    def _close_evidence(self):
        if self.drawer is not None:
            self.drawer.destroy()
            self.drawer = None

    def _position_drawer(self, _event=None):
        if self.drawer is None:
            return
        parent = self.master
        left = self.winfo_x()
        top = self.body.winfo_rooty() - parent.winfo_rooty()
        if left >= 265:
            # The evidence office owns the edge of its neighbour's register.
            # Keep the marked hole and both halves of the patch visible.
            width = min(350, max(240, int(left*.43)))
            height = min(169, max(135, self.body.winfo_height()//2))
            x = left - width + 5
            seam = self.gasket.winfo_rooty() - parent.winfo_rooty() if self.gasket.winfo_ismapped() else top
            # Anchor near the lower jaw to keep the agent's active row and
            # the task title readable while the receipt is open.
            offset = self.gasket.winfo_height()+42 if self.gasket.winfo_ismapped() else 9
            y = min(max(0, seam + offset), max(0, parent.winfo_height()-height))
        elif parent.winfo_height()-self.winfo_y()-self.winfo_height() >= 112:
            width = min(360, self.winfo_width()-6)
            height = min(146, parent.winfo_height()-self.winfo_y()-self.winfo_height())
            x, y = left+2, self.winfo_y()+self.winfo_height()
        else:
            # A stand-alone inspector has no neighbouring office. Dock the
            # receipt in its own rows, leaving the source selectable.
            if self.drawer.winfo_manager() != "pack":
                self.drawer.place_forget()
                self.drawer.pack(in_=self, side="bottom", fill="x", before=self.body)
                self.drawer.grid_propagate(False)
            self.drawer.configure(height=min(123, max(78, self.winfo_height()//4)))
            return
        if self.drawer.winfo_manager() == "pack":
            self.drawer.pack_forget()
            self.drawer.grid_propagate(True)
        self.drawer.place(x=x, y=y, width=width, height=height)
        self.drawer.lift()

    def toggle_evidence(self):
        if self.drawer is not None:
            self._close_evidence()
            self.footnote.focus_set()
            return
        drawer = tk.Frame(self.master, bg="#e6c32a", bd=4, relief="raised")
        self.drawer = drawer
        self._position_drawer()
        drawer.columnconfigure(1, weight=1)
        drawer.rowconfigure(1, weight=1)
        tk.Label(drawer, text="3", fg="#191107", bg="#f5d633", font=("DejaVu Serif", 47, "bold italic"),
                 bd=3, relief="raised").grid(row=0, column=0, rowspan=2, sticky="ns")
        bar = tk.Frame(drawer, bg="#eddb6c")
        bar.grid(row=0, column=1, sticky="ew")
        tk.Label(bar, text="Evidence — Footnotes (³)", fg=INK, bg="#eddb6c",
                 font=("DejaVu Sans", 10, "bold")).pack(side="left")
        tk.Button(bar, text="×", command=self.toggle_evidence, pady=0, bd=1).pack(side="right")
        frame, field = paper_text(drawer, wrap="word")
        frame.grid(row=1, column=1, sticky="nsew")
        field.configure(bg="#e6edb7", font=MONO)
        fill_text(field, [(self.document.get("name", "No file") + "\n\n", "header"),
                          (display_evidence(self.document.get("evidence")), "")])
        field.focus_set()
        field.bind("<Escape>", lambda event: self.toggle_evidence())
        field.bind("<F3>", lambda event: self.toggle_evidence())
        if self.on_evidence is not None:
            self.on_evidence(self.document)

    def _draw_gasket(self, event):
        c, w, h = self.gasket, event.width, event.height
        c.delete("all")
        if w < 20 or h < 12:
            return
        upper, lower = int(h*.13), int(h*.86)
        edge = max(4, min(13, w//48))
        nick = max(4, edge//2)
        shape = [edge, upper, w-edge*2, upper, w-edge*2, upper+nick, w-edge, upper+nick,
                 w-edge, h*.4, w-edge*2, h*.4, w-edge*2, h*.64, w-2, h*.64,
                 w-2, lower, edge, lower, edge, h*.78, 2, h*.78, 2, h*.31, edge, h*.31]
        c.configure(bg="#8e7649")
        c.create_polygon(*shape, outline="#622f46", width=6, fill="#d76f90")
        c.create_polygon(*shape, outline="#ffbfbe", width=3, fill="#e989a0")
        # Fine grain on the cast surface, deterministic across redraws.
        rng = random.Random(73)
        for _ in range(min(1900, w*h//75)):
            x = rng.randrange(edge+1, max(edge+2, w-2*edge))
            y = rng.uniform(upper+3, lower-3)
            c.create_line(x, y, x+rng.choice((1, 2)), y+1,
                          fill=rng.choice(("#f0a1b5", "#df7897", "#ea95a6", "#f3abb7")))
        c.create_line(edge, upper+2, w-edge*2, upper+2, fill="#f8cad1", width=2)
        c.create_line(edge, lower-1, w-3, lower-1, fill="#a54967", width=3)

        def teeth(base, direction, pitch, phase, depths, hue):
            y0 = 0 if direction == 1 else h
            c.create_rectangle(0, base, w, y0, fill=hue, outline="")
            c.create_line(0, base, w, base, fill="#a58a48", width=3)
            c.create_line(0, base-direction, w, base-direction, fill="#fff2a9", width=2)
            x, i = phase, 0
            while x < w:
                width = pitch * (.45, .58, .43, .53)[i % 4]
                stop = base + direction * (h * depths[i % len(depths)])
                left, right = x+2, min(w, x+width-2)
                points = [x, base-direction*2, x+width, base-direction*2, right, stop, left, stop]
                shadow = [v+2 for v in points]
                c.create_polygon(*shadow, fill="#795448", outline="#795448")
                c.create_polygon(*points, fill=hue, outline="#b39454")
                c.create_line(x+1, base, left+1, stop-direction, right-1, stop-direction,
                              fill="#fff4b3", width=2)
                c.create_line(right, stop, x+width, base, fill="#a2883c", width=2)
                x += pitch
                i += 1
        pitch = min(72, max(36, w/10.4))
        teeth(upper, 1, pitch, -pitch*.2, (.19, .11, .16, .22, .13), "#f3d45e")
        teeth(lower, -1, pitch*1.18, -3, (.18, .09, .22, .14), "#efdfa4")

        radius = min(h*.145, w*.032, 19)
        for pos, dy, angle in ((.075, .035, 11), (.287, -.02, 24), (.64, .04, 15), (.762, -.07, -19)):
            self._screw(c, w*pos, h*(.51+dy), radius, angle)
        cx, cy = w*.465, h*.515
        font = ("DejaVu Serif", -max(10, min(int(w*.042), int(h*.215))), "bold")
        for dx, dy, colour in ((1, 2, "#ffd0c6"), (-1, -1, "#c66c78"), (0, 0, "#7f324f")):
            c.create_text(cx+dx, cy+dy, text="0.8 units", font=font, fill=colour)
        c.create_line(cx-w*.095, cy+h*.135, cx+w*.095, cy+h*.135, fill="#983c57", width=2)
        x, y = w*.89, h*.55
        c.create_oval(x-radius, y-radius, x+radius, y+radius, fill="#6a3d43",
                      outline="#ffc1c6", width=2, tags="receipt")
        c.create_oval(x-radius*.82, y-radius*.83, x+radius*.81, y+radius*.8,
                      outline="#86573b", fill="#523d29", tags="receipt")
        c.create_text(x+radius*1.55, y-radius*.98, text="³", fill="#602139",
                      font=("DejaVu Serif", -max(13,int(radius*2.9)), "bold italic"), tags="receipt")
        c.create_rectangle(x-radius*1.45, y-radius*2.1, min(w, x+radius*2.6), y+radius*1.3,
                           outline="#6c3443", state="normal" if c.focus_get() is c else "hidden",
                           dash=(3, 2), tags="focus")


    def _hit_gasket(self, event):
        if event.x >= self.gasket.winfo_width() * .845:
            self.toggle_evidence()
        else:
            self.gasket.focus_set()

    @staticmethod
    def _screw(c, x, y, r, degrees):
        for dx, dy, size, colour in ((2, 3, 1.09, "#9e5f60"), (0, 0, 1.08, "#fff3db"),
                                     (0, 0, .93, "#785636"), (-1, -1, .79, "#c9ac78"),
                                     (0, 1, .66, "#a88c58")):
            q = r*size
            c.create_oval(x+dx-q, y+dy-q, x+dx+q, y+dy+q, outline=colour, fill=colour)
        for rotate in (0, math.pi/2):
            angle = math.radians(degrees) + rotate
            xx, yy = math.cos(angle)*r*.55, math.sin(angle)*r*.55
            c.create_line(x-xx+1, y-yy+1.2, x+xx+1, y+yy+1.2, fill="#efdb9d", width=max(2,r*.32))
            c.create_line(x-xx, y-yy, x+xx, y+yy, fill="#4c3928", width=max(2,r*.27))

    def _draw_rulers(self, event):
        c, w, h = self.rulers, event.width, event.height
        c.delete("all")
        bands = (("mm", "#f4e59c", 10, 0), ("in", "#e8bcdb", 1, 0),
                 ("pt", "#c7c8e6", 12, 0), ("cic", "#d2dfb0", 10, 0),
                 ("ver", "#e5c8b9", 10, 2850))
        namew = 35 if w >= 370 else 28
        for row, (unit, colour, step, start) in enumerate(bands):
            y1, y2 = int(h*row/5), int(h*(row+1)/5)-1
            c.create_rectangle(0, y1, w, y2, fill=colour, outline="#89766c")
            c.create_line(0, y1+1, w, y1+1, fill="#f8f3d9")
            c.create_line(namew, y1, namew, y2, fill="#907b72")
            size = -max(7, min(13, y2-y1-2))
            c.create_text(namew/2, (y1+y2)/2, text=unit, fill=INK, font=("DejaVu Sans", size, "bold"))
            ticks = max(32, min(64, w//9)) * (1.35 if row==1 else 1)
            x, idx = namew+(8 if row!=4 else 20), 0
            while x < w:
                if idx % 5 == 0:
                    c.create_text(x+2, (y1+y2)/2-1, anchor="w", text=str(start+(idx//5)*step),
                                  fill=INK, font=("DejaVu Sans Mono", size))
                depth = (y2-y1)*(.32 if idx%5==0 else .15)
                c.create_line(x, y2, x, y2-depth, fill="#68554f")
                x += ticks/5
                idx += 1
