import os
import struct
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from collections import Counter
from pathlib import Path
from unittest import mock

from crash_doctor import doctor, watch
from crash_doctor.watch import dump_module, parse_events, read_dump
from shared.store import Store
from tests.helpers import fake_game, mod, rdar, stage

EVENTS = """Event[0]
  Source: Application Error
  Description:
Faulting application name: Cyberpunk2077.exe, version: 2.3.0.0, time stamp: 0x1
Faulting module name: Bad.dll, version: 1.0.0.0
Faulting process id: 0x1
Faulting application start time: 0x1DD6CEF3291ECD2
Faulting module path: {game}\\red4ext\\plugins\\Bad Plugin\\Bad.dll
Event[1]
  Description:
Faulting application name: pythonw.exe, version: 3.12
Faulting module path: C:\\x\\torch.dll
"""


def minidump(modules, crash_addr, stack_words, tid=7, code=0xC0000005, more_threads=()):
    """A small minidump: exception (none when crash_addr is None, like a freeze snapshot), module list, and thread `tid`
    whose stack holds stack_words, then more_threads [(tid, words)]. modules: [(base, size, path)]."""
    threads = [(tid, stack_words), *more_threads]
    n = 3 if crash_addr is not None else 2
    exc = 32 + 12 * n
    ml = exc + (32 if crash_addr is not None else 0)
    names_off = ml + 4 + 108 * len(modules)
    names, rvas = b"", []
    for _, _, path in modules:
        e = path.encode("utf-16-le")
        rvas.append(names_off + len(names))
        names += struct.pack("<I", len(e)) + e
    tl = names_off + len(names)
    mem = tl + 4 + 48 * len(threads)
    stacks, entries = b"", b""
    for t, words in threads:
        st = struct.pack(f"<{len(words)}Q", *words)
        entries += struct.pack("<IIIIQQII", t, 0, 0, 0, 0, 0, len(st), mem + len(stacks)) + b"\0" * 8
        stacks += st
    head = struct.pack("<4sIIIIIQ", b"MDMP", 0, n, 32, 0, 0, 0)
    if crash_addr is not None:
        head += struct.pack("<III", 6, 32, exc)
    head += struct.pack("<III", 4, tl - ml, ml) + struct.pack("<III", 3, mem - tl, tl)
    if crash_addr is not None:
        head += struct.pack("<IIIIQQ", tid, 0, code, 0, 0, crash_addr)
    return (head + struct.pack("<I", len(modules)) + b"".join(struct.pack("<QIIII", b, n, 0, 0, r) + b"\0" * 84
                                                               for (b, n, _), r in zip(modules, rvas))
            + names + struct.pack("<I", len(threads)) + entries + stacks)


class Doctor(unittest.TestCase):
    def setUp(self):
        tmp = Path(tempfile.mkdtemp())
        self.game = fake_game(tmp)
        self.store = Store(tmp / "data")
        self.store.settings["game_dir"] = str(self.game)
        s = self.store
        stage(s, mod("cet", ["bin/x64/version.dll", "bin/x64/plugins/cyber_engine_tweaks/LICENSE"], framework="Cyber Engine Tweaks"))
        stage(s, mod("rs", ["engine/tools/scc.exe"], framework="redscript"))
        stage(s, mod("fancy", ["r6/scripts/Fancy HUD/hud.reds"], needs=["redscript"]))
        stage(s, mod("luamod", ["bin/x64/plugins/cyber_engine_tweaks/mods/luamod/init.lua"], needs=["Cyber Engine Tweaks"]))
        stage(s, mod("bad", ["red4ext/plugins/Bad Plugin/Bad.dll"], needs=["RED4ext"]))

    def write(self, rel, text):
        p = self.game / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(text)

    def test_log_errors_map_to_mods(self):
        self.write("r6/logs/redscript_rCURRENT.log",
                   f"[INFO - x] Compiling\n[ERROR - Mon] At {self.game}\\r6\\scripts\\Fancy HUD\\hud.reds:12:5:\n  unresolved\n")
        self.write("bin/x64/plugins/cyber_engine_tweaks/mods/luamod/luamod.log", "[error] attempt to index a nil value\n")
        self.write("bin/x64/plugins/cyber_engine_tweaks/cyber_engine_tweaks.log", "[info] fine\n[error] Unsupported game version\n")
        self.write("red4ext/logs/red4ext.log", "[error] something without a path\n")
        found = doctor.log_errors(self.game, 0, doctor.owner_index(self.store))
        self.assertEqual(set(found), {"fancy", "luamod", "cet"})
        self.assertTrue(found["fancy"]["strong"])
        self.assertFalse(found["luamod"]["strong"])

    def test_crash_module_and_missing_framework(self):
        crash = {"crashed": True, "module": parse_events(EVENTS.format(game=self.game))[0][0]}
        probs = {p["id"]: p for p in doctor.diagnose(self.store, 0, crash)}
        self.assertEqual(probs["crash-bad"]["fixes"][0]["action"], ["disable", "bad"])  # no update -> one fix -> automatic
        self.assertEqual(len(probs["crash-bad"]["fixes"]), 1)
        need = probs["need-RED4ext"]
        self.assertEqual([f["action"][0] for f in need["fixes"]], ["install_fw", "disable_many"])

    def test_crash_in_game_files_does_not_blame_a_mod(self):
        crash = {"crashed": True, "module": f"{self.game}\\bin\\x64\\Cyberpunk2077.exe"}
        ids = [p["id"] for p in doctor.diagnose(self.store, 0, crash)]
        self.assertFalse([i for i in ids if i.startswith("crash-")], ids)  # CET owns version.dll, not bin/x64
        crash["module"] = f"{self.game}\\bin\\x64\\version.dll"  # CET's own file: CET is to blame
        self.assertIn("crash-cet", [p["id"] for p in doctor.diagnose(self.store, 0, crash)])

    def test_bisect_without_any_crash_names_nobody(self):
        state = {"suspects": list("abcd"), "testing": list("ab"), "restore": list("abcd")}
        while "culprit" not in state:
            state = doctor.bisect_step(state, False)
        self.assertIsNone(state["culprit"])

    def test_bisect_confirms_a_mod_found_by_elimination(self):
        state = {"suspects": list("abcd"), "testing": list("ab"), "restore": list("abcd")}
        state = doctor.bisect_step(doctor.bisect_step(state, False), False)  # a, b, then c clear: d is left
        self.assertTrue(state.get("verifying"))
        self.assertEqual(doctor.bisect_step(state, True)["culprit"], "d")

    def test_huge_log_line_stays_cheap(self):
        t = time.perf_counter()
        doctor.owners_in("error r6/" + "a " * 500_000, doctor.owner_index(self.store), self.game)  # 1 MB, all spaces
        self.assertLess(time.perf_counter() - t, 2)

    def test_update_is_offered_first(self):
        fixes = doctor.mod_fixes(self.store.mod("bad"), lambda m: True)
        self.assertEqual([f["action"][0] for f in fixes], ["update", "disable"])

    def test_unknown_crash_offers_choices(self):
        self.store.mods = [m for m in self.store.mods if m["id"] != "bad"]
        self.store.doctor["last_good"] = {"cet": "1", "rs": "1", "fancy": "1"}
        probs = doctor.diagnose(self.store, 0, {"crashed": True, "module": "C:\\Windows\\ntdll.dll"})
        unknown = next(p for p in probs if p["id"] == "unknown")
        self.assertEqual(unknown["fixes"][0]["action"], ["disable_many", ["luamod"]])
        self.assertEqual(unknown["fixes"][1]["action"], ["bisect_start"])

    def test_archive_conflicts(self):
        s = Store(Path(tempfile.mkdtemp()))
        stage(s, mod("x", ["archive/pc/mod/x.archive"]), {"archive/pc/mod/x.archive": rdar([1, 2, 3])})
        stage(s, mod("y", ["archive/pc/mod/y.archive"]), {"archive/pc/mod/y.archive": rdar([3, 4])})
        stage(s, mod("z", ["archive/pc/mod/z.archive"]), {"archive/pc/mod/z.archive": rdar([9])})
        self.assertEqual(doctor.archive_hashes(s.staging / "x/archive/pc/mod/x.archive"), {1, 2, 3})
        [p] = doctor.archive_conflicts(s)
        self.assertEqual(p["id"], "conflict-x-y")
        self.assertEqual([f["action"] for f in p["fixes"]],
                         [["ignore", "conflict-x-y"], ["prefer", "x", "y"], ["disable", "x"], ["disable", "y"]])
        s.doctor["ignored"] = ["conflict-x-y"]
        self.assertEqual(doctor.archive_conflicts(s), [])

    def test_loose_files_two_mods_replace_are_a_conflict_too(self):
        s = Store(Path(tempfile.mkdtemp()))
        s.mods = [mod("car", ["r6/scripts/car.reds", "r6/tweaks/car.yaml"], name="Car"),
                  mod("car2", ["R6/Scripts/car.reds", "r6/tweaks/car.yaml"], name="Car v2"), mod("x", ["r6/scripts/x.reds"])]
        [p] = doctor.archive_conflicts(s)
        self.assertEqual((p["id"], p["title"]), ("conflict-car-car2", "Car and Car v2 change the same 2 game file(s)"))
        self.assertEqual(p["fixes"][1]["action"], ["prefer", "car", "car2"])  # the user picks which one wins

    def test_best_order(self):
        s = Store(Path(tempfile.mkdtemp()))
        for mid, hashes, kw in [
                ("patch", [1, 2], {}),
                ("addon", range(1, 21), {"requires": [{"mod_id": 100, "name": "Big"}]}),  # bigger, but needs big
                ("big", range(1, 11), {"source": {"type": "nexus", "mod_id": 100, "file_id": 1}}),
                ("other", [99], {}),
                ("k1", [50, 51], {}),  # smaller, but the user chose to keep k2 winning
                ("k2", [50, 51, 52], {}),
                ("fw", [], {"framework": "ArchiveXL"})]:
            rel = f"archive/pc/mod/{mid}.archive"
            stage(s, mod(mid, [rel], **kw), {rel: rdar(hashes)})
        s.doctor["ignored"] = ["conflict-k1-k2"]
        self.assertEqual(doctor.best_order(s), ["fw", "big", "addon", "patch", "other", "k1", "k2"])
        # the local AI's rules: patch after other; addon before big loses to addon's own requirement
        self.assertEqual(doctor.best_order(s, [("other", "patch"), ("addon", "big"), ("gone", "patch")]),
                         ["fw", "big", "addon", "other", "patch", "k1", "k2"])

    def test_best_order_skips_a_mod_added_while_it_reads(self):
        s = Store(Path(tempfile.mkdtemp()))
        s.mods = [mod("a", []), mod("b", [])]
        with mock.patch.object(doctor, "overlaps", return_value=({"a": 1, "b": 1, "new": 5}, Counter({("a", "new"): 1}))):
            self.assertEqual(doctor.best_order(s), ["a", "b"])  # "new" came from a framework install meanwhile

    def test_moved_counts_only_mods_that_left_their_place(self):
        self.assertEqual(doctor.moved(list("abcde"), list("eabcd")), 1)  # one mod to the top shifts the others, it moves one
        self.assertEqual(doctor.moved(list("abc"), list("abc")), 0)
        self.assertEqual(doctor.moved(list("abc"), list("cba")), 2)
        self.assertEqual(doctor.moved(list("ab"), list("xba")), 1)  # a mod added meanwhile does not count

    def test_bisect_finds_culprit(self):
        state = {"suspects": list("abcdefg"), "testing": list("abcd"), "restore": list("abcdefg")}
        culprit, launches = "f", 0
        while "culprit" not in state:
            state = doctor.bisect_step(state, culprit in state["testing"])
            launches += 1
        self.assertEqual(state["culprit"], "f")
        self.assertLessEqual(launches, 3)

    def test_minidump_names_crashing_module(self):
        name = "C:\\G\\red4ext\\plugins\\Bad\\Bad.dll".encode("utf-16-le")
        dirs, exc, mods = 32, 56, 88  # header, 2 directory entries, exception stream, module list, name
        exc_stream = struct.pack("<IIIIQQ", 1, 0, 0xC0000005, 0, 0, 0x5010)  # 32 bytes: 56 + 32 = 88
        module = struct.pack("<QIIII", 0x5000, 0x100, 0, 0, mods + 4 + 108) + b"\0" * 84
        blob = (struct.pack("<4sIIIIIQ", b"MDMP", 0, 2, dirs, 0, 0, 0) + struct.pack("<III", 6, len(exc_stream), exc)
                + struct.pack("<III", 4, 112, mods) + exc_stream + struct.pack("<I", 1) + module + struct.pack("<I", len(name)) + name)
        p = Path(tempfile.mkdtemp()) / "minidump.dmp"
        p.write_bytes(blob)
        self.assertEqual(dump_module(p), "C:\\G\\red4ext\\plugins\\Bad\\Bad.dll")
        p.write_bytes(b"junk")
        self.assertIsNone(dump_module(p))

    def test_call_path_names_mods_on_the_crashing_stack(self):
        exe = f"{self.game}\\bin\\x64\\Cyberpunk2077.exe"
        mods = [(0x1000, 0x1000, exe), (0x5000, 0x100, f"{self.game}\\red4ext\\plugins\\Bad Plugin\\Bad.dll"),
                (0x9000, 0x100, f"{self.game}\\bin\\x64\\version.dll")]
        p = Path(tempfile.mkdtemp()) / "minidump.dmp"
        p.write_bytes(minidump(mods, 0x1010, [0x123, 0x9010, 0x1500, 0x5010, 0x9020]))
        d = read_dump(p)
        self.assertEqual(d["module"], exe)
        self.assertEqual(d["code"], 0xC0000005)
        self.assertEqual([m.rsplit("\\", 1)[1] for m in d["stack"]], ["version.dll", "Cyberpunk2077.exe", "Bad.dll"])  # newest call first
        probs = {p["id"]: p for p in doctor.diagnose(self.store, 0, {"crashed": True, **d})}
        self.assertEqual(probs["stack-cet"]["severity"], "high")  # ran last before the crash: main suspect
        self.assertEqual(probs["stack-bad"]["severity"], "medium")
        self.assertFalse(probs["stack-cet"]["auto"])  # a stack hint never disables a mod by itself
        self.assertNotIn("unknown", probs)
        self.assertIn("Where: version.dll+0x10", probs["stack-cet"]["detail"])  # where in the module: an offset its author can look up

    def test_freeze_snapshot_blames_mod_code_on_the_main_thread(self):
        mods = [(0x1000, 0x1000, f"{self.game}\\bin\\x64\\Cyberpunk2077.exe"),
                (0x5000, 0x100, f"{self.game}\\red4ext\\plugins\\Bad Plugin\\Bad.dll"),
                (0x9000, 0x100, f"{self.game}\\bin\\x64\\version.dll")]
        p = Path(tempfile.mkdtemp()) / "freeze.dmp"
        p.write_bytes(minidump(mods, None, [0x1010, 0x5020], more_threads=[(8, [0x9010]), (9, [0x5030])]))
        hang = read_dump(p)
        self.assertIsNone(hang["module"])  # a snapshot has no crash
        self.assertEqual([m.rsplit("\\", 1)[1] for m in hang["stack"]], ["Cyberpunk2077.exe", "Bad.dll"])
        self.assertEqual([m.rsplit("\\", 1)[1] for m in hang["others"]], ["version.dll"])  # Bad.dll is already on the main thread
        probs = {p["id"]: p for p in doctor.diagnose(self.store, 0, {"crashed": True, "hang": hang})}
        self.assertEqual(probs["freeze-bad"]["title"], f"{self.store.mod('bad')['name']} was running when the game froze")
        self.assertIn(self.store.mod("cet")["name"], probs["freeze-bad"]["detail"])  # other threads are named, not blamed
        p.write_bytes(minidump(mods, None, [0x1010], more_threads=[(8, [0x9010])]))
        probs = {p["id"]: p for p in doctor.diagnose(self.store, 0, {"crashed": True, "hang": read_dump(p)})}
        self.assertEqual(probs["freeze"]["fixes"][0]["action"], ["bisect_start"])  # no mod code on the main thread

    def test_watch_snapshots_a_freeze_and_drops_it_when_the_game_comes_back(self):
        for recovers, clean_exit in ((False, False), (True, False), (False, True)):
            states = iter([True, True, True] + ([False] if recovers else [True]) + [True] * 5)
            waits = iter([watch.WAIT_TIMEOUT] * 6 + [0])
            clock = iter(range(0, 10_000, 10))
            k32 = mock.Mock()
            k32.OpenProcess.return_value = 1
            k32.WaitForSingleObject.side_effect = lambda h, ms, waits=waits: next(waits)
            k32.GetExitCodeProcess.return_value = 0  # Windows gave no exit code
            if clean_exit:
                k32.GetExitCodeProcess.side_effect = lambda h, ref: 1  # exit code 0
            snap = Path(tempfile.mkdtemp()) / "freeze-1.dmp"
            snap.write_bytes(b"MDMP")
            stop, seen = threading.Event(), []

            def done(started, code, freeze, seen=seen, stop=stop):
                seen.append(freeze)
                stop.set()

            with mock.patch.object(watch, "kernel32", return_value=k32), mock.patch.object(watch, "find_pid", return_value=42), \
                    mock.patch.object(watch, "hung", side_effect=lambda pid, states=states: next(states)), \
                    mock.patch.object(watch, "write_dump", return_value=snap) as dump, \
                    mock.patch.object(watch.time, "time", side_effect=lambda clock=clock: next(clock)):
                watch.watch(lambda t: None, done, stop, interval=0, hang_dir=snap.parent)
            dump.assert_called_once_with(42, snap.parent)  # one snapshot per freeze, after HANG_AFTER seconds
            kept = not recovers and not clean_exit  # a long load or a slow clean quit is no freeze: its snapshot is deleted
            self.assertEqual(seen, [snap] if kept else [None])
            self.assertEqual(snap.exists(), kept)

    @unittest.skipUnless(sys.platform == "win32", "Windows only")
    def test_real_snapshot_of_a_hung_window(self):
        proc = subprocess.Popen([sys.executable, "-c", "import tkinter, time; r = tkinter.Tk(); r.update(); time.sleep(60)"])
        self.addCleanup(proc.wait)
        self.addCleanup(proc.kill)
        for _ in range(40):  # Windows calls a window "Not responding" after 5 s without messages
            if watch.hung(proc.pid):
                break
            time.sleep(0.5)
        else:
            self.fail("the window never became hung")
        folder = Path(tempfile.mkdtemp())
        snap = watch.write_dump(proc.pid, folder)
        self.assertTrue(snap and snap.exists())
        hang = read_dump(snap)
        self.assertIsNone(hang["module"])
        self.assertTrue(any(m.lower().endswith(".dll") for m in hang["stack"] + hang["others"]), hang)
        self.assertFalse(watch.hung(os.getpid()))  # this test process has no hung window

    def test_hand_installed_mods(self):
        self.write("archive/pc/mod/handmade.archive", "x")
        self.write("red4ext/plugins/Manual/Manual.dll", "x")
        self.write("r6/scripts/Fancy HUD/other.reds", "x")  # a folder this app installed: not by hand
        hand = [rel for rel, _ in doctor.hand_installed(self.store)]
        self.assertEqual(sorted(hand), ["archive/pc/mod/handmade.archive", "red4ext/plugins/Manual"])
        probs = {p["id"]: p for p in doctor.diagnose(self.store, 0, {"crashed": True})}
        [pid] = [i for i in probs if i.startswith("manual-")]
        self.assertIn("handmade.archive", probs[pid]["detail"])
        self.store.doctor["ignored"] = [pid]
        self.assertFalse([p for p in doctor.diagnose(self.store, 0, {"crashed": True}) if p["id"] == pid])

    def test_game_update_since_last_good_launch(self):
        self.assertEqual(doctor.game_updated(self.store), [])  # no good launch on record
        self.store.doctor["last_good_game"] = "2.3"
        with mock.patch.object(doctor, "game_version", return_value="2.3"):
            self.assertEqual(doctor.game_updated(self.store), [])
        with mock.patch.object(doctor, "game_version", return_value="2.31"):
            [p] = doctor.game_updated(self.store, lambda m: m["id"] == "cet")
        self.assertEqual(p["fixes"][0]["action"], ["update", "cet"])
        self.assertEqual(p["severity"], "high")
        self.assertIn("2.3 -> 2.31", p["detail"])

    def test_slow_dll_mod_from_red4ext_log(self):
        self.write("red4ext/logs/red4ext.log",
                   "[2026-11-19 10:00:00.000] [RED4ext] [info] Loading plugins...\n"
                   f"[2026-11-19 10:00:00.100] [RED4ext] [info] Loading plugin from '{self.game}\\red4ext\\plugins\\Bad Plugin\\Bad.dll'...\n"
                   "[2026-11-19 10:00:07.600] [RED4ext] [info] Bad (version: 1.0) has been loaded\n"
                   f"[2026-11-19 10:01:00.000] [RED4ext] [info] Loading plugin from '{self.game}\\red4ext\\plugins\\Bad Plugin\\Bad.dll'...\n"
                   "[2026-11-19 11:00:00.000] [RED4ext] [info] Game closed\n")  # an hour: a quiet log, not a slow load
        self.assertEqual(doctor.slow_mods(self.game, 0, doctor.owner_index(self.store)), {"bad": 7.5})
        self.assertIn("slow-bad", [p["id"] for p in doctor.diagnose(self.store)])

    def test_report_hides_this_pc_paths(self):
        crash = {"crashed": True, "exit_code": 0xC0000005, "code": 0xC0000005,
                 "module": f"{self.game}\\red4ext\\plugins\\Bad Plugin\\Bad.dll",
                 "stack": [f"{str(self.game).upper()}\\bin\\x64\\x.dll"]}  # Windows paths may differ in case
        text = doctor.report(self.store, doctor.diagnose(self.store, 0, crash), crash)
        self.assertNotIn(str(self.game).lower(), text.lower())
        self.assertNotIn(os.path.expanduser("~"), text)
        self.assertIn("<game>\\red4ext\\plugins\\Bad Plugin\\Bad.dll", text)
        self.assertIn("Exit code: 0xC0000005", text)
        self.assertIn(self.store.mod("bad")["name"], text)

    def test_crash_code_in_plain_words(self):
        crash = {"crashed": True, "module": f"{self.game}\\red4ext\\plugins\\Bad Plugin\\Bad.dll", "code": 0xC0000005}
        probs = {p["id"]: p for p in doctor.diagnose(self.store, 0, crash)}
        self.assertIn("memory it does not own", probs["crash-bad"]["detail"])

    def test_advanced_crash_reporter_report(self):
        self.write(f"{doctor.ACR}/report-1.txt", f"SUMMARY\nLikely cause: {self.game}\\red4ext\\plugins\\Bad Plugin\\Bad.dll\n"
                                                 "INSTALLED MODS\nr6/scripts/Fancy HUD/hud.reds\n")
        ids = [p["id"] for p in doctor.diagnose(self.store, 0, {"crashed": True})]
        self.assertIn("acr-bad", ids)
        self.assertNotIn("acr-fancy", ids)  # the installed-mod list is not a verdict
        (self.game / doctor.ACR / "report-1.txt").unlink()
        self.write(f"{doctor.ACR}/report-2.txt", "SUMMARY\nNo mod found\nINSTALLED MODS\nr6/scripts/Fancy HUD/hud.reds\n")
        probs = {p["id"]: p for p in doctor.diagnose(self.store, 0, {"crashed": True})}
        self.assertEqual(probs["acr-report-2.txt"]["severity"], "info")
        self.store.doctor["ignored"] = ["acr-report-2.txt"]
        self.assertNotIn("acr-report-2.txt", [p["id"] for p in doctor.diagnose(self.store, 0, {"crashed": True})])

    def test_parse_events_only_game(self):
        [(module, started)] = parse_events(EVENTS.format(game="C:\\G"))
        self.assertEqual(module, "C:\\G\\red4ext\\plugins\\Bad Plugin\\Bad.dll")
        self.assertEqual(time.strftime("%Y-%m-%d", time.gmtime(started)), "2026-11-05")  # FILETIME decoded


if __name__ == "__main__":
    unittest.main()
