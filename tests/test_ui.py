"""Builds the real window (hidden) on a throwaway store and drives its actions."""
import json
import tempfile
import threading
import time
import tkinter as tk
from tkinter import ttk
import unittest
import uuid
from multiprocessing import AuthenticationError
from multiprocessing.connection import Client, Listener
from pathlib import Path
from unittest import mock

from manager_ui import ui
from shared.store import VERSION, Store
from tests.helpers import fake_game, mod, stage


class Window(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())
        self.store = Store(self.tmp / "data")
        self.root = tk.Tk()
        self.root.withdraw()
        find = mock.patch.object(ui, "find_game", lambda: "")  # never point a test at the real game on this PC
        find.start()
        self.addCleanup(find.stop)
        self.app = ui.App(self.root, store=self.store)
        self.root.update()

    def tearDown(self):
        self.app.ctl.stop.set()
        self.root.after_cancel(self.app.tick)
        self.root.destroy()

    def tab_text(self, name):
        return self.app.tabs.tab(self.app.tab[name], "text")

    def test_style_switch_recolors_and_is_kept(self):
        self.addCleanup(ui.use_theme, "Cyberpunk")  # colours are module globals shared by every test
        self.assertEqual(self.store.settings["theme"], "Cyberpunk")
        self.app.theme_var.set("Arasaka")
        self.app.set_theme()
        self.assertEqual(self.store.settings["theme"], "Arasaka")
        self.assertEqual(ui.ACCENT, ui.THEMES["Arasaka"]["ACCENT"])
        self.assertEqual(self.app.prob_detail.cget("bg"), ui.THEMES["Arasaka"]["PANEL"])
        self.assertEqual(Store(self.tmp / "data").settings["theme"], "Arasaka")  # saved for the next start

    def test_first_run_shows_setup_and_empty_state(self):
        app = self.app
        self.assertEqual(self.tab_text("Mods"), "Mods (0)")
        self.assertEqual(app.mods_empty.winfo_manager(), "pack")
        self.assertIn("✖ Game folder", app.banner_text.cget("text"))
        self.assertEqual(str(app.tabs.select()), str(app.tab["Settings"]))
        game = fake_game(self.tmp)
        app.game_var.set(str(game))
        app.save_settings()
        self.assertIn("✔ Game folder", app.banner_text.cget("text"))
        self.assertIn("found", app.game_info.cget("text"))
        app.game_var.set(str(self.tmp))
        app.save_settings()
        self.assertIn("No bin", app.game_info.cget("text"))

    def test_mod_list_search_toggle_remove(self):
        app, s = self.app, self.store
        stage(s, mod("a", ["archive/pc/mod/a.archive"], name="Alpha HUD"))
        stage(s, mod("b", ["r6/scripts/b/b.reds"], name="Beta Cars", enabled=False))
        app.refresh_mods()
        self.assertEqual(self.tab_text("Mods"), "Mods (2)")
        self.assertEqual(app.mods.item("b", "tags"), ("off",))
        self.assertEqual(app.mods.item("a", "values")[3], "Archive")
        app.search.set("beta")
        self.assertEqual(app.mods.get_children(), ("b",))
        app.search.set("zzz")
        self.assertIn("No mod matches", app.mods_empty_text.cget("text"))
        app.search.set("")
        app.mods_show.set("Off")
        app.refresh_mods()
        self.assertEqual(app.mods.get_children(), ("b",))
        app.mods_show.set("All")
        app.refresh_mods()
        self.assertIn("2 mods  ·  1 on", app.pack_sub.cget("text"))
        app.mods.selection_set(("a", "b"))
        app.toggle()  # one is off -> both on
        self.assertTrue(s.mod("a")["enabled"] and s.mod("b")["enabled"])
        app.toggle()  # all on -> both off
        self.assertFalse(s.mod("a")["enabled"] or s.mod("b")["enabled"])
        with mock.patch.object(ui.messagebox, "askyesno", return_value=True):
            app.mods.selection_set(("a",))
            app.remove()
        self.assertIsNone(s.mod("a"))
        self.assertFalse((s.staging / "a").exists())

    def test_rail_and_add_dialog(self):
        app = self.app
        app.rail_items["Downloads"].event_generate("<Button-1>")
        self.root.update()
        self.assertEqual(str(app.tabs.select()), str(app.tab["Downloads"]))
        self.assertEqual(app.page_title.cget("text"), "Downloads")
        self.assertEqual(str(app.rail_items["Downloads"].cget("style")), "RailOn.TLabel")
        win = app.add_dialog()
        rows = [w for w in win.winfo_children() if str(w.cget("style")) == "Opt.TFrame"]
        self.assertEqual(len(rows), 5)
        rows[1].event_generate("<Button-1>")  # Paste links
        self.assertEqual(str(app.tabs.select()), str(app.tab["Add mods"]))
        self.assertFalse(win.winfo_exists())

    def test_add_links_placeholder_and_count(self):
        app = self.app
        self.assertEqual(app._links_text(), "")  # example text is not a link
        app._placeholder(False)
        app.links.insert("1.0", "https://www.nexusmods.com/cyberpunk2077/mods/107\n\nhttps://github.com/a/b\n")
        app._count_links()
        self.assertEqual(app.link_count.cget("text"), "2 link(s) ready")
        with mock.patch.object(app.ctl, "add_links") as add:
            app.add_links()
        add.assert_called_once()
        self.assertEqual(str(app.tabs.select()), str(app.tab["Downloads"]))
        self.assertIn("Added 2 link(s)", app.status.cget("text"))

    def test_browse_tab_cards_pages_and_install(self):
        app = self.app
        self.assertEqual(app.browse_req, 0)  # no game folder yet: Settings shows, and nothing loads from the web
        stage(self.store, mod("cet", [], source={"type": "github", "repo": "maximegmd/CyberEngineTweaks"}))
        mods = [{"id": 107, "name": "Cyber Engine Tweaks", "summary": "Scripting", "author": "yamashi", "downloads": 20381596,
                 "endorsements": 248536, "updated": "2026-10-26T01:28:58Z", "thumb": "https://staticdelivery.nexusmods.com/a.png",
                 "category": "Modders Resources"},
                {"id": 5, "name": "Nova LUT", "summary": "", "author": "", "downloads": 9, "endorsements": 0, "updated": "",
                 "thumb": "", "category": ""}]
        app.browse_req = 3
        with mock.patch.object(app.ctl, "bg") as bg:
            app.show_browse({"req": 2, "page": 0, "total": 1, "categories": {}, "mods": []})  # an older search: ignored
            self.assertEqual(app.results.winfo_children(), [])
            app.show_browse({"req": 3, "page": 0, "total": 41, "categories": {"Weapons": 1464, "Audio": 1438}, "mods": mods})
        bg.assert_called_once()  # thumbnails load in the background
        self.assertEqual(list(app.cat_vars), ["Audio", "Weapons"])
        self.assertEqual(len(app.results.winfo_children()), 2)
        self.assertEqual(app.browse_info.cget("text"), "41 mods")
        self.assertEqual([w.cget("text") for w in app.pager.winfo_children()], ["1", "2", "3"])
        self.assertTrue(app.prev_btn.instate(["disabled"]))
        self.assertFalse(app.next_btn.instate(["disabled"]))
        self.assertEqual(app.browse_buttons[107].cget("text"), "✔  Installed")  # the framework came from GitHub
        with mock.patch.object(app.ctl, "add_links") as add:
            app.browse_install(mods[1])
        add.assert_called_once_with("https://www.nexusmods.com/cyberpunk2077/mods/5")
        self.assertTrue(app.browse_buttons[5].instate(["disabled"]))
        app.show_browse({"req": 3, "error": "timed out"})
        self.assertIn("timed out", app.browse_info.cget("text"))

    def test_find_mod_from_video(self):
        app = self.app
        with mock.patch.object(app.ctl, "bg") as bg:
            app.find_video()
            bg.assert_not_called()  # nothing pasted
            app.video.set(" https://youtu.be/8Yexbiw0T28 ")
            app.find_video()
        bg.assert_called_once_with(app.ctl.find_in_video, "https://youtu.be/8Yexbiw0T28")
        a, b = "https://www.nexusmods.com/cyberpunk2077/mods/1", "https://github.com/a/b"
        app.show_found({"title": "Top mods", "rows": [{"name": "Nova LUT", "url": a, "why": "Linked in the video", "endorsements": 5},
                                                      {"name": "a/b", "url": b, "why": "Linked in the video", "endorsements": None}]})
        self.assertEqual(app.found.get_children(), (a, b))
        self.assertEqual(app.video_info.cget("text"), '"Top mods"   2 mod(s) linked in the video.')
        app.found.selection_set(b)
        with mock.patch.object(app.ctl, "add_links") as add:
            app.add_found()
        add.assert_called_once_with(b)
        self.assertEqual(str(app.tabs.select()), str(app.tab["Downloads"]))
        app.show_found({"error": "HTTP Error 404: Not Found"})
        self.assertEqual(app.found.get_children(), ())
        self.assertIn("404", app.video_info.cget("text"))

    def test_download_banner_progress_and_tabs(self):
        app = self.app
        j = {"id": 1, "name": "HUD", "src": {"type": "nexus", "mod_id": 1, "file_id": 2}, "state": "needkey",
             "status": "Needs key", "progress": ""}
        app.ctl.jobs.append(j)
        app.show_job(j)
        self.assertEqual(app.dl_banner.winfo_manager(), "pack")
        self.assertIn("API key", app.dl_banner_text.cget("text"))
        self.assertEqual(self.tab_text("Downloads"), "Downloads (1)")
        self.assertEqual(app.jobs.item("1", "tags"), ("needkey",))
        j.update(state="done", status="Done")
        app.show_job(j)
        self.assertEqual(app.dl_banner.winfo_manager(), "")
        self.assertEqual(self.tab_text("Downloads"), "Downloads")
        self.assertIn("All downloads finished", app.status.cget("text"))

    def test_link_pipe_needs_the_token_both_ways(self):
        pipe = rf"\\.\pipe\cp2077mm-test-{uuid.uuid4().hex}"
        server = Listener(pipe, family="AF_PIPE")
        top = tk.Toplevel(self.root)
        app = ui.App(top, server=server, store=self.store, token=b"t0k3n")
        self.addCleanup(lambda: (app.ctl.stop.set(), top.after_cancel(app.tick), server.close()))

        def events():
            time.sleep(0.3)
            out = []
            while not app.events.empty():
                out.append(app.events.get()[0])
            return out

        with mock.patch.object(app.ctl, "handle_nxm") as nxm:
            with self.assertRaises(AuthenticationError):  # wrong token: the window refuses, nothing is sent
                with Client(pipe, family="AF_PIPE", authkey=b"wrong") as c:
                    c.send_bytes(b"nxm://cyberpunk2077/mods/1/files/2?key=a&expires=1")
            self.assertNotIn("show", events())
            with Client(pipe, family="AF_PIPE", authkey=b"t0k3n") as c:
                c.send_bytes(b"nxm://cyberpunk2077/mods/1/files/2?key=a&expires=1")
                self.assertEqual(c.recv_bytes(), b"ok")  # the window confirms it took the link
            self.assertIn("show", events())
            app.ctl.stop.set()  # closing: even the right token is refused, so the sender keeps its link
            with self.assertRaises((AuthenticationError, OSError, EOFError)):
                with Client(pipe, family="AF_PIPE", authkey=b"t0k3n") as c:
                    c.send_bytes(b"nxm://cyberpunk2077/mods/1/files/3?key=a&expires=1")
        nxm.assert_called_once()

    def test_ai_app_tool_call_over_the_pipe(self):
        pipe = rf"\\.\pipe\cp2077mm-test-{uuid.uuid4().hex}"
        server = Listener(pipe, family="AF_PIPE")
        top = tk.Toplevel(self.root)
        app = ui.App(top, server=server, store=self.store, token=b"t0k3n")
        self.addCleanup(lambda: (app.ctl.stop.set(), top.after_cancel(app.tick), server.close()))
        with Client(pipe, family="AF_PIPE", authkey=b"t0k3n") as c:
            c.send_bytes(json.dumps({"tool": "status", "args": {}}).encode())
            self.assertIn("Game folder: not set", json.loads(c.recv_bytes())["text"])
        with Client(pipe, family="AF_PIPE", authkey=b"t0k3n") as c:
            c.send_bytes(b'{"tool": "nope"}')
            self.assertIn("error", json.loads(c.recv_bytes()))

    def test_ai_tab_setup_text_and_advice(self):
        app = self.app
        self.assertIn("AI helpers", app.tab)
        app.copy_setup(True)
        server = json.loads(self.root.clipboard_get())["mcpServers"]["cp2077-ai-mod-manager"]
        self.assertEqual(server["args"][-1], "--mcp")
        app.copy_setup(False)
        self.assertTrue(self.root.clipboard_get().startswith("claude mcp add --scope user cp2077-ai-mod-manager -- "))
        advice = {"summary": "s", "fixes": [({"title": "t"}, {"label": "Turn A off"}, "w")], "order": ["a"], "moved": 1}
        for answer in (False, True):
            with mock.patch.object(ui.messagebox, "askyesno", return_value=answer) as ask, mock.patch.object(app.ctl, "bg") as bg:
                app.show_ai(advice)
            self.assertIn("1 mod(s) change place", ask.call_args.args[1])
            self.assertIn("t  →  Turn A off", ask.call_args.args[1])
            self.assertEqual(bg.call_count, int(answer))
        with mock.patch.object(ui.messagebox, "showinfo") as info:
            app.show_ai({"summary": "", "fixes": [], "order": None, "moved": 0})
        self.assertIn("nothing to change", info.call_args.args[1])
        with mock.patch.object(ui.messagebox, "showerror") as err:
            app.show_ai({"error": "The local AI did not start."})
        err.assert_called_once_with("Local AI", "The local AI did not start.")

    def test_bug_report_goes_to_the_local_ai(self):
        app = self.app
        with mock.patch.object(ui.messagebox, "showinfo") as info, mock.patch.object(app.ctl, "bg") as bg:
            app.ask_bug()  # nothing written
        bg.assert_not_called()
        info.assert_called_once()
        app.bug_text.insert("1.0", "  My modded car does not show up \n")
        with mock.patch.object(app.ctl, "bg") as bg:
            app.ask_bug()
        bg.assert_called_once_with(app.ctl.ask_ai, "My modded car does not show up")
        with mock.patch.object(ui.messagebox, "showinfo") as info:
            app.show_ai({"summary": "s", "cause": "The car mod is off.", "fixes": [], "order": None, "moved": 0})
        self.assertTrue(info.call_args.args[1].startswith("Why it happens: The car mod is off."))

    def test_stalled_pipe_callers_are_cut_off(self):
        pipe = rf"\\.\pipe\cp2077mm-test-{uuid.uuid4().hex}"
        server = Listener(pipe, family="AF_PIPE")
        top = tk.Toplevel(self.root)
        app = ui.App(top, server=server, store=self.store, token=b"t0k3n")
        app.handshake_timeout = 0.5
        stalls = [Client(pipe, family="AF_PIPE") for _ in range(6)]  # connect, never answer the challenge
        self.addCleanup(lambda: [s.close() for s in stalls])
        self.addCleanup(lambda: (app.ctl.stop.set(), top.after_cancel(app.tick), server.close()))
        time.sleep(1.5)  # every stalled check has been cut off by now
        with Client(pipe, family="AF_PIPE", authkey=b"t0k3n") as c:
            c.send_bytes(b"show")
            self.assertEqual(c.recv_bytes(), b"ok")

    def test_cut_off_caller_is_closed_once(self):
        closes, cut = [], threading.Event()

        class Stalled:  # answers nothing; reading ends only when the time limit closes it
            def send_bytes(self, b):
                pass

            def recv_bytes(self, n=None):
                cut.wait(5)
                raise OSError("closed")

            def close(self):
                closes.append(threading.current_thread().name)
                cut.set()

        self.app.handshake_timeout = 0.2
        self.app._take(Stalled())
        self.assertEqual(len(closes), 1)  # the timer closed it; the handshake thread must not close the handle again

    def test_unrequested_link_needs_a_yes(self):
        app = self.app
        for answer in (False, True):
            link = {"type": "nexus", "mod_id": 9, "file_id": 8}
            with mock.patch.object(ui.messagebox, "askyesno", return_value=answer) as ask, \
                    mock.patch.object(app.ctl, "add_link") as add:
                app.events.put(("confirm", {"link": link, "text": "Install?"}))
                app.root.after_cancel(app.tick)
                app._drain()
            self.assertEqual(ask.call_args.kwargs["default"], "no")
            self.assertEqual(add.call_args_list, [mock.call(link)] * answer)

    def test_pack_choice_and_outdated_questions(self):
        app = self.app
        link = {"type": "collection", "slug": "abc", "revision": None}
        files = [{"mod_id": 6, "file_id": 60, "optional": True, "name": "Extra", "outdated": ""},
                 {"mod_id": 7, "file_id": 70, "optional": False, "name": "Old", "outdated": "it is old"}]
        with mock.patch.object(app.ctl, "add_link") as add:
            picks = app.pack_choice({"link": link, "name": "P", "files": files})
            picks[70].set(False)  # skip the old mod
            app.choice_ok()
            add.assert_called_once_with({**link, "chosen": [60]})
            add.reset_mock()
            one = {"type": "nexus", "mod_id": 3}
            app.ask_outdated({"link": one, "name": "Old", "why": "it is old"})(True)
            app.ask_outdated({"link": one, "name": "Old", "why": "it is old"})(False)
            add.assert_called_once_with({**one, "anyway": True})

    def test_pipe_squatter_gets_nothing(self):
        """A program that holds the pipe name but does not know the token learns no link."""
        pipe = rf"\\.\pipe\cp2077mm-test-{uuid.uuid4().hex}"
        got = []

        def squatter():
            with Listener(pipe, family="AF_PIPE", authkey=b"guess") as ls:
                try:
                    with ls.accept() as c:
                        got.append(c.recv_bytes())
                except (AuthenticationError, OSError, EOFError):
                    pass

        t = threading.Thread(target=squatter, daemon=True)
        t.start()
        time.sleep(0.2)
        with self.assertRaises((AuthenticationError, OSError, EOFError)):
            with Client(pipe, family="AF_PIPE", authkey=b"real-token") as c:
                c.send_bytes(b"nxm://secret")
        t.join(3)
        self.assertEqual(got, [])

    def test_close_waits_for_a_running_copy_and_select_all(self):
        top = tk.Toplevel(self.root)
        app = ui.App(top, store=self.store)
        stage(self.store, mod("a", ["archive/pc/mod/a.archive"]))
        stage(self.store, mod("b", ["archive/pc/mod/b.archive"]))
        app.refresh_mods()
        app.select_all()
        self.assertEqual(set(app.mods.selection()), {"a", "b"})
        self.assertIn(VERSION, top.title())
        released = []

        def busy_copy():
            with self.store.lock:  # what a deploy holds while it copies
                time.sleep(0.5)
                released.append(time.perf_counter())

        threading.Thread(target=busy_copy, daemon=True).start()
        time.sleep(0.1)
        top.after_cancel(app.tick)
        app.close()
        self.assertTrue(app.ctl.stop.is_set())
        self.assertTrue(top.winfo_exists())  # still open (repaints, takes no input) while the copy runs
        self.assertTrue(int(top.attributes("-disabled")))
        end = time.time() + 5
        while top.winfo_exists() and time.time() < end:
            self.root.update()
            time.sleep(0.02)
        self.assertFalse(top.winfo_exists())
        self.assertTrue(released)  # it closed only after the copy finished
        other = []
        t = threading.Thread(target=lambda: other.append(self.store.lock.acquire(blocking=False)))
        t.start()
        t.join()
        self.assertEqual(other, [False])  # the window keeps the lock: no other thread can copy now
        self.store.settings["game_dir"] = str(fake_game(self.tmp))
        app.ctl.redeploy(sync=True)  # after close: no new copy starts, not even here
        app.ctl.launch()
        self.assertEqual(self.store.deployed, {})

    def test_mod_packs_in_the_window(self):
        app = self.app
        self.store.mods = [mod("a", []), mod("b", [], enabled=False)]
        with mock.patch.object(ui.simpledialog, "askstring", return_value=" Night "):
            app.save_pack()
        self.assertEqual((app.pack_pick.get(), list(app.pack_pick.cget("values"))), ("Night", [ui.CURRENT, "Night"]))
        self.store.settings["game_dir"] = str(fake_game(self.tmp))
        for pick, pack in (("Night", "Night"), (ui.CURRENT, None)):
            app.pack_pick.set(pick)
            with mock.patch.object(app.ctl, "bg") as bg:
                app.launch()
            bg.assert_called_once_with(app.ctl.launch, pack)
        app.ctl.save_pack("Day")
        app.merge_packs()
        [box] = [w for w in app.merge_win.winfo_children() if isinstance(w, tk.Listbox)]
        box.selection_set(0, 1)
        app.merge_ok()
        self.assertEqual((app.pack_pick.get(), self.store.packs["Merged pack"]), ("Merged pack", [["a", True], ["b", False]]))
        with mock.patch.object(ui.messagebox, "askyesno", return_value=True):
            app.delete_pack()
        self.assertNotIn("Merged pack", self.store.packs)
        self.assertEqual(app.pack_pick.get(), ui.CURRENT)

    def test_settings_names_the_other_app_and_fixes_it_in_one_click(self):
        app, on = self.app, []
        app.register, app.registered, app.nxm_owner = lambda: on.append(True), lambda: bool(on), lambda: "Vortex"
        app.refresh_settings()
        self.assertIn("sends files to Vortex", app.nxm_info.cget("text"))
        self.assertEqual(str(app.nxm_btn.cget("style")), "Big.TButton")
        job = {"state": "waiting", "name": "Nx", "src": {"mod_id": 5, "file_id": 6}, "manual": True}  # the hidden click failed
        app.ctl.jobs.append(job)
        with mock.patch.object(app.ctl, "reopen") as reopen:
            app.nxm_btn.invoke()
        reopen.assert_called_once_with(job)  # its last click went to Vortex
        self.assertIn("sends files to this app", app.nxm_info.cget("text"))
        self.assertEqual(app.nxm_btn.cget("text"), "Set it up again")

    def test_update_asks_first_and_turns_the_button_back_on(self):
        app, rel = self.app, {"tag": "v9.0.0", "url": "https://x/a.exe"}
        with mock.patch.object(app.ctl, "bg") as bg:
            app.update_app()
        bg.assert_called_once_with(app.ctl.update_app)
        self.assertTrue(app.update_btn.instate(["disabled"]))  # no second update while one runs
        with mock.patch.object(ui.messagebox, "askyesno", return_value=False):
            app.show_update({"ask": rel})
        self.assertFalse(app.update_btn.instate(["disabled"]))
        with mock.patch.object(ui.messagebox, "askyesno", return_value=True), mock.patch.object(app.ctl, "bg") as bg:
            app.show_update({"ask": rel})
        bg.assert_called_once_with(app.ctl.update_app, rel)
        app.show_update({"text": "Update failed: offline"})
        self.assertFalse(app.update_btn.instate(["disabled"]))

    def test_my_mod_packs_page(self):
        app = self.app
        self.store.mods = [mod("a", [], name="Alpha")]
        self.store.packs = {"Mine": [["a", True]], "Neon City": [["a", False], ["nexus-9-90", True]]}
        self.store.pack_info = {"Neon City": {"url": "https://www.nexusmods.com/games/cyberpunk2077/collections/x",
                                              "names": {"nexus-9-90": "Neon Signs"}}}
        app.refresh_packs()
        cards = lambda: [w for w in app.packs_box.winfo_children() if isinstance(w, ttk.Frame)]
        text = lambda w: " ".join(str(c.cget("text")) for c in w.winfo_children() if isinstance(c, ttk.Label))
        self.assertEqual(len(cards()), 2)
        app.packs_show.set("nexus")
        app.refresh_pack_page()
        [card] = cards()
        self.assertIn("1 not installed yet", text(card))
        app.pack_open.add("Neon City")
        app.refresh_pack_page()
        self.assertIn("Neon Signs   (not installed yet)", text(cards()[0]))
        [done] = [b for f in cards()[0].winfo_children() if isinstance(f, ttk.Frame) for b in f.winfo_children()
                  if isinstance(b, ttk.Button) and b.cget("text") == "Complete installation"]
        with mock.patch.object(app.ctl, "complete_pack", return_value=1) as complete:
            done.invoke()
        complete.assert_called_once_with("Neon City")
        app.packs_show.set("all")
        app.packs_q.set("mine")
        with mock.patch.object(app, "redeploy") as redeploy:
            app.use_pack("Mine")
        redeploy.assert_called_once()
        app.refresh_pack_page()  # the window does this when the "mods" event arrives
        self.assertIn("IN USE", " ".join(text(f) for f in cards()[0].winfo_children() if isinstance(f, ttk.Frame)))
        self.store.settings["game_dir"] = str(fake_game(self.tmp))
        with mock.patch.object(app.ctl, "bg") as bg:
            app.launch_pack("Mine")
        bg.assert_called_once_with(app.ctl.launch, "Mine")

    def test_problem_picker_applies_chosen_fix(self):
        app = self.app
        p = {"id": "x", "title": "Bad mod crashed", "detail": "d", "severity": "high", "auto": True,
             "fixes": [{"label": "Update", "action": ["update", "m"], "recommended": True},
                       {"label": "Disable", "action": ["disable", "m"], "recommended": False}]}
        app.show_problems({"list": [p], "crashed": False})
        self.assertEqual(self.tab_text("Crash doctor"), "Crash doctor (1)")
        self.assertEqual(len(app.fix_box.winfo_children()), 2)
        self.assertEqual(app.fix_choice.get(), 0)
        app.fix_choice.set(1)
        with mock.patch.object(app.ctl, "apply") as apply:
            app.apply_fix()
        apply.assert_called_once_with(p, p["fixes"][1])
        self.assertEqual(self.tab_text("Crash doctor"), "Crash doctor")
        self.assertIn("Disable", app.status.cget("text"))


if __name__ == "__main__":
    unittest.main()
