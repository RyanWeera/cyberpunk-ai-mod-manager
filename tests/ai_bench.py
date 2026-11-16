"""Bug-chat benchmark with the real local AI: `py -u tests/ai_bench.py practice|hidden [model.gguf]`.
Each case is a temp mod list in a fake game folder, a player's bug report, and the one correct result. A case passes
when the mod list after the AI's fixes is right and nothing else changed. "practice" may be used to improve the model;
"hidden" has other mod names and other wording, and only checks the result."""
import queue
import random
import sys
import tempfile
import time
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from local_ai import llm
from manager_ui.controller import Controller
from shared.store import Store
from tests.helpers import fake_game, mod, stage

NAMES = {  # (thing the player sees, mod name)
    "practice": {
        "car": [("car", "Porsche 911 Turbo 930 - Vehicle Add-on"), ("bike", "Yaiba Kusanagi Sport Bike Add-on"),
                ("truck", "Militech Behemoth Pickup - Vehicle")],
        "look": [("jacket", "Samurai Jacket Retexture - Gold"), ("hair", "New Hairstyles for V - Pack 2"),
                 ("tattoo", "Arm Tattoos Collection")],
        "gun": [("rifle", "Kang Tao Smart Rifle Overhaul"), ("katana", "Arasaka Katana Reskin - Black")],
        "filler": ["Better Lighting", "Minimal HUD", "No Intro Videos", "Immersive Rippers", "Better Traffic"],
    },
    "hidden": {
        "car": [("Quadra", "Quadra Type-66 Javelina Add-on"), ("van", "Mahir Supron Camper Van - New Car"),
                ("motorcycle", "Arch Nazare Racer Bike")],
        "look": [("boots", "Netrunner Boots Pack"), ("makeup", "Vibrant Makeup Options for V"),
                 ("coat", "Long Trench Coat - Black Variant")],
        "gun": [("shotgun", "Carnage Shotgun Rework"), ("pistol", "Malorian Arms 3516 Retexture")],
        "filler": ["Weather Overhaul", "Clean Crosshair", "Faster Elevators", "NPC Variety Pack", "Sharper Textures"],
    },
}
WORDS = {
    "practice": {
        "off": ["My modded {thing} is not loading in, can you fix that", "The {thing} mod does nothing in the game",
                "I can't find my new {thing} anywhere"],
        "lose": ["I installed {a} but the game still shows {b}", "{a} is not working, I still see the old {thing} from {b}"],
        "fw": ["My {thing} mod is invisible in the game", "The {thing} does not show up since I changed some settings"],
        "none": ["The game is a bit slow in Dogtown", "Johnny's voice is too quiet in cutscenes"],
    },
    "hidden": {
        "off": ["where is my {thing}?? i added the mod yesterday", "{thing} from my mod list isn't in game, help",
                "The game ignores my {thing} mod"],
        "lose": ["I want {a}, not {b}, but the game uses {b}", "Wrong look: {b} shows instead of {a}"],
        "fw": ["My {thing} from a mod is missing or invisible", "Since today my {thing} mod shows nothing"],
        "none": ["The map takes long to open", "Panam's quest marker sometimes disappears"],
    },
}
FW_FILES = {"ArchiveXL": "red4ext/plugins/ArchiveXL/ArchiveXL.dll", "TweakXL": "red4ext/plugins/TweakXL/TweakXL.dll"}


def frameworks(off=()):
    return [mod(f"gh-{fw}", [f], name=fw, framework=fw, source={"type": "github"}, enabled=fw not in off)
            for fw, f in FW_FILES.items()]


def cases(which, seed=7):
    rnd, n, w, out = random.Random(f"{which}{seed}"), NAMES[which], WORDS[which], []
    def filler(k):
        return [mod(f"f{i}", [f"archive/pc/mod/f{i}.archive"], name=x) for i, x in enumerate(rnd.sample(n["filler"], k))]
    for kind in ("car", "look", "gun"):
        for thing, name in n[kind]:
            target = mod("t", [f"archive/pc/mod/{thing}.archive", f"r6/tweaks/{thing}.yaml"], name=name, enabled=False,
                         needs=["TweakXL"])
            out.append(("off", rnd.choice(w["off"]).format(thing=thing), frameworks() + filler(2) + [target], "t"))
            a = mod("t", [f"r6/tweaks/{thing}_look.yaml"], name=name, needs=["TweakXL"])
            b = mod("r", [f"r6/tweaks/{thing}_look.yaml"], name=f"{name.split(' - ')[0]} - Original Look", needs=["TweakXL"])
            out.append(("lose", rnd.choice(w["lose"]).format(a=a["name"], b=b["name"], thing=thing),
                        frameworks() + filler(1) + [a, b], "t"))
            x = mod("t", [f"archive/pc/mod/{thing}.archive", f"archive/pc/mod/{thing}.xl"], name=name, needs=["ArchiveXL"])
            out.append(("fw", rnd.choice(w["fw"]).format(thing=thing), frameworks(off=["ArchiveXL"]) + filler(2) + [x],
                        "gh-ArchiveXL"))
    for report in w["none"]:
        out.append(("none", report, frameworks() + filler(4), None))
    return out


def run_case(kind, report, mods, target):
    tmp = Path(tempfile.mkdtemp())
    store = Store(tmp / "data")
    store.settings.update(game_dir=str(fake_game(tmp)), virus_scan=False)
    for m in mods:
        stage(store, m)
    events = queue.Queue()
    c = Controller(store, lambda k, p=None: events.put((k, p)), workers=1, watch_game=False)
    before = {m["id"]: m["enabled"] for m in store.mods}
    c.ask_ai(report)
    a = next((p for k, p in reversed(list(events.queue)) if k == "ai"), {"error": "no answer"})
    if "error" in a:
        return False, a["error"], a
    with mock.patch.object(c, "bg") as bg:
        c.apply_ai(a)
    after = {m["id"]: m["enabled"] for m in store.mods}
    order = [m["id"] for m in store.mods]
    changed = {k for k in before if before[k] != after[k]}
    if kind == "off" or kind == "fw":
        ok = changed == {target}
    elif kind == "lose":
        ok = not changed and order.index("t") > order.index("r")
    else:
        ok = not changed and not bg.called
    return ok, ", ".join(f["label"] for _, f, _ in a["fixes"]) or "no fixes", a


def main():
    which = sys.argv[1] if len(sys.argv) > 1 else "practice"
    if len(sys.argv) > 2:
        llm.MODEL_FILE = Path(sys.argv[2]).resolve()
    llm.start()
    try:
        results, t = [], time.monotonic()
        for kind, report, mods, target in cases(which):
            ok, what, a = run_case(kind, report, mods, target)
            results.append((kind, ok))
            print(f"{'PASS' if ok else 'FAIL'} [{kind}] {report!r} -> {what} | cause: {a.get('cause', '')[:160]}", flush=True)
        for kind in ("off", "lose", "fw", "none"):
            k = [ok for kd, ok in results if kd == kind]
            print(f"{kind}: {sum(k)}/{len(k)}")
        print(f"TOTAL {sum(ok for _, ok in results)}/{len(results)} in {time.monotonic() - t:.0f} s")
    finally:
        llm.stop()


if __name__ == "__main__":
    main()
