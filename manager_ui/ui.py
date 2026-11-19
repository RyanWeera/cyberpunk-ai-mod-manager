"""The window. Tk runs on the main thread; Controller events arrive on a queue that _drain polls."""
import json
import os
import sys
import queue
import threading
import time
import tkinter as tk
import webbrowser
from datetime import UTC, datetime
from multiprocessing import AuthenticationError
from multiprocessing.connection import answer_challenge, deliver_challenge
from pathlib import Path
from tkinter import filedialog, font, messagebox, simpledialog, ttk

from ai_link import tools
from downloads import nexus
from downloads.fetch import FRAMEWORKS, thumbnails
from local_ai import llm
from manager_ui.controller import CORE, Controller
from shared.store import VERSION, Store, find_game, kind_of
# HOT = accent under the mouse, INK = text on the accent, SEL = selected rows and text
THEMES = {
    "Cyberpunk": {"BG": "#0e1016", "PANEL": "#181b24", "PANEL2": "#232837", "FG": "#e8e8e8", "DIM": "#8a8f9c", "ACCENT": "#fcee0a",
                  "HOT": "#fff35c", "INK": "#000000", "HOVER": "#30364a", "SEL": "#3a3f12", "BANNER": "#2b2610"},
    "Arasaka": {"BG": "#0a0a0b", "PANEL": "#151517", "PANEL2": "#212125", "FG": "#f2f2f2", "DIM": "#8e8e96", "ACCENT": "#e3122d",
                "HOT": "#ff3b52", "INK": "#ffffff", "HOVER": "#33333a", "SEL": "#4a0c15", "BANNER": "#2a0b10"},
    "Militech": {"BG": "#0f120d", "PANEL": "#191e16", "PANEL2": "#252c20", "FG": "#e4e8dc", "DIM": "#8c947e", "ACCENT": "#f29d35",
                 "HOT": "#ffb85c", "INK": "#000000", "HOVER": "#343d2c", "SEL": "#3e3216", "BANNER": "#2b2311"},
}
BG = PANEL = PANEL2 = FG = DIM = ACCENT = HOT = INK = HOVER = SEL = BANNER = ""  # set by use_theme


def use_theme(name):
    globals().update(THEMES.get(name) or THEMES["Cyberpunk"])


use_theme("Cyberpunk")
OK, BAD, WARN = "#3ddc84", "#ff6b6b", "#ffb347"
KEY_PAGE = "https://next.nexusmods.com/settings/api-keys"
EXAMPLES = ("https://www.nexusmods.com/cyberpunk2077/mods/107\n"
            "https://www.nexusmods.com/games/cyberpunk2077/collections/iszwwe\n"
            "https://github.com/psiberx/cp2077-codeware")
LEVEL = {"high": "Serious", "medium": "Warning", "info": "Info"}
ACTIVE = ("queued", "running", "waiting", "needkey")
CURRENT = "Current mod list"  # the pack picker item that launches the list as it is
ICONS = "Segoe Fluent Icons"  # Windows 11; App.__init__ picks the Windows 10 font when it is missing
# rail pages and their icon glyphs, top to bottom; Settings sits at the bottom
RAIL = (("Browse", "\ue774"), ("Mods", "\ue8f1"), ("My mod packs", "\ue7b8"), ("Downloads", "\ue896"), ("Crash doctor", "\ue90f"),
        ("Find mod from video", "\ue714"), ("AI helpers", "\ue99a"), (None, None), ("Add mods", "\ue710"))


class Tip:
    """Help text that shows while the mouse is over a widget."""

    def __init__(self, widget, text):
        self.w, self.text, self.top = widget, text, None
        widget.bind("<Enter>", self.show, add="+")
        widget.bind("<Leave>", self.hide, add="+")
        widget.bind("<ButtonPress>", self.hide, add="+")

    def show(self, _):
        self.hide(None)
        self.top = tk.Toplevel(self.w)
        self.top.wm_overrideredirect(True)
        self.top.wm_geometry(f"+{self.w.winfo_rootx() + 8}+{self.w.winfo_rooty() + self.w.winfo_height() + 4}")
        tk.Label(self.top, text=self.text() if callable(self.text) else self.text, bg=PANEL2, fg=FG, padx=8, pady=5, justify="left", wraplength=460,
                 font=("Segoe UI", 9)).pack()

    def hide(self, _):
        if self.top:
            self.top.destroy()
            self.top = None


def paint(img, shape, bg):
    """Draw shape(x, y) -> colour or None into img, smoothed with 3x3 samples per pixel. Edges blend into bg;
    pixels the shape does not touch stay see-through, so the picture fits any row or card colour."""
    rgb = lambda c: tuple(int(c[i:i + 2], 16) for i in (1, 3, 5))
    back = rgb(bg)
    for y in range(img.height()):
        row, clear = [], []
        for x in range(img.width()):
            hits = [shape(x + (i + 0.5) / 3, y + (j + 0.5) / 3) for i in range(3) for j in range(3)]
            if not any(hits):
                row.append(bg)
                clear.append(x)
                continue
            mix = [rgb(c) if c else back for c in hits]
            row.append("#" + "".join(f"{sum(m[k] for m in mix) // 9:02x}" for k in range(3)))
        img.put("{" + " ".join(row) + "}", to=(0, y))
        for x in clear:
            img.transparency_set(x, y, True)
    return img


def pill(x, y, w, h):
    """True inside a w x h stadium (a rectangle with round ends)."""
    rx = min(max(x, h / 2), w - h / 2)
    return (x - rx) ** 2 + (y - h / 2) ** 2 <= (h / 2) ** 2


def switch(on):
    """A toggle switch picture for the mod list: an accent pill with the knob right when on, a grey one when off."""
    w, h = 40, 22
    img = tk.PhotoImage(width=w, height=h)
    fill, knob, cx = (ACCENT, INK, w - h / 2) if on else (HOVER, DIM, h / 2)
    return paint(img, lambda x, y: knob if (x - cx) ** 2 + (y - h / 2) ** 2 <= (h / 2 - 4) ** 2 else
                 fill if pill(x, y, w, h) else None, PANEL)


def mark(img, kind, on):
    """A check box ("check") or option button ("radio") picture in the theme colours."""
    n = img.width()
    r = n / 2
    if kind == "radio":
        def shape(x, y):
            d = ((x - r) ** 2 + (y - r) ** 2) ** 0.5
            return (ACCENT if on and d <= r * 0.45 else (ACCENT if on else DIM) if r - 1.6 <= d <= r else
                    PANEL if d < r else None)
    else:
        def shape(x, y):
            k = n * 0.22  # corner radius
            cx, cy = min(max(x, k), n - k), min(max(y, k), n - k)
            d = ((x - cx) ** 2 + (y - cy) ** 2) ** 0.5
            if d > k:
                return None
            if not on:
                return DIM if d > k - 1.6 else PANEL
            for (ax, ay), (bx, by) in (((0.24, 0.52), (0.42, 0.7)), ((0.42, 0.7), (0.77, 0.3))):  # the tick
                ax, ay, bx, by = ax * n, ay * n, bx * n, by * n
                t = max(0, min(1, ((x - ax) * (bx - ax) + (y - ay) * (by - ay)) / ((bx - ax) ** 2 + (by - ay) ** 2)))
                if (x - ax - t * (bx - ax)) ** 2 + (y - ay - t * (by - ay)) ** 2 <= (n * 0.08) ** 2:
                    return INK
            return ACCENT
    return paint(img, shape, BG)


def short(n):
    """20381596 -> '20.4M', like a store page."""
    for size, unit in ((1e9, "B"), (1e6, "M"), (1e3, "K")):
        if n >= size:
            return f"{n / size:.1f}".removesuffix(".0") + unit
    return str(n)


def ago(iso, now=None):
    """'2026-11-17T10:00:00Z' -> '3 days ago'."""
    try:
        days = ((now or datetime.now(UTC)) - datetime.fromisoformat(iso)).days
    except ValueError:
        return ""
    for size, unit in ((365, "year"), (30, "month"), (1, "day")):
        if days >= size:
            n = days // size
            return f"{n} {unit}{'s' if n > 1 else ''} ago"
    return "today"


class App:
    def __init__(self, root, server=None, first_url=None, register=None, registered=None, store=None, token=b"", owner=None):
        self.root, self.events, self.token = root, queue.Queue(), token
        self.handshakes, self.handshake_timeout = threading.BoundedSemaphore(4), 5.0
        self.register, self.registered = register, registered or (lambda: False)
        self.asked_nxm, self.nxm_owner = False, owner or (lambda: "")
        self.store = store or Store()
        if not self.store.game():
            self.store.settings["game_dir"] = find_game() or self.store.settings["game_dir"]
            self.store.save()
        self.ctl = Controller(self.store, lambda kind, payload=None: self.events.put((kind, payload)))
        if server:  # take links at once, before the window is built
            threading.Thread(target=self._serve, args=(server,), daemon=True).start()
        self.problems, self.was_busy, self.banner_hidden = [], False, False
        self.k = root.winfo_fpixels("1i") / 96  # pixel sizes below are for 100% display scaling
        root.title(f"Cyberpunk 2077 AI Mod Manager {VERSION}")
        root.protocol("WM_DELETE_WINDOW", self.close)
        root.geometry(f"{self.px(1180)}x{self.px(760)}")
        root.minsize(self.px(900), self.px(560))
        use_theme(self.store.settings["theme"])
        root.configure(bg=BG)
        if ICONS not in font.families(root):
            globals()["ICONS"] = "Segoe MDL2 Assets"  # Windows 10 has the same glyphs under this name
        icon = Path(__file__).with_name("icon.png")
        if icon.exists():
            self.icon = tk.PhotoImage(file=str(icon))
            root.iconphoto(True, self.icon)
        self._style()
        self._header()
        self._statusbar()
        body = ttk.Frame(root)
        body.pack(fill="both", expand=True)
        self.rail = ttk.Frame(body, style="Rail.TFrame", padding=(8, 10))
        self.rail.pack(side="left", fill="y")
        ttk.Frame(body, style="Line.TFrame", width=1).pack(side="left", fill="y")
        self.tabs = ttk.Notebook(body, style="Pages.TNotebook")
        self.tabs.pack(side="left", fill="both", expand=True, padx=(8, 12), pady=(4, 4))
        self.tab, self.scrollers, self.rail_items, self.badges = {}, [], {}, {}
        root.bind_all("<MouseWheel>", self._wheel)
        for name in ("Browse", "Mods", "My mod packs", "Add mods", "Find mod from video", "Downloads", "Crash doctor", "AI helpers", "Settings"):
            f = self.tab[name] = ttk.Frame(self.tabs, padding=12)
            self.tabs.add(f, text=name)
            getattr(self, "_tab_" + name.split()[0].lower())(f)
        self._rail()
        self.refresh_mods()
        self.refresh_setup()
        self.refresh_history()
        self.tick = root.after(150, self._drain)
        if first_url:
            self.ctl.bg(self.ctl.handle_nxm, first_url)  # a bad link (e.g. another game) becomes a message, not a crash
        self.tabs.bind("<<NotebookTabChanged>>", lambda e: self._page_changed())
        if not self.store.game():
            self.tabs.select(self.tab["Settings"])
        self._page_changed()

    def px(self, n):
        return int(n * self.k)

    # ---------- look ----------
    def _style(self):
        s = ttk.Style()
        s.theme_use("clam")
        s.configure(".", background=BG, foreground=FG, fieldbackground=PANEL, bordercolor=PANEL2, lightcolor=BG, darkcolor=BG,
                    troughcolor=BG, font=("Segoe UI", 10))
        s.configure("Pages.TNotebook", background=BG, borderwidth=0, tabmargins=0, bordercolor=BG, lightcolor=BG, darkcolor=BG)
        s.layout("Pages.TNotebook.Tab", [])  # the icon rail picks the page
        s.configure("Rail.TFrame", background=PANEL)
        s.configure("Line.TFrame", background=PANEL2)
        for name, bg, fg in (("Rail", PANEL, DIM), ("RailOn", PANEL2, ACCENT)):
            s.configure(f"{name}.TLabel", background=bg, foreground=fg, font=(ICONS, 15), anchor="center", padding=(0, 9), width=4)
            s.map(f"{name}.TLabel", background=[("active", HOVER)], foreground=[("active", ACCENT)])
        s.configure("Badge.TLabel", background=ACCENT, foreground=INK, font=("Segoe UI Semibold", 7), padding=(4, 0))
        s.configure("Outline.TButton", background=BG, foreground=FG, bordercolor=PANEL2, lightcolor=BG, darkcolor=BG, borderwidth=1)
        s.map("Outline.TButton", background=[("active", HOVER)], bordercolor=[("active", ACCENT)])
        s.configure("Get.TButton", background=PANEL, foreground=ACCENT, bordercolor=ACCENT, lightcolor=PANEL, darkcolor=PANEL,
                    borderwidth=1, font=("Segoe UI Semibold", 10))
        s.map("Get.TButton", background=[("active", HOVER), ("disabled", PANEL)], foreground=[("disabled", DIM)],
              bordercolor=[("disabled", PANEL2)])
        s.configure("Link.TButton", background=BG, foreground=ACCENT, padding=(8, 4), font=("Segoe UI Semibold", 10))
        s.map("Link.TButton", background=[("active", PANEL)], foreground=[("active", HOT)])
        s.configure("Pill.Toolbutton", background=PANEL, foreground=FG, padding=(14, 4), borderwidth=0, anchor="center")
        s.map("Pill.Toolbutton", background=[("selected", SEL), ("active", HOVER)], foreground=[("selected", ACCENT)])
        s.configure("Page.TButton", background=PANEL, foreground=FG, padding=(8, 4), width=-3, anchor="center")
        s.map("Page.TButton", background=[("active", HOVER), ("disabled", BG)], foreground=[("disabled", DIM)])
        s.configure("PageOn.TButton", background=ACCENT, foreground=INK, padding=(8, 4), width=-3, anchor="center",
                    font=("Segoe UI Semibold", 10))
        s.map("PageOn.TButton", background=[("active", HOT)])
        s.configure("Mods.Treeview", rowheight=self.px(54))
        s.configure("Mods.Treeview.Heading", background=PANEL, foreground=DIM, font=("Segoe UI Semibold", 10), padding=(6, 8))
        s.map("Mods.Treeview.Heading", background=[("active", PANEL)])
        s.configure("Head.TLabel", foreground=FG, font=("Segoe UI Semibold", 13))
        s.configure("Pack.TLabel", foreground=FG, font=("Segoe UI Semibold", 18))
        s.configure("Avatar.TLabel", background=PANEL2, foreground=ACCENT, font=("Segoe UI Black", 18), anchor="center", width=3,
                    padding=(0, 8))
        s.configure("Big.TLabel", foreground=DIM, font=(ICONS, 44))
        s.configure("Glyph.TLabel", foreground=DIM, font=(ICONS, 11))
        s.configure("Opt.TFrame", background=PANEL)
        s.configure("OptHot.TFrame", background=HOVER)
        for name, bg in (("Opt", PANEL), ("OptHot", HOVER)):
            s.configure(f"{name}Icon.TLabel", background=bg, foreground=ACCENT, font=(ICONS, 18))
            s.configure(f"{name}Title.TLabel", background=bg, foreground=FG, font=("Segoe UI Semibold", 11))
            s.configure(f"{name}Dim.TLabel", background=bg, foreground=DIM)
        s.configure("TButton", background=PANEL2, foreground=FG, padding=(12, 6), borderwidth=0)
        s.map("TButton", background=[("active", HOVER), ("disabled", PANEL)], foreground=[("disabled", DIM)])
        s.configure("Big.TButton", background=ACCENT, foreground=INK, font=("Segoe UI Semibold", 10))
        s.map("Big.TButton", background=[("active", HOT)])
        s.configure("Treeview", background=PANEL, fieldbackground=PANEL, foreground=FG, rowheight=self.px(26), borderwidth=0)
        s.configure("Treeview.Heading", background=PANEL2, foreground=ACCENT, borderwidth=0, padding=(6, 4))
        s.map("Treeview.Heading", background=[("active", PANEL2)])
        s.map("Treeview", background=[("selected", SEL)], foreground=[("selected", FG)])
        s.configure("TEntry", fieldbackground=PANEL, foreground=FG, insertcolor=FG, padding=5)
        s.configure("TCombobox", background=PANEL, arrowcolor=DIM, bordercolor=PANEL2, lightcolor=PANEL, darkcolor=PANEL, padding=(6, 3),
                    arrowsize=self.px(11))
        s.map("TCombobox", fieldbackground=[("readonly", PANEL)], foreground=[("readonly", FG)], background=[("active", HOVER)],
              arrowcolor=[("active", ACCENT)], bordercolor=[("focus", DIM), ("active", DIM)],
              selectbackground=[("readonly", PANEL)], selectforeground=[("readonly", FG)])
        s.configure("Search.TEntry", fieldbackground=PANEL, bordercolor=PANEL, lightcolor=PANEL, darkcolor=PANEL, padding=(6, 7))
        s.map("Search.TEntry", bordercolor=[("focus", PANEL)], lightcolor=[("focus", PANEL)])
        s.configure("Hint.TLabel", background=PANEL, foreground=DIM, font=("Segoe UI", 11))
        thumb = {"expand": "1", "sticky": "nswe"}  # thin and flat: no arrow buttons
        s.layout("Vertical.TScrollbar", [("Vertical.Scrollbar.trough", {"sticky": "ns", "children": [("Vertical.Scrollbar.thumb", thumb)]})])
        s.configure("Vertical.TScrollbar", background=HOVER, troughcolor=BG, bordercolor=BG, lightcolor=HOVER, darkcolor=HOVER,
                    arrowsize=self.px(9), gripcount=0, borderwidth=0)
        s.map("Vertical.TScrollbar", background=[("pressed", DIM), ("active", DIM)], lightcolor=[("pressed", DIM), ("active", DIM)],
              darkcolor=[("pressed", DIM), ("active", DIM)])
        if not hasattr(self, "marks"):  # ttk keeps an element for good: make it once per window, then redraw its pictures
            n = self.px(17)
            self.marks = {(k, on): tk.PhotoImage(width=n, height=n) for k in ("check", "radio") for on in (True, False)}
            for k, name in (("check", "Checkbutton"), ("radio", "Radiobutton")):
                s.element_create(f"Mark{id(self)}.{k}", "image", self.marks[k, False], ("selected", self.marks[k, True]),
                                 sticky="w", width=n + self.px(7))  # the extra width is the gap before the text
                s.layout(f"T{name}", [(f"{name}.padding", {"sticky": "nswe", "children": [
                    (f"Mark{id(self)}.{k}", {"side": "left", "sticky": ""}),
                    (f"{name}.focus", {"side": "left", "sticky": "w", "children": [(f"{name}.label", {"sticky": "nswe"})]})]})])
        for (k, on), img in self.marks.items():
            mark(img, k, on)
        for opt, val in (("background", PANEL2), ("foreground", FG), ("selectBackground", ACCENT), ("selectForeground", INK)):
            self.root.option_add(f"*TCombobox*Listbox.{opt}", val)
        s.configure("Card.TFrame", bordercolor=PANEL, lightcolor=PANEL, darkcolor=PANEL, borderwidth=1, relief="solid")
        s.configure("CardHot.TFrame", background=PANEL, bordercolor=DIM, lightcolor=PANEL, darkcolor=PANEL, borderwidth=1, relief="solid")
        s.configure("Horizontal.TProgressbar", background=ACCENT, troughcolor=PANEL, borderwidth=0)
        s.configure("TCheckbutton", background=BG, foreground=FG, indicatorbackground=PANEL, indicatorforeground=ACCENT)
        s.map("TCheckbutton", background=[("active", BG)], indicatorbackground=[("selected", PANEL2)])
        s.configure("TRadiobutton", background=BG, foreground=FG, indicatorbackground=PANEL, indicatorforeground=ACCENT)
        s.map("TRadiobutton", background=[("active", BG)])
        s.configure("Dim.TLabel", foreground=DIM)
        s.configure("Title.TLabel", foreground=ACCENT, font=("Segoe UI Semibold", 15))
        s.configure("Section.TLabel", foreground=ACCENT, font=("Segoe UI Semibold", 11))
        s.configure("Card.TFrame", background=PANEL)
        s.configure("Card.TLabel", background=PANEL, foreground=FG)
        s.configure("CardDim.TLabel", background=PANEL, foreground=DIM)
        s.configure("Banner.TFrame", background=BANNER)
        s.configure("Banner.TLabel", background=BANNER, foreground=ACCENT)
        s.configure("CardTitle.TLabel", background=PANEL, foreground=FG, font=("Segoe UI Semibold", 12))
        s.configure("CardLink.TLabel", background=PANEL, foreground=ACCENT, font=("Segoe UI Semibold", 12, "underline"))
        s.configure("Chip.TLabel", background=PANEL2, foreground=DIM, padding=(8, 1), font=("Segoe UI", 9))
        s.configure("Side.TCheckbutton", padding=(0, 3))

    def _buttons(self, parent, spec):
        """spec: (label, command, tooltip). Labels starting with ▶ get the main colour."""
        bar = ttk.Frame(parent, style=parent.cget("style") or "TFrame")
        for label, cmd, tip in spec:
            b = ttk.Button(bar, text=label, command=cmd, style="Big.TButton" if label.startswith("▶") else "TButton")
            b.pack(side="left", padx=(0, 6))
            if tip:
                Tip(b, tip)
        return bar

    def _tree(self, parent, cols, widths, height=14, select="browse"):
        frame = ttk.Frame(parent)
        t = ttk.Treeview(frame, columns=cols, show="headings", height=height, selectmode=select)
        for c, w in zip(cols, widths, strict=True):
            t.heading(c, text=c, anchor="w")
            t.column(c, width=self.px(w), anchor="w", stretch=w > 150)
        sb = ttk.Scrollbar(frame, orient="vertical", command=t.yview)
        t.configure(yscrollcommand=sb.set)
        t.pack(side="left", fill="both", expand=True)
        sb.pack(side="right", fill="y")
        for tag, color in (("off", DIM), ("done", OK), ("failed", BAD), ("waiting", WARN), ("needkey", WARN), ("queued", DIM),
                           ("high", BAD), ("medium", WARN), ("info", DIM)):
            t.tag_configure(tag, foreground=color)
        return frame, t

    def _text(self, parent, height, font=("Consolas", 10)):
        return tk.Text(parent, height=height, bg=PANEL, fg=FG, insertbackground=FG, relief="flat", wrap="word", font=font,
                       padx=8, pady=6, highlightthickness=0, selectbackground=SEL)

    def _header(self):
        head = ttk.Frame(self.root, padding=(14, 10, 16, 10))
        head.pack(fill="x")
        if hasattr(self, "icon"):
            self.logo = self.icon.subsample(max(1, self.icon.width() // self.px(30)))
            ttk.Label(head, image=self.logo).pack(side="left", padx=(0, 10))
        ttk.Label(head, text="CP2077 AI Mod Manager", style="Head.TLabel").pack(side="left")
        ttk.Label(head, text="  ›", style="Dim.TLabel", font=("Segoe UI", 13)).pack(side="left")
        self.page_title = ttk.Label(head, style="Dim.TLabel", font=("Segoe UI", 13))
        self.page_title.pack(side="left", padx=(6, 0))
        b = ttk.Button(head, text="▶  Launch game", style="Big.TButton", command=self.launch)
        b.pack(side="right")
        Tip(b, "Copies your mods into the game, starts Cyberpunk 2077 and watches it for crashes.")
        self.pack_pick = ttk.Combobox(head, state="readonly", width=30, values=[CURRENT, *self.store.packs])
        self.pack_pick.set(self.store.pack if self.store.pack in self.store.packs else CURRENT)
        self.pack_pick.pack(side="right", padx=(0, 8))
        Tip(self.pack_pick, "The mod pack to launch. 'Current mod list' launches the mods as the Mods tab shows them.")
        ttk.Label(head, text="Mod pack", style="Dim.TLabel").pack(side="right", padx=(0, 6))
        self.game_pill = ttk.Label(head, text="● Game not running", style="Dim.TLabel")
        self.game_pill.pack(side="right", padx=14)
        ttk.Frame(self.root, style="Line.TFrame", height=1).pack(fill="x")
        self.banner = ttk.Frame(self.root, style="Banner.TFrame", padding=(14, 8))
        self.banner_text = ttk.Label(self.banner, style="Banner.TLabel")
        self.banner_text.pack(side="left")
        ttk.Button(self.banner, text="✕", width=3, command=self.hide_banner).pack(side="right")
        ttk.Button(self.banner, text="Open Settings", command=lambda: self.tabs.select(self.tab["Settings"])).pack(side="right", padx=6)

    def _rail(self):
        """The page bar on the left, like a launcher: an icon per page, its name shows on hover."""
        for name, glyph in (*RAIL, ("Settings", "\ue713")):
            if not name:
                ttk.Frame(self.rail, style="Line.TFrame", height=1).pack(fill="x", pady=8)
                continue
            b = self.rail_items[name] = ttk.Label(self.rail, text=glyph, style="Rail.TLabel", cursor="hand2")
            b.pack(side="bottom" if name == "Settings" else "top", pady=2)
            b.bind("<Enter>", lambda e, b=b: b.state(["active"]))
            b.bind("<Leave>", lambda e, b=b: b.state(["!active"]))
            b.bind("<Button-1>", lambda e, n=name: self.add_dialog() if n == "Add mods" else self.tabs.select(self.tab[n]))
            Tip(b, lambda n=name: "Add mods" if n == "Add mods" else self.tabs.tab(self.tab[n], "text"))
            if name in ("Downloads", "Crash doctor"):
                self.badges[name] = ttk.Label(b, style="Badge.TLabel")

    def _page_changed(self):
        page = self.tabs.tab(self.tabs.select(), "text")
        self.page_title.configure(text=page)
        for name, b in self.rail_items.items():
            b.configure(style="RailOn.TLabel" if str(self.tab[name]) == str(self.tabs.select()) else "Rail.TLabel")
        self._browse_shown()

    def _statusbar(self):
        bar = ttk.Frame(self.root, padding=(16, 2, 16, 10))
        bar.pack(side="bottom", fill="x")
        self.status = ttk.Label(bar, style="Dim.TLabel", text="Ready.")
        self.status.pack(side="left", fill="x", expand=True)
        self.progress = ttk.Progressbar(bar, length=self.px(220), mode="determinate")
        self.progress_text = ttk.Label(bar, style="Dim.TLabel")
        self.progress_text.pack(side="right", padx=8)  # the bar shows only while mods download

    def _scroller(self, parent):
        """(outer frame, canvas, inner frame): a frame that scrolls with the mouse wheel."""
        outer = ttk.Frame(parent)
        c = tk.Canvas(outer, bg=BG, highlightthickness=0, bd=0)
        inner = ttk.Frame(c)
        win = c.create_window(0, 0, window=inner, anchor="nw")
        sb = ttk.Scrollbar(outer, orient="vertical", command=c.yview)
        c.configure(yscrollcommand=sb.set)
        inner.bind("<Configure>", lambda e: c.configure(scrollregion=c.bbox("all")))
        c.bind("<Configure>", lambda e: c.itemconfigure(win, width=e.width))
        c.pack(side="left", fill="both", expand=True)
        sb.pack(side="right", fill="y")
        self.scrollers.append(c)
        return outer, c, inner

    def _wheel(self, e):
        w = self.root.winfo_containing(e.x_root, e.y_root)
        for c in self.scrollers:
            if w and str(w).startswith(str(c)) and c.yview() != (0.0, 1.0):
                c.yview_scroll(-e.delta // 120, "units")

    # ---------- tabs ----------
    def _tab_browse(self, f):
        self.browse_req, self.browse_page, self.browse_total, self.browse_after = 0, 0, 0, None
        self.browse_buttons, self.thumb_imgs, self.cat_vars, self.pack_names = {}, [], {}, {}
        self.blank = tk.PhotoImage(width=self.px(160), height=self.px(90))
        kinds = ttk.Frame(f)
        kinds.pack(fill="x", pady=(0, 10))
        self.browse_kind = tk.StringVar(value="mods")
        for text, value, tip in (("Mods", "mods", "Single mods from Nexus Mods."),
                                 ("Mod packs", "packs", "Nexus collections: ready-made packs of many mods that work together.")):
            b = ttk.Radiobutton(kinds, text=text, value=value, variable=self.browse_kind, style="Pill.Toolbutton", command=self._browse_kind)
            b.pack(side="left", padx=(0, 6))
            Tip(b, tip)
        top = ttk.Frame(f)
        top.pack(fill="x")
        self.browse_q = tk.StringVar()
        self.browse_q.trace_add("write", lambda *_: self._browse_soon())
        box, entry = self._search_box(top, self.browse_q, "Search mods…")
        box.pack(fill="x")
        self.browse_hint = entry.winfo_children()[0]
        entry.bind("<Return>", lambda e: self.browse(0))
        Tip(entry, "Search Nexus Mods by name.")
        body = ttk.Frame(f)
        body.pack(fill="both", expand=True, pady=(10, 0))
        side_box, _, self.side = self._scroller(body)
        side_box.configure(width=self.px(290))
        side_box.pack_propagate(False)
        side_box.pack(side="left", fill="y", padx=(0, 12))
        ttk.Label(self.side, text="Categories", style="Section.TLabel").pack(anchor="w", pady=(0, 4))
        self.cat_box = ttk.Frame(self.side)
        self.cat_box.pack(fill="x")
        self.adult = tk.BooleanVar()
        ttk.Checkbutton(self.side, text="Show adult mods", variable=self.adult, style="Side.TCheckbutton",
                        command=lambda: self.browse(0)).pack(side="bottom", anchor="w", pady=(10, 0))
        main = ttk.Frame(body)
        main.pack(side="left", fill="both", expand=True)
        bar = ttk.Frame(main)
        bar.pack(fill="x", pady=(0, 6))
        self.browse_sort = ttk.Combobox(bar, state="readonly", width=17, values=list(nexus.SORTS))
        self.browse_view = ttk.Combobox(bar, state="readonly", width=4, values=["20", "50", "100"])
        for label, c in (("Sort by:", self.browse_sort), ("View:", self.browse_view)):
            ttk.Label(bar, text=label, style="Dim.TLabel").pack(side="left", padx=(0, 6))
            c.current(0)
            c.bind("<<ComboboxSelected>>", lambda e: self.browse(0))
            c.pack(side="left", padx=(0, 14))
        self.browse_info = ttk.Label(bar, style="Dim.TLabel", text="Loading mods from Nexus…")
        self.browse_info.pack(side="left")
        self.next_btn = ttk.Button(bar, text="›", style="Page.TButton", command=lambda: self.browse(self.browse_page + 1))
        self.next_btn.pack(side="right")
        self.pager = ttk.Frame(bar)
        self.pager.pack(side="right", padx=2)
        self.prev_btn = ttk.Button(bar, text="‹", style="Page.TButton", command=lambda: self.browse(self.browse_page - 1))
        self.prev_btn.pack(side="right")
        box, self.results_canvas, self.results = self._scroller(main)
        box.pack(fill="both", expand=True)

    def _browse_shown(self):
        if str(self.tabs.select()) == str(self.tab["Browse"]) and not self.browse_req:
            self.browse(0)

    def _browse_kind(self):
        """Mods <-> Mod packs: each has its own categories, so the list is built again on the next load."""
        for w in self.cat_box.winfo_children():
            w.destroy()
        self.cat_vars = {}
        self.browse_hint["text"] = "Search mod packs…" if self.browse_kind.get() == "packs" else "Search mods…"
        self.browse(0)

    def _browse_soon(self):
        """Search while the user types, once they pause."""
        if self.browse_after:
            self.root.after_cancel(self.browse_after)
        self.browse_after = self.root.after(450, lambda: self.browse(0))

    def browse(self, page):
        self.browse_after = None
        self.browse_req += 1
        req, words = self.browse_req, self.browse_q.get().strip()
        args = (words, [c for c, v in self.cat_vars.items() if v.get()], nexus.SORTS[self.browse_sort.get()], max(page, 0), self.adult.get(),
                int(self.browse_view.get()))
        self.browse_info.configure(text="Searching…", foreground=DIM)
        kind = self.browse_kind.get()
        find = nexus.browse_packs if kind == "packs" else nexus.browse

        def run():
            try:
                self.events.put(("browse", {"req": req, "page": args[3], "per": args[5], "kind": kind, **find(*args)}))
            except Exception as e:  # noqa: BLE001 - network boundary: shown in the tab
                self.events.put(("browse", {"req": req, "error": str(e)}))
        threading.Thread(target=run, daemon=True).start()

    def show_browse(self, p):
        if p["req"] != self.browse_req:
            return  # an older search finished after a newer one started
        if "error" in p:
            return self.browse_info.configure(text=f"Nexus Mods did not answer: {p['error']}", foreground=BAD)
        if not self.cat_vars and p["categories"]:  # counts of the first, unfiltered load: the whole game
            for name, n in sorted(p["categories"].items()):
                v = self.cat_vars[name] = tk.BooleanVar()
                ttk.Checkbutton(self.cat_box, text=f"{name}  ({n:,})", variable=v, style="Side.TCheckbutton",
                                command=lambda: self.browse(0)).pack(anchor="w")
        self.browse_page, self.browse_total = p["page"], p["total"]
        pages = max(1, -(-p["total"] // p.get("per", 20)))
        what = "mod packs" if p.get("kind") == "packs" else "mods"
        self.browse_info.configure(text=f"{p['total']:,} {what}" if p["total"] else f"No {what[:-1]} matches. Try fewer words or other categories.",
                                   foreground=FG if p["total"] else WARN)
        for w in self.pager.winfo_children():
            w.destroy()
        last = -1
        for n in sorted({0, pages - 1, *range(max(0, p["page"] - 1), min(pages, p["page"] + 2))}):  # 1 2 3 … 965
            if n - last > 1:
                ttk.Label(self.pager, text="…", style="Dim.TLabel").pack(side="left", padx=4)
            ttk.Button(self.pager, text=f"{n + 1:,}", style="PageOn.TButton" if n == p["page"] else "Page.TButton",
                       command=lambda n=n: self.browse(n)).pack(side="left", padx=2)
            last = n
        self.prev_btn.state(["!disabled" if p["page"] > 0 else "disabled"])
        self.next_btn.state(["!disabled" if p["page"] + 1 < pages else "disabled"])
        for w in self.results.winfo_children():
            w.destroy()
        self.browse_buttons, self.thumb_imgs, self.thumb_labels, self.pack_names = {}, [], {}, {}
        for m in p["mods"]:
            self._browse_card(m)
        self._browse_marks()
        self.results_canvas.yview_moveto(0)
        urls = [m["thumb"] for m in p["mods"] if m["thumb"]]
        if urls:
            req, w, h = p["req"], self.px(160), self.px(90)
            self.ctl.bg(lambda: self.events.put(("thumbs", {"req": req, "paths": thumbnails(urls, self.store.root / "thumbs", w, h)})))

    def _browse_card(self, m):
        pack = "slug" in m
        page = (f"https://www.nexusmods.com/games/{nexus.GAME}/collections/{m['slug']}" if pack
                else f"https://www.nexusmods.com/{nexus.GAME}/mods/{m['id']}")
        card = ttk.Frame(self.results, style="Card.TFrame", padding=10)
        card.pack(fill="x", pady=(0, 8), padx=(0, 6))
        card.columnconfigure(1, weight=1)

        def leave(e):  # moving onto a child also fires <Leave>, so look where the mouse really is
            w = str(self.root.winfo_containing(e.x_root, e.y_root) or "")
            if w != str(card) and not w.startswith(f"{card}."):
                card.configure(style="Card.TFrame")
        card.bind("<Enter>", lambda e: card.configure(style="CardHot.TFrame"))
        card.bind("<Leave>", leave)
        thumb = ttk.Label(card, image=self.blank, style="CardDim.TLabel", cursor="hand2")
        thumb.grid(row=0, column=0, rowspan=3, sticky="n", padx=(0, 12))
        thumb.bind("<Button-1>", lambda e: webbrowser.open(page))
        self.thumb_labels.setdefault(m["thumb"], []).append(thumb)
        head = ttk.Frame(card, style="Card.TFrame")
        head.grid(row=0, column=1, sticky="w")
        title = ttk.Label(head, text=m["name"], style="CardTitle.TLabel", cursor="hand2")
        title.pack(side="left")
        title.bind("<Button-1>", lambda e: webbrowser.open(page))
        title.bind("<Enter>", lambda e: title.configure(style="CardLink.TLabel"))
        title.bind("<Leave>", lambda e: title.configure(style="CardTitle.TLabel"))
        Tip(title, f"Open the {'mod pack' if pack else 'mod'}'s page on Nexus Mods.")
        if m["author"]:
            ttk.Label(head, text=f"  by {m['author']}", style="CardDim.TLabel").pack(side="left")
        summary = ttk.Label(card, text=m["summary"], style="Card.TLabel", justify="left")
        summary.grid(row=1, column=1, sticky="we", pady=(2, 6))
        def wrap(e):
            summary.configure(wraplength=max(200, e.width - self.px(420)))
            title.configure(wraplength=max(150, e.width - self.px(420) - self.px(8) * len(m["author"])))
        card.bind("<Configure>", wrap)
        chips = ttk.Frame(card, style="Card.TFrame")
        chips.grid(row=2, column=1, sticky="w")
        for text in (m["category"], f"{m['mod_count']:,} mods" if pack else ""):
            if text:
                ttk.Label(chips, text=text, style="Chip.TLabel").pack(side="left", padx=(0, 6))
        stats = ttk.Frame(card, style="Card.TFrame")
        stats.grid(row=0, column=2, rowspan=3, sticky="ne", padx=(12, 0))
        b = ttk.Button(stats, text="+  Install", style="Get.TButton", width=12, command=lambda: self.browse_install(m))
        b.pack(anchor="e", pady=(0, 10))
        self.browse_buttons[m["id"]] = b
        if pack:
            self.pack_names[m["id"]] = m["name"].strip()
        line = ttk.Frame(stats, style="Card.TFrame")
        line.pack(anchor="e")
        for glyph, n, tip in (("\ue896", m["downloads"], "downloads"), ("\ueb51", m["endorsements"], "endorsements")):
            ttk.Label(line, text=glyph, style="CardDim.TLabel", font=(ICONS, 10)).pack(side="left", padx=(10, 4))
            num = ttk.Label(line, text=short(n), style="CardTitle.TLabel", font=("Segoe UI Semibold", 10))
            num.pack(side="left")
            Tip(num, f"{n:,} {tip}")
        if m["updated"]:
            ttk.Label(stats, text=f"Updated {ago(m['updated'])}", style="CardDim.TLabel").pack(anchor="e", pady=(4, 0))

    def show_thumbs(self, p):
        if p["req"] != self.browse_req:
            return
        for url, path in p["paths"].items():
            try:
                img = tk.PhotoImage(file=path)
            except tk.TclError:  # a broken cache file: the blank stays
                continue
            self.thumb_imgs.append(img)  # Tk drops images that Python does not keep
            for label in self.thumb_labels.get(url, ()):
                label.configure(image=img)

    def _browse_marks(self):
        """Install buttons say 'Installed' for mods that are in the mod list."""
        ids = {m["source"].get("mod_id") for m in self.store.mods}
        repos = {str(m["source"].get("repo", "")).lower() for m in self.store.mods}
        ids |= {nid for repo, nid in FRAMEWORKS.values() if repo.lower() in repos}
        ids |= {slug for slug, name in self.pack_names.items() if name in self.store.packs}
        for mod_id, b in self.browse_buttons.items():
            if mod_id in ids:
                b.configure(text="✔  Installed", style="TButton")
                b.state(["disabled"])

    def browse_install(self, m):
        if "slug" in m:
            self.ctl.add_links(f"https://www.nexusmods.com/games/{nexus.GAME}/collections/{m['slug']}")
            msg = (f"Added the mod pack {m['name']} ({m['mod_count']:,} mods). I download and set up each mod, and save the pack "
                   "so you can pick it next to 'Launch game'. If it has optional mods, I ask which ones you want first.")
        else:
            self.ctl.add_links(f"https://www.nexusmods.com/{nexus.GAME}/mods/{m['id']}")
            msg = f"Added {m['name']}. I download and set it up with the mods it needs. Follow it in the Downloads tab."
        b = self.browse_buttons[m["id"]]
        b.configure(text="Added", style="TButton")
        b.state(["disabled"])
        self.say(msg)

    def _tab_mods(self, f):
        head = ttk.Frame(f)
        head.pack(fill="x", pady=(0, 12))
        self.pack_avatar = ttk.Label(head, style="Avatar.TLabel")
        self.pack_avatar.pack(side="left", padx=(0, 14))
        names = ttk.Frame(head)
        names.pack(side="left")
        self.pack_title = ttk.Label(names, style="Pack.TLabel")
        self.pack_title.pack(anchor="w")
        self.pack_sub = ttk.Label(names, style="Dim.TLabel")
        self.pack_sub.pack(anchor="w")
        menu = tk.Menu(self.root, tearoff=0, bg=PANEL2, fg=FG, activebackground=ACCENT, activeforeground=INK, bd=0)
        for label, cmd in (("Save this list as a pack…", self.save_pack), ("Use the picked pack", self.use_pack),
                           ("Delete the picked pack", self.delete_pack), ("Open the mod's folder", self.open_folder)):
            menu.add_command(label=label, command=cmd)
        more = ttk.Button(head, text="•••", width=4, style="Outline.TButton", command=lambda: menu.tk_popup(
            more.winfo_rootx(), more.winfo_rooty() + more.winfo_height()))
        more.pack(side="right")
        Tip(more, "Mod packs: save this list as a pack, use or delete the pack picked next to 'Launch game'.")
        self.pack_pick.bind("<<ComboboxSelected>>", lambda e: self._pack_head(), add="+")
        top = ttk.Frame(f)
        top.pack(fill="x")
        add = ttk.Button(top, text="+  Add mods", style="Big.TButton", command=self.add_dialog)
        add.pack(side="right")
        Tip(add, "Browse Nexus Mods, paste links, add files from this PC, or install the core frameworks.")
        merge = ttk.Button(top, text="Merge packs", style="Outline.TButton", command=self.merge_packs)
        merge.pack(side="right", padx=8)
        Tip(merge, "Make one big pack from several packs.")
        self.search = tk.StringVar()
        self.search.trace_add("write", lambda *_: self.refresh_mods())
        self._search_box(top, self.search, "Search mods…")[0].pack(side="left", fill="x", expand=True)
        row = ttk.Frame(f)
        row.pack(fill="x", pady=(10, 0))
        self.mods_show = tk.StringVar(value="All")
        for name in ("All", "On", "Off"):
            ttk.Radiobutton(row, text=name, value=name, variable=self.mods_show, style="Pill.Toolbutton",
                            command=self.refresh_mods).pack(side="left", padx=(0, 6))
        for label, cmd, tip in (("Apply to game", self.redeploy, "Copy the mods that are on into the game folder now."),
                                ("⇅  Auto-sort", self.auto_sort, ("Put the mods in the best load order: frameworks first, each "
                                 "mod below the mods it needs, small patches below the big mods they change. Then check for "
                                 "missing mods. Undo it in the Crash doctor tab."))):
            b = ttk.Button(row, text=label, style="Link.TButton", command=cmd)
            b.pack(side="right")
            Tip(b, tip)
        self.mods_box = ttk.Frame(f)
        self.mods_box.pack(fill="both", expand=True, pady=10)
        self.mods_tree_frame = ttk.Frame(self.mods_box)
        # On, Type and Source are kept as data; the Mod cell shows them under the name
        t = self.mods = ttk.Treeview(self.mods_tree_frame, columns=("On", "Mod", "Version", "Type", "Source", "Actions"),
                                     displaycolumns=("Mod", "Version", "Actions"), show="tree headings", style="Mods.Treeview",
                                     selectmode="extended")
        t.column("#0", width=self.px(70), stretch=False, anchor="center")
        for c, w in (("Mod", 430), ("Version", 220), ("Actions", 130)):
            t.heading(c, text="Mod" if c == "Mod" else c, anchor="w")
            t.column(c, width=self.px(w), anchor="w", stretch=c == "Mod")
        t.tag_configure("off", foreground=DIM)
        sb = ttk.Scrollbar(self.mods_tree_frame, orient="vertical", command=t.yview)
        t.configure(yscrollcommand=sb.set)
        t.pack(side="left", fill="both", expand=True)
        sb.pack(side="right", fill="y")
        self.switches = (switch(True), switch(False))
        t.bind("<Double-1>", lambda e: self.toggle())
        t.bind("<space>", lambda e: self.toggle())
        t.bind("<Delete>", lambda e: self.remove())
        t.bind("<Control-a>", self.select_all)
        t.bind("<Control-A>", self.select_all)  # Caps Lock on
        t.bind("<Alt-Up>", lambda e: self.move(-1))
        t.bind("<Alt-Down>", lambda e: self.move(1))
        t.bind("<Button-1>", self._mods_click, add="+")
        t.tag_configure("hover", background=PANEL2)
        self.hover_row = ""
        t.bind("<Motion>", lambda e: self._mods_hover(t.identify_row(e.y)))
        t.bind("<Leave>", lambda e: self._mods_hover(""))
        t.bind("<Button-3>", self._mods_menu)
        self.menu = tk.Menu(self.root, tearoff=0, bg=PANEL2, fg=FG, activebackground=ACCENT, activeforeground=INK, bd=0)
        for label, cmd in (("Turn on / off", self.toggle), ("Update", self.update), ("Open folder", self.open_folder),
                           ("Move up    Alt+Up", lambda: self.move(-1)), ("Move down    Alt+Down", lambda: self.move(1)),
                           ("Remove", self.remove)):
            self.menu.add_command(label=label, command=cmd)
        self.mods_empty = ttk.Frame(self.mods_box, padding=40)
        ttk.Label(self.mods_empty, text="\ue8f1", style="Big.TLabel").pack(pady=(30, 8))
        self.mods_empty_head = ttk.Label(self.mods_empty, style="Pack.TLabel")
        self.mods_empty_head.pack()
        self.mods_empty_text = ttk.Label(self.mods_empty, style="Dim.TLabel", font=("Segoe UI", 11), justify="center")
        self.mods_empty_text.pack(pady=(4, 16))
        self.mods_empty_add = ttk.Button(self.mods_empty, text="+  Add mods", style="Big.TButton", command=self.add_dialog)
        self.mods_empty_add.pack()

    def _tab_my(self, f):
        """My mod packs: the Nexus packs the user installed and the packs they saved or merged, as cards."""
        self.pack_open = set()
        top = ttk.Frame(f)
        top.pack(fill="x")
        find = ttk.Button(top, text="+  Find mod packs", style="Big.TButton", command=self.find_packs)
        find.pack(side="right")
        Tip(find, "Browse the mod packs (Nexus collections) on Nexus Mods.")
        for text, cmd, tip in (("Merge packs", self.merge_packs, "Make one big pack from several packs."),
                               ("Save my mod list as a pack", self.save_pack, "Keep the mods that are on now, in their order.")):
            b = ttk.Button(top, text=text, style="Outline.TButton", command=cmd)
            b.pack(side="right", padx=(0, 8))
            Tip(b, tip)
        self.packs_q = tk.StringVar()
        self.packs_q.trace_add("write", lambda *_: self.refresh_pack_page())
        self._search_box(top, self.packs_q, "Search my mod packs…")[0].pack(side="left", fill="x", expand=True, padx=(0, 8))
        row = ttk.Frame(f)
        row.pack(fill="x", pady=(10, 0))
        self.packs_show = tk.StringVar(value="all")
        for text, value in (("All", "all"), ("From Nexus", "nexus"), ("My own", "own")):
            ttk.Radiobutton(row, text=text, value=value, variable=self.packs_show, style="Pill.Toolbutton",
                            command=self.refresh_pack_page).pack(side="left", padx=(0, 6))
        outer, _, self.packs_box = self._scroller(f)
        outer.pack(fill="both", expand=True, pady=10)

    def find_packs(self):
        self.tabs.select(self.tab["Browse"])
        if self.browse_kind.get() != "packs":
            self.browse_kind.set("packs")
            self._browse_kind()

    def refresh_pack_page(self):
        for w in self.packs_box.winfo_children():
            w.destroy()
        q, show = self.packs_q.get().strip().lower(), self.packs_show.get()
        names = [n for n in self.store.packs if q in n.lower() and (show == "all" or (show == "nexus") == (n in self.store.pack_info))]
        for n in names:
            self._pack_card(n)
        if not names:
            text = ("No mod pack matches." if self.store.packs else
                    "No mod packs yet. Install one from 'Find mod packs', or save your mod list as a pack.")
            ttk.Label(self.packs_box, text=text, style="Dim.TLabel", font=("Segoe UI", 11)).pack(pady=40)

    def _pack_card(self, name):
        pack, info = self.store.packs[name], self.store.pack_info.get(name)
        have = {m["id"]: m for m in self.store.mods}
        installed, on = sum(mid in have for mid, _ in pack), sum(bool(e) for _, e in pack)
        card = ttk.Frame(self.packs_box, style="Card.TFrame", padding=12)
        card.pack(fill="x", pady=(0, 8), padx=(0, 4))
        card.columnconfigure(1, weight=1)
        ttk.Label(card, text="".join(w[0] for w in name.split()[:2]).upper() or "M", style="Avatar.TLabel").grid(
            row=0, column=0, rowspan=2, padx=(0, 14))
        head = ttk.Frame(card, style="Opt.TFrame")
        head.grid(row=0, column=1, sticky="w")
        ttk.Label(head, text=name, style="CardTitle.TLabel").pack(side="left")
        ttk.Label(head, text="Nexus mod pack" if info else "My own pack", style="Chip.TLabel").pack(side="left", padx=(10, 0))
        if self.store.pack == name:
            ttk.Label(head, text="IN USE", style="Badge.TLabel").pack(side="left", padx=(8, 0))
        missing = len(pack) - installed
        ttk.Label(card, style="CardDim.TLabel", text=f"{len(pack)} mods  ·  {installed} installed  ·  {on} on"
                  + (f"  ·  {missing} not installed yet" if missing else "")).grid(row=1, column=1, sticky="w", pady=(4, 0))
        btns = ttk.Frame(card, style="Opt.TFrame")
        btns.grid(row=0, column=2, rowspan=2, sticky="e")
        is_open = name in self.pack_open

        def toggle():
            self.pack_open.symmetric_difference_update({name})
            self.refresh_pack_page()
        for text, style, cmd, tip in (
                ("Use", "Get.TButton", lambda: self.use_pack(name), "Turn on this pack's mods in its order and turn off the rest."),
                ("▶  Launch", "TButton", lambda: self.launch_pack(name), "Start the game with this pack."),
                ("Hide mods" if is_open else "Show mods", "TButton", toggle, "The mods in this pack, lowest priority first."),
                *((("Nexus page", "TButton", lambda: webbrowser.open(info["url"]), "Open this pack on Nexus Mods."),) if info else ()),
                ("Delete", "TButton", lambda: self.delete_pack(name), "Forget this pack. Its mods stay installed.")):
            b = ttk.Button(btns, text=text, style=style, command=cmd)
            b.pack(side="left", padx=(6, 0))
            Tip(b, tip)
        if is_open:
            names = (info or {}).get("names", {})
            lines = [f"{'●' if e else '○'}  {have[mid]['name'] if mid in have else names.get(mid, mid)}"
                     + ("" if mid in have else "   (not installed yet)") for mid, e in pack]
            ttk.Label(card, text="\n".join(lines) or "This pack has no mods.", style="Card.TLabel", justify="left").grid(
                row=2, column=1, columnspan=2, sticky="w", pady=(10, 0))

    def launch_pack(self, name):
        self.pack_pick.set(name)
        self._pack_head()
        self.launch()

    def _search_box(self, parent, var, hint):
        """A search field with a magnifier and a grey hint while it is empty."""
        box = ttk.Frame(parent, style="Card.TFrame")
        ttk.Label(box, text="\ue721", style="CardDim.TLabel", font=(ICONS, 11)).pack(side="left", padx=(10, 0))
        entry = ttk.Entry(box, textvariable=var, font=("Segoe UI", 11), style="Search.TEntry")
        entry.pack(side="left", fill="x", expand=True)
        label = ttk.Label(entry, text=hint, style="Hint.TLabel", cursor="xterm")
        label.bind("<Button-1>", lambda e: entry.focus_set())
        def show(*_):
            label.place_forget() if var.get() else label.place(x=self.px(6), rely=0.5, anchor="w")
        var.trace_add("write", show)
        show()
        return box, entry

    def _pack_head(self):
        name = self.pack_pick.get()
        self.pack_title.configure(text=name)
        self.pack_avatar.configure(text="".join(w[0] for w in name.split()[:2]).upper() or "M")
        on = len(self.store.enabled())
        self.pack_sub.configure(text=f"{len(self.store.mods)} mods  ·  {on} on  ·  lower in the list wins when two mods change "
                                     "the same file")

    def _tab_add(self, f):
        ttk.Label(f, text="Paste links, one per line", style="Section.TLabel").pack(anchor="w")
        ttk.Label(f, style="Dim.TLabel", text="Nexus mod pages, Nexus collections (mod packs), GitHub pages, or direct "
                                              ".zip / .7z / .rar links. I find each mod's required mods and frameworks myself.").pack(anchor="w", pady=(2, 6))
        self.links = self._text(f, 12)
        self.links.pack(fill="both", expand=True)
        self.links.tag_configure("ph", foreground=DIM)
        self.links.bind("<FocusIn>", lambda e: self._placeholder(False))
        self.links.bind("<FocusOut>", lambda e: self._placeholder(True))
        self.links.bind("<KeyRelease>", lambda e: self._count_links())
        row = ttk.Frame(f)
        row.pack(fill="x", pady=8)
        self._buttons(row, [
            ("▶ Download and set up everything", self.add_links, "Download, virus-scan, install and set up every link."),
            ("Paste", self.paste, "Paste links from the clipboard."),
            ("Add files from this PC…", self.add_files, "Install mod archives you already downloaded."),
            ("Install the core frameworks", self.install_core, "RED4ext, Cyber Engine Tweaks, redscript, ArchiveXL, TweakXL and Codeware.")]).pack(side="left")
        self.link_count = ttk.Label(row, style="Dim.TLabel")
        self.link_count.pack(side="right")
        self._placeholder(True)
        card = ttk.Frame(f, style="Card.TFrame", padding=12)
        card.pack(fill="x", pady=(4, 0))
        ttk.Label(card, text="How downloads work", style="Card.TLabel", font=("Segoe UI Semibold", 10)).pack(anchor="w")
        for line in ("•  Frameworks (RED4ext, CET, redscript, ArchiveXL, TweakXL, Codeware, Input Loader, Mod Settings) "
                     "come from GitHub: no account, no clicks.",
                     "•  Nexus with a free account: Nexus gives a file only to a person who clicks. I open each file's page in "
                     "your browser. Click 'Slow download' and I do the rest, then open the next page.",
                     "•  Nexus Premium: no clicks at all.  •  Every download is virus-scanned first (you can turn that off in Settings)."):
            ttk.Label(card, text=line, style="CardDim.TLabel", wraplength=self.px(1080), justify="left").pack(anchor="w", pady=1)

    def _tab_find(self, f):
        ttk.Label(f, text="Paste a video link", style="Section.TLabel").pack(anchor="w")
        ttk.Label(f, style="Dim.TLabel", wraplength=self.px(1080), justify="left",
                  text="YouTube works best: I read the mod links, the title and the chapters in the description. TikTok, "
                       "Instagram and other sites give me only the caption. You can also paste a caption or a mod name."
                  ).pack(anchor="w", pady=(2, 6))
        row = ttk.Frame(f)
        row.pack(fill="x")
        self.video = tk.StringVar()
        entry = ttk.Entry(row, textvariable=self.video)
        entry.pack(side="left", fill="x", expand=True, padx=(0, 6))
        entry.bind("<Return>", lambda e: self.find_video())
        self._buttons(row, [("▶ Find the mods", self.find_video, None)]).pack(side="left")
        self.video_info = ttk.Label(f, style="Dim.TLabel", wraplength=self.px(1080), justify="left")
        self.video_info.pack(anchor="w", pady=(10, 0))
        self._buttons(f, [
            ("▶ Add the selected mods", self.add_found, "Download and install the selected mods, like links in the Add mods tab."),
            ("Open page", self.open_found, "Open the mod's page in your browser to check that it is the mod in the video. "
                                           "Shortcut: double-click.")]).pack(side="bottom", anchor="w")
        box, self.found = self._tree(f, ("Mod", "Found by", "Endorsements"), (460, 380, 120), 14, select="extended")
        box.pack(fill="both", expand=True, pady=8)
        self.found.bind("<Double-1>", lambda e: self.open_found())

    def _tab_downloads(self, f):
        self.dl_banner = ttk.Frame(f, style="Banner.TFrame", padding=(12, 8))
        self.dl_banner_text = ttk.Label(self.dl_banner, style="Banner.TLabel", wraplength=self.px(820), justify="left")
        self.dl_banner_text.pack(side="left")
        self.dl_banner_btn = ttk.Button(self.dl_banner, style="Big.TButton")
        self.dl_banner_btn.pack(side="right")
        self.dl_bar = self._buttons(f, [
            ("Open Nexus page", lambda: self._job_do(self.ctl.reopen), "Open the Nexus page of the selected download again."),
            ("Skip", lambda: self._job_do(self.ctl.skip), "Do not download the selected file."),
            ("Retry", lambda: self._job_do(self.ctl.retry), "Try the selected download again."),
            ("Retry all failed", self.retry_failed, None),
            ("Clear finished", self.clear_jobs, "Remove finished and failed rows from this list.")])
        self.dl_bar.pack(fill="x")
        box, self.jobs = self._tree(f, ("Mod", "Status", "Progress"), (380, 520, 100), 14)
        box.pack(fill="both", expand=True, pady=8)
        ttk.Label(f, text="Messages", style="Section.TLabel").pack(anchor="w")
        self.log = self._text(f, 7)
        self.log.pack(fill="both", pady=(4, 0))
        self.log.configure(state="disabled")

    def _tab_crash(self, f):
        card = ttk.Frame(f, style="Card.TFrame", padding=12)
        card.pack(fill="x")
        self.game_state = ttk.Label(card, style="Card.TLabel", text="● The game is not running. I watch for crashes in the background.")
        self.game_state.pack(anchor="w")
        self.bisect_state = ttk.Label(card, style="CardDim.TLabel")  # shown only while a search runs
        self._buttons(f, [
            ("▶ Check my mods now", lambda: self.ctl.bg(self.ctl.check_now), "Look for missing frameworks, mod errors in the game logs and conflicts."),
            ("Ask the local AI", self.ask_ai, "The AI on this PC reads your load order and the problems, then suggests fixes "
                                              "and mod moves. You see them before anything changes. Set it up in the AI helpers tab."),
            ("Find the bad mod for me", self.bisect_begin, "Turns half the mods off for each game start until the mod that crashes the game is found."),
            ("Stop the search", self.bisect_cancel, "Stop the bad-mod search and turn your mods back on."),
            ("Save crash report", self.save_report, ("Save a text file about the last crash or freeze and your mods, to share with "
                                                     "a mod author. Folder paths of this PC are hidden."))]).pack(fill="x", pady=(8, 4))
        self._buttons(f, [
            ("Copy for an AI", self.copy_report, "Copies the crash report as a question for ChatGPT, Claude or another AI chat. "
                                                 "Nothing is sent anywhere; it only goes to your clipboard. Folder paths are hidden."),
            ("Open saved crash logs", self.open_crash_logs, "After each crash I keep a zip of the crash report and the mod logs, "
                                                            "because the game deletes old logs.")]).pack(fill="x", pady=(0, 8))
        hist = ttk.Frame(f)  # packed at the bottom first, so the problem list cannot squeeze it out
        hist.pack(side="bottom", fill="x")
        head = ttk.Frame(hist)
        head.pack(fill="x", pady=(10, 4))
        ttk.Label(head, text="Fixes done", style="Section.TLabel").pack(side="left")
        ttk.Label(head, text="   Select one and press Undo to reverse it.", style="Dim.TLabel").pack(side="left")
        self._buttons(head, [("Undo", self.undo, "Reverse the selected fix.")]).pack(side="right")
        box, self.history = self._tree(hist, ("When", "Fix"), (140, 860), 4)
        box.pack(fill="x")
        pane = ttk.Frame(f)
        pane.pack(fill="both", expand=True)
        box, self.probs = self._tree(pane, ("Level", "Problem"), (90, 520), 10)
        box.pack(side="left", fill="both", expand=True)
        self.probs.bind("<<TreeviewSelect>>", lambda e: self.show_problem())
        right = ttk.Frame(pane, padding=(12, 0, 0, 0))
        right.pack(side="left", fill="both", expand=True)
        self.prob_detail = self._text(right, 8, font=("Segoe UI", 10))
        self.prob_detail.pack(fill="x")
        ttk.Label(right, text="How do you want to fix it?", style="Section.TLabel").pack(anchor="w", pady=(10, 4))
        self.fix_box = ttk.Frame(right)
        self.fix_box.pack(fill="x")
        self.fix_choice = tk.IntVar(value=0)
        self.fix_btn = ttk.Button(right, text="▶ Apply this fix", style="Big.TButton", command=self.apply_fix)
        self.fix_btn.pack(anchor="w", pady=8)
        self.show_problem()

    def _tab_ai(self, f):
        wrap = self.px(1080)
        ttk.Label(f, text="Local AI (free, runs on this PC)", style="Section.TLabel").pack(anchor="w")
        ttk.Label(f, style="Dim.TLabel", wraplength=wrap, justify="left", text=(
            "An open AI model (Qwen3.5) comes with this app and runs on your PC: free, no account, no install, and offline. "
            "'Ask the local AI' in the Crash doctor tab lets it read your load order and the problems I found, pick fixes, and "
            "find patches that must load after the mod they change. You see every change first, and you can undo it. It runs "
            "only while this window is open and the game is not running: it stops when the game starts or the window closes. "
            "A graphics card makes it fast; it also runs on the processor, more slowly.")).pack(anchor="w", pady=(2, 8))
        self._buttons(f, [
            ("▶ Check local AI", self.setup_ai, "Starts the local AI once to check it. When you run the app from its source "
                                               "code, this first downloads the AI (about 1.3 GB)."),
            ("Ask the local AI", self.ask_ai, "Same as the button in the Crash doctor tab.")]).pack(anchor="w")
        self.ai_info = ttk.Label(f, style="Dim.TLabel", wraplength=wrap, justify="left",
                                 text="The local AI is ready to use." if llm.ready() else
                                 "The local AI files are missing. Click 'Check local AI' to download them (about 1.3 GB).")
        self.ai_info.pack(anchor="w", pady=(8, 22))
        ttk.Label(f, text="Tell the local AI about a bug", style="Section.TLabel").pack(anchor="w")
        ttk.Label(f, style="Dim.TLabel", wraplength=wrap, justify="left", text=(
            "Write what goes wrong in the game, for example: 'My modded car does not show up'. The local AI reads your mods "
            "and the problems I found, tells you the most likely cause, and offers fixes. You see every change first.")
                  ).pack(anchor="w", pady=(2, 6))
        self.bug_text = self._text(f, 3, font=("Segoe UI", 10))
        self.bug_text.pack(fill="x")
        self._buttons(f, [("▶ Ask about this bug", self.ask_bug, "The local AI explains the bug and offers fixes.")]
                      ).pack(anchor="w", pady=(6, 22))
        ttk.Label(f, text="Claude and other AI apps", style="Section.TLabel").pack(anchor="w")
        ttk.Label(f, style="Dim.TLabel", wraplength=wrap, justify="left", text=(
            "Give a mod link or a video link to Claude and ask it to set the mods up: it adds them here, sorts the load order "
            "and checks for problems. This works in AI apps that support MCP, such as Claude Desktop, Claude Code and Cursor. "
            "The AI uses this window, so you see what it does. AI apps add only Nexus Mods links and the core frameworks: "
            "paste other links in the Add mods tab yourself. With a free Nexus account you still click 'Slow download' for "
            "each Nexus file.")).pack(anchor="w", pady=(2, 8))
        self._buttons(f, [("Copy setup for Claude Desktop", lambda: self.copy_setup(True), "Copies the MCP setup text for Claude Desktop and similar apps."),
                          ("Copy setup for Claude Code", lambda: self.copy_setup(False), "Copies the command that adds this app to Claude Code.")]).pack(anchor="w")
        ttk.Label(f, style="Dim.TLabel", wraplength=wrap, justify="left", text=(
            "Claude Desktop: open Settings > Developer > Edit Config, paste the text into claude_desktop_config.json (if the file "
            "already has \"mcpServers\", add only the cp2077-ai-mod-manager part), save, and restart Claude. Other apps: paste it "
            "where the app adds MCP servers.\nClaude Code: paste the command in a terminal.\nIf you move this app, copy the setup "
            "again.")).pack(anchor="w", pady=(8, 0))

    def _tab_settings(self, f):
        st = self.store.settings
        self.game_var, self.key_var = tk.StringVar(value=st["game_dir"]), tk.StringVar(value=st["api_key"])
        f.columnconfigure(1, weight=1)
        ttk.Label(f, text="Game", style="Section.TLabel").grid(row=0, column=0, sticky="w", pady=(0, 6))
        ttk.Label(f, text="Cyberpunk 2077 folder").grid(row=1, column=0, sticky="w")
        e = ttk.Entry(f, textvariable=self.game_var)
        e.grid(row=1, column=1, sticky="we", padx=8)
        e.bind("<FocusOut>", lambda _: self.save_settings())
        e.bind("<Return>", lambda _: self.save_settings())
        self._buttons(f, [("Browse…", self.browse_game, None), ("Find it for me", self.detect_game, "Look in Steam, GOG and Epic.")]).grid(row=1, column=2, sticky="w")
        self.game_info = ttk.Label(f)
        self.game_info.grid(row=2, column=1, sticky="w", padx=8, pady=(2, 14))

        ttk.Label(f, text="Nexus Mods", style="Section.TLabel").grid(row=3, column=0, sticky="w", pady=(0, 6))
        ttk.Label(f, text="API key").grid(row=4, column=0, sticky="w")
        self.key_entry = ttk.Entry(f, textvariable=self.key_var, show="•")
        self.key_entry.grid(row=4, column=1, sticky="we", padx=8)
        self.key_entry.bind("<FocusOut>", lambda _: self.save_settings())
        self.key_entry.bind("<Return>", lambda _: self.save_settings())
        self.show_key = tk.BooleanVar(value=False)
        keys = self._buttons(f, [("Check key", self.check_key, None),
                                 ("Get my key", lambda: webbrowser.open(KEY_PAGE), "Opens your Nexus account page. Copy the personal API key from there.")])
        keys.grid(row=4, column=2, sticky="w")
        ttk.Checkbutton(f, text="Show", variable=self.show_key,
                        command=lambda: self.key_entry.configure(show="" if self.show_key.get() else "•")).grid(row=5, column=1, sticky="w", padx=8)
        self.key_info = ttk.Label(f, style="Dim.TLabel", text="Free and Premium accounts both have a personal API key.")
        self.key_info.grid(row=6, column=1, sticky="w", padx=8, pady=(2, 6))
        ttk.Label(f, text="Download button").grid(row=7, column=0, sticky="w")
        self.nxm_info = ttk.Label(f)
        self.nxm_info.grid(row=7, column=1, sticky="w", padx=8)
        self.nxm_btn = ttk.Button(f, command=self.register_nxm)
        self.nxm_btn.grid(row=7, column=2, sticky="w")
        Tip(self.nxm_btn, "Makes the 'Mod Manager Download' and 'Slow download' buttons on Nexus send files to this app.")

        ttk.Label(f, text="Options", style="Section.TLabel").grid(row=8, column=0, sticky="w", pady=(18, 6))
        self.flags = {}
        for row, (key, text) in enumerate([
                ("virus_scan", "Scan every download for viruses with Microsoft Defender (turn off to go faster with many mods)"),
                ("auto_fix", "After a crash, fix a problem by myself when it has only one fix"),
                ("include_optional", "Tick the optional mods of a mod pack at first")], start=9):
            v = self.flags[key] = tk.BooleanVar(value=st[key])
            ttk.Checkbutton(f, text=text, variable=v, command=self.save_settings).grid(row=row, column=0, columnspan=3, sticky="w", pady=3)
        ttk.Label(f, text="Style").grid(row=12, column=0, sticky="w", pady=(10, 3))
        self.theme_var = tk.StringVar(value=st["theme"])
        pick = ttk.Frame(f)
        pick.grid(row=12, column=1, columnspan=2, sticky="w", pady=(10, 3))
        for name in THEMES:
            ttk.Radiobutton(pick, text=name, value=name, variable=self.theme_var, command=self.set_theme).pack(side="left", padx=(0, 14))
        foot = ttk.Frame(f)
        foot.grid(row=13, column=0, columnspan=3, sticky="we", pady=(22, 0))
        ttk.Button(foot, text="Open data folder", command=lambda: os.startfile(self.store.root)).pack(side="right")
        ttk.Label(foot, style="Dim.TLabel", text=f"Mods, backups and settings are kept in {self.store.root}").pack(side="left", fill="x")
        self.refresh_settings()

    # ---------- helpers ----------
    def _selected(self, tree):
        return list(tree.selection())

    def _safe(self, fn, *a):
        try:
            fn(*a)
        except Exception as e:  # noqa: BLE001 - UI boundary
            messagebox.showerror("Mod manager", str(e))

    def say(self, text):
        self.status.configure(text=text)

    def select_all(self, _=None):
        self.mods.selection_set(self.mods.get_children())
        return "break"

    def close(self):
        """No new copy starts. The window shows a message and takes no input (it does not freeze) while a
        running copy finishes, then it closes by itself."""
        if self.ctl.stop.is_set():
            return
        self.ctl.stop.set()
        llm.block()  # at once: a copy that still runs does not need the local AI
        self.say("Finishing the current copy into the game folder…")
        try:
            self.root.attributes("-disabled", True)  # no clicks while closing (the window still repaints)
        except tk.TclError:
            pass
        self._close_when_free()

    def _close_when_free(self):
        if self.store.lock.acquire(blocking=False):
            # Keep the lock until the process ends: no thread may start a copy or swap mod files now.
            llm.block()  # the local AI runs only while the window is open
            self.root.destroy()
        else:
            self.root.after(200, self._close_when_free)

    def redeploy(self):
        if self.store.game():
            self.say("Setting up your mods in the game folder…")
            self.ctl.redeploy()
        else:
            self.say("Set the game folder in Settings first.")

    # ---------- actions ----------
    def launch(self):
        if not self.store.game():
            messagebox.showinfo("Mod manager", "Pick the Cyberpunk 2077 folder in Settings first.")
            return self.tabs.select(self.tab["Settings"])
        busy = sum(j["state"] in ("queued", "running") for j in self.ctl.jobs)
        if busy and not messagebox.askyesno("Downloads running", f"{busy} download(s) are still running. Start the game anyway?"):
            return
        pack = self.picked_pack()
        self.ctl.bg(self.ctl.launch, pack)  # copying mods can take a while; errors arrive as messages
        self.say(f"Starting the game with the mod pack '{pack}'. I watch it for crashes." if pack else
                 "Starting the game. I watch it for crashes.")

    # ---------- mod packs ----------
    def picked_pack(self):
        name = self.pack_pick.get()
        return name if name in self.store.packs else None

    def refresh_packs(self):
        self.pack_pick.configure(values=[CURRENT, *self.store.packs])
        if self.pack_pick.get() not in (CURRENT, *self.store.packs):  # its pack was deleted
            self.pack_pick.set(CURRENT)
        self.refresh_pack_page()

    def save_pack(self):
        name = simpledialog.askstring("Save mod pack", "Name of the mod pack:", parent=self.root,
                                      initialvalue=self.picked_pack() or self.store.pack)
        if not name or not name.strip():
            return
        name = name.strip()
        if name in self.store.packs and not messagebox.askyesno("Save mod pack", f"Replace the mod pack '{name}'?"):
            return
        self.ctl.save_pack(name)
        self.refresh_packs()
        self.pack_pick.set(name)
        self.say(f"Saved the mod pack '{name}'. Pick it next to 'Launch game' to launch it.")

    def use_pack(self, pack=None):
        pack = pack or self.picked_pack()
        if not pack:
            return messagebox.showinfo("Mod packs", "Pick a mod pack next to 'Launch game' first.")
        self.say(self.ctl.use_pack(pack))
        return self.redeploy()

    def delete_pack(self, pack=None):
        pack = pack or self.picked_pack()
        if pack and messagebox.askyesno("Mod packs", f"Forget the mod pack '{pack}'? Its mods stay installed."):
            self.ctl.delete_pack(pack)
            self.refresh_packs()  # resets the pick if it was this pack

    def merge_packs(self):
        names = list(self.store.packs)
        if len(names) < 2:
            return messagebox.showinfo("Merge mod packs", "Save or add two or more mod packs first. Each Nexus collection you add "
                                                          "is a mod pack.")
        win = self.merge_win = tk.Toplevel(self.root, bg=BG)
        win.title("Merge mod packs")
        ttk.Label(win, text="Pick the packs to merge. When two mods change the same file, the pack lower in this list wins. "
                            "You can change that later in the Crash doctor tab.", wraplength=self.px(460)).pack(padx=14, pady=(12, 6))
        box = tk.Listbox(win, selectmode="multiple", height=min(15, len(names)), bg=PANEL2, fg=FG, selectbackground=ACCENT,
                         selectforeground=INK, highlightthickness=0, bd=0, exportselection=False)
        for n in names:
            box.insert("end", f"{n}  ({len(self.store.packs[n])} mods)")
        box.pack(fill="both", expand=True, padx=14)
        row = ttk.Frame(win, padding=(14, 10))
        row.pack(fill="x")
        ttk.Label(row, text="New pack").pack(side="left")
        new = tk.StringVar(value="Merged pack")
        ttk.Entry(row, textvariable=new, width=28).pack(side="left", padx=6)

        def merge():
            picked = [names[i] for i in box.curselection()]
            try:
                n = self.ctl.merge_packs(picked, new.get())
            except ValueError as e:
                return messagebox.showinfo("Merge mod packs", str(e), parent=win)
            win.destroy()
            self.refresh_packs()
            self.pack_pick.set(new.get().strip())
            return self.say(f"Made the mod pack '{new.get().strip()}' with {n} mods. Launch it, or pick 'Use the picked pack' "
                            "in the ••• menu to see it.")
        ttk.Button(row, text="▶ Merge", style="Big.TButton", command=merge).pack(side="right")
        self.merge_ok = merge  # for tests

    def _mods_hover(self, row):
        """Light up the row under the mouse."""
        if row == self.hover_row:
            return
        t = self.mods
        if self.hover_row and t.exists(self.hover_row):
            t.tk.call(t, "tag", "remove", "hover", self.hover_row)
        if row:
            t.tk.call(t, "tag", "add", "hover", row)
        self.hover_row = row

    def _mods_click(self, e):
        """The switch turns a mod on or off; the Actions cell holds update, remove and more."""
        iid, col = self.mods.identify_row(e.y), self.mods.identify_column(e.x)
        if not iid or self.mods.identify_region(e.x, e.y) not in ("tree", "cell"):
            return None
        if col == "#0":
            self._set_on([iid], not self.store.mod(iid)["enabled"])
            return "break"
        if col == "#3":  # Actions: the three glyphs fill about the left three quarters of the cell
            x, _, w, _ = self.mods.bbox(iid, "Actions")
            self.mods.selection_set(iid)
            act = min(2, int((e.x - x) * 4 / max(w, 1)))
            (self.update, self.remove, lambda: self.menu.tk_popup(e.x_root, e.y_root))[act]()
            return "break"
        return None

    def add_dialog(self):
        """Ways to add mods, as a list to pick from. Typing a name and pressing Enter searches Nexus."""
        win = self.add_win = tk.Toplevel(self.root, bg=BG, padx=self.px(18), pady=self.px(14))
        win.title("Add mods")
        win.transient(self.root)
        win.resizable(False, False)
        ttk.Label(win, text="Add mods", style="Pack.TLabel").pack(anchor="w")
        q = tk.StringVar()
        box, entry = self._search_box(win, q, "Search Nexus Mods and press Enter…")
        box.pack(fill="x", pady=(10, 12))

        def go(fn):
            win.destroy()
            fn()

        def search(_=None):
            self.browse_q.set(q.get().strip())
            go(lambda: self.tabs.select(self.tab["Browse"]))
        entry.bind("<Return>", search)
        for glyph, title, text, fn in (
                ("\ue774", "Browse Nexus Mods", "Find mods by name or category, and install them with one click.",
                 lambda: self.tabs.select(self.tab["Browse"])),
                ("\ue71b", "Paste links", "Nexus mods and collections, GitHub pages, or direct .zip / .7z / .rar links.",
                 lambda: self.tabs.select(self.tab["Add mods"])),
                ("\ue8e5", "Add files from this PC", "Mod archives you downloaded yourself.", self.add_files),
                ("\ue896", "Install the core frameworks", ("RED4ext, Cyber Engine Tweaks, redscript, ArchiveXL, TweakXL "
                                                           "and Codeware. Most mods need them."), self.install_core),
                ("\ue714", "Find mods from a video", "Paste a YouTube, TikTok or Instagram link to get the mods it shows.",
                 lambda: self.tabs.select(self.tab["Find mod from video"]))):
            row = ttk.Frame(win, style="Opt.TFrame", padding=(14, 10), cursor="hand2")
            row.pack(fill="x", pady=(0, 6))
            parts = [row, ttk.Label(row, text=glyph, style="OptIcon.TLabel", width=3),
                     ttk.Label(row, text=title, style="OptTitle.TLabel"), ttk.Label(row, text=text, style="OptDim.TLabel")]
            parts[1].grid(row=0, column=0, rowspan=2, padx=(0, 10))
            parts[2].grid(row=0, column=1, sticky="w")
            parts[3].grid(row=1, column=1, sticky="w")
            for w in parts:
                w.bind("<Button-1>", lambda e, fn=fn: go(fn))
                w.bind("<Enter>", lambda e, p=parts: self._hot(p, "OptHot"))
                w.bind("<Leave>", lambda e, p=parts: self._hot(p, "Opt"))
        entry.focus_set()
        return win

    def pack_choice(self, p):
        """A mod pack with optional mods, or with mods that may not work with the game any more: the user picks what to download."""
        win = self.choice_win = tk.Toplevel(self.root, bg=BG, padx=self.px(18), pady=self.px(14))
        win.title(f"Mod pack: {p['name']}")
        win.transient(self.root)
        ttk.Label(win, text=p["name"], style="Pack.TLabel").pack(anchor="w")
        ttk.Label(win, text="Pick what to download with this mod pack.", style="Dim.TLabel").pack(anchor="w", pady=(0, 6))
        buttons = ttk.Frame(win)
        buttons.pack(side="bottom", fill="x", pady=(12, 0))
        outer, canvas, inner = self._scroller(win)
        canvas.configure(width=self.px(480))
        outer.pack(fill="both", expand=True)
        picks, wrap = {}, self.px(440)
        old = [f for f in p["files"] if f.get("outdated")]
        for title, text, group in (
                ("Optional mods", "The pack works without these. Tick the ones you want.", [f for f in p["files"] if f not in old]),
                ("Mods that may not work any more", "Untick a mod to skip it.", old)):
            if not group:
                continue
            ttk.Label(inner, text=title, style="Section.TLabel").pack(anchor="w", pady=(8, 0))
            ttk.Label(inner, text=text, style="Dim.TLabel", wraplength=wrap).pack(anchor="w", pady=(0, 4))
            for f in group:
                on = self.store.settings["include_optional"] or not f["optional"]
                v = picks[f["file_id"]] = tk.BooleanVar(value=on)
                ttk.Checkbutton(inner, text=f["name"], variable=v).pack(anchor="w", pady=(2, 0))
                if f.get("outdated"):
                    ttk.Label(inner, text=f["outdated"][0].upper() + f["outdated"][1:] + ".", style="Dim.TLabel",
                              wraplength=wrap).pack(anchor="w", padx=(self.px(24), 0))
        inner.update_idletasks()
        canvas.configure(height=min(inner.winfo_reqheight() + self.px(4), self.px(400)))  # scrolls only for a long list

        def download():
            win.destroy()
            chosen = [i for i, v in picks.items() if v.get()]
            self.ctl.add_link({**p["link"], "chosen": chosen})
            self.tabs.select(self.tab["Downloads"])
            self.say(f"Downloading the mod pack {p['name']} with {len(chosen)} of the {len(picks)} mods you could pick.")
        ttk.Button(buttons, text="Download the pack", style="Big.TButton", command=download).pack(side="right")
        ttk.Button(buttons, text="Cancel", command=win.destroy).pack(side="right", padx=(0, 8))
        self.choice_ok = download  # for tests
        return picks

    def ask_outdated(self, p):
        """A mod that may not work with the game any more: Download it anyway, or Skip it."""
        win = tk.Toplevel(self.root, bg=BG, padx=self.px(18), pady=self.px(14))
        win.title("This mod may not work")
        win.transient(self.root)
        win.resizable(False, False)
        ttk.Label(win, text=p["name"], style="Head.TLabel").pack(anchor="w")
        ttk.Label(win, text=f"This mod may not work with the game any more: {p['why']}.", style="Dim.TLabel",
                  wraplength=self.px(420)).pack(anchor="w", pady=(4, 12))

        def pick(get):
            win.destroy()
            if get:
                self.ctl.add_link({**p["link"], "anyway": True})
            self.say(f"Downloading {p['name']}." if get else f"Skipped {p['name']}.")
        row = ttk.Frame(win)
        row.pack(fill="x")
        ttk.Button(row, text="Download", style="Big.TButton", command=lambda: pick(True)).pack(side="right")
        ttk.Button(row, text="Skip", command=lambda: pick(False)).pack(side="right", padx=(0, 8))
        return pick

    def _hot(self, parts, name):
        for w, kind in zip(parts, ("TFrame", "Icon.TLabel", "Title.TLabel", "Dim.TLabel"), strict=True):
            w.configure(style=f"{name}.{kind}" if kind == "TFrame" else f"{name}{kind}")

    def _mods_menu(self, e):
        iid = self.mods.identify_row(e.y)
        if iid:
            if iid not in self.mods.selection():
                self.mods.selection_set(iid)
            self.menu.tk_popup(e.x_root, e.y_root)

    def _set_on(self, ids, on):
        self.ctl.set_enabled(ids, on)
        self.redeploy()

    def toggle(self):
        ids = self._selected(self.mods)
        if ids:
            self._set_on(ids, not all(self.store.mod(i)["enabled"] for i in ids))
        return "break"

    def move(self, d):
        ids = self._selected(self.mods)
        if ids:
            self.ctl.move(ids[0], d)
            self.redeploy()

    def auto_sort(self):
        self.say("Finding the best load order…")
        self.ctl.bg(self.ctl.auto_sort)  # reads every archive: not on the window thread

    def update(self):
        for mid in self._selected(self.mods):
            self.ctl.bg(self.ctl.update_mod, mid)
        if self.mods.selection():
            self.say("Looking for newer versions…")

    def remove(self):
        ids = self._selected(self.mods)
        if not ids:
            return
        what = self.store.mod(ids[0])["name"] if len(ids) == 1 else f"these {len(ids)} mods"
        if messagebox.askyesno("Remove", f"Remove {what} from the game and from this app?"):
            for mid in ids:
                self.ctl.remove(mid)
            self.redeploy()

    def open_folder(self):
        ids = self._selected(self.mods)
        if ids:
            os.startfile(self.store.staging / ids[0])

    def install_core(self):
        for fw in CORE:
            self.ctl.bg(self.ctl.add_framework, fw)
        self.say("Installing the core frameworks from GitHub…")
        self.tabs.select(self.tab["Downloads"])

    def _placeholder(self, show):
        text = self.links.get("1.0", "end").strip()
        if show and not text:
            self.links.insert("1.0", EXAMPLES, "ph")
        elif not show and self.links.tag_ranges("ph"):
            self.links.delete("1.0", "end")
        self._count_links()

    def _links_text(self):
        return "" if self.links.tag_ranges("ph") else self.links.get("1.0", "end")

    def _count_links(self):
        n = sum(1 for ln in self._links_text().splitlines() if ln.strip() and not ln.strip().startswith("#"))
        self.link_count.configure(text=f"{n} link(s) ready" if n else "")

    def paste(self):
        try:
            text = self.root.clipboard_get()
        except tk.TclError:
            return self.say("The clipboard is empty.")
        self._placeholder(False)
        self.links.insert("end", ("\n" if self._links_text().strip() else "") + text.strip())
        self._count_links()

    def add_links(self):
        text = self._links_text()
        n = sum(1 for ln in text.splitlines() if ln.strip())
        if not n:
            return self.say("Paste at least one link first.")
        self.ctl.add_links(text)
        self.links.delete("1.0", "end")
        self._count_links()
        self.say(f"Added {n} link(s). Follow them in the Downloads tab.")
        self.tabs.select(self.tab["Downloads"])

    def add_files(self):
        files = filedialog.askopenfilenames(filetypes=[("Mod archives", "*.zip *.7z *.rar"), ("All files", "*.*")])
        for p in files:
            self.ctl.add_links(p)
        if files:
            self.say(f"Installing {len(files)} file(s)…")
            self.tabs.select(self.tab["Downloads"])

    def find_video(self):
        entry = self.video.get().strip()
        if not entry:
            return self.say("Paste a video link first.")
        self.video_info.configure(text="Reading the video page…", foreground=DIM)
        self.ctl.bg(self.ctl.find_in_video, entry)

    def show_found(self, p):
        self.found.delete(*self.found.get_children())
        if "error" in p:
            return self.video_info.configure(text=f"I could not read that page: {p['error']}", foreground=BAD)
        rows = p["rows"]
        for r in rows:
            self.found.insert("", "end", iid=r["url"], values=(r["name"], r["why"], r["endorsements"] or ""))
        n, linked = len(rows), sum(r["why"] == "Linked in the video" for r in rows)
        text = ("I found no mod. If you know the mod's name, paste the name." if not n else
                f"{n} mod(s) linked in the video." if linked == n else
                f"{n} mod(s): {linked} linked in the video, {n - linked} found by name. Open a page to check a mod before you add it.")
        self.video_info.configure(text=(f'"{p["title"]}"   ' if p["title"] else "") + text, foreground=FG if n else WARN)

    def add_found(self):
        urls = self._selected(self.found)
        if not urls:
            return self.say("Select one or more mods in the list first.")
        self.ctl.add_links("\n".join(urls))
        self.say(f"Added {len(urls)} mod(s). Follow them in the Downloads tab.")
        self.tabs.select(self.tab["Downloads"])

    def open_found(self):
        if self.found.focus():
            webbrowser.open(self.found.focus())

    def setup_ai(self):
        if not llm.ready() and not messagebox.askyesno("Local AI", (
                "The local AI files are not next to this app. I download the llama.cpp engine (MIT license, 36 MB, from "
                "GitHub) and the Qwen3.5 model (Apache 2.0 license, 1.3 GB, from this app's GitHub releases), and check both files.\n\nGo on?")):
            return
        self.ai_info.configure(text="Setting up the local AI…", foreground=DIM)
        self.ctl.bg(self.ctl.setup_ai)

    def ask_ai(self):
        self.say("The local AI is reading your mods and problems… The first answer can take a minute.")
        self.ctl.bg(self.ctl.ask_ai)

    def ask_bug(self):
        text = self.bug_text.get("1.0", "end").strip()
        if not text:
            return messagebox.showinfo("Local AI", "First write what goes wrong in the game.")
        self.say("The local AI is reading your bug report, mods and problems… The first answer can take a minute.")
        self.ctl.bg(self.ctl.ask_ai, text)

    def show_ai(self, p):
        if "error" in p:
            self.say("The local AI could not help.")
            return messagebox.showerror("Local AI", p["error"])
        text = (p["summary"] or "The local AI gave no summary.")[:900]
        if p.get("cause"):
            text = f"Why it happens: {p['cause'][:600]}\n\n{text}"
        if p["order"]:
            text += f"\n\nLoad order: {p['moved']} mod(s) change place."
        if p["fixes"]:
            text += "\n\nFixes:\n" + "\n".join(f"•  {pr['title']}  →  {f['label']}" + (f"\n    {why[:200]}" if why else "")
                                               for pr, f, why in p["fixes"])
        if not (p["order"] or p["fixes"]):
            self.say("The local AI found nothing to change.")
            return messagebox.showinfo("Local AI", text + "\n\nThe local AI found nothing to change.")
        if messagebox.askyesno("Local AI", text + "\n\nMake these changes? You can undo each one in the Crash doctor tab."):
            self.ctl.bg(self.ctl.apply_ai, p)
            self.say("Done. You can undo each change in the Crash doctor tab.")

    def copy_setup(self, desktop):
        """The MCP setup that starts this app as `--mcp` (the exe, or python with app.py when run from source)."""
        cmd = ([sys.executable] if getattr(sys, "frozen", False) else
               [sys.executable, str(Path(__file__).resolve().parents[1] / "app.py")]) + ["--mcp"]
        text = (json.dumps({"mcpServers": {"cp2077-ai-mod-manager": {"command": cmd[0], "args": cmd[1:]}}}, indent=2) if desktop else
                "claude mcp add --scope user cp2077-ai-mod-manager -- " + " ".join(f'"{c}"' for c in cmd))  # quoted: Git Bash drops the backslashes of an unquoted path
        self.root.clipboard_clear()
        self.root.clipboard_append(text)
        self.say("Copied. Paste it as the text in the AI helpers tab says.")

    def _job_do(self, fn):
        ids = self._selected(self.jobs)
        j = next((j for j in self.ctl.jobs if ids and str(j["id"]) == ids[0]), None)
        if not j:
            return self.say("Select a download first.")
        if fn == self.ctl.reopen and j["src"]["type"] != "nexus":
            return self.say("That download is not from Nexus.")
        if fn(j) is False:
            self.say("Skip works only before a download starts, and Retry only after it failed.")

    def retry_failed(self):
        n = 0
        for j in list(self.ctl.jobs):
            if j["state"] == "failed" and not j["status"].startswith(("Blocked", "Skipped")):
                self.ctl.retry(j)
                n += 1
        self.say(f"Trying {n} download(s) again." if n else "Nothing to retry.")

    def clear_jobs(self):
        with self.ctl.lock:
            self.ctl.jobs = [j for j in self.ctl.jobs if j["state"] not in ("done", "failed")]
        self.jobs.delete(*self.jobs.get_children())
        for j in self.ctl.jobs:
            self.show_job(j)
        self.refresh_progress()

    def show_problem(self):
        ids = self._selected(self.probs)
        p = next((p for p in self.problems if ids and p["id"] == ids[0]), None)
        for w in self.fix_box.winfo_children():
            w.destroy()
        self.prob_detail.configure(state="normal")
        self.prob_detail.delete("1.0", "end")
        if p:
            self.prob_detail.insert("1.0", f"{p['title']}\n\n{p['detail']}")
            best = next((i for i, x in enumerate(p["fixes"]) if x["recommended"]), 0)
            self.fix_choice.set(best)
            for i, x in enumerate(p["fixes"]):
                ttk.Radiobutton(self.fix_box, text=x["label"] + ("   (recommended)" if i == best else ""), variable=self.fix_choice,
                                value=i).pack(anchor="w", pady=1)
            self.fix_btn.state(["!disabled"])
        else:
            self.prob_detail.insert("1.0", "No problem selected.\n\nPress 'Check my mods now' to look for problems. "
                                           "When the game crashes, I check it by myself and show the problems here.")
            self.fix_btn.state(["disabled"])
        self.prob_detail.configure(state="disabled")

    def apply_fix(self):
        ids = self._selected(self.probs)
        p = next((p for p in self.problems if ids and p["id"] == ids[0]), None)
        if p:
            f = p["fixes"][self.fix_choice.get()]
            self.ctl.apply(p, f)
            self.problems.remove(p)
            self.probs.delete(p["id"])
            if self.problems:
                self.probs.selection_set(self.problems[0]["id"])
            self.show_problem()
            self.refresh_history()
            self.refresh_tabs()
            self.say(f"Done: {f['label']}.")

    def undo(self):
        ids = self._selected(self.history)
        if ids:
            self.ctl.undo(int(ids[0]))
            self.refresh_history()
            self.say("Fix reversed.")

    def save_report(self):
        path = filedialog.asksaveasfilename(title="Save crash report", defaultextension=".txt", filetypes=[("Text", "*.txt")],
                                            initialfile=f"cp2077-crash-report-{time.strftime('%Y%m%d-%H%M')}.txt")
        if not path:
            return
        try:
            Path(path).write_text(self.ctl.crash_report(), encoding="utf-8")
        except OSError as e:
            return messagebox.showerror("Save crash report", f"Could not save the report: {e}")
        self.say(f"Crash report saved: {path}")

    def copy_report(self):
        self.root.clipboard_clear()
        self.root.clipboard_append(self.ctl.crash_report(for_ai=True))
        self.say("Report copied. Paste it into an AI chat.")

    def open_crash_logs(self):
        d = self.store.root / "crash_logs"
        if not d.is_dir():
            return self.say("No crash logs are saved yet. I save them after each crash.")
        os.startfile(d)

    def bisect_begin(self):
        if messagebox.askyesno("Find the bad mod", "I will turn half of your mods off. Start the game and play until it crashes, "
                                                   "or quit normally if it does not. I repeat this until I find the bad mod.\n\nStart?"):
            self.ctl.bisect_begin()
            self.refresh_bisect()

    def bisect_cancel(self):
        self.ctl.bisect_cancel()
        self.refresh_bisect()

    def browse_game(self):
        d = filedialog.askdirectory(title="Pick the Cyberpunk 2077 folder")
        if d:
            self.game_var.set(os.path.normpath(d))
            self.save_settings()

    def detect_game(self):
        g = find_game()
        if g:
            self.game_var.set(g)
            self.save_settings()
        else:
            messagebox.showinfo("Mod manager", "I could not find Cyberpunk 2077. Use Browse to pick its folder.")

    def save_settings(self):
        st = self.store.settings
        old_key, old_game = st["api_key"], st["game_dir"]
        st.update(game_dir=self.game_var.get().strip(), api_key=self.key_var.get().strip(), **{k: v.get() for k, v in self.flags.items()})
        self.store.save()
        if st["api_key"] and st["api_key"] != old_key:
            self.check_key()
        if st["game_dir"] != old_game and self.store.game():
            self.redeploy()
        self.refresh_settings()
        self.refresh_mods()

    def set_theme(self):
        self.store.settings["theme"] = self.theme_var.get()
        self.store.save()
        use_theme(self.store.settings["theme"])
        self._style()
        self.root.configure(bg=BG)
        self._recolor(self.root)
        self.switches = (switch(True), switch(False))  # pictures keep the colours they were drawn with
        self.refresh_mods()

    def _recolor(self, w):
        """Plain Tk widgets keep the colours they were made with; ttk ones follow _style."""
        for c in w.winfo_children():
            if isinstance(c, tk.Text):
                c.configure(bg=PANEL, fg=FG, insertbackground=FG, selectbackground=SEL)
                c.tag_configure("ph", foreground=DIM)
            elif isinstance(c, tk.Listbox):
                c.configure(bg=PANEL2, fg=FG, selectbackground=ACCENT, selectforeground=INK)
            elif isinstance(c, tk.Menu):
                c.configure(bg=PANEL2, fg=FG, activebackground=ACCENT, activeforeground=INK)
            elif isinstance(c, (tk.Toplevel, tk.Canvas)):
                c.configure(bg=BG)
            elif isinstance(c, ttk.Treeview):
                for tag in ("off", "queued", "info"):
                    c.tag_configure(tag, foreground=DIM)
                c.tag_configure("hover", background=PANEL2)
            self._recolor(c)

    def check_key(self):
        if self.key_var.get().strip() != self.store.settings["api_key"]:
            return self.save_settings()  # saving a new key runs the check
        if not self.store.settings["api_key"]:
            return self.key_info.configure(text="Paste your key first.", foreground=WARN)
        self.key_info.configure(text="Checking…", foreground=DIM)
        self.ctl.bg(self.ctl.check_key)

    def register_nxm(self):
        if self.register:
            self._safe(self.register)
            self.refresh_settings()
            self.say("Nexus download buttons now send files to this app.")
            waiting = next((j for j in self.ctl.jobs if j["state"] == "waiting"), None)
            if waiting:  # its last click went to the other app: show its page again
                self.ctl.reopen(waiting)

    def hide_banner(self):
        self.banner_hidden = True
        self.banner.pack_forget()

    # ---------- refresh ----------
    def refresh_settings(self):
        g = self.game_var.get().strip()
        if self.store.game():
            self.game_info.configure(text="✔  Cyberpunk 2077 found", foreground=OK)
        elif g:
            self.game_info.configure(text="✖  No bin\\x64\\Cyberpunk2077.exe in this folder. Pick the game's main folder.", foreground=BAD)
        else:
            self.game_info.configure(text="Not set. Press 'Find it for me' or 'Browse…'.", foreground=WARN)
        on, other = self.registered(), self.nxm_owner()
        self.nxm_info.configure(foreground=OK if on else WARN, text="✔  'Slow download' on Nexus sends files to this app" if on else
                                f"✖  'Slow download' on Nexus sends files to {other}, not to this app" if other else
                                "✖  'Slow download' on Nexus does nothing yet")
        self.nxm_btn.configure(text="Set it up again" if on else "\u25b6  Fix it: use this app", style="TButton" if on else "Big.TButton")
        self.refresh_setup()

    def refresh_setup(self):
        steps = [("Game folder", bool(self.store.game())), ("Nexus API key", bool(self.store.settings["api_key"])),
                 ("Nexus download button", self.registered())]
        if all(ok for _, ok in steps) or self.banner_hidden:
            return self.banner.pack_forget()
        self.banner_text.configure(text="Finish setup:     " + "     ".join(f"{'✔' if ok else '✖'} {name}" for name, ok in steps))
        if not self.banner.winfo_manager():
            self.banner.pack(fill="x", padx=12, pady=(10, 0), after=self.game_pill.master)

    def refresh_mods(self):
        sel = set(self.mods.selection())
        self.mods.delete(*self.mods.get_children())
        q, show = self.search.get().strip().lower(), self.mods_show.get()
        shown = 0
        for m in self.store.mods:
            if q and q not in m["name"].lower() or show == "On" and not m["enabled"] or show == "Off" and m["enabled"]:
                continue
            s = m["source"]
            src = f"Nexus #{s['mod_id']}" if s["type"] == "nexus" else f"GitHub {s['repo']}" if s["type"] == "github" else \
                "Local file" if s["type"] == "file" else "Web link"
            self.mods.insert("", "end", iid=m["id"], image=self.switches[0 if m["enabled"] else 1],
                             values=("✔" if m["enabled"] else "○", f"{m['name']}\n{kind_of(m)}  ·  {src}", m.get("version", "") or "–",
                                     kind_of(m), src, "⟳        ✕        •••"), tags=() if m["enabled"] else ("off",))
            shown += 1
        keep = [i for i in sel if self.mods.exists(i)]
        if keep:
            self.mods.selection_set(keep)
        if shown:
            self.mods_empty.pack_forget()
            self.mods_tree_frame.pack(fill="both", expand=True)
        else:
            self.mods_tree_frame.pack_forget()
            self.mods_empty_head.configure(text="No match" if self.store.mods else "Ready to mod Night City?")
            self.mods_empty_text.configure(text="No mod matches your search or filter." if self.store.mods else
                                           "No mods yet. Find mods on Nexus, paste links, or start with the core frameworks.")
            self.mods_empty_add.pack() if not self.store.mods else self.mods_empty_add.pack_forget()
            self.mods_empty.pack(fill="both", expand=True)
        self.refresh_packs()
        self._pack_head()
        self.refresh_tabs()
        self._browse_marks()

    def show_job(self, j):
        vals = (j["name"], j["status"], j["progress"])
        iid = str(j["id"])
        tag = (j["state"],)
        if self.jobs.exists(iid):
            self.jobs.item(iid, values=vals, tags=tag)
        else:
            self.jobs.insert("", "end", iid=iid, values=vals, tags=tag)
        self.refresh_progress()

    def refresh_progress(self):
        jobs = list(self.ctl.jobs)
        active = sum(j["state"] in ACTIVE for j in jobs)
        done = len(jobs) - active
        self.progress["value"] = done * 100 / len(jobs) if jobs else 0
        self.progress.pack(side="right", before=self.progress_text) if active else self.progress.pack_forget()
        self.progress_text.configure(text=f"{done} of {len(jobs)} downloads finished" if active else "")
        if not active and self.was_busy:
            failed = sum(j["state"] == "failed" for j in jobs)
            self.say("All downloads finished." + (f" {failed} failed: see the Downloads tab." if failed else ""))
        self.was_busy = bool(active)
        waiting = next((j for j in jobs if j["state"] == "waiting"), None)
        needkey = any(j["state"] == "needkey" for j in jobs)
        if waiting and self.register and not self.registered() and not self.asked_nxm:
            self.asked_nxm = True  # once per run: the user may want the other app to keep Nexus links
            if messagebox.askyesno("Nexus download button", "Your 'Slow download' clicks go to another app (such as Vortex), "
                                   "not to this one, so nothing happens here.\n\nSend Nexus downloads to this app?"):
                self.register_nxm()  # also opens the waiting page again
        if waiting:
            left = sum(j["state"] == "waiting" for j in jobs)
            self.dl_banner_text.configure(text=f"Your browser shows the Nexus page for '{waiting['name']}'. Click 'Slow download' "
                                               f"there and I do the rest. {left} click{'s' if left > 1 else ''} left.")
            self.dl_banner_btn.configure(text="Open page again", command=lambda: self.ctl.reopen(waiting))
        elif needkey:
            self.dl_banner_text.configure(text="Nexus downloads wait for your API key. Add it in Settings and they continue by themselves.")
            self.dl_banner_btn.configure(text="Open Settings", command=lambda: self.tabs.select(self.tab["Settings"]))
        if waiting or needkey:
            if not self.dl_banner.winfo_manager():
                self.dl_banner.pack(fill="x", pady=(0, 8), before=self.dl_bar)
        else:
            self.dl_banner.pack_forget()
        self.refresh_tabs()

    def refresh_tabs(self):
        active = sum(j["state"] in ACTIVE for j in self.ctl.jobs)
        self.tabs.tab(self.tab["Mods"], text=f"Mods ({len(self.store.mods)})")
        self.tabs.tab(self.tab["Downloads"], text=f"Downloads ({active})" if active else "Downloads")
        self.tabs.tab(self.tab["Crash doctor"], text=f"Crash doctor ({len(self.problems)})" if self.problems else "Crash doctor")
        for name, n in (("Downloads", active), ("Crash doctor", len(self.problems))):
            if name in self.badges:  # the rail is built after the first refresh
                self.badges[name].configure(text=n)
                self.badges[name].place(relx=1, x=-2, y=2, anchor="ne") if n else self.badges[name].place_forget()
        if self.rail_items:
            self.page_title.configure(text=self.tabs.tab(self.tabs.select(), "text"))

    def refresh_history(self):
        self.history.delete(*self.history.get_children())
        for i, h in enumerate(self.store.doctor.get("history", [])):
            self.history.insert("", "end", iid=str(i), values=(time.strftime("%d %b %H:%M", time.localtime(h["time"])), f"{h['problem']}  →  {h['fix']}"))
        self.refresh_bisect()

    def refresh_bisect(self):
        b = self.store.doctor.get("bisect")
        if b:
            self.bisect_state.configure(text=f"Bad-mod search running: {len(b['suspects'])} suspects left. Start the game; "
                                             "play until it crashes or quit normally.")
            self.bisect_state.pack(anchor="w")
        else:
            self.bisect_state.pack_forget()

    def show_problems(self, payload):
        self.problems = payload["list"]
        self.probs.delete(*self.probs.get_children())
        for p in self.problems:
            self.probs.insert("", "end", iid=p["id"], values=(LEVEL.get(p["severity"], p["severity"]), p["title"]), tags=(p["severity"],))
        if self.problems:
            self.probs.selection_set(self.problems[0]["id"])
        self.show_problem()
        self.refresh_history()
        self.refresh_tabs()
        if payload["crashed"]:
            self.tabs.select(self.tab["Crash doctor"])
            self.root.deiconify()
            self.root.lift()
            n = sum(p["severity"] == "high" for p in self.problems)
            messagebox.showinfo("The game crashed", f"I checked the crash. {n} problem(s) need you to pick a fix." if n else
                                "I fixed what I found. Start the game again.")
        else:
            self.say(f"Found {len(self.problems)} problem(s). See the Crash doctor tab." if self.problems else "No problems found.")

    def log_line(self, text):
        self.log.configure(state="normal")
        self.log.insert("end", time.strftime("%H:%M:%S  ") + text + "\n")
        self.log.see("end")
        self.log.configure(state="disabled")
        self.say(text)

    def _drain(self):
        try:
            while True:
                kind, p = self.events.get_nowait()
                if kind == "job":
                    self.show_job(p)
                elif kind == "mods":
                    self.refresh_mods()
                elif kind == "log":
                    self.log_line(p)
                elif kind == "key":
                    self.key_info.configure(text=p, foreground=OK if p.startswith("Key OK") else BAD)
                elif kind == "game":
                    self.game_pill.configure(text="● Game running" if p else "● Game not running", foreground=OK if p else DIM)
                    self.game_state.configure(text="● The game is running. I am watching for a crash." if p else
                                              "● The game is not running. I watch for crashes in the background.")
                    self.refresh_bisect()
                elif kind == "problems":
                    self.show_problems(p)
                elif kind == "video":
                    self.show_found(p)
                elif kind == "browse":
                    self.show_browse(p)
                elif kind == "thumbs":
                    self.show_thumbs(p)
                elif kind == "ai_status":
                    self.ai_info.configure(text=p, foreground=OK if p.startswith("✔") else BAD if p.startswith("✖") else DIM)
                elif kind == "ai":
                    self.show_ai(p)
                elif kind == "pack_choice":
                    self.pack_choice(p)
                elif kind == "outdated":
                    self.ask_outdated(p)
                elif kind == "show":
                    self.root.deiconify()
                    self.root.lift()
                elif kind == "confirm":  # a Nexus link this app did not ask for: it may come from any web page
                    self.root.deiconify()
                    self.root.lift()
                    if messagebox.askyesno("Nexus download", " ".join(p["text"].split())[:300] + "\n\nSay yes only if you clicked this download "
                                                                        "yourself on Nexus Mods.", default=messagebox.NO):
                        self.ctl.add_link(p["link"])
                        self.tabs.select(self.tab["Downloads"])
        except queue.Empty:
            pass
        self.tick = self.root.after(150, self._drain)

    def _serve(self, server):
        """Second app instances (started by nxm:// links) connect to the named pipe here."""
        while True:
            try:
                conn = server.accept()
            except OSError:
                return  # the pipe was closed
            except Exception:  # noqa: BLE001 - one bad connection must not end the hand-over
                time.sleep(0.2)
                continue
            threading.Thread(target=self._take, args=(conn,), daemon=True).start()  # a stalled client blocks only itself

    def _take(self, conn):
        """Both sides prove they know the token; only then is the nxm link (or "show") read.
        At most 4 connections are checked at once, and each gets `handshake_timeout` seconds, so callers that
        stall (other accounts can open the pipe for reading) cannot use up the app's threads."""
        if not self.handshakes.acquire(blocking=False):
            conn.close()
            return
        once = threading.Lock()

        def close():  # the timer and this thread can both get here; a second CloseHandle could hit a reused handle
            if once.acquire(blocking=False):
                conn.close()

        cut = threading.Timer(self.handshake_timeout, close)
        cut.start()
        try:
            if self.ctl.stop.is_set():
                return  # closing: refuse, so the sender keeps its link and opens the next window when this one is gone
            deliver_challenge(conn, self.token)
            answer_challenge(conn, self.token)
            msg = conn.recv_bytes(1 << 20).decode("utf-8", "replace").strip()
            if self.ctl.stop.is_set():
                return  # closing began meanwhile: no "ok", so the sender keeps the link
            if msg.startswith("{"):  # a tool call from an AI app (ai_link/mcp.py); nxm hand-overs never start with "{"
                cut.cancel()  # the caller proved the token; its tool may take longer than a handshake
                conn.send_bytes(json.dumps(tools.run(self.ctl, msg)).encode())
                return
            conn.send_bytes(b"ok")  # the sender counts the link as delivered only after this
        except (AuthenticationError, OSError, EOFError, TypeError):  # TypeError: the connection was cut mid-read
            return
        finally:
            cut.cancel()
            close()
            self.handshakes.release()
        if msg.lower().startswith("nxm://"):
            self.ctl.bg(self.ctl.handle_nxm, msg)
        if msg.lower().startswith("nxm://") or msg == "show":
            self.events.put(("show", None))


def run(server=None, first_url=None, register=None, registered=None, token=b"", owner=None):
    try:
        import ctypes
        ctypes.windll.shcore.SetProcessDpiAwareness(1)  # sharp text on scaled displays
    except (AttributeError, OSError):
        pass
    root = tk.Tk()
    App(root, server, first_url, register, registered, token=token, owner=owner)
    root.mainloop()
