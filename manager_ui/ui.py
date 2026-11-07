"""The window. Tk runs on the main thread; Controller events arrive on a queue that _drain polls."""
import hmac
import os
import queue
import threading
import time
import tkinter as tk
import webbrowser
from pathlib import Path
from tkinter import filedialog, messagebox, ttk

from manager_ui.controller import CORE, Controller
from shared.store import Store, find_game

BG, PANEL, PANEL2, FG, DIM, ACCENT = "#0e1016", "#181b24", "#232837", "#e8e8e8", "#8a8f9c", "#fcee0a"
OK, BAD, WARN = "#3ddc84", "#ff6b6b", "#ffb347"
KEY_PAGE = "https://next.nexusmods.com/settings/api-keys"
EXAMPLES = ("https://www.nexusmods.com/cyberpunk2077/mods/107\n"
            "https://www.nexusmods.com/games/cyberpunk2077/collections/iszwwe\n"
            "https://github.com/psiberx/cp2077-codeware")
LEVEL = {"high": "Serious", "medium": "Warning", "info": "Info"}
ACTIVE = ("queued", "running", "waiting", "needkey")


def kind_of(m):
    if m.get("framework"):
        return "Framework"
    tags = []
    for r in m["files"]:
        r = r.lower()
        tag = ("REDmod" if r.startswith("mods/") else "CET" if "cyber_engine_tweaks/mods/" in r else "RED4ext" if r.startswith("red4ext/")
               else "Redscript" if r.endswith(".reds") else "Tweak" if r.startswith("r6/tweaks/") else "Archive" if r.startswith("archive/") else "Other")
        if tag not in tags:
            tags.append(tag)
    return ", ".join([t for t in tags if t != "Other"][:3] or tags)


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
        tk.Label(self.top, text=self.text, bg=PANEL2, fg=FG, padx=8, pady=5, justify="left", wraplength=460,
                 font=("Segoe UI", 9)).pack()

    def hide(self, _):
        if self.top:
            self.top.destroy()
            self.top = None


class App:
    def __init__(self, root, server=None, first_url=None, register=None, registered=None, store=None, token=""):
        self.root, self.events, self.token = root, queue.Queue(), token
        self.register, self.registered = register, registered or (lambda: False)
        self.store = store or Store()
        if not self.store.game():
            self.store.settings["game_dir"] = find_game() or self.store.settings["game_dir"]
            self.store.save()
        self.ctl = Controller(self.store, lambda kind, payload=None: self.events.put((kind, payload)))
        self.problems, self.was_busy, self.banner_hidden = [], False, False
        self.k = root.winfo_fpixels("1i") / 96  # pixel sizes below are for 100% display scaling
        root.title("Cyberpunk 2077 AI Mod Manager")
        root.geometry(f"{self.px(1180)}x{self.px(760)}")
        root.minsize(self.px(900), self.px(560))
        root.configure(bg=BG)
        icon = Path(__file__).with_name("icon.png")
        if icon.exists():
            self.icon = tk.PhotoImage(file=str(icon))
            root.iconphoto(True, self.icon)
        self._style()
        self._header()
        self._statusbar()
        self.tabs = ttk.Notebook(root)
        self.tabs.pack(fill="both", expand=True, padx=12, pady=(8, 4))
        self.tab = {}
        for name in ("Mods", "Add mods", "Downloads", "Crash doctor", "Settings"):
            f = self.tab[name] = ttk.Frame(self.tabs, padding=12)
            self.tabs.add(f, text=name)
            getattr(self, "_tab_" + name.split()[0].lower())(f)
        self.refresh_mods()
        self.refresh_setup()
        self.refresh_history()
        self.tick = root.after(150, self._drain)
        if server:
            threading.Thread(target=self._serve, args=(server,), daemon=True).start()
        if first_url:
            self.ctl.bg(self.ctl.handle_nxm, first_url)  # a bad link (e.g. another game) becomes a message, not a crash
        if not self.store.game():
            self.tabs.select(self.tab["Settings"])

    def px(self, n):
        return int(n * self.k)

    # ---------- look ----------
    def _style(self):
        s = ttk.Style()
        s.theme_use("clam")
        s.configure(".", background=BG, foreground=FG, fieldbackground=PANEL, bordercolor=PANEL2, lightcolor=BG, darkcolor=BG,
                    troughcolor=BG, font=("Segoe UI", 10))
        s.configure("TNotebook", background=BG, borderwidth=0)
        s.configure("TNotebook.Tab", background=PANEL, foreground=DIM, padding=(16, 7), borderwidth=0)
        s.map("TNotebook.Tab", background=[("selected", PANEL2)], foreground=[("selected", ACCENT)])
        s.configure("TButton", background=PANEL2, foreground=FG, padding=(12, 6), borderwidth=0)
        s.map("TButton", background=[("active", "#30364a"), ("disabled", PANEL)], foreground=[("disabled", DIM)])
        s.configure("Big.TButton", background=ACCENT, foreground="#000", font=("Segoe UI Semibold", 10))
        s.map("Big.TButton", background=[("active", "#fff35c")])
        s.configure("Treeview", background=PANEL, fieldbackground=PANEL, foreground=FG, rowheight=self.px(26), borderwidth=0)
        s.configure("Treeview.Heading", background=PANEL2, foreground=ACCENT, borderwidth=0, padding=(6, 4))
        s.map("Treeview.Heading", background=[("active", PANEL2)])
        s.map("Treeview", background=[("selected", "#3a3f12")], foreground=[("selected", FG)])
        s.configure("TEntry", fieldbackground=PANEL, foreground=FG, insertcolor=FG, padding=5)
        s.configure("Vertical.TScrollbar", background=PANEL2, troughcolor=BG, arrowcolor=DIM, borderwidth=0)
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
        s.configure("Banner.TFrame", background="#2b2610")
        s.configure("Banner.TLabel", background="#2b2610", foreground=ACCENT)

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
                       padx=8, pady=6, highlightthickness=0, selectbackground="#3a3f12")

    def _header(self):
        head = ttk.Frame(self.root, padding=(16, 12, 16, 0))
        head.pack(fill="x")
        ttk.Label(head, text="CYBERPUNK 2077  ·  AI MOD MANAGER", style="Title.TLabel").pack(side="left")
        b = ttk.Button(head, text="▶  Launch game", style="Big.TButton", command=self.launch)
        b.pack(side="right")
        Tip(b, "Copies your mods into the game, starts Cyberpunk 2077 and watches it for crashes.")
        self.game_pill = ttk.Label(head, text="● Game not running", style="Dim.TLabel")
        self.game_pill.pack(side="right", padx=14)
        self.banner = ttk.Frame(self.root, style="Banner.TFrame", padding=(14, 8))
        self.banner_text = ttk.Label(self.banner, style="Banner.TLabel")
        self.banner_text.pack(side="left")
        ttk.Button(self.banner, text="✕", width=3, command=self.hide_banner).pack(side="right")
        ttk.Button(self.banner, text="Open Settings", command=lambda: self.tabs.select(self.tab["Settings"])).pack(side="right", padx=6)

    def _statusbar(self):
        bar = ttk.Frame(self.root, padding=(16, 2, 16, 10))
        bar.pack(side="bottom", fill="x")
        self.status = ttk.Label(bar, style="Dim.TLabel", text="Ready.")
        self.status.pack(side="left", fill="x", expand=True)
        self.progress = ttk.Progressbar(bar, length=self.px(220), mode="determinate")
        self.progress_text = ttk.Label(bar, style="Dim.TLabel")
        self.progress.pack(side="right")
        self.progress_text.pack(side="right", padx=8)

    # ---------- tabs ----------
    def _tab_mods(self, f):
        top = ttk.Frame(f)
        top.pack(fill="x")
        ttk.Label(top, text="Search").pack(side="left")
        self.search = tk.StringVar()
        self.search.trace_add("write", lambda *_: self.refresh_mods())
        ttk.Entry(top, textvariable=self.search, width=26).pack(side="left", padx=(6, 14))
        self._buttons(top, [
            ("Turn on / off", self.toggle, "Turn the selected mods on or off. Shortcut: Space, or click the On column."),
            ("Move up", lambda: self.move(-1), "Higher in the list = lower priority."),
            ("Move down", lambda: self.move(1), "Lower in the list wins when two mods change the same file."),
            ("Update", self.update, "Download the newest version of the selected mods."),
            ("Remove", self.remove, "Remove the selected mods from the game and from this app. Shortcut: Delete."),
            ("Open folder", self.open_folder, "Show the files of the selected mod."),
            ("Apply to game", lambda: self.redeploy(), "Copy the mods that are on into the game folder now.")]).pack(side="left")
        self.mods_box = ttk.Frame(f)
        self.mods_box.pack(fill="both", expand=True, pady=10)
        self.mods_tree_frame, self.mods = self._tree(self.mods_box, ("On", "Mod", "Version", "Type", "Source"),
                                                     (54, 430, 110, 210, 240), 20, select="extended")
        self.mods.bind("<Double-1>", lambda e: self.toggle())
        self.mods.bind("<space>", lambda e: self.toggle())
        self.mods.bind("<Delete>", lambda e: self.remove())
        self.mods.bind("<Button-1>", self._mods_click, add="+")
        self.mods.bind("<Button-3>", self._mods_menu)
        self.menu = tk.Menu(self.root, tearoff=0, bg=PANEL2, fg=FG, activebackground=ACCENT, activeforeground="#000", bd=0)
        for label, cmd in (("Turn on / off", self.toggle), ("Update", self.update), ("Open folder", self.open_folder),
                           ("Move up", lambda: self.move(-1)), ("Move down", lambda: self.move(1)), ("Remove", self.remove)):
            self.menu.add_command(label=label, command=cmd)
        self.mods_empty = ttk.Frame(self.mods_box, style="Card.TFrame", padding=30)
        self.mods_empty_text = ttk.Label(self.mods_empty, style="Card.TLabel", font=("Segoe UI", 12), justify="center")
        self.mods_empty_text.pack(pady=(0, 14))
        self._buttons(self.mods_empty, [("▶ Add mods", lambda: self.tabs.select(self.tab["Add mods"]), None),
                                        ("Install the core frameworks", self.install_core,
                                         "RED4ext, Cyber Engine Tweaks, redscript, ArchiveXL, TweakXL and Codeware. Most mods need them.")]).pack()
        self.mods_info = ttk.Label(f, style="Dim.TLabel")
        self.mods_info.pack(side="bottom", anchor="w", before=self.mods_box)  # stays visible however tall the list is

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
            ("Find the bad mod for me", self.bisect_begin, "Turns half the mods off for each game start until the mod that crashes the game is found."),
            ("Stop the search", self.bisect_cancel, "Stop the bad-mod search and turn your mods back on.")]).pack(fill="x", pady=8)
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
        self._buttons(f, [("Use this app for Nexus downloads", self.register_nxm,
                           "Makes the 'Mod Manager Download' and 'Slow download' buttons on Nexus send files to this app.")]).grid(row=7, column=2, sticky="w")

        ttk.Label(f, text="Options", style="Section.TLabel").grid(row=8, column=0, sticky="w", pady=(18, 6))
        self.flags = {}
        for row, (key, text) in enumerate([
                ("virus_scan", "Scan every download for viruses with Microsoft Defender (turn off to go faster with many mods)"),
                ("auto_fix", "After a crash, fix a problem by myself when it has only one fix"),
                ("include_optional", "Download optional files in collections")], start=9):
            v = self.flags[key] = tk.BooleanVar(value=st[key])
            ttk.Checkbutton(f, text=text, variable=v, command=self.save_settings).grid(row=row, column=0, columnspan=3, sticky="w", pady=3)
        foot = ttk.Frame(f)
        foot.grid(row=12, column=0, columnspan=3, sticky="we", pady=(22, 0))
        ttk.Button(foot, text="Open data folder", command=lambda: os.startfile(self.store.root)).pack(side="right")
        ttk.Label(foot, style="Dim.TLabel", text=f"Mods, downloads and backups are kept in {self.store.root}").pack(side="left", fill="x")
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
        self.ctl.bg(self.ctl.launch)  # copying mods can take a while; errors arrive as messages
        self.say("Starting the game. I watch it for crashes.")

    def _mods_click(self, e):
        if self.mods.identify_region(e.x, e.y) == "cell" and self.mods.identify_column(e.x) == "#1":
            iid = self.mods.identify_row(e.y)
            if iid:
                self._set_on([iid], not self.store.mod(iid)["enabled"])
                return "break"
        return None

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
        on = self.registered()
        self.nxm_info.configure(text="✔  This app gets Nexus downloads" if on else "✖  Not set up yet", foreground=OK if on else WARN)
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
        q = self.search.get().strip().lower()
        shown = 0
        for m in self.store.mods:
            if q and q not in m["name"].lower():
                continue
            s = m["source"]
            src = f"Nexus #{s['mod_id']}" if s["type"] == "nexus" else f"GitHub {s['repo']}" if s["type"] == "github" else \
                "Local file" if s["type"] == "file" else "Web link"
            self.mods.insert("", "end", iid=m["id"], values=("✔" if m["enabled"] else "○", m["name"], m.get("version", ""), kind_of(m), src),
                             tags=() if m["enabled"] else ("off",))
            shown += 1
        keep = [i for i in sel if self.mods.exists(i)]
        if keep:
            self.mods.selection_set(keep)
        if shown:
            self.mods_empty.pack_forget()
            self.mods_tree_frame.pack(fill="both", expand=True)
        else:
            self.mods_tree_frame.pack_forget()
            self.mods_empty_text.configure(text="No mod matches your search." if self.store.mods else
                                           "No mods yet.\nPaste links in 'Add mods', or start with the core frameworks.")
            self.mods_empty.pack(fill="both", expand=True)
        on = len(self.store.enabled())
        self.mods_info.configure(text=f"{len(self.store.mods)} mods, {on} on.   Lower in the list wins when two mods change the same file.   "
                                      "Right-click a mod for more.")
        self.refresh_tabs()

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
        self.progress_text.configure(text=f"{done} of {len(jobs)} downloads finished" if active else "")
        if not active and self.was_busy:
            failed = sum(j["state"] == "failed" for j in jobs)
            self.say("All downloads finished." + (f" {failed} failed: see the Downloads tab." if failed else ""))
        self.was_busy = bool(active)
        waiting = next((j for j in jobs if j["state"] == "waiting"), None)
        needkey = any(j["state"] == "needkey" for j in jobs)
        if waiting:
            self.dl_banner_text.configure(text=f"Your browser shows the Nexus page for '{waiting['name']}'. Click 'Slow download' "
                                               "there and I do the rest.")
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
                elif kind == "show":
                    self.root.deiconify()
                    self.root.lift()
        except queue.Empty:
            pass
        self.tick = self.root.after(150, self._drain)

    def _serve(self, server):
        """Second app instances (started by nxm:// links) send "<token> <nxm link or show>" here.
        Anything without the token (another user, a web page poking localhost) is ignored."""
        while True:
            conn, _ = server.accept()
            with conn:
                conn.settimeout(2)
                try:
                    data = conn.recv(4096).decode("utf-8", "replace").strip()
                except OSError:
                    continue
            tok, _, msg = data.partition(" ")
            if not self.token or not hmac.compare_digest(tok, self.token):
                continue
            if msg.lower().startswith("nxm://"):
                self.ctl.bg(self.ctl.handle_nxm, msg)
            if msg.lower().startswith("nxm://") or msg == "show":
                self.events.put(("show", None))


def run(server=None, first_url=None, register=None, registered=None, token=""):
    try:
        import ctypes
        ctypes.windll.shcore.SetProcessDpiAwareness(1)  # sharp text on scaled displays
    except (AttributeError, OSError):
        pass
    root = tk.Tk()
    App(root, server, first_url, register, registered, token=token)
    root.mainloop()
