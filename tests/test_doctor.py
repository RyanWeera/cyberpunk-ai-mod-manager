import struct
import tempfile
import time
import unittest
from pathlib import Path

from crash_doctor import doctor
from crash_doctor.watch import dump_module, parse_events
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

    def test_parse_events_only_game(self):
        [(module, started)] = parse_events(EVENTS.format(game="C:\\G"))
        self.assertEqual(module, "C:\\G\\red4ext\\plugins\\Bad Plugin\\Bad.dll")
        self.assertEqual(time.strftime("%Y-%m-%d", time.gmtime(started)), "2026-11-05")  # FILETIME decoded


if __name__ == "__main__":
    unittest.main()
