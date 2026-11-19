"""The hidden-browser auto-clicker: WebSocket framing, the nxm:// capture and the Clicker flow,
plus the controller wiring (a waiting job goes to the clicker when Settings turns automatic clicks on)."""
import contextlib
import queue
import tempfile
import time
import unittest
from pathlib import Path
from unittest import mock

from downloads import nexus_browser
from downloads.nexus_browser import scan_nxm, ws_frame, ws_message
from manager_ui import controller as ctl_mod
from manager_ui.controller import Controller
from shared.store import DEFAULTS, Store
from tests.helpers import make_zip


def wait_for(pred, timeout=5, step=0.02):
    end = time.time() + timeout
    while time.time() < end:
        if pred():
            return True
        time.sleep(step)
    return False


class WsFraming(unittest.TestCase):
    def test_round_trip(self):
        for size in (0, 5, 125, 126, 1000, 65535, 65536, 200001):
            buf = ws_frame(b"x" * size)
            got, rest = ws_message(buf)
            self.assertEqual((got[0], got[1]), (1, True))
            self.assertEqual(got[2], b"x" * size)
            self.assertEqual(rest, b"")

    def test_partial_buffer_waits(self):
        buf = ws_frame(b"hello")
        got, rest = ws_message(buf[:3])
        self.assertIsNone(got)
        got, rest = ws_message(buf[:3] + buf[3:])
        self.assertEqual(got[2], b"hello")
        self.assertEqual(rest, b"")

    def test_ping_opcode(self):
        got, _ = ws_message(ws_frame(b"pin", 0x9))
        self.assertEqual(got[0], 0x9)


class ScanNxm(unittest.TestCase):
    def test_finds_it_anywhere(self):
        deep = {"a": [{"b": None}, "nxm://cyberpunk2077/mods/1/files/2?key=k&expires=9"]}
        self.assertEqual(scan_nxm({"params": deep}), "nxm://cyberpunk2077/mods/1/files/2?key=k&expires=9")
        self.assertEqual(scan_nxm({"params": {"x": "https://cdn.example/p.zip"}}), "")


class FakeTab:
    """A DevTools tab that answers with per-JS values; callables answer freshly each time."""

    def __init__(self, script):
        self.script, self.closed = script, False

    def wait_ready(self, seconds=45):
        pass

    def run(self, js):
        v = self.script.get(js, 0)
        return v() if callable(v) else v

    def wait_nxm(self, seconds=90):
        return self.script.get("nxm")


class FakeBrowser:
    def __init__(self, tab):
        self.tab, self.alive_, self.quits = tab, True, 0

    def alive(self):
        return self.alive_

    def open(self, url):
        self.url = url
        return self.tab

    def close(self, tab):
        tab.closed = True

    def quit(self):
        self.alive_, self.quits = False, self.quits + 1


def job(mid=1, fid=2):
    return {"src": {"mod_id": mid, "file_id": fid}, "state": "waiting"}


class ClickerFlow(unittest.TestCase):
    def run_click(self, tab, waiting=lambda j: True, login=lambda self, url: True):
        logs, links, fails = [], [], []
        with mock.patch.object(nexus_browser.Clicker, "_login", login):
            c = nexus_browser.Clicker(Path("p"), log=logs.append, on_link=links.append,
                                      on_fail=lambda j, w: fails.append(w), waiting=waiting)
            c.browser = FakeBrowser(tab)
            c.click(job())
        return logs, links, fails, c

    def test_success_hands_the_nxm_link_over(self):
        tab = FakeTab({nexus_browser.LOGIN_JS: 0, nexus_browser.CLICK_JS: 1,
                       "nxm": "nxm://cyberpunk2077/mods/1/files/2?key=k&expires=9"})
        logs, links, fails, c = self.run_click(tab)
        self.assertEqual(links, ["nxm://cyberpunk2077/mods/1/files/2?key=k&expires=9"])
        self.assertEqual((fails, logs), ([], []))
        self.assertTrue(tab.closed)

    def test_a_skipped_job_loses_its_link(self):
        tab = FakeTab({nexus_browser.LOGIN_JS: 0, nexus_browser.CLICK_JS: 1, "nxm": "nxm://cyberpunk2077/mods/1/files/2"})
        logs, links, fails, c = self.run_click(tab, waiting=lambda j: False)
        self.assertEqual((links, fails), ([], []))

    def test_no_link_after_the_click_falls_back(self):
        tab = FakeTab({nexus_browser.LOGIN_JS: 0, nexus_browser.CLICK_JS: 1, "nxm": None})
        logs, links, fails, c = self.run_click(tab)
        self.assertEqual(fails, ["Nexus gave no download link after the click"])
        self.assertEqual(links, [])

    def test_login_needed_and_refused_falls_back(self):
        tab = FakeTab({nexus_browser.LOGIN_JS: 1, nexus_browser.CLICK_JS: 1, "nxm": "nxm://cyberpunk2077/mods/1/files/2"})
        logs, links, fails, c = self.run_click(tab, login=lambda self: False)
        self.assertEqual(len(fails), 1)
        self.assertIn("login", fails[0])
        self.assertEqual(links, [])

    def test_after_one_login_the_click_goes_on(self):
        answers = iter([1, 0])  # logged out on the first try, logged in on the retry
        tab = FakeTab({nexus_browser.LOGIN_JS: lambda: next(answers), nexus_browser.CLICK_JS: 1,
                       "nxm": "nxm://cyberpunk2077/mods/1/files/2"})
        logs, links, fails, c = self.run_click(tab)
        self.assertEqual(len(links), 1)  # the second try, after the patched login, hands the link over
        self.assertEqual(fails, [])


class FakeClicker:
    """Stands in for the real Clicker in the controller wiring test: records the jobs it is asked to click."""

    def __init__(self):
        self.jobs = []

    def click(self, j):
        self.jobs.append(j)

    def close(self):
        pass


class Wiring(unittest.TestCase):
    """The controller hands waiting jobs to the clicker, not to the user's browser, when the setting is on."""

    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())
        self.store = Store(self.tmp / "data")
        self.store.settings.update(api_key="k", virus_scan=False)
        self.events = queue.Queue()
        for name in ("block", "stop"):
            patcher = mock.patch.object(ctl_mod.llm, name)
            self.addCleanup(patcher.stop)
            patcher.start()
        github = mock.patch.object(ctl_mod.nexus, "github_copy", return_value=None)
        self.addCleanup(github.stop)
        github.start()
        timer = mock.patch.object(ctl_mod.threading, "Timer")
        self.addCleanup(timer.stop)
        timer.start()

    def controller(self):
        return Controller(self.store, lambda k, p=None: self.events.put((k, p)), workers=2, watch_game=False)

    def patches(self, z):
        return [mock.patch.object(ctl_mod.nexus, "mod_info", lambda mid, key="": {
                    "name": "Nx", "version": "1", "requires": [],
                    "files": [{"fileId": 7, "name": "Nx", "version": "1", "category": "MAIN", "primary": 1, "date": 1}]}),
                mock.patch.object(ctl_mod.nexus, "validate", lambda key: {"name": "u", "is_premium": False}),
                mock.patch.object(ctl_mod.nexus, "download_url", lambda *a: "cdn"),
                mock.patch.object(ctl_mod, "download", lambda url, folder, prog=None, headers=None: z)]

    def wait_idle(self, c, timeout=20):
        end = time.time() + timeout
        while time.time() < end:
            with c.lock:
                settled = c.jobs and all(j["state"] in ("done", "failed", "waiting", "needkey") for j in c.jobs)
            if settled and c.work.unfinished_tasks == 0:
                return
            time.sleep(0.05)
        raise AssertionError([j["status"] for j in c.jobs])

    def test_waiting_job_is_clicked_by_the_hidden_browser(self):
        z = make_zip(self.tmp / "nx.zip", {"a.archive": "x"})
        with mock.patch.object(ctl_mod.webbrowser, "open") as browser, contextlib.ExitStack() as stack:
            for p in self.patches(z):
                stack.enter_context(p)
            c = self.controller()
            self.addCleanup(lambda: (c.stop.set(), c.clicker.close()))
            fake = FakeClicker()
            c.clicker = fake
            c.add_links("https://www.nexusmods.com/cyberpunk2077/mods/55")
            self.assertTrue(wait_for(lambda: bool(fake.jobs)))
            self.assertEqual(c.jobs[0]["status"], "Clicking 'Slow download' for you")
            browser.assert_not_called()
            c.handle_nxm("nxm://cyberpunk2077/mods/55/files/7?key=K&expires=1")
            self.wait_idle(c)
        self.assertEqual(c.jobs[0]["state"], "done")
        self.assertEqual([j["src"]["mod_id"] for j in fake.jobs], [55])

    def test_automatic_click_failure_reopens_the_page(self):
        with mock.patch.object(ctl_mod.webbrowser, "open") as browser:
            c = self.controller()
            self.addCleanup(lambda: (c.stop.set(), c.clicker.close()))
            j = {"src": {"mod_id": 55, "file_id": 7}, "state": "waiting"}
            c._manual_click(j, "no button on the page")
            self.assertTrue(wait_for(lambda: browser.call_count == 1))
            self.assertIn("mods/55?tab=files&file_id=7", browser.call_args[0][0])

    def test_the_setting_defaults_to_on(self):
        self.assertTrue(DEFAULTS["auto_click"])
        self.assertEqual(self.store.settings["auto_click"], True)
