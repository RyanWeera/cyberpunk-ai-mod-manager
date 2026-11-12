"""Find why the game crashed (or will), and list every way to fix each problem.

A problem is {"id", "title", "detail", "severity", "auto", "fixes": [{"label", "action", "recommended"}]}.
`action` is a list the controller runs, e.g. ["disable", mod_id]. A problem with exactly one fix and
auto=True is fixed without asking; any other problem is shown to the user to pick a fix.
"""
import bisect
import heapq
import re
import struct
from collections import Counter
from itertools import combinations

from downloads.fetch import FRAMEWORKS, FW_BY_NEXUS

LEVEL_OK = re.compile(r"(?i)\[(info|debug|trace)\b")
ERR = re.compile(r"(?i)\b(error|critical|fatal|exception|failed)\b")
PATH = re.compile(r"(?:^|[\s'\"(\[=])((?:archive|bin|engine|r6|red4ext|mods)/[^:'\"()<>|*?,\r\n]+)")
ARCH = re.compile(r"([^\s/:'\"]+\.archive)\b")
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
    """(sizes, pairs): how many archive resources each enabled mod changes, and how many resources each
    pair of mods (lower, higher in the list) both change."""
    owners, sizes = {}, {}
    for m in store.enabled():
        res = set().union(*(archive_hashes(store.staging / m["id"] / r) for r in m["files"] if r.lower().endswith(".archive")))
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


def diagnose(store, since=0.0, crash=None, has_update=None):
    """All problems, worst first. `crash` is watch.crash_evidence() output when the game just crashed."""
    has_update = has_update or (lambda m: False)
    game = store.game()
    idx = owner_index(store)
    probs = missing_frameworks(store) + missing_requirements(store)
    blamed = set()
    if crash and crash.get("module") and game:
        mid = owners_in(crash["module"], idx, game)
        if mid:
            m = store.mod(mid)
            blamed.add(mid)
            probs.append(problem(f"crash-{mid}", f"The game crashed inside {m['name']}", crash["module"], mod_fixes(m, has_update)))
    if game:
        # `since` is when polling saw the game (up to one poll late): look a little further back for early log lines
        for mid, hit in log_errors(game, since - 15 if since else 0, idx).items():
            m = store.mod(mid)
            if mid in blamed or not m:
                continue
            probs.append(problem(f"log-{mid}", f"{m['name']} reported errors", "\n".join(hit["lines"]), mod_fixes(m, has_update),
                                 severity="high" if hit["strong"] else "medium", auto=hit["strong"]))
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
