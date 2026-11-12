"""The local AI: an open model that runs on this PC in Ollama (free, no account, works offline once downloaded).
It reads the mod list and the Crash doctor's problems and picks fixes from the fixes the doctor offers, and names
the mods that must load after another one (patches, add-ons). It is a general model that gets this app's modding
rules in its instructions; it was not trained only for Cyberpunk. It can only choose; the app makes every change."""
import json
import os
import subprocess
import time
import urllib.error
import urllib.request
import webbrowser
from pathlib import Path

from downloads.fetch import http_json
from shared.store import kind_of

URL = "http://127.0.0.1:11434"
DOWNLOAD_PAGE = "https://ollama.com/download"
HOME = Path(os.environ.get("LOCALAPPDATA", str(Path.home()))) / "Programs" / "Ollama"
RULES = """You help a player fix Cyberpunk 2077 mod problems inside a mod manager. You get the mod list in load order
and the problems the manager found, each with numbered fixes. Answer only in the JSON format you are given.
Load order: a mod lower in the list wins when two mods change the same thing. Frameworks (RED4ext, Cyber Engine Tweaks,
redscript, ArchiveXL, TweakXL, Codeware, Input Loader, Mod Settings) always go first; the manager does that itself.
"fixes": pick at most one fix for each problem, only from the numbered fixes given, and only when the problem's
details support it. After a game update, mods that hook the game's code (RED4ext plugins, Cyber Engine Tweaks mods,
redscript, frameworks) cause most crashes: prefer updating them. Turning a mod off is better than removing it.
"summary": 2 to 5 short, plain sentences for the player: what is wrong and what you change."""
SCHEMA = {"type": "object", "required": ["fixes", "summary"], "properties": {
    "fixes": {"type": "array", "items": {"type": "object", "required": ["problem", "fix", "why"], "properties": {
        "problem": {"type": "integer"}, "fix": {"type": "integer"}, "why": {"type": "string"}}}},
    "summary": {"type": "string"}}}
ORDER = """You know Cyberpunk 2077 mods. For each mod number, list the numbers of the other mods in the list that it \
builds on. A patch, add-on, fix, compatibility file, preset, translation or extra pack builds on the mod its name starts \
with and on every mod its name says it is for. Example: "A - Patch for B" builds on A and on B. Most mods build on no \
other mod: give them []."""  # one line: small models answer worse when the rules have line breaks


def order_schema(n):
    """Asks about each of the n mods by number: small models leave a free list of pairs empty, but fill one slot per mod."""
    nums = [str(i) for i in range(1, n + 1)]
    return {"type": "object", "required": ["builds_on"], "properties": {"builds_on": {
        "type": "object", "required": nums, "properties": {k: {"type": "array", "items": {"type": "integer"}} for k in nums}}}}


def running():
    try:
        return bool(http_json(f"{URL}/api/version", timeout=3))
    except (OSError, ValueError):
        return False


def exe():
    """ollama.exe, or None when Ollama is not installed. Not shutil.which: on Windows it looks in the current
    folder first (often Downloads), where any file named ollama.exe would run."""
    dirs = [HOME] + [Path(d) for d in os.environ.get("PATH", "").split(os.pathsep) if Path(d).is_absolute()]
    return next((str(d / "ollama.exe") for d in dirs if (d / "ollama.exe").is_file()), None)


def install():
    """Install Ollama with winget (Microsoft's installer: it checks the download's hash). Without winget, open the
    download page for the user and stop."""
    winget = Path(os.environ.get("LOCALAPPDATA", "")) / "Microsoft/WindowsApps/winget.exe"  # full path, as in exe()
    if winget.is_absolute() and winget.exists():
        subprocess.run([str(winget), "install", "--id", "Ollama.Ollama", "--exact", "--silent", "--disable-interactivity",
                        "--accept-package-agreements", "--accept-source-agreements"],
                       capture_output=True, timeout=1800, creationflags=subprocess.CREATE_NO_WINDOW)
    if not exe():
        webbrowser.open(DOWNLOAD_PAGE)
        raise RuntimeError("Ollama did not install by itself. Install it from the page I opened, then click "
                           "'Download local AI' again.")


def start(wait=40):
    """Start Ollama when it is installed but not running, and wait until it answers."""
    if running():
        return
    path = exe()
    if not path:
        raise RuntimeError("The local AI is not installed. Click 'Download local AI' in the AI helpers tab.")
    tray = Path(path).with_name("ollama app.exe")
    subprocess.Popen([str(tray)] if tray.exists() else [path, "serve"], creationflags=subprocess.DETACHED_PROCESS,
                     stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, close_fds=True)
    end = time.monotonic() + wait
    while not running():
        if time.monotonic() > end:
            raise RuntimeError("Ollama did not start. Start the Ollama app, then try again.")
        time.sleep(1)


def has_model(model):
    full = model if ":" in model else model + ":latest"
    return full in {m["name"] for m in http_json(f"{URL}/api/tags", timeout=10)["models"]}


def pull(model, progress):
    """Download the model. progress(percent) is called each time the percent changes."""
    req = urllib.request.Request(f"{URL}/api/pull", json.dumps({"model": model}).encode(), {"Content-Type": "application/json"})
    last, total, done = None, {}, {}
    with urllib.request.urlopen(req, timeout=120) as r:
        for line in r:
            d = json.loads(line)
            if d.get("error"):
                raise RuntimeError(d["error"])
            if d.get("total") and d.get("completed") is not None:  # one count per file (layer): add them up
                total[d.get("digest")], done[d.get("digest")] = d["total"], d["completed"]
                pct = sum(done.values()) * 100 // sum(total.values())
                if pct != last:
                    last = pct
                    progress(pct)


def chat(model, system, user, schema):
    body = {"model": model, "stream": False, "think": False, "format": schema, "options": {"temperature": 0, "num_ctx": 16384},
            "messages": [{"role": "system", "content": system}, {"role": "user", "content": user}]}
    try:
        return json.loads(http_json(f"{URL}/api/chat", body, timeout=900)["message"]["content"])
    except urllib.error.HTTPError as e:
        if e.code == 404:
            raise RuntimeError(f"The model {model} is not downloaded. Click 'Download local AI' in the AI helpers tab.") from None
        raise RuntimeError(f"The local AI failed: {e.read().decode('utf-8', 'replace')[:200]}") from None


def advise(model, mods, probs):
    """{"summary", "fixes": [(problem, fix, why)], "after": [(lo id, hi id)]}: hi must load after (win over) lo.
    Two questions, because a small model answers each one better alone. Picks that do not exist are dropped,
    and each problem gets at most one fix."""
    b = chat(model, ORDER, "Mods:\n" + "\n".join(f"{n}. {m['name']}" for n, m in enumerate(mods, 1)),
             order_schema(len(mods))).get("builds_on")
    after = []
    for k, v in (b.items() if isinstance(b, dict) else []):
        hi = int(k) if k.isdecimal() else 0
        if 0 < hi <= len(mods) and isinstance(v, list):
            after += [(mods[lo - 1]["id"], mods[hi - 1]["id"]) for lo in v if type(lo) is int and 0 < lo <= len(mods) and lo != hi]
    if not probs:
        return {"summary": "The Crash doctor found no problems.", "fixes": [], "after": after}
    lines = []
    for n, m in enumerate(mods, 1):
        needs = [r["name"] for r in m.get("requires", [])] + list(m.get("needs", []))
        lines.append(f"{n}. {m['name']} [{kind_of(m)}]" + ("" if m["enabled"] else " (off)") + (f", needs: {', '.join(needs)}" if needs else ""))
    plines = []
    for i, p in enumerate(probs, 1):
        plines.append(f"P{i} ({p['severity']}) {p['title']}: {str(p['detail'])[:300]}")
        plines += [f"  {j}. {f['label']}" + (" (recommended)" if f["recommended"] else "") for j, f in enumerate(p["fixes"], 1)]
    a = chat(model, RULES, "Mods in load order (1 = loads first, the last one wins):\n" + "\n".join(lines) +
             "\n\nProblems found:\n" + "\n".join(plines), SCHEMA)
    fixes, done = [], set()
    for x in a.get("fixes", []):
        p, f = x.get("problem"), x.get("fix")
        if type(p) is int and type(f) is int and 0 < p <= len(probs) and 0 < f <= len(probs[p - 1]["fixes"]) and p not in done:
            done.add(p)
            fixes.append((probs[p - 1], probs[p - 1]["fixes"][f - 1], str(x.get("why", ""))))
    return {"summary": str(a.get("summary", "")).strip(), "fixes": fixes, "after": after}
