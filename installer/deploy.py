"""Copy enabled mods from staging into the game folder. Lower in the mod list wins.
Game files a mod replaces are backed up first and put back when no mod needs that path."""
import os
import shutil
import time
from pathlib import Path

from shared.store import GAME_EXE

ARCHIVES = "archive/pc/mod"
MODLIST, LOAD_ORDER = ARCHIVES + "/modlist.txt", "__load_order__"  # deployed[MODLIST] = LOAD_ORDER while we own it
KEEP = {"archive/pc/mod", "r6/scripts", "r6/tweaks", "r6/input", "red4ext/plugins", "bin/x64",
        "bin/x64/plugins", "bin/x64/plugins/cyber_engine_tweaks/mods", "mods", "engine/config/platform/pc"}


def plan(store):
    want = {}
    for m in store.enabled():  # later mods overwrite earlier ones
        for rel in m["files"]:
            if rel.lower() != MODLIST:  # the load order file is written by deploy(), never copied from a mod
                want[rel.lower()] = (m["id"], rel)
    return want


def _remove(store, game, key):
    dst, bak = game / key, store.backups / key
    if bak.exists():
        os.replace(bak, dst)
    else:
        dst.unlink(missing_ok=True)
        d = dst.parent
        while d != game and d.relative_to(game).as_posix().lower() not in KEEP and d.is_dir() and not any(d.iterdir()):
            d.rmdir()
            d = d.parent
    store.deployed.pop(key, None)


def _switch_game(store, game):
    """The game folder changed since the last deploy: take the mods out of the old folder first."""
    old = Path(store.deploy_game) if store.deploy_game else None
    if old and store.deployed and os.path.normcase(str(old)) != os.path.normcase(str(game)):
        if (old / GAME_EXE).exists():
            for key in list(store.deployed):
                _remove(store, old, key)
        else:  # the old folder is gone: its backups can never go back, so keep them aside, unused
            store.deployed.clear()
            if any(store.backups.iterdir()):
                os.replace(store.backups, store.backups.with_name(f"backups-{int(time.time())}"))
                store.backups.mkdir()
    store.deploy_game = str(game)


def load_order(store, game):
    """(managed, others): archive file names in load order. The game loads archive/pc/mod in modlist.txt
    order and the first archive loaded wins, so the highest-priority mod goes first. Archives this app
    does not manage go last."""
    managed = [Path(r).name for m in reversed(store.enabled()) for r in m["files"]
               if r.lower().startswith(ARCHIVES + "/") and r.lower().endswith(".archive") and r.count("/") == 3]
    managed = list(dict.fromkeys(managed))
    d = game / ARCHIVES
    low = {n.lower() for n in managed}
    others = sorted(p.name for p in d.glob("*.archive") if p.name.lower() not in low) if d.is_dir() else []
    return managed, others


def deploy(store):
    """Sync the game folder with the enabled mods. Returns how many files were copied or removed.
    A file is copied only when its owner changed or it is missing, so configs that the game or a mod
    edits in place are kept. State is saved even when a copy fails part way."""
    game = store.game()
    if not game:
        raise RuntimeError("Set the Cyberpunk 2077 folder in Settings first.")
    with store.lock:
        try:
            _switch_game(store, game)
            # v1.0.0 wrote modlist.txt without recording it: then the file there is ours, not the user's
            ours_untracked = MODLIST not in store.deployed and any(k.startswith(ARCHIVES + "/") for k in store.deployed)
            want = plan(store)
            gone = [k for k in store.deployed if k not in want and k != MODLIST]
            for key in gone:
                _remove(store, game, key)
            changed = len(gone)
            for key, (mid, rel) in want.items():
                src, dst = store.staging / mid / rel, game / rel
                if key not in store.deployed and dst.exists():
                    bak = store.backups / key
                    if not bak.exists():
                        bak.parent.mkdir(parents=True, exist_ok=True)
                        shutil.copy2(dst, bak)
                if store.deployed.get(key) != mid or not dst.exists():
                    dst.parent.mkdir(parents=True, exist_ok=True)
                    shutil.copy2(src, dst)
                    changed += 1
                store.deployed[key] = mid
            managed, others = load_order(store, game)
            if managed:  # the user's own modlist.txt is backed up once, like any game file we replace
                f, bak = game / MODLIST, store.backups / MODLIST
                if MODLIST not in store.deployed and f.exists() and not bak.exists() and not ours_untracked:
                    bak.parent.mkdir(parents=True, exist_ok=True)
                    shutil.copy2(f, bak)
                f.write_text("\n".join(managed + others) + "\n", "utf-8")
                store.deployed[MODLIST] = LOAD_ORDER
            elif MODLIST in store.deployed or ours_untracked:  # no managed archive is on: give the folder back
                store.deployed[MODLIST] = LOAD_ORDER
                _remove(store, game, MODLIST)  # puts the user's file back, or deletes ours
            return changed
        finally:
            store.save()
