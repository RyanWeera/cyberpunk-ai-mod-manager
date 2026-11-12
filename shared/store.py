"""App data folder, settings and the installed-mod list, kept in one JSON file."""
import glob
import json
import os
import re
import threading
from pathlib import Path

VERSION = "1.0.2"  # window title, MCP server and the Nexus Application-Version header
DATA = Path(os.environ.get("CP2077MM_DATA") or Path(os.environ.get("LOCALAPPDATA", str(Path.home()))) / "CP2077ModManager")
GAME_EXE = "bin/x64/Cyberpunk2077.exe"
DEFAULTS = {"game_dir": "", "api_key": "", "virus_scan": True, "auto_fix": True, "include_optional": True, "ai_model": "qwen3.5:4b"}


def kind_of(m):
    """A mod's file types for people: "Framework", or e.g. "Archive, Redscript"."""
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


class Store:
    """mods[0] is the lowest priority; the last mod in the list wins every conflict."""

    def __init__(self, root=DATA):
        self.root = Path(root)
        self.downloads, self.staging, self.backups = (self.root / n for n in ("downloads", "mods", "backups"))
        for p in (self.downloads, self.staging, self.backups):
            p.mkdir(parents=True, exist_ok=True)
        self.file = self.root / "state.json"
        self.lock = threading.RLock()
        d = json.loads(self.file.read_text("utf-8")) if self.file.exists() else {}
        self.settings = {**DEFAULTS, **d.get("settings", {})}
        self.mods = d.get("mods", [])
        self.deployed = d.get("deployed", {})  # lowercase game-relative path -> mod id (None: copy again)
        self.deploy_game = d.get("deploy_game", "")  # the game folder `deployed` describes
        # False only for a state file written by v1.0.0, until its first v1.0.1 deploy decides who owns modlist.txt
        self.modlist_tracked = d.get("modlist_tracked", not self.file.exists())
        self.doctor = d.get("doctor", {})      # last_launch, last_good, bisect, history, ignored

    def save(self):
        with self.lock:
            tmp = self.file.with_suffix(".tmp")
            body = {"settings": self.settings, "mods": self.mods, "deployed": self.deployed, "deploy_game": self.deploy_game,
                    "modlist_tracked": self.modlist_tracked, "doctor": self.doctor}
            tmp.write_text(json.dumps(body, indent=1), "utf-8")
            os.replace(tmp, self.file)

    def mod(self, mod_id):
        return next((m for m in self.mods if m["id"] == mod_id), None)

    def enabled(self):
        return [m for m in self.mods if m["enabled"]]

    def game(self):
        g = self.settings["game_dir"]
        return Path(g) if g and (Path(g) / GAME_EXE).exists() else None


def find_game():
    """Look in Steam libraries, GOG and Epic for the Cyberpunk 2077 folder."""
    cands = []
    try:
        import winreg
        with winreg.OpenKey(winreg.HKEY_CURRENT_USER, r"Software\Valve\Steam") as k:
            steam = Path(winreg.QueryValueEx(k, "SteamPath")[0])
        vdf = steam / "steamapps/libraryfolders.vdf"
        libs = [steam] + ([Path(p.replace("\\\\", "\\")) for p in re.findall(r'"path"\s+"([^"]+)"', vdf.read_text("utf-8", "ignore"))] if vdf.exists() else [])
        cands += [lib / "steamapps/common/Cyberpunk 2077" for lib in libs]
    except OSError:
        pass
    try:
        import winreg
        with winreg.OpenKey(winreg.HKEY_LOCAL_MACHINE, r"SOFTWARE\WOW6432Node\GOG.com\Games\1423049311") as k:
            cands.append(Path(winreg.QueryValueEx(k, "path")[0]))
    except OSError:
        pass
    for item in glob.glob(os.path.join(os.environ.get("ProgramData", r"C:\ProgramData"), r"Epic\EpicGamesLauncher\Data\Manifests\*.item")):
        try:
            d = json.loads(Path(item).read_text("utf-8"))
            if "cyberpunk" in d.get("DisplayName", "").lower():
                cands.append(Path(d["InstallLocation"]))
        except (OSError, ValueError, KeyError):
            pass
    cands += [Path(r"C:\Program Files (x86)\Steam\steamapps\common\Cyberpunk 2077"), Path(r"C:\GOG Games\Cyberpunk 2077"),
              Path(r"C:\Program Files\Epic Games\Cyberpunk2077")]
    return next((str(c) for c in cands if (c / GAME_EXE).exists()), "")
