"""End-to-end crash check with a REAL crash (Windows only, about 1 minute, not part of the unit suite).

A copy of python.exe named Cyberpunk2077.exe runs tests/fake_game.py in a fake game folder and crashes
inside a mod DLL (a renamed copy of vcruntime140.dll, made to read address 8), writing a minidump like the
game's crash reporter. The real watcher (exit code), the minidump reader and the real pipeline must then:
  1. find the mod and turn it off by itself (one fix only),
  2. on a second crash with an update available, ask the user instead (two fixes),
  3. after a normal exit, save the mod set as the last good one.
Run: python tests/e2e_crash.py
"""
import os
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from manager_ui.controller import Controller  # noqa: E402
from shared.store import Store  # noqa: E402
from tests.helpers import make_zip  # noqa: E402

PY = Path(sys.executable).resolve().parent
tmp = Path(tempfile.mkdtemp())
game = tmp / "Cyberpunk 2077"
bin64 = game / "bin/x64"
bin64.mkdir(parents=True)
(game / "archive/pc/mod").mkdir(parents=True)
shutil.copy2(PY / "python.exe", bin64 / "Cyberpunk2077.exe")
for dll in PY.glob("*.dll"):
    shutil.copy2(dll, bin64 / dll.name)

os.environ["LOCALAPPDATA"] = str(tmp / "local")  # crash reports go to a temp REDEngine\ReportQueue
store = Store(tmp / "data")
store.settings.update(game_dir=str(game), virus_scan=True, auto_fix=True)
store.mods.append({"id": "fw-red4ext", "name": "RED4ext", "version": "x", "enabled": True, "source": {"type": "file"},
                   "files": [], "needs": [], "requires": [], "framework": "RED4ext"})  # pretend it is installed
events = []
ctl = Controller(store, lambda k, p=None: events.append((k, p)))

ctl.add_links(str(make_zip(tmp / "Bad Plugin.zip", {"BadPlugin/BadPlugin.dll": (PY / "vcruntime140.dll").read_bytes()})))
ctl.add_links(str(make_zip(tmp / "Good Mod.zip", {"good.archive": "x"})))
while not (len(store.mods) == 3 and (game / "red4ext/plugins/BadPlugin/BadPlugin.dll").exists()):
    time.sleep(0.5)
bad = next(m for m in store.mods if m["name"] == "Bad Plugin")
print("installed:", [m["name"] for m in store.mods], "| plugin deployed")

CRASH, CLEAN = "crash", "clean"


def play(mode):
    """Start the 'game' like a launcher would; the manager only watches."""
    n = len(events)
    env = {**os.environ, "PYTHONHOME": str(PY)}
    p = subprocess.Popen([str(bin64 / "Cyberpunk2077.exe"), str(Path(__file__).with_name("fake_game.py")), mode,
                          str(game / "red4ext/plugins/BadPlugin/BadPlugin.dll")], cwd=bin64, env=env)
    print("game exit code:", hex(p.wait() & 0xFFFFFFFF))
    end = time.time() + 40
    while time.time() < end:
        new = events[n:]
        if any(k == "problems" for k, _ in new) or any(k == "log" and "closed normally" in p for k, p in new):
            time.sleep(1.5)  # let the redeploy finish
            return new
        time.sleep(0.5)
    raise SystemExit(f"FAIL: no reaction after the game exited: {new}")


# 1. Real crash, one possible fix -> fixed without asking.
new = play(CRASH)
print("1:", [p for k, p in new if k == "log"])
assert not store.mod(bad["id"])["enabled"], "bad mod should be off"
assert not (game / "red4ext/plugins/BadPlugin/BadPlugin.dll").exists(), "plugin should be removed from the game"
assert (game / "archive/pc/mod/good.archive").exists(), "good mod must stay"
print("1 OK: crash traced to Bad Plugin and fixed by itself")

# 2. Real crash with an update available -> two fixes -> the user picks.
ctl.undo(len(store.doctor["history"]) - 1)
ctl.has_update = lambda m: True
while not (game / "red4ext/plugins/BadPlugin/BadPlugin.dll").exists():
    time.sleep(0.5)
new = play(CRASH)
asked = next(p for k, p in new if k == "problems")
prob = next(p for p in asked["list"] if p["id"] == f"crash-{bad['id']}")
print("2: asked the user:", prob["title"], "->", [f["label"] for f in prob["fixes"]])
assert store.mod(bad["id"])["enabled"], "with two fixes nothing is applied without the user"
ctl.apply(prob, prob["fixes"][1])  # the user picks "Disable"
assert not store.mod(bad["id"])["enabled"]
print("2 OK: user picked", prob["fixes"][1]["label"])

# 3. Normal exit -> this setup becomes the last good one.
time.sleep(2)
play(CLEAN)
assert store.doctor.get("last_good"), "last good setup should be saved"
print("3 OK: normal exit saved as last good:", sorted(store.doctor["last_good"]))
ctl.stop.set()
print("E2E PASSED")
