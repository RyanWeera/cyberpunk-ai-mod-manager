"""Builds the real window (hidden) on a throwaway store and drives its actions."""
import json
import tempfile
import threading
import time
import tkinter as tk
import unittest
import uuid
from multiprocessing import AuthenticationError
from multiprocessing.connection import Client, Listener
from pathlib import Path
from unittest import mock

from manager_ui import ui
from shared.store import Store
from tests.helpers import fake_game, mod, stage


class Window(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())
        self.store = Store(self.tmp / "data")
        self.root = tk.Tk()
        self.root.withdraw()
        with mock.patch.object(ui, "find_game", lambda: ""):
            self.app = ui.App(self.root, store=self.store)
        self.root.update()

    def tearDown(self):
        self.app.ctl.stop.set()
        self.root.after_cancel(self.app.tick)
        self.root.destroy()

    def tab_text(self, name):
        return self.app.tabs.tab(self.app.tab[name], "text")

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
            app.show_ai({"error": "Ollama did not start."})
        err.assert_called_once_with("Local AI", "Ollama did not start.")

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
        self.assertIn("1.0.1", top.title())
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
