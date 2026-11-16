import os
import queue
import stat
import tempfile
import threading
import time
import unittest
from pathlib import Path
from unittest import mock

from manager_ui import controller as ctl_mod
from manager_ui.controller import Controller
from shared.store import Store
from tests.helpers import fake_game, make_zip, mod


def wait_idle(c, timeout=20):
    end = time.time() + timeout
    while time.time() < end:
        with c.lock:
            settled = c.jobs and all(j["state"] in ("done", "failed", "waiting", "needkey") for j in c.jobs)
        if settled and c.work.unfinished_tasks == 0:  # every worker finished its job and the follow-up deploy
            return
        time.sleep(0.05)
    raise AssertionError([j["status"] for j in c.jobs])


class Pipeline(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())
        self.game = fake_game(self.tmp)
        self.store = Store(self.tmp / "data")
        self.store.settings.update(game_dir=str(self.game), virus_scan=False)
        self.events = queue.Queue()
        for name in ("block", "stop"):  # no test may leave the real local AI blocked or start its 2-minute timer
            patcher = mock.patch.object(ctl_mod.llm, name)
            self.addCleanup(patcher.stop)
            setattr(self, name, patcher.start())
        timer = mock.patch.object(ctl_mod.threading, "Timer")
        self.addCleanup(timer.stop)
        self.timer = timer.start()

    def controller(self):
        return Controller(self.store, lambda k, p=None: self.events.put((k, p)), workers=2, watch_game=False)

    def test_local_file_installs_its_frameworks_and_deploys(self):
        fw_zip = make_zip(self.tmp / "tweak.zip", {"red4ext/plugins/TweakXL/TweakXL.dll": "x"})
        rs_zip = make_zip(self.tmp / "rs.zip", {"engine/tools/scc.exe": "x"})
        r4_zip = make_zip(self.tmp / "r4.zip", {"red4ext/RED4ext.dll": "x", "bin/x64/winmm.dll": "x"})
        zips = {"psiberx/cp2077-tweak-xl": fw_zip, "jac3km4/redscript": rs_zip, "wopss/RED4ext": r4_zip}
        with mock.patch.object(ctl_mod, "github_latest", lambda repo: {"tag": "v1", "url": repo, "name": "x.zip"}), \
                mock.patch.object(ctl_mod, "download", lambda url, folder, prog=None, headers=None: zips[url]):
            c = self.controller()
            c.add_links(str(make_zip(self.tmp / "My Tweak.zip", {"tweaks/a.yaml": "x", "s/a.reds": "x"})))
            wait_idle(c)
        names = sorted(m["name"] for m in self.store.mods)
        self.assertEqual(names, ["My Tweak", "RED4ext", "TweakXL", "redscript"])
        self.assertTrue((self.game / "r6/tweaks/My Tweak/a.yaml").exists())
        self.assertTrue((self.game / "red4ext/plugins/TweakXL/TweakXL.dll").exists())
        self.assertEqual(self.store.mods[-1]["name"], "My Tweak")  # frameworks go first, mods last

    def test_files_from_the_pc_stay_untouched_even_read_only_or_in_downloads(self):
        ro = make_zip(self.tmp / "ro.zip", {"a.archive": "x"})
        os.chmod(ro, stat.S_IREAD)
        self.addCleanup(os.chmod, ro, stat.S_IWRITE | stat.S_IREAD)
        inside = make_zip(self.store.downloads / "b.zip", {"b.archive": "x"})  # picked again from the app's own folder
        c = self.controller()
        c.add_links(f"{ro}\n{inside}")
        wait_idle(c)
        self.assertEqual([j["status"] for j in c.jobs], ["Done", "Done"])
        self.assertTrue(ro.exists() and not os.access(ro, os.W_OK))
        self.assertTrue(inside.exists())
        self.assertEqual([p.name for p in self.store.downloads.iterdir()], ["b.zip"])  # job folders are cleaned up

    def test_job_folders_left_by_a_cut_off_close_are_removed_at_start(self):
        old = self.store.downloads / "job-old"
        old.mkdir(parents=True)
        (old / "archive.zip").write_text("x")
        self.controller()
        self.assertFalse(old.exists())

    def test_unrequested_nxm_link_asks_first(self):
        with mock.patch.object(ctl_mod.nexus, "mod_info", lambda mid, key="": {"name": "Shady Mod"}), \
                mock.patch.object(ctl_mod.nexus, "collection_info", lambda slug: {"name": "Night City Pack"}):
            c = self.controller()
            c.handle_nxm("nxm://cyberpunk2077/mods/9/files/8?key=a&expires=1&x= https://evil.example/p.zip")
            c.handle_nxm("nxm://cyberpunk2077/collections/abcd/revisions/1")
        self.assertEqual(c.jobs, [])  # nothing queued without the user's yes
        asked = [p for k, p in list(self.events.queue) if k == "confirm"]
        self.assertIn("Shady Mod", asked[0]["text"])
        self.assertEqual(asked[0]["link"]["type"], "nexus")  # the hidden second line is not part of what a yes adds
        self.assertIn("collection 'Night City Pack'", asked[1]["text"])

    def test_free_account_waits_for_click_then_downloads(self):
        self.store.settings["api_key"] = "k"
        z = make_zip(self.tmp / "nx.zip", {"a.archive": "x"})
        with mock.patch.object(ctl_mod.nexus, "mod_info", lambda mid, key="": {"name": "Nx", "version": "1", "requires": [],
                                                                             "files": [{"fileId": 7, "name": "Nx", "version": "1", "category": "MAIN", "primary": 1, "date": 1}]}), \
                mock.patch.object(ctl_mod.nexus, "validate", lambda key: {"name": "u", "is_premium": False}), \
                mock.patch.object(ctl_mod.nexus, "download_url", lambda *a: "cdn"), \
                mock.patch.object(ctl_mod, "download", lambda url, folder, prog=None, headers=None: z), \
                mock.patch.object(ctl_mod.webbrowser, "open") as browser:
            c = self.controller()
            c.add_links("https://www.nexusmods.com/cyberpunk2077/mods/55")
            wait_idle(c)
            self.assertEqual(c.jobs[0]["state"], "waiting")
            self.assertIn("mods/55?tab=files&file_id=7", browser.call_args[0][0])
            c.handle_nxm("nxm://cyberpunk2077/mods/55/files/7?key=K&expires=1")
            wait_idle(c)
        self.assertEqual(c.jobs[0]["state"], "done")
        self.assertTrue((self.game / "archive/pc/mod/a.archive").exists())
        self.assertNotIn("key", self.store.mods[0]["source"])  # download keys are not stored

    def test_jobs_wait_for_key_then_premium_needs_no_click(self):
        z = make_zip(self.tmp / "nx.zip", {"a.archive": "x"})
        info = {"name": "Nx", "version": "1", "requires": [],
                "files": [{"fileId": 7, "name": "Nx", "version": "1", "category": "MAIN", "primary": 1, "date": 1}]}
        with mock.patch.object(ctl_mod.nexus, "mod_info", lambda mid, key="": info), \
                mock.patch.object(ctl_mod.nexus, "validate", lambda key: {"name": "u", "is_premium": True}), \
                mock.patch.object(ctl_mod.nexus, "download_url", lambda *a: "cdn"), \
                mock.patch.object(ctl_mod, "download", lambda url, folder, prog=None, headers=None: z), \
                mock.patch.object(ctl_mod.webbrowser, "open") as browser:
            c = self.controller()
            c.add_links("https://www.nexusmods.com/cyberpunk2077/mods/55")
            wait_idle(c)
            self.assertEqual(c.jobs[0]["state"], "needkey")
            self.store.settings["api_key"] = "k"
            c.check_key()
            wait_idle(c)
        self.assertEqual(c.jobs[0]["state"], "done")
        browser.assert_not_called()

    def test_job_ids_stay_unique_and_skip_retry_respect_state(self):
        c = Controller(self.store, lambda k, p=None: None, workers=0, watch_game=False)  # no workers: jobs stay queued
        a, b = c._job("a", {"type": "file"}), c._job("b", {"type": "file"})
        c.jobs = [b]  # what "Clear finished" leaves
        self.assertNotIn(c._job("c", {"type": "file"})["id"], (a["id"], b["id"]))
        self.assertFalse(c.retry(b))  # queued, not failed
        self.assertTrue(c.skip(b))
        self.assertEqual(b["state"], "failed")
        b["state"] = "running"
        self.assertFalse(c.skip(b))  # cannot skip a running download

    def test_skipped_job_is_not_run(self):
        c = Controller(self.store, lambda k, p=None: None, workers=0, watch_game=False)
        j = c._job("x", {"type": "file", "path": "nope.zip"})
        c.skip(j)
        threading.Thread(target=c._worker, daemon=True).start()
        c.work.join()
        self.assertEqual(j["status"], "Skipped")

    def test_fix_and_undo(self):
        c = self.controller()
        self.store.mods = [{"id": "a", "name": "A", "enabled": True, "files": [], "source": {"type": "file"}},
                           {"id": "b", "name": "B", "enabled": True, "files": [], "source": {"type": "file"}}]
        p = {"title": "t"}
        c.apply(p, {"label": "Disable A", "action": ["disable", "a"]})
        c.apply(p, {"label": "Let A win", "action": ["prefer", "a", "b"]})
        self.assertEqual([(m["id"], m["enabled"]) for m in self.store.mods], [("b", True), ("a", False)])
        self.assertEqual(self.store.doctor["ignored"], ["conflict-b-a"])  # Auto-sort keeps the user's winner
        c.undo(1)
        c.undo(0)
        self.assertEqual([(m["id"], m["enabled"]) for m in self.store.mods], [("a", True), ("b", True)])
        self.assertEqual(self.store.doctor["ignored"], [])

    def test_file_stopped_by_defender_is_shown_as_a_virus(self):
        errors = {"Bad": OSError(22, "Invalid argument", "C:/Users/me/x.zip", 225),  # what real-time protection gives
                  "Bad2": OSError(22, "Invalid argument", "C:/Users/me/y.zip")}  # what Python's open() gives for it

        def install(path, new, name):
            raise errors[name]
        with mock.patch.object(ctl_mod.install, "install", install):
            c = self.controller()
            c.add_links("\n".join(str(make_zip(self.tmp / f"{n}.zip", {"archive/pc/mod/a.archive": "x"})) for n in errors))
            wait_idle(c)
        status = {j["name"]: j["status"] for j in c.jobs if j["state"] == "failed"}
        self.assertEqual(status["Bad"], "Blocked: virus found (Microsoft Defender stopped the file)")
        self.assertTrue(status["Bad2"].startswith("Blocked: Windows refused to read the file"), status)

    def test_mod_packs_save_use_merge_and_launch(self):
        c = self.controller()
        self.store.mods = [mod("fw", [], framework="RED4ext"), mod("a", []), mod("b", [], enabled=False), mod("c", [])]
        c.save_pack("One")  # fw, a, c on
        self.store.mods = [mod("fw", [], framework="RED4ext"), mod("c", []), mod("b", []), mod("a", [], enabled=False),
                           mod("d", [])]
        c.save_pack("Two")
        self.store.packs["One"].append(["gone", True])  # a mod removed since
        self.assertIn("1 not installed yet", c.use_pack("One"))
        self.assertEqual([(m["id"], m["enabled"]) for m in self.store.mods],
                         [("d", False), ("fw", True), ("a", True), ("b", False), ("c", True)])  # outside the pack: first, off
        self.store.packs["Coll"] = [["b", True], ["a", True]]  # a collection pack lists no frameworks: they stay on
        c.use_pack("Coll")
        self.assertEqual([m["id"] for m in self.store.enabled()], ["fw", "b", "a"])
        self.assertEqual(self.store.pack, "Coll")
        self.assertEqual(c.merge_packs(["Two", "Coll"], " Big "), 5)
        self.assertEqual(self.store.packs["Big"], [["fw", True], ["c", True], ["d", True], ["b", True], ["a", True]])
        with self.assertRaises(ValueError):
            c.merge_packs(["Two"], "Small")
        with mock.patch.object(ctl_mod.subprocess, "Popen") as popen:
            c.launch("Two")
        popen.assert_called_once()
        self.block.assert_called_once_with()  # the local AI stays off while the game runs
        self.timer.assert_called_once()
        self.assertEqual([m["id"] for m in self.store.enabled()], ["fw", "c", "b", "d"])
        self.assertEqual(Store(self.store.root).packs["Big"][0], ["fw", True])  # kept in state.json
        c.delete_pack("Two")
        self.assertEqual((list(self.store.packs), self.store.pack), (["One", "Coll", "Big"], ""))

    def test_a_collection_is_also_a_mod_pack(self):
        files = [{"mod_id": 5, "file_id": 50, "optional": False, "name": "Car", "version": "1"},
                 {"mod_id": 2380, "file_id": 9, "optional": False, "name": "RED4ext", "version": "1"}]
        c = self.controller()
        with mock.patch.object(ctl_mod.nexus, "collection", lambda *a: {"name": "My Pack ", "revision": 3, "files": files}), \
                mock.patch.object(c, "_job"), mock.patch.object(c, "add_framework"):
            c._add({"type": "collection", "slug": "abc"})
        self.assertEqual(self.store.packs, {"My Pack": [["nexus-5-50", True]]})

    def test_launch_asks_windows_when_the_game_runs_as_admin(self):
        c = self.controller()
        exe, cwd = str(self.game / "bin/x64/Cyberpunk2077.exe"), str(self.game / "bin/x64")
        with mock.patch.object(ctl_mod.subprocess, "Popen", side_effect=OSError(None, "elevation", None, 740)), \
                mock.patch.object(ctl_mod.os, "startfile", create=True) as start:
            c.launch()
        start.assert_called_once_with(exe, arguments="", cwd=cwd)
        with mock.patch.object(ctl_mod.subprocess, "Popen", side_effect=OSError(None, "other", None, 2)), \
                mock.patch.object(ctl_mod.os, "startfile", create=True) as start, self.assertRaises(OSError):
            c.launch()
        start.assert_not_called()
        self.assertEqual(self.block.call_args_list[-2:], [mock.call(), mock.call(False)])  # no game: the AI may start again
        self.store.deployed["bin/x64/cyberpunk2077.exe"] = "m"  # a mod replaced the game exe: never elevate it
        with mock.patch.object(ctl_mod.subprocess, "Popen", side_effect=OSError(None, "elevation", None, 740)), \
                mock.patch.object(ctl_mod.deploy, "deploy"), \
                mock.patch.object(ctl_mod.os, "startfile", create=True) as start, self.assertRaises(OSError):
            c.launch()
        start.assert_not_called()

    def test_auto_sort_and_undo(self):
        c = self.controller()
        self.store.mods = [mod("a", []), mod("fw", [], framework="RED4ext")]
        c.auto_sort()
        self.assertEqual([m["id"] for m in self.store.mods], ["fw", "a"])
        self.assertTrue(any(k == "problems" for k, _ in list(self.events.queue)))
        c.auto_sort()  # already sorted: no second history entry
        self.assertEqual(len(self.store.doctor["history"]), 1)
        c.undo(0)
        self.assertEqual([m["id"] for m in self.store.mods], ["a", "fw"])

    def test_local_ai_advice_apply_and_undo(self):
        c = self.controller()
        self.store.mods = [mod("patch", [], name="A - Patch"), mod("a", [], name="A")]
        advice = {"summary": "s", "fixes": [({"title": "t"}, {"label": "x", "action": ["ignore", "i"]}, "")] * 12, "after": [("a", "patch")]}
        with mock.patch.object(ctl_mod.llm, "start"), mock.patch.object(ctl_mod.llm, "advise", return_value=advice):
            c.ask_ai()
        a = [p for k, p in list(self.events.queue) if k == "ai"][-1]
        self.assertEqual((a["order"], a["moved"], len(a["fixes"])), (["a", "patch"], 1, 10))  # the window lists 10 fixes
        c.apply_ai({**a, "fixes": [({"title": "t"}, {"label": "Turn A off", "action": ["disable", "a"]}, "why")]})
        self.assertEqual([(m["id"], m["enabled"]) for m in self.store.mods], [("a", False), ("patch", True)])
        self.assertEqual([h["fix"] for h in self.store.doctor["history"]], ["Sorted by the local AI (1 mod(s) changed place)", "Turn A off"])
        c.undo(1)
        c.undo(0)
        self.assertEqual([(m["id"], m["enabled"]) for m in self.store.mods], [("patch", True), ("a", True)])
        with mock.patch.object(ctl_mod.llm, "start", side_effect=RuntimeError("The local AI did not start.")):
            c.ask_ai()
        self.assertEqual([p for k, p in list(self.events.queue) if k == "ai"][-1], {"error": "The local AI did not start."})

    def test_the_local_ai_stops_while_the_game_runs(self):
        c = self.controller()
        with mock.patch.object(ctl_mod.llm, "block") as block:
            c._game_started(0)
        block.assert_called_once_with()
        with mock.patch.object(ctl_mod.llm, "start") as start:
            c.ask_ai("my car is missing")
            c.setup_ai()
        start.assert_not_called()
        self.assertEqual([p for k, p in list(self.events.queue) if k == "ai"][-1], {"error": ctl_mod.GAME_ON})
        with mock.patch.object(ctl_mod.watch, "crash_evidence", return_value={"crashed": False}),                 mock.patch.object(ctl_mod.llm, "block") as block:
            c._game_exited(0)
        self.assertFalse(c.game_on)
        block.assert_called_once_with(False)  # the AI may start again
        with mock.patch.object(self.store, "save", side_effect=OSError("disk full")):
            c._game_started(1)  # no error: else the game watch would skip the exit and the AI would stay off
        self.assertTrue(c.game_on)
        self.block.reset_mock()
        with mock.patch.object(ctl_mod.subprocess, "Popen", side_effect=OSError(None, "other", None, 2)), \
                self.assertRaises(OSError):
            c.launch()  # a second launch fails while the game runs
        self.block.assert_called_once_with()  # the AI stays off

    def test_bug_report_fixes_become_changes_the_user_can_apply(self):
        c = self.controller()
        car, b, w = mod("car", [], name="Car", enabled=False), mod("b", [], name="B"), mod("w", [], name="W", source={"type": "nexus", "mod_id": 9, "file_id": 90})
        self.store.mods = [car, b, w]
        advice = {"summary": "s", "fixes": [], "after": [], "cause": "c", "mod_fixes": [
            (car, "turn_on", "off"), (b, "turn_on", "already on"), (car, "load_last", "wins"),
            (w, "update", "old"), (w, "load_last", "already last")]}
        with mock.patch.object(ctl_mod.llm, "start"), mock.patch.object(ctl_mod.llm, "advise", return_value=advice) as adv:
            c.ask_ai("  my car is missing ")
        self.assertEqual(adv.call_args.args[2], "my car is missing")
        a = [p for k, p in list(self.events.queue) if k == "ai"][-1]
        self.assertNotIn("error", a, a)
        self.assertNotIn("mod_fixes", a)
        self.assertEqual([(p["title"], f["label"]) for p, f, _ in a["fixes"]], [
            ("Your bug report", "Turn on Car"), ("Your bug report", "Load Car last, so it wins"),
            ("Your bug report", "Update W to its newest version"), ("Your bug report", "Load W last, so it wins")])
        with mock.patch.object(c, "bg"):  # the update runs in the background: not here
            c.apply_ai(a)
        self.assertEqual([(m["id"], m["enabled"]) for m in self.store.mods], [("b", True), ("car", True), ("w", True)])

    def test_auto_fix_only_single_fix_problems_after_crash(self):
        c = self.controller()
        self.store.mods = [{"id": "a", "name": "A", "enabled": True, "files": [], "source": {"type": "file"}}]
        one = {"id": "1", "title": "t", "auto": True, "severity": "high", "fixes": [{"label": "Disable A", "action": ["disable", "a"]}]}
        two = {"id": "2", "title": "t2", "auto": True, "severity": "high", "fixes": [{"label": "x", "action": ["ignore", "2"]}] * 2}
        c.handle_problems([one, two], crashed=True)
        self.assertFalse(self.store.mods[0]["enabled"])
        asked = [p for k, p in list(self.events.queue) if k == "problems"][0]["list"]
        self.assertEqual([p["id"] for p in asked], ["2"])

    def test_bisect_round_trip(self):
        c = self.controller()
        self.store.mods = [{"id": i, "name": i, "enabled": True, "files": [], "source": {"type": "file"}} for i in "abcd"]
        c.bisect_begin()
        self.assertEqual([m["enabled"] for m in self.store.mods], [True, True, False, False])
        c._bisect_result(crashed=False)  # c or d is bad
        self.assertEqual([m["enabled"] for m in self.store.mods], [True, True, True, False])
        c._bisect_result(crashed=True)  # c is bad
        self.assertNotIn("bisect", self.store.doctor)
        self.assertEqual([m["enabled"] for m in self.store.mods], [True, True, False, True])  # c auto-disabled


if __name__ == "__main__":
    unittest.main()
