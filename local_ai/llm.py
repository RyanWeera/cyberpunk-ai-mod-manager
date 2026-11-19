"""The local AI: an open model (Qwen3.5, Apache 2.0 license) that runs on this PC in llama.cpp's llama-server
(MIT license). Both ship in the app's local_ai folder: no install, no account, offline. The engine runs only while
the window is open and the game is not running. It reads the mod list and the Crash doctor's problems and picks fixes
from the fixes the doctor offers, and names the mods that must load after another one (patches, add-ons). It can
only choose; the app makes every change."""
import hashlib
import json
import os
import secrets
import socket
import subprocess
import sys
import threading
import time
import urllib.error
import zipfile
from pathlib import Path

from downloads.fetch import download, http_json
from shared.store import kind_of

# Next to the exe in the release zip, next to this file when run from source (fetch() fills it then).
HOME = Path(sys.executable).parent / "local_ai" if getattr(sys, "frozen", False) else Path(__file__).parent
ENGINE = {"url": "https://github.com/ggml-org/llama.cpp/releases/download/b12032/llama-b12032-bin-win-vulkan-x64.zip",
          "sha256": "19a8bbf269fa683ae7bc04f886c9c2a93e0655c2606f37d2b60069bf9e8bcc32"}  # Vulkan: any graphics card, else the CPU
MODEL = {"url": "https://github.com/RyanWeera/cyberpunk-ai-mod-manager/releases/download/model-v1/"
                "Qwen3.5-2B-CP2077-Q4_K_M.gguf",  # Qwen3.5-2B tuned for mod bugs, see MODEL.md
         "sha256": "f39ce8f3c3531d0c06620d085f02d8256c85095b2d550c366778af1b0402d679", "file": "Qwen3.5-2B-CP2077-Q4_K_M.gguf"}
SERVER = HOME / "engine" / "llama-server.exe"
MODEL_FILE = HOME / "model" / MODEL["file"]
_server, URL, KEY = None, "", ""  # the llama-server that start() ran, its address, and its key (no other program may use it)
_lock, _blocked = threading.RLock(), False  # blocked: the game runs or the window closes
BLOCKED = "The game is running. The local AI waits until you close the game, so the game gets all of your PC."
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
REPORT = """
The player also describes a bug in their own words. "cause": 1 to 3 short, plain sentences on the most likely reason,
from the mod list and the problems. "mod_fixes": changes for the mods the bug is about, only when the list supports them:
"turn_on" a mod that is off, "update" a mod that may be too old for the game, "load_last" so a mod wins over the other
mods that change the same thing, "turn_off" a mod that breaks it. Give [] when no change fits."""
ACTIONS = ["turn_on", "turn_off", "update", "load_last"]
REPORT_SCHEMA = {**SCHEMA, "required": [*SCHEMA["required"], "cause", "mod_fixes"], "properties": {
    **SCHEMA["properties"], "cause": {"type": "string"},
    "mod_fixes": {"type": "array", "items": {"type": "object", "required": ["mod", "action", "why"], "properties": {
        "mod": {"type": "integer"}, "action": {"type": "string", "enum": ACTIONS}, "why": {"type": "string"}}}}}}
ORDER = """You know Cyberpunk 2077 mods. For each mod number, list the numbers of the other mods in the list that it \
builds on. A patch, add-on, fix, compatibility file, preset, translation or extra pack builds on the mod its name starts \
with and on every mod its name says it is for. Example: "A - Patch for B" builds on A and on B. Most mods build on no \
other mod: give them []."""  # one line: small models answer worse when the rules have line breaks


def order_schema(n):
    """Asks about each of the n mods by number: small models leave a free list of pairs empty, but fill one slot per mod."""
    nums = [str(i) for i in range(1, n + 1)]
    return {"type": "object", "required": ["builds_on"], "properties": {"builds_on": {
        "type": "object", "required": nums, "properties": {k: {"type": "array", "items": {"type": "integer"}} for k in nums}}}}


def ready():
    return SERVER.is_file() and MODEL_FILE.is_file()


def fetch(progress):
    """For a run from source (the release zip has both files): download the engine and the model and check their
    hashes. progress(what, done_bytes, total_bytes)."""
    tmp = HOME / "download"
    tmp.mkdir(parents=True, exist_ok=True)
    if not SERVER.is_file():
        z = checked(download(ENGINE["url"], tmp, lambda d, t: progress("engine", d, t)), ENGINE["sha256"])
        with zipfile.ZipFile(z) as zf:
            zf.extractall(SERVER.parent)  # zipfile drops absolute paths and ".." parts
        z.unlink()
    if not MODEL_FILE.is_file():
        f = checked(download(MODEL["url"], tmp, lambda d, t: progress("model", d, t)), MODEL["sha256"])
        MODEL_FILE.parent.mkdir(exist_ok=True)
        os.replace(f, MODEL_FILE)


def checked(path, sha256):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        while chunk := f.read(1 << 20):
            h.update(chunk)
    if h.hexdigest() != sha256:
        path.unlink()
        raise RuntimeError(f"{path.name} is not the file I expect (wrong SHA-256). Try again later.")
    return path


def running():
    return _server is not None and _server.poll() is None


def block(on=True):
    """on: the game starts or the window closes. End the engine and refuse to start it until block(False)."""
    global _blocked
    with _lock:
        _blocked = on
        if on:
            stop()


def start(wait=180):
    """Start the engine with the model, on a free local port, and wait until the model is loaded."""
    global _server, URL, KEY
    with _lock:  # block() cannot slip in between the check and the start
        if _blocked:
            raise RuntimeError(BLOCKED)
        if running():
            return
        if not ready():
            raise RuntimeError("The local AI files are missing. Click 'Download local AI' in the AI helpers tab.")
        with socket.socket() as s:
            s.bind(("127.0.0.1", 0))
            port = s.getsockname()[1]
        KEY = secrets.token_urlsafe(24)
        proc = _server = subprocess.Popen([str(SERVER), "-m", str(MODEL_FILE), "--host", "127.0.0.1", "--port", str(port),
                                           "-c", "16384", "--jinja", "--reasoning-budget", "0", "--no-webui", "--api-key", KEY],
                                          cwd=SERVER.parent, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                                          stderr=subprocess.DEVNULL, creationflags=subprocess.CREATE_NO_WINDOW)
        URL = f"http://127.0.0.1:{port}"
    end = time.monotonic() + wait
    while True:
        try:
            if http_json(f"{URL}/health", timeout=3).get("status") == "ok":
                break
        except (OSError, ValueError):
            pass  # not listening yet, or 503 while the model loads
        if proc.poll() is not None or time.monotonic() > end:
            with _lock:
                if _server is proc:
                    stop()
                raise RuntimeError(BLOCKED if _blocked else "The local AI did not start. Close other big programs "
                                   "(it needs about 2 GB of memory) and try again.")
        time.sleep(0.5)
    with _lock:  # block() ran while the model loaded: it ended this engine
        if _server is not proc:
            raise RuntimeError(BLOCKED)


def stop():
    """Give the PC back (the game starts, or the window closes): end the engine, which frees the model's memory."""
    global _server
    with _lock:
        if _server:
            _server.kill()
            _server.wait(10)
            _server = None


def chat(system, user, schema):
    body = {"temperature": 0, "response_format": {"type": "json_schema", "json_schema": {"name": "answer", "schema": schema}},
            "chat_template_kwargs": {"enable_thinking": False},
            "messages": [{"role": "system", "content": system}, {"role": "user", "content": user}]}
    try:
        r = http_json(f"{URL}/v1/chat/completions", body, {"Authorization": f"Bearer {KEY}"}, timeout=900)
        return json.loads(r["choices"][0]["message"]["content"])
    except urllib.error.HTTPError as e:
        raise RuntimeError(f"The local AI failed: {e.read().decode('utf-8', 'replace')[:200]}") from None



def advise(mods, probs, report=""):
    """{"summary", "fixes": [(problem, fix, why)], "after": [(lo id, hi id)]}: hi must load after (win over) lo.
    With the player's `report` of a bug, also "cause" and "mod_fixes": [(mod, action, why)].
    Two questions, because a small model answers each one better alone. Picks that do not exist are dropped,
    and each problem gets at most one fix."""
    b = chat(ORDER, "Mods:\n" + "\n".join(f"{n}. {m['name']}" for n, m in enumerate(mods, 1)),
             order_schema(len(mods))).get("builds_on")
    after = []
    for k, v in (b.items() if isinstance(b, dict) else []):
        hi = int(k) if k.isdecimal() else 0
        if 0 < hi <= len(mods) and isinstance(v, list):
            after += [(mods[lo - 1]["id"], mods[hi - 1]["id"]) for lo in v if type(lo) is int and 0 < lo <= len(mods) and lo != hi]
    if not probs and not report:
        return {"summary": "The Crash doctor found no problems.", "fixes": [], "after": after}
    lines = []
    for n, m in enumerate(mods, 1):
        needs = [r["name"] for r in m.get("requires", [])] + list(m.get("needs", []))
        lines.append(f"{n}. {m['name']} [{kind_of(m)}]" + ("" if m["enabled"] else " (off)") + (f", needs: {', '.join(needs)}" if needs else ""))
    plines = []
    for i, p in enumerate(probs, 1):
        plines.append(f"P{i} ({p['severity']}) {p['title']}: {str(p['detail'])[:300]}")
        plines += [f"  {j}. {f['label']}" + (" (recommended)" if f["recommended"] else "") for j, f in enumerate(p["fixes"], 1)]
    a = chat(RULES + (REPORT if report else ""), "Mods in load order (1 = loads first, the last one wins):\n" +
             "\n".join(lines) + "\n\nProblems found:\n" + ("\n".join(plines) or "none") +
             (f"\n\nThe player says: {report[:1000]}" if report else ""), REPORT_SCHEMA if report else SCHEMA)
    fixes, done = [], set()
    for x in a.get("fixes", []):
        p, f = x.get("problem"), x.get("fix")
        if type(p) is int and type(f) is int and 0 < p <= len(probs) and 0 < f <= len(probs[p - 1]["fixes"]) and p not in done:
            done.add(p)
            fixes.append((probs[p - 1], probs[p - 1]["fixes"][f - 1], str(x.get("why", ""))))
    out = {"summary": str(a.get("summary", "")).strip(), "fixes": fixes, "after": after}
    if report:
        picks = [x for x in a.get("mod_fixes", []) if isinstance(x, dict)]
        out["cause"] = str(a.get("cause", "")).strip()
        out["mod_fixes"] = [(mods[x["mod"] - 1], x["action"], str(x.get("why", ""))) for x in picks
                            if type(x.get("mod")) is int and 0 < x["mod"] <= len(mods) and x.get("action") in ACTIONS]
    return out
