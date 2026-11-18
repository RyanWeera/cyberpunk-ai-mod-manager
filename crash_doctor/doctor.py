"""Find why the game crashed (or will), and list every way to fix each problem.

A problem is {"id", "title", "detail", "severity", "auto", "fixes": [{"label", "action", "recommended"}]}.
`action` is a list the controller runs, e.g. ["disable", mod_id]. A problem with exactly one fix and
auto=True is fixed without asking; any other problem is shown to the user to pick a fix.
"""
import bisect
import ctypes
import heapq
import os
import re
import struct
import time
from collections import Counter
from datetime import datetime
from itertools import combinations, pairwise

from downloads.fetch import FRAMEWORKS, FW_BY_NEXUS

LEVEL_OK = re.compile(r"(?i)\[(info|debug|trace)\b")
ERR = re.compile(r"(?i)\b(error|critical|fatal|exception|failed)\b")
PATH = re.compile(r"(?:^|[\s'\"(\[=])((?:archive|bin|engine|r6|red4ext|mods)/[^:'\"()<>|*?,\r\n]+)")
ARCH = re.compile(r"([^\s/:'\"]+\.archive)\b")
ACR = "bin/x64/plugins/AdvancedCrashReporter/reports"  # reports of the Advanced Crash Reporter mod, when it is installed
CODES = {  # Windows exception codes in plain words
    0xC0000005: "Code 0xC0000005: the game read or wrote memory it does not own. A DLL mod made for another game version "
                "often causes this.",
    0xC0000409: "Code 0xC0000409: the game found damaged memory on its call stack and stopped itself.",
    0xC00000FD: "Code 0xC00000FD: a function called itself again and again until the stack was full.",
    0xC0000374: "Code 0xC0000374: the game's memory heap became damaged, often by a mod that wrote past its memory.",
    0xC000001D: "Code 0xC000001D: the processor found an instruction it does not know.",
    0x80000003: "Code 0x80000003: the game or a mod stopped on purpose after a fatal error.",
    0xE06D7363: "Code 0xE06D7363: a C++ error was thrown and nothing caught it.",
}
HAND = ["archive/pc/mod", "mods", "red4ext/plugins", "bin/x64/plugins/cyber_engine_tweaks/mods", "bin/x64/plugins",
        "r6/scripts", "r6/tweaks"]  # where players put mods by hand
STAMP = re.compile(r"^\[(\d{4}-\d\d-\d\d[ T]\d\d:\d\d:\d\d(?:\.\d+)?)")
SLOW = 3.0  # seconds: a DLL mod that loads slower than this slows every game start
LOGS = ["r6/logs", "red4ext/logs", "red4ext/plugins", "bin/x64/plugins/cyber_engine_tweaks"]
# Folders that hold the game's own files and many mods' files: a crash or log there never blames one mod.
SHARED = {"archive", "archive/pc", "archive/pc/mod", "archive/pc/content", "bin", "bin/x64", "bin/x64/plugins",
          "bin/x64/plugins/cyber_engine_tweaks/mods", "engine", "engine/config", "engine/config/base", "engine/config/platform",
          "engine/config/platform/pc", "engine/tools", "mods", "r6", "r6/cache", "r6/config", "r6/input", "r6/logs",
          "r6/scripts", "r6/tweaks", "red4ext", "red4ext/logs", "red4ext/plugins"}


def problem(pid, title, detail, fixes, severity="high", auto=True):
    return {"id": pid, "title": title, "detail": detail, "fixes": fixes, "severity": severity, "auto": auto}


def fix(label, *action, recommended=False):
    return {"label": label, "action": list(action), "recommended": recommended}


def owner_index(store):
    """(files, dirs): lowercase game path -> mod id. A folder below the shared game folders belongs to a mod
    when that mod alone has files directly in it (or, with no direct files, alone has files under it)."""
    files, direct, under = {}, {}, {}
    for m in store.mods:
        for rel in m["files"]:
            r = rel.lower()
            parts = r.split("/")
            files[r] = m["id"]
            if r.startswith("archive/pc/mod/"):
                files.setdefault(parts[-1], m["id"])
            direct.setdefault("/".join(parts[:-1]), set()).add(m["id"])
            for i in range(1, len(parts)):
                under.setdefault("/".join(parts[:i]), set()).add(m["id"])
    dirs = {}
    for d, mods in under.items():
        if d in SHARED:
            continue
        own = direct.get(d, set())
        if len(own) == 1 or (not own and len(mods) == 1):
            dirs[d] = next(iter(own or mods))
    return files, dirs


def owner_of(path, idx):
    files, dirs = idx
    s = path.lower().replace("\\", "/").strip("/ ")[:300]  # real paths are short; a huge log line must not explode
    cands = [s] + [s[:i] for i in range(len(s) - 1, 0, -1) if s[i] == " "]  # paths may hold spaces
    for c in cands:
        if c in files:
            return files[c]
    hits = []
    for c in cands:
        p = c
        while "/" in p:
            p = p.rsplit("/", 1)[0]
            if p in dirs:
                hits.append(p)
                break
    return dirs[max(hits, key=len)] if hits else None


def owners_in(text, idx, game=None):
    t = text.lower().replace("\\", "/")
    if game:
        t = t.replace(str(game).lower().replace("\\", "/").rstrip("/") + "/", " ")
    found = [owner_of(m, idx) for m in PATH.findall(t)] + [idx[0].get(a) for a in ARCH.findall(t)]
    return next((f for f in found if f), None)


def log_errors(game, since, idx):
    """{mod id: {"lines": [...], "strong": bool}} from framework logs written since `since`.
    strong = a redscript or RED4ext-plugin error, which stops mods from loading; CET Lua errors are weak."""
    found = {}
    for base in LOGS:
        d = game / base
        if not d.is_dir():
            continue
        for log in d.rglob("*.log"):
            try:
                st = log.stat()
                if st.st_mtime < since:
                    continue
                with open(log, "rb") as fh:  # only the end of the log: some grow to gigabytes
                    fh.seek(max(0, st.st_size - 2_000_000))
                    lines = fh.read().decode("utf-8", "replace").splitlines()[-5000:]
            except OSError:
                continue
            # A log inside a mod's own folder is about that mod; shared log folders need a path in the line.
            log_owner = None if base.endswith("/logs") else owner_of(log.relative_to(game).as_posix(), idx)
            for i, line in enumerate(lines):
                if LEVEL_OK.search(line) or not ERR.search(line):
                    continue
                mid = owners_in(" ".join(lines[i:i + 3]), idx, game) or log_owner
                if not mid:
                    continue
                hit = found.setdefault(mid, {"lines": [], "strong": False})
                hit["strong"] |= not base.startswith("bin/")
                if len(hit["lines"]) < 8:
                    hit["lines"].append(f"{log.name}: {line.strip()[:220]}")
    return found


def mod_fixes(m, has_update):
    fixes = [fix(f"Disable {m['name']}", "disable", m["id"])]
    if has_update(m):
        fixes.insert(0, fix(f"Update {m['name']} to its newest version", "update", m["id"], recommended=True))
    return fixes


def missing_frameworks(store):
    have = {m.get("framework") for m in store.enabled()}
    out = []
    for fw in FRAMEWORKS:
        users = [m for m in store.enabled() if fw in m.get("needs", [])]
        if not users or fw in have:
            continue
        off = next((m for m in store.mods if m.get("framework") == fw), None)
        first = fix(f"Turn {fw} back on", "enable", off["id"], recommended=True) if off else \
            fix(f"Install {fw} (automatic download)", "install_fw", fw, recommended=True)
        out.append(problem(f"need-{fw}", f"{fw} is missing", "Needed by: " + ", ".join(m["name"] for m in users[:6]),
                           [first, fix(f"Disable the {len(users)} mod(s) that need it", "disable_many", [m["id"] for m in users])]))
    return out


def missing_requirements(store):
    have = {m["source"].get("mod_id") for m in store.enabled() if m["source"]["type"] == "nexus"}
    need = {}
    for m in store.enabled():
        for r in m.get("requires", []):
            if r["mod_id"] not in have and r["mod_id"] not in FW_BY_NEXUS:
                need.setdefault(r["mod_id"], (r["name"], []))[1].append(m)
    return [problem(f"req-{rid}", f"Required mod missing: {name}", "Needed by: " + ", ".join(m["name"] for m in users[:6]),
                    [fix(f"Download {name}", "queue_nexus", rid, recommended=True),
                     fix(f"Disable the {len(users)} mod(s) that need it", "disable_many", [m["id"] for m in users])])
            for rid, (name, users) in need.items()]


def archive_hashes(path):
    """Resource hashes inside a .archive (RDAR v12: 40-byte header, index of 56-byte file entries)."""
    try:
        with open(path, "rb") as f:
            magic, _, ipos, isz = struct.unpack("<IIQI", f.read(20))
            if magic != 0x52414452:
                return set()
            f.seek(ipos)
            idx = f.read(isz)
        n = struct.unpack_from("<I", idx, 16)[0]
        return {struct.unpack_from("<Q", idx, 28 + i * 56)[0] for i in range(min(n, (len(idx) - 28) // 56))}
    except (OSError, struct.error):
        return set()


def overlaps(store):
    """(sizes, pairs): how many archive resources and loose game files each enabled mod changes, and how many
    of them each pair of mods (lower, higher in the list) both change."""
    owners, sizes = {}, {}
    for m in store.enabled():
        res = set().union(*(archive_hashes(store.staging / m["id"] / r) for r in m["files"] if r.lower().endswith(".archive")))
        res |= {"file:" + r.lower() for r in m["files"] if not r.lower().endswith(".archive")}  # only one copy can be in the game
        sizes[m["id"]] = len(res)
        for h in res:
            owners.setdefault(h, []).append(m["id"])
    return sizes, Counter(p for ids in owners.values() for p in combinations(ids, 2))


def archive_conflicts(store):
    _, pairs = overlaps(store)
    ignored = set(store.doctor.get("ignored", []))
    out = []
    for (lo, hi), n in pairs.most_common(50):
        pid = f"conflict-{lo}-{hi}"
        if pid in ignored:
            continue
        a, b = store.mod(lo), store.mod(hi)
        out.append(problem(pid, f"{a['name']} and {b['name']} change the same {n} game file(s)",
                           f"{b['name']} wins now (it is lower in the list).",
                           [fix(f"Keep {b['name']} winning", "ignore", pid, recommended=True),
                            fix(f"Let {a['name']} win", "prefer", lo, hi),
                            fix(f"Disable {a['name']}", "disable", lo), fix(f"Disable {b['name']}", "disable", hi)],
                           severity="info", auto=False))
    return out


def changed_since_good(store):
    good = store.doctor.get("last_good")
    changed = [m for m in store.enabled() if good is not None and good.get(m["id"]) != m.get("version", "")]
    fixes = [fix("Find the bad mod automatically (disables half the mods per game launch)", "bisect_start", recommended=not changed)]
    if changed:
        fixes.insert(0, fix(f"Disable the {len(changed)} mod(s) added or changed since the last launch that worked",
                            "disable_many", [m["id"] for m in changed], recommended=True))
    return problem("unknown", "The crash left no clue in the logs",
                   ("Changed since the last good launch: " + ", ".join(m["name"] for m in changed[:8])) if changed else
                   "No launch without a crash is on record yet.", fixes, auto=False)


def best_order(store, after=()):
    """Every mod id in the load order most likely to work (index 0 = lowest priority). Frameworks go first,
    each mod wins over the mods it requires, and of two mods that change the same game resources the one
    that changes fewer wins: it is usually a patch or add-on for the bigger one. A conflict the user chose
    to keep keeps its winner. `after`: more (lo, hi) pairs where hi must win, such as the patches the local AI
    found by name; they rank below the rules above them, above the size rule. A mod that no rule moves keeps its place
    relative to the others."""
    mods = list(store.mods)  # a framework install can insert into the live list while the archives are read
    pos = {m["id"]: i for i, m in enumerate(mods)}
    fw = {m["id"] for m in mods if m.get("framework")}
    beats = {mid: set() for mid in pos}  # mod -> the mods that must win over it

    def wins(lo, hi):  # hi wins over lo, unless the rules so far already make lo win over hi
        # ponytail: one graph walk per rule; fine for hundreds of mods, keep reachability sets if it gets slow
        seen, todo = set(), [hi]
        while todo:
            x = todo.pop()
            if x == lo:
                return
            if x not in seen:
                seen.add(x)
                todo += beats[x]
        beats[lo].add(hi)

    nexus = {}
    for m in mods:
        if m["source"]["type"] == "nexus":
            nexus.setdefault(m["source"]["mod_id"], []).append(m["id"])
    rules = [(rid, m["id"]) for m in mods for r in m.get("requires", []) for rid in nexus.get(r["mod_id"], [])]
    sizes, pairs = overlaps(store)
    kept = set(store.doctor.get("ignored", []))
    rules += [(a, b) if f"conflict-{a}-{b}" in kept else (b, a) for a, b in pairs
              if {f"conflict-{a}-{b}", f"conflict-{b}-{a}"} & kept]
    rules += [(lo, hi) for lo, hi in after if lo in pos and hi in pos]
    rules += [sorted(p, key=sizes.get, reverse=True) for p, _ in pairs.most_common() if sizes[p[0]] != sizes[p[1]]]
    for lo, hi in rules:
        if lo != hi and lo in pos and hi in pos and not {lo, hi} & fw:  # in pos: not a mod added meanwhile
            wins(lo, hi)
    waits = Counter(hi for his in beats.values() for hi in his)
    ready = [(mid not in fw, pos[mid]) for mid in pos if not waits[mid]]
    heapq.heapify(ready)
    out = []
    while ready:  # the earliest free mod next, frameworks before all others
        mid = mods[heapq.heappop(ready)[1]]["id"]
        out.append(mid)
        for hi in beats[mid]:
            waits[hi] -= 1
            if not waits[hi]:
                heapq.heappush(ready, (hi not in fw, pos[hi]))
    return out


def moved(old, new):
    """How many mods the order `new` moves compared with `old`: all but the most that keep their order."""
    pos, tails = {mid: i for i, mid in enumerate(old)}, []
    places = [pos[mid] for mid in new if mid in pos]
    for i in places:  # the longest increasing run of old places (patience sort) is what stays
        k = bisect.bisect(tails, i)
        tails[k:k + 1] = [i]
    return len(places) - len(tails)


def acr_report(game, since):
    """(path, text) of the newest Advanced Crash Reporter report written since `since`, or None."""
    d = game / ACR
    reps = []
    for f in d.glob("*") if d.is_dir() else []:
        try:
            if f.is_file() and f.stat().st_mtime >= since:
                reps.append((f.stat().st_mtime, f))
        except OSError:
            continue
    if not reps:
        return None
    f = max(reps)[1]
    try:
        with open(f, "rb") as fh:  # the summary is at the top; the full report lists every installed mod
            return f, fh.read(6000).decode("utf-8", "replace")
    except OSError:
        return None


def call_path(ev, idx, game, blamed, store, has_update, what="crashed"):
    """Mods whose code is on the stack of the crashing thread (or of the main thread in a freeze). With no mod blamed yet,
    the newest one is the main suspect."""
    probs, places = [], ev.get("places") or {}
    for module in ev.get("stack") or []:
        mid = owners_in(module, idx, game)
        if not mid or mid in blamed:
            continue
        m = store.mod(mid)
        first = not blamed
        blamed.add(mid)
        if first:
            title, why = f"{m['name']} was running when the game {what}", "This is a strong hint, not proof."
        else:
            title, why = f"{m['name']} is in the call path", "An earlier call on the same thread, so it is less likely the cause."
        where = f"\nWhere: {places[module]}" if module in places else ""
        probs.append(problem(f"{'stack' if what == 'crashed' else 'freeze'}-{mid}", title, f"{module}{where}\n{why}",
                             mod_fixes(m, has_update), severity="high" if first else "medium", auto=False))
    return probs


def freeze_problems(hang, idx, game, blamed, store, has_update):
    """Problems from the snapshot of a game that stopped responding and never came back."""
    probs = call_path(hang, idx, game, blamed, store, has_update, what="froze")
    others = [store.mod(mid)["name"] for mid in dict.fromkeys(owners_in(m, idx, game) for m in hang.get("others") or [])
              if mid and mid not in blamed]
    note = ("\nMod code on other game threads (each may be waiting, not stuck): " + ", ".join(others)) if others else ""
    if probs:
        probs[0]["detail"] += note
    else:
        probs.append(problem("freeze", "The game froze, with no mod code on its main thread",
                             "The game stopped responding and did not come back. A mesh, texture or tweak mod (no code) can do "
                             "this too." + note,
                             [fix("Find the bad mod automatically (disables half the mods per game launch)", "bisect_start")],
                             auto=False))
    return probs


def hand_installed(store):
    """[(game path, modified time)] of mods in the game folder that this app did not install, newest first."""
    game = store.game()
    if not game:
        return []
    ours = set()
    for m in store.mods:
        for rel in m["files"]:
            parts = rel.lower().split("/")
            ours.update("/".join(parts[:i]) for i in range(1, len(parts) + 1))
    out = []
    for root in HAND:
        d = game / root
        if not d.is_dir():
            continue
        for e in d.iterdir():
            rel = f"{root}/{e.name}"
            if rel.lower() in ours or rel.lower() in HAND or e.name.lower() == "cyber_engine_tweaks":
                continue
            try:
                out.append((rel, e.stat().st_mtime))
            except OSError:
                continue
    return sorted(out, key=lambda x: -x[1])


def recent_hand_installed(store, days=7):
    hand = [h for h in hand_installed(store) if h[1] >= time.time() - days * 86400]
    if not hand:
        return []
    pid = f"manual-{int(hand[0][1])}"
    if pid in store.doctor.get("ignored", []):
        return []
    return [problem(pid, f"{len(hand)} mod(s) installed by hand changed in the last {days} days",
                    "This app did not install these, so it cannot turn them off. To test one, move it out of the game "
                    "folder:\n" + "\n".join(f"{time.strftime('%Y-%m-%d', time.localtime(t))}  {rel}" for rel, t in hand[:20]),
                    [fix("Hide this", "ignore", pid)], severity="medium", auto=False)]


def game_version(game):
    """File version of the game's exe, e.g. "3.0.81.8013", or its size when Windows gives no version."""
    exe = str(game / "bin/x64/Cyberpunk2077.exe")
    try:
        v = ctypes.windll.version
        n = v.GetFileVersionInfoSizeW(exe, None)
        buf, ptr, size = ctypes.create_string_buffer(n), ctypes.c_void_p(), ctypes.c_uint()
        if n and v.GetFileVersionInfoW(exe, 0, n, buf) and v.VerQueryValueW(buf, "\\", ctypes.byref(ptr), ctypes.byref(size)):
            ms, ls = struct.unpack_from("<II", ctypes.string_at(ptr, 16), 8)  # VS_FIXEDFILEINFO: signature, struct version, MS, LS
            return f"{ms >> 16}.{ms & 0xFFFF}.{ls >> 16}.{ls & 0xFFFF}"
        return f"size {os.path.getsize(exe)}"
    except (OSError, AttributeError):
        return None


def game_updated(store, has_update=None):
    """A problem when the game changed since the last launch that worked: frameworks often break after a patch."""
    has_update = has_update or (lambda m: False)
    game, old = store.game(), store.doctor.get("last_good_game")
    new = game and old and game_version(game)
    pid = f"game-{new}"
    if not new or new == old or pid in store.doctor.get("ignored", []):
        return []
    ups = [m for m in store.enabled() if m.get("framework") and has_update(m)]
    fixes = [fix(f"Update {m['name']} to its newest version", "update", m["id"], recommended=True) for m in ups]
    return [problem(pid, "The game was updated since the last launch that worked",
                    f"Game version {old} -> {new}. RED4ext, Cyber Engine Tweaks, redscript and other frameworks often need an "
                    "update after a game patch. " + ("Updates found: " + ", ".join(m["name"] for m in ups) if ups else
                                                    "No framework update is known yet. Check their Nexus pages."),
                    fixes + [fix("Hide this", "ignore", pid)], severity="high" if ups else "medium", auto=False)]


def slow_mods(game, since, idx, limit=SLOW):
    """{mod id: seconds} for DLL mods that took longer than `limit` to load, from RED4ext's log: the time from the line
    that loads the mod to the next line."""
    d = game / "red4ext/logs"
    found = {}
    try:
        logs = [f for f in d.glob("red4ext*.log") if f.stat().st_mtime >= since] if d.is_dir() else []
        if not logs:
            return {}
        lines = max(logs, key=lambda f: f.stat().st_mtime).read_text(encoding="utf-8", errors="replace").splitlines()[-5000:]
        stamped = [(datetime.fromisoformat(m[1].replace(" ", "T")), line) for line in lines if (m := STAMP.match(line))]
    except (OSError, ValueError):  # the log changed while read, or a bad time stamp
        return {}
    for (t, line), (t2, _) in pairwise(stamped):
        if re.search(r"(?i)loading plugin", line) and (mid := owners_in(line, idx, game)):
            secs = (t2 - t).total_seconds()
            if max(limit, found.get(mid, 0)) < secs < 120:  # a longer gap is a quiet log, not a slow load
                found[mid] = secs
    return found


def report(store, probs, crash=None):
    """Plain-text report to share with a mod author. Folder paths of this PC are hidden."""
    game = store.game()
    out = [f"Cyberpunk 2077 AI Mod Manager crash report, {time.strftime('%Y-%m-%d %H:%M')}"]
    if game:
        out.append(f"Game version: {game_version(game)}")
    if crash:
        out += ["", "== LAST GAME EXIT ==", f"Crashed: {'yes' if crash.get('crashed') else 'no'}"]
        if crash.get("exit_code") is not None:
            out.append(f"Exit code: 0x{crash['exit_code']:08X}")
        if crash.get("code") in CODES:
            out.append(CODES[crash["code"]])
        if crash.get("module"):
            out.append(f"Crashed in: {crash['module']}  {(crash.get('places') or {}).get(crash['module'], '')}")
        for name, ev in (("Crashing thread", crash), ("Main thread at the freeze", crash.get("hang") or {})):
            if ev.get("stack"):
                out.append(f"{name}, newest call first:")
                out += [f"  {(ev.get('places') or {}).get(m, '')}  {m}" for m in ev["stack"]]
        if (crash.get("hang") or {}).get("others"):
            out.append("Other threads at the freeze: " + ", ".join(crash["hang"]["others"]))
    out += ["", "== PROBLEMS FOUND =="] + ([f"[{p['severity']}] {p['title']}\n  " + p["detail"].replace("\n", "\n  ") for p in probs] or ["None"])
    out += ["", "== MODS IN LOAD ORDER (lower wins) =="]
    out += [f"{'on ' if m['enabled'] else 'off'}  {m['name']}  {m.get('version', '')}" for m in store.mods]
    hand = hand_installed(store)
    if hand:
        out += ["", "== MODS INSTALLED BY HAND =="]
        out += [f"{time.strftime('%Y-%m-%d', time.localtime(t))}  {rel}" for rel, t in hand]
    text = "\n".join(out) + "\n"
    for path, name in ((str(game) if game else None, "<game>"), (os.path.expanduser("~"), "%USERPROFILE%")):
        for p in {path, path.replace("\\", "/")} if path else ():
            text = re.sub(re.escape(p), name, text, flags=re.IGNORECASE)  # Windows paths may differ in case
    return text


def diagnose(store, since=0.0, crash=None, has_update=None):
    """All problems, worst first. `crash` is watch.crash_evidence() output when the game just crashed."""
    has_update = has_update or (lambda m: False)
    game = store.game()
    idx = owner_index(store)
    probs = missing_frameworks(store) + missing_requirements(store) + game_updated(store, has_update)
    blamed = set()
    if crash and crash.get("module") and game:
        mid = owners_in(crash["module"], idx, game)
        if mid:
            m = store.mod(mid)
            blamed.add(mid)
            why = CODES.get(crash.get("code"))
            where = (crash.get("places") or {}).get(crash["module"])
            probs.append(problem(f"crash-{mid}", f"The game crashed inside {m['name']}",
                                 crash["module"] + (f"\nWhere: {where}" if where else "") + (f"\n{why}" if why else ""),
                                 mod_fixes(m, has_update)))
    if crash and game:
        probs += call_path(crash, idx, game, blamed, store, has_update)
        if crash.get("hang"):
            probs += freeze_problems(crash["hang"], idx, game, blamed, store, has_update)
        rep = crash.get("crashed") and acr_report(game, since - 15 if since else 0)
        if rep:
            mid = owners_in(rep[1].split("INSTALLED MODS")[0], idx, game)  # the full mod list comes after the verdict
            m = store.mod(mid) if mid else None
            detail = f"Report: {rep[0]}\n\n{rep[1][:1500]}"
            if m and mid not in blamed:
                blamed.add(mid)
                probs.append(problem(f"acr-{mid}", f"Advanced Crash Reporter names {m['name']}", detail, mod_fixes(m, has_update),
                                     auto=False))
            elif not m and f"acr-{rep[0].name}" not in store.doctor.get("ignored", []):
                pid = f"acr-{rep[0].name}"
                probs.append(problem(pid, "Advanced Crash Reporter wrote a report", detail, [fix("Hide this report", "ignore", pid)],
                                     severity="info", auto=False))
    if game:
        # `since` is when polling saw the game (up to one poll late): look a little further back for early log lines
        for mid, hit in log_errors(game, since - 15 if since else 0, idx).items():
            m = store.mod(mid)
            if mid in blamed or not m:
                continue
            probs.append(problem(f"log-{mid}", f"{m['name']} reported errors", "\n".join(hit["lines"]), mod_fixes(m, has_update),
                                 severity="high" if hit["strong"] else "medium", auto=hit["strong"]))
        for mid, secs in slow_mods(game, since - 15 if since else 0, idx).items():
            m = store.mod(mid)
            if m:
                probs.append(problem(f"slow-{mid}", f"{m['name']} took {secs:.0f} seconds to load",
                                     "Seen in RED4ext's log. A slow DLL mod makes every game start slow, and can make the game "
                                     "freeze at startup.", mod_fixes(m, has_update), severity="medium", auto=False))
    if crash and crash.get("crashed"):
        probs += recent_hand_installed(store)
    if crash and crash.get("crashed") and not any(p["severity"] == "high" for p in probs):
        probs.append(changed_since_good(store))
    return probs + archive_conflicts(store)


def bisect_start(store):
    suspects = [m["id"] for m in store.enabled() if not m.get("framework")]
    return {"suspects": suspects, "testing": suspects[: (len(suspects) + 1) // 2], "restore": [m["id"] for m in store.enabled()]}


def bisect_step(state, crashed):
    """Next state after one launch. Only `testing` suspects are on; a crash means the bad mod is among them.
    A mod found only by elimination (no crash seen during the search) is confirmed by one more launch with
    every mod on; if that launch does not crash either, there is no single bad mod (culprit None)."""
    crashes = state.get("crashes", 0) + bool(crashed)
    if state.get("verifying"):
        return {**state, "crashes": crashes, "culprit": state["suspects"][0] if crashed else None}
    s = state["testing"] if crashed else [x for x in state["suspects"] if x not in state["testing"]]
    if len(s) <= 1:
        if s and not crashes:
            return {**state, "suspects": s, "testing": s, "crashes": 0, "verifying": True}
        return {**state, "suspects": s, "testing": s, "crashes": crashes, "culprit": s[0] if s else None}
    return {**state, "suspects": s, "testing": s[: (len(s) + 1) // 2], "crashes": crashes}
