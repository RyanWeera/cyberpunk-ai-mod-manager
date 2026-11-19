"""Everything the window does, without Tk: links -> jobs -> download -> virus scan -> install -> deploy,
plus game launch, crash handling, fixes and undo. Worker threads report through notify(kind, payload)."""
import errno
import itertools
import os
import queue
import re
import shutil
import subprocess
import sys
import tempfile
import threading
import time
import webbrowser
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

from crash_doctor import doctor, watch
from downloads import nexus
from downloads.fetch import FRAMEWORKS, FW_BY_NEXUS, FW_BY_REPO, SYS32, app_latest, download, github_latest
from installer import deploy, install
from local_ai import llm
from shared.store import GAME_EXE, VERSION
from video_finder import finder
from virus_scan import scan

# Swaps in the new exe once the old one has quit (a running exe is locked), then starts it. If the swap never works,
# the old exe starts again.
SWAP = r"""for ($i = 0; $i -lt 240; $i++) {
  try { Move-Item -LiteralPath $env:CP_NEW -Destination $env:CP_EXE -Force -ErrorAction Stop; break } catch { Start-Sleep -Milliseconds 500 } }
Start-Process -FilePath $env:CP_EXE"""

# a compatibility patch's name; NOT_COMPAT: names that only speak of a game patch or say "compatible with" an item
COMPAT = re.compile(r"patch|compat|integration|bridge", re.IGNORECASE)
NOT_COMPAT = re.compile(r"deleted|obsolete|deprecated|incompatible|not compatible|patch \d|\d compatible", re.IGNORECASE)
CORE = ["RED4ext", "Cyber Engine Tweaks", "redscript", "ArchiveXL", "TweakXL", "Codeware"]
GAME_ON = llm.BLOCKED


class Controller:
    def __init__(self, store, notify, workers=3, watch_game=True):
        self.store, self.notify = store, notify
        self.jobs, self.lock, self.work, self.ids = [], threading.RLock(), queue.Queue(), itertools.count(1)
        self.premium, self.clicking, self.stop = False, None, threading.Event()
        self.game_on = False  # the local AI does not run while the game runs: the game gets the whole PC
        for old in store.downloads.glob("job-*"):  # left by jobs that the app's close cut off (only one window runs)
            shutil.rmtree(old, ignore_errors=True)
        for _ in range(workers):
            threading.Thread(target=self._worker, daemon=True).start()
        if watch_game:
            threading.Thread(target=watch.watch, args=(self._game_started, self._game_exited, self.stop),
                             kwargs={"hang_dir": store.root / "freezes"}, daemon=True).start()
        if self.key:
            self.bg(self.check_key)

    @property
    def key(self):
        return self.store.settings["api_key"].strip()

    def bg(self, fn, *args):
        def run():
            try:
                fn(*args)
            except Exception as e:  # noqa: BLE001 - background boundary: show the error, keep the app alive
                self.notify("log", f"Error: {e}")
        threading.Thread(target=run, daemon=True).start()

    def check_key(self):
        try:
            u = nexus.validate(self.key)
            self.premium = bool(u.get("is_premium"))
            kind = "Premium: downloads need no clicks" if self.premium else "Free: one click per Nexus file, the rest is automatic"
            self.notify("key", f"Key OK for {u.get('name')}. {kind}.")
        except Exception as e:  # noqa: BLE001 - shown to the user
            self.premium = False
            return self.notify("key", f"Key check failed: {e}")
        with self.lock:  # Nexus jobs that waited for the key (or for a click a Premium account does not need)
            js = [j for j in self.jobs if j["state"] == "needkey" or (self.premium and j["state"] == "waiting")]
            for j in js:
                j["state"] = "queued"
        for j in js:
            self._set(j, "Queued")
            self.work.put(j)

    # ---------- links and jobs ----------
    def add_links(self, text):
        self.bg(self._expand, [ln for ln in text.splitlines() if ln.strip()])

    def add_link(self, link):
        """One link that is parsed already, such as the nxm link the user said yes to."""
        self.bg(self._add, link)

    def _expand(self, lines):
        for line in lines:
            try:
                link = nexus.parse_link(line)
                if link:
                    self._add(link)
            except Exception as e:  # noqa: BLE001 - one bad link must not stop the others
                self.notify("log", f"{line.strip()}: {e}")

    def _have(self, pred):
        with self.lock:
            return any(pred(m["source"]) for m in self.store.mods) or any(pred(j["src"]) for j in self.jobs if j["state"] != "failed")

    def _job(self, name, src, **meta):
        with self.lock:
            j = {"id": next(self.ids), "name": name, "src": src, "state": "queued", "status": "Queued", "progress": "", **meta}
            self.jobs.append(j)
        self.notify("job", j)
        self.work.put(j)
        return j

    def _set(self, j, status, progress=""):
        j["status"], j["progress"] = status, progress
        self.notify("job", j)

    def _add(self, link):
        t = link["type"]
        if t == "collection":
            c = nexus.collection(link["slug"], link.get("revision"), self.key)
            ask = lambda f: (f["optional"] or f.get("outdated")) and f["mod_id"] not in FW_BY_NEXUS
            if "chosen" not in link and any(map(ask, c["files"])):  # the window asks which ones to download, then adds it again
                return self.notify("pack_choice", {"link": link, "name": c["name"].strip(), "files": [f for f in c["files"] if ask(f)]})
            files = [f for f in c["files"] if not ask(f) or f["file_id"] in link.get("chosen", ())]
            self.notify("log", f"Collection '{c['name']}' revision {c['revision']}: {len(files)} files")
            with self.store.lock:  # the collection is also a mod pack (a mod's id is known before its download)
                self.store.packs[c["name"].strip()] = [[f"nexus-{f['mod_id']}-{f['file_id']}", True] for f in files
                                                       if f["mod_id"] not in FW_BY_NEXUS]
                self.store.pack_info[c["name"].strip()] = {
                    "url": f"https://www.nexusmods.com/games/{nexus.GAME}/collections/{link['slug']}",
                    "names": {f"nexus-{f['mod_id']}-{f['file_id']}": f["name"] for f in files}}
                self.store.save()
            self.notify("mods", None)
            for f in files:
                self._add({"type": "nexus", **f})
        elif t == "nexus":
            mod_id, file_id = link["mod_id"], link.get("file_id")
            if mod_id in FW_BY_NEXUS:
                return self.add_framework(FW_BY_NEXUS[mod_id])
            if self._have(lambda s: s.get("type") == "nexus" and s["mod_id"] == mod_id and file_id in (None, s["file_id"])):
                return
            src = {"type": "nexus", "mod_id": mod_id, "file_id": file_id, "key": link.get("key"), "expires": link.get("expires")}
            if file_id and link.get("name"):  # collection entry: already known, no lookup needed
                return self._job(link["name"], src, version=link.get("version", ""))
            info = nexus.mod_info(mod_id, self.key)
            if info.get("outdated") and not link.get("anyway"):  # the window asks: Download (adds it again with "anyway") or Skip
                return self.notify("outdated", {"link": link, "name": info["name"], "why": info.get("outdated")})
            f = nexus.pick_file(info["files"], file_id)
            if not f:
                raise RuntimeError(f"Nexus mod {mod_id} has no file to download")
            src["file_id"] = int(f["fileId"])
            self._job(info["name"], src, version=f["version"], requires=info["requires"])
            for r in info["requires"]:
                try:
                    self._add({"type": "nexus", "mod_id": r["mod_id"]})
                except Exception as e:  # noqa: BLE001 - a broken requirement must not cancel the mod
                    self.notify("log", f"Requirement {r['name']}: {e}")
            try:
                self._compat(mod_id, info["name"])
            except Exception as e:  # noqa: BLE001 - network boundary: the mod itself is added already
                self.notify("log", f"Compatibility check for {info['name']}: {e}")
        elif t == "github":
            fw = FW_BY_REPO.get(link["repo"].lower())
            if fw:
                return self.add_framework(fw)
            if not self._have(lambda s: s.get("repo") == link["repo"]):
                self._job(link["repo"].split("/")[1], {"type": "github", "repo": link["repo"]})
        else:
            self._job(Path(link.get("path") or link["url"].split("?")[0]).stem, link)

    def _compat(self, mod_id, name):
        """Add the compatibility patches for mod_id and the user's other mods. A patch is a Nexus mod that requires
        mod_id, has a patch-like name, needs or names another of the user's mods (not a framework), and needs no mod
        the user does not have. Collections bring their own patches, so their files skip this."""
        with self.lock:
            have = {s["mod_id"]: n for s, n in [(m["source"], m["name"]) for m in self.store.mods] +
                    [(j["src"], j["name"]) for j in self.jobs if j["state"] != "failed"] if s.get("type") == "nexus"}
        others = {i: n for i, n in have.items() if i != mod_id and i not in FW_BY_NEXUS}
        if not others:
            return
        found = [d for d in nexus.mods_requiring(mod_id) if d["mod_id"] not in have and COMPAT.search(d["name"])
                 and not NOT_COMPAT.search(d["name"])]
        key = lambda n: re.sub(r"[^a-z0-9]", "", n.lower())

        def partner(d):
            try:
                reqs = {r["mod_id"] for r in nexus.mod_info(d["mod_id"], self.key)["requires"]} - set(FW_BY_NEXUS)
            except Exception:  # noqa: BLE001 - a patch page that fails to load is skipped
                return None
            if not reqs <= set(have):
                return None  # it needs a mod that is not in this list: it is not for this list
            return next((n for i, n in others.items() if i in reqs or len(key(n)) >= 6 and key(n) in key(d["name"])), None)

        with ThreadPoolExecutor(6) as pool:  # ponytail: 24 candidates cap, enough for all but the biggest hub mods
            for d, other in zip(found[:24], pool.map(partner, found[:24])):
                if other:
                    self.notify("log", f"Adding the compatibility patch '{d['name']}' for {name} and {other}.")
                    self._add({"type": "nexus", "mod_id": d["mod_id"]})

    def add_framework(self, fw):
        with self.lock:
            have = next((m for m in self.store.mods if m.get("framework") == fw), None)
            if have and not have["enabled"]:
                have["enabled"] = True
                self.store.save()
                self.notify("mods", None)
            if have or any(j.get("framework") == fw and j["state"] != "failed" for j in self.jobs):
                return
        self._job(fw, {"type": "github", "repo": FRAMEWORKS[fw][0]}, framework=fw)

    def handle_nxm(self, url):
        """An nxm:// link from the browser. A link that a waiting download asked for continues that download.
        Any other link may come from any web page, so the user is asked before anything is installed. The question
        carries the parsed link, never the text: on "yes" only what the question names is queued."""
        link = nexus.parse_link(url)
        if link["type"] != "nexus":
            name = link["slug"]
            try:
                name = nexus.collection_info(link["slug"])["name"]
            except Exception:  # noqa: BLE001 - offline or gone: ask with the slug instead
                pass
            return self.notify("confirm", {"link": link, "text": f"Download and install the Nexus collection '{name}'?"})
        with self.lock:
            j = next((j for j in self.jobs if j["state"] == "waiting" and j["src"]["mod_id"] == link["mod_id"]
                      and j["src"]["file_id"] == link["file_id"]), None)
            if j:
                j["src"].update(key=link["key"], expires=link["expires"])
                j["state"] = "queued"
        if j:
            self._set(j, "Queued")
            self.work.put(j)
            self._next_click()
            return
        name = f"Nexus mod {link['mod_id']}"
        try:
            name = nexus.mod_info(link["mod_id"], self.key)["name"]
        except Exception:  # noqa: BLE001 - offline or unknown: ask with the number instead
            pass
        self.notify("confirm", {"link": link, "text": f"Download and install '{name}' (file {link['file_id']}) from Nexus?"})

    def _next_click(self):
        """Free accounts: open the Nexus page of the next waiting file, one at a time."""
        with self.lock:
            if self.clicking and self.clicking["state"] == "waiting":
                return
            self.clicking = j = next((j for j in self.jobs if j["state"] == "waiting"), None)
        if j:
            self._set(j, "Click 'Slow download' in your browser")
            self.reopen(j)

    def reopen(self, j):
        webbrowser.open(nexus.files_page(j["src"]["mod_id"], j["src"]["file_id"]))

    def skip(self, j):
        """Only a download that has not started can be skipped."""
        with self.lock:
            if j["state"] not in ("queued", "waiting", "needkey"):
                return False
            j["state"] = "failed"
        self._set(j, "Skipped")
        self._next_click()
        return True

    def retry(self, j):
        with self.lock:
            if j["state"] != "failed":
                return False
            j["state"] = "queued"
        self._set(j, "Queued")
        self.work.put(j)
        return True

    def _worker(self):
        while True:
            j = self.work.get()
            with self.lock:  # a job skipped while it sat in the queue is dropped here
                start = j["state"] == "queued"
                if start:
                    j["state"] = "running"
            if not start:
                self.work.task_done()
                continue
            try:
                self._run(j)
            except Exception as e:  # noqa: BLE001 - job boundary: mark failed, keep the worker alive
                j["state"] = "failed"
                virus = getattr(e, "winerror", None) in (225, 226)  # real-time protection stopped or removed the file
                refused = isinstance(e, OSError) and e.errno == errno.EINVAL and e.filename  # Python's open() shows a block so
                self._set(j, "Blocked: virus found (Microsoft Defender stopped the file)" if virus else
                          "Blocked: Windows refused to read the file. An antivirus may have stopped it: see Windows Security > "
                          "Protection history" if refused else f"Failed: {e}")
            self._after_job()
            self.work.task_done()

    def _run(self, j):
        s = j["src"]

        def prog(done, total):
            self._set(j, "Downloading", f"{done * 100 // total}%" if total else f"{done >> 20} MB")

        if s["type"] == "nexus":
            if not self.key:
                j["state"] = "needkey"
                return self._set(j, "Needs your Nexus API key (Settings). I continue when you add it.")
            if not (self.premium or s.get("key")):
                if "github" not in s:  # looked up once: a retry or a later click does not ask GitHub again
                    self._set(j, "Looking for the author's GitHub release")
                    try:
                        s["github"] = nexus.github_copy(s["mod_id"], j.get("version", ""))
                    except Exception as e:  # noqa: BLE001 - network boundary: the Nexus click still works
                        s["github"] = None
                        self.notify("log", f"{j['name']}: GitHub check failed ({e})")
                if not s["github"]:
                    j["state"] = "waiting"
                    return self._set(j, "Waiting for your click on Nexus")
        # One new folder per job: no two jobs share a file, and nothing here is ever the user's own file.
        work = Path(tempfile.mkdtemp(prefix="job-", dir=self.store.downloads))
        try:
            if s["type"] == "nexus" and s.get("github") and not (self.premium or s.get("key")):
                self.notify("log", f"{j['name']}: same version on the author's GitHub ({s['github']['repo']}), so no click is needed.")
                path = download(s["github"]["url"], work, prog)
            elif s["type"] == "nexus":
                self._set(j, "Getting download link")
                path = download(nexus.download_url(self.key, s["mod_id"], s["file_id"], s.get("key"), s.get("expires")),
                                work, prog)
            elif s["type"] == "github":
                rel = github_latest(s["repo"])
                j["version"] = rel["tag"]
                path = download(rel["url"], work, prog)
            elif s["type"] == "url":
                path = download(s["url"], work, prog)
            else:  # a file from the PC is only read. Defender gets a copy under a fixed plain name (its command line).
                path = Path(s["path"])
                if self.store.settings["virus_scan"]:
                    self._set(j, "Copying")
                    suffix = path.suffix.lower()
                    path = work / ("archive" + (suffix if re.fullmatch(r"\.[a-z0-9]{1,5}", suffix) else ""))
                    shutil.copyfile(s["path"], path)
            if self.store.settings["virus_scan"]:
                self._set(j, "Scanning for viruses")
                clean, detail = scan.scan(path)
                if clean is False:
                    j["state"] = "failed"
                    return self._set(j, f"Blocked: virus found ({detail})")
                if clean is None:
                    self.notify("log", f"{j['name']}: virus scan skipped ({detail})")
            self._set(j, "Installing")
            self.install_job(j, path)
        finally:
            shutil.rmtree(work, ignore_errors=True)
        j["state"] = "done"
        self._set(j, "Done")

    def install_job(self, j, path):
        s = j["src"]
        mid = j.get("replace") or (f"nexus-{s['mod_id']}-{s['file_id']}" if s["type"] == "nexus" else
                                   f"gh-{install.safe(s['repo'].replace('/', '-'))}" if s["type"] == "github" else
                                   f"{s['type']}-{install.safe(j['name'])}")
        new = self.store.staging / f"{mid}.new"  # unpack beside the old copy, swap under the lock (a deploy may be reading it)
        rels = install.install(path, new, j["name"])
        fw = j.get("framework") or FW_BY_REPO.get(s.get("repo", "").lower()) or FW_BY_NEXUS.get(s.get("mod_id"))
        rec = {"id": mid, "name": j["name"], "version": j.get("version", ""), "enabled": True,
               "source": {k: v for k, v in s.items() if k not in ("key", "expires", "github")}, "files": rels,
               "needs": sorted(install.needs_for(rels) - {fw}), "requires": j.get("requires", []), "framework": fw,
               "installed": time.time()}
        with self.store.lock:
            shutil.rmtree(self.store.staging / mid, ignore_errors=True)
            new.replace(self.store.staging / mid)
            old = self.store.mod(mid)
            if old:
                rec["enabled"], rec["requires"] = old["enabled"], rec["requires"] or old.get("requires", [])
                self.store.mods[self.store.mods.index(old)] = rec
                for k, v in self.store.deployed.items():  # updated files: copy them into the game again
                    if v == mid:
                        self.store.deployed[k] = None
            elif fw:
                self.store.mods.insert(0, rec)  # frameworks first: other mods should win any overlap
            else:
                self.store.mods.append(rec)
            self.store.save()
        for need in rec["needs"]:  # automatic setup: fetch every framework the new files need
            self.add_framework(need)
        self.notify("mods", None)

    def _after_job(self):
        with self.lock:
            busy = any(j["state"] in ("queued", "running") for j in self.jobs)
        self._next_click()  # open the next Nexus page before the slower deploy
        if not busy and self.store.game():
            self.redeploy(sync=True, quiet=True)

    # ---------- mod list ----------
    def redeploy(self, sync=False, quiet=False):
        def run():
            with self.store.lock:
                if self.stop.is_set():  # the window is closing: start no new copy
                    return
                n = deploy.deploy(self.store)
            if n or not quiet:
                self.notify("log", f"Mods are set up in the game folder ({n} file change(s))." if n else
                                   "Everything is already set up in the game folder.")
        self.bg(run) if not sync else self._try(run)

    def _try(self, fn):
        try:
            fn()
        except Exception as e:  # noqa: BLE001 - shown to the user
            self.notify("log", f"Error: {e}")

    def set_enabled(self, ids, on):
        with self.store.lock:
            for m in self.store.mods:
                if m["id"] in ids:
                    m["enabled"] = on
            self.store.save()
        self.notify("mods", None)

    def move(self, mid, delta):
        with self.store.lock:
            mods = self.store.mods
            i = next(i for i, m in enumerate(mods) if m["id"] == mid)
            k = max(0, min(len(mods) - 1, i + delta))
            mods.insert(k, mods.pop(i))
            self.store.save()
        self.notify("mods", None)

    # ---------- mod packs ----------
    def save_pack(self, name):
        """Keep the mod list as it is now (which mods are on, and their order) as the mod pack `name`."""
        name = name.strip()
        if not name:
            raise ValueError("Give the mod pack a name.")
        with self.store.lock:
            self.store.packs[name] = [[m["id"], m["enabled"]] for m in self.store.mods]
            self.store.pack = name
            self.store.save()
        self.notify("mods", None)

    def use_pack(self, name):
        """Turn on the pack's mods in the pack's order and turn off the rest. A framework the pack does not
        list stays on: the pack's mods need it."""
        with self.store.lock:
            pack = self.store.packs[name]
            pos, on = {mid: n for n, (mid, _) in enumerate(pack)}, {mid for mid, e in pack if e}
            self.store.mods.sort(key=lambda m: pos.get(m["id"], -1))  # mods outside the pack go first (lowest)
            for m in self.store.mods:
                m["enabled"] = m["id"] in on if m["id"] in pos else bool(m.get("framework")) and m["enabled"]
            self.store.pack = name
            self.store.save()
            missing = len(pos.keys() - {m["id"] for m in self.store.mods})
        self.notify("mods", None)
        msg = f"Mod pack '{name}': {len(self.store.enabled())} mod(s) on" + (f", {missing} not installed yet." if missing else ".")
        self.notify("log", msg)
        return msg

    def complete_pack(self, name):
        """Download the pack's Nexus mods that are not installed yet. Returns how many it queued. Every link gets a
        name, so each mod downloads straight away with no per-mod lookup (a merged pack can hold thousands)."""
        with self.store.lock:
            have = {m["id"] for m in self.store.mods}
            names = {k: v for info in self.store.pack_info.values() for k, v in info.get("names", {}).items()}
            links = [{"type": "nexus", "mod_id": int(m[1]), "file_id": int(m[2]), "name": names.get(mid)}
                     for mid, _ in self.store.packs.get(name, []) if mid not in have
                     for m in [re.fullmatch(r"nexus-(\d+)-(\d+)", mid)] if m]

        def run():
            unnamed = sorted({link["mod_id"] for link in links if not link["name"]})
            found = {}
            for i in range(0, len(unnamed), 100):
                try:
                    found.update(nexus.mod_names(unnamed[i:i + 100]))
                except Exception as e:  # noqa: BLE001 - network boundary: the mod id stands in for the name
                    self.notify("log", f"Mod names: {e}")
            for link in links:
                link["name"] = link["name"] or found.get(link["mod_id"], {}).get("name") or f"Nexus mod {link['mod_id']}"
            for link in links:
                try:
                    self._add(link)
                except Exception as e:  # noqa: BLE001 - one bad mod must not stop the others
                    self.notify("log", f"{link['name'] or link['mod_id']}: {e}")
        self.bg(run)
        return len(links)

    # ---------- app update ----------
    def update_app(self, rel=None):
        """Get this app's newest version from GitHub. With no rel it only looks: the window asks, then calls this again
        with the release. The exe is downloaded and must pass a Defender scan; a script swaps it in once the window has
        closed (Windows cannot overwrite a running exe) and starts it. From source: git pull. Every outcome, errors
        too, is an "update" event, so the window can turn its button back on."""
        try:
            if not getattr(sys, "frozen", False):
                r = subprocess.run(["git", "pull", "--ff-only"], cwd=Path(__file__).resolve().parents[1], capture_output=True,
                                   text=True, errors="replace", env={**os.environ, "LC_ALL": "C"},  # English: read below
                                   creationflags=scan.NO_WINDOW, check=False)
                if r.returncode:
                    raise RuntimeError(f"git pull failed: {(r.stderr or r.stdout).strip()}")
                return self.notify("update", {"text": "You have the newest version." if "Already up to date" in r.stdout else
                                              "Updated from GitHub. Close and open the app to use the new version."})
            if rel is None:
                rel = app_latest()
                return self.notify("update", {"ask": rel} if rel else {"text": f"You have the newest version ({VERSION})."})
            self.notify("update", {"text": f"Downloading version {rel['tag']}…", "busy": True})
            folder = self.store.root / "update"
            folder.mkdir(exist_ok=True)
            new = download(rel["url"], folder)
            ok, why = scan.scan(new)
            if ok is not True:  # a file Defender could not scan does not run either
                new.unlink(missing_ok=True)
                raise RuntimeError(f"Microsoft Defender found {why}" if ok is False else
                                   f"Microsoft Defender could not scan the new version ({why})")
            # PYINSTALLER_RESET_ENVIRONMENT: else the new exe takes the old one's temp folder, which is deleted on exit
            subprocess.Popen([str(SYS32 / "WindowsPowerShell/v1.0/powershell.exe"), "-NoProfile", "-NonInteractive", "-Command", SWAP],
                             env={**os.environ, "CP_NEW": str(new), "CP_EXE": sys.executable, "PYINSTALLER_RESET_ENVIRONMENT": "1"},
                             creationflags=scan.NO_WINDOW | subprocess.CREATE_NEW_PROCESS_GROUP)  # DETACHED_PROCESS: PowerShell runs nothing
            self.notify("update", {"text": f"Installing version {rel['tag']}. The app restarts by itself.", "restart": True})
        except Exception as e:  # noqa: BLE001 - shown to the user
            self.notify("update", {"text": f"Update failed: {e}"})

    def merge_packs(self, names, new):
        """One pack with every mod of `names`. A mod in several packs is on if one of them turns it on, and takes
        its place from the last pack, so a later pack wins when two mods change the same file."""
        new = new.strip()
        if len(names) < 2 or not new:
            raise ValueError("Pick two or more mod packs and give the new pack a name.")
        with self.store.lock:
            order, on = {}, set()
            for n in names:
                for mid, e in self.store.packs[n]:
                    order.pop(mid, None)
                    order[mid] = None
                    if e:
                        on.add(mid)
            self.store.packs[new] = [[mid, mid in on] for mid in order]
            self.store.save()
        self.notify("mods", None)
        return len(order)

    def delete_pack(self, name):
        with self.store.lock:
            self.store.packs.pop(name, None)
            self.store.pack_info.pop(name, None)
            if self.store.pack == name:
                self.store.pack = ""
            self.store.save()
        self.notify("mods", None)

    def remove(self, mid):
        with self.store.lock:
            self.store.mods = [m for m in self.store.mods if m["id"] != mid]
            self.store.save()
            shutil.rmtree(self.store.staging / mid, ignore_errors=True)
        self.notify("mods", None)

    def update_mod(self, mid):
        m = self.store.mod(mid)
        s = m["source"]
        if s["type"] == "github":
            self._job(m["name"], dict(s), replace=mid, framework=m.get("framework"))
        elif s["type"] == "nexus":
            info = nexus.mod_info(s["mod_id"], self.key)
            f = nexus.newer_file(info["files"], s["file_id"])
            if not f:
                return self.notify("log", f"{m['name']} is already the newest version.")
            self._job(m["name"], {"type": "nexus", "mod_id": s["mod_id"], "file_id": int(f["fileId"])}, replace=mid,
                      version=f["version"], requires=info["requires"])
        else:
            self.notify("log", f"{m['name']} came from a file or plain link. Add the new file to update it.")

    def has_update(self, m):
        s = m["source"]
        try:
            if s["type"] == "nexus":
                return bool(nexus.newer_file(nexus.mod_info(s["mod_id"], self.key)["files"], s["file_id"]))
            if s["type"] == "github":
                return github_latest(s["repo"])["tag"] != m.get("version")
        except Exception:  # noqa: BLE001 - offline or API down: offer no update
            return False
        return False

    # ---------- game and crashes ----------
    def launch(self, pack=None):
        """Start the game with the mods that are on, or first set the mod list to the mod pack `pack`."""
        game = self.store.game()
        if not game:
            raise RuntimeError("Set the Cyberpunk 2077 folder in Settings first.")
        for p in doctor.game_updated(self.store, self.has_update):  # warn only: the game may run fine
            self.notify("log", f"Warning: {p['title']}. Click 'Check my mods now' for framework updates.")
        with self.store.lock:
            if self.stop.is_set():  # the window is closing
                return
            if pack:
                self.use_pack(pack)
            deploy.deploy(self.store)
        redmod = any(r.lower().startswith("mods/") for m in self.store.enabled() for r in m["files"])
        exe, args, cwd = str(game / GAME_EXE), ["-modded"] if redmod else [], str(game / "bin/x64")
        llm.block()  # free the local AI's memory before the game loads, and keep it off while the game runs
        try:
            try:
                subprocess.Popen([exe] + args, cwd=cwd)
            except OSError as e:
                if getattr(e, "winerror", None) != 740 or GAME_EXE.lower() in self.store.deployed:
                    raise  # never elevate an exe that a mod put there
                os.startfile(exe, arguments=" ".join(args), cwd=cwd)  # "Run as administrator" is set: Windows asks the user first
        except OSError:
            if not (self.game_on or self.stop.is_set()):  # no game started, and the window is not closing
                llm.block(False)
            raise
        # The game watch sees the game within seconds; a game that never showed up must not keep the AI off.
        t = threading.Timer(120, lambda: self.game_on or self.stop.is_set() or llm.block(False))
        t.daemon = True
        t.start()

    def _game_started(self, t):
        self.game_on = True  # also when the game starts from Steam or GOG
        llm.block()
        self.notify("game", True)
        with self.store.lock:
            self.store.doctor["last_launch"] = t
            try:
                self.store.save()
            except OSError as e:  # an error here would make the game watch skip this game's exit: the AI would stay off
                self.notify("log", f"Could not save the launch time: {e}")

    def _game_exited(self, started, exit_code=None, freeze=None):
        self.game_on = False
        if not self.stop.is_set():  # the window is not closing
            llm.block(False)
        self.notify("game", False)
        ev = watch.crash_evidence(started, exit_code)
        if freeze:  # the game stopped responding and never came back: the user closed it, or the game's own timer did
            ev.update(crashed=True, hang=watch.read_dump(freeze))
            self.notify("log", f"The game froze. I saved a snapshot of its threads: {freeze}")
        with self.store.lock:
            self.store.doctor["last_exit"] = ev  # for the crash report, also after the app restarts
            try:
                self.store.save()
            except OSError as e:
                self.notify("log", f"Could not save the crash details: {e}")
        if self.store.doctor.get("bisect"):
            return self._bisect_result(ev["crashed"])
        if not ev["crashed"]:
            with self.store.lock:
                self.store.doctor["last_good"] = {m["id"]: m.get("version", "") for m in self.store.enabled()}
                if game := self.store.game():
                    self.store.doctor["last_good_game"] = doctor.game_version(game)
                self.store.save()
            return self.notify("log", "The game closed normally. This mod setup is saved as the last good one.")
        self.notify("log", "The game crashed. Looking for the cause...")
        self.handle_problems(doctor.diagnose(self.store, started, ev, self.has_update), crashed=True)

    def crash_report(self):
        """Text of a report about the last game exit and the current mods, to share with a mod author."""
        since, ev = self.store.doctor.get("last_launch", 0), self.store.doctor.get("last_exit")
        return doctor.report(self.store, doctor.diagnose(self.store, since, ev, self.has_update), ev)

    def check_now(self):
        since = self.store.doctor.get("last_launch", 0)
        probs = doctor.diagnose(self.store, since, None, self.has_update)
        self.handle_problems(probs)
        return probs

    def handle_problems(self, probs, crashed=False):
        ask = []
        for p in probs:
            if crashed and self.store.settings["auto_fix"] and p["auto"] and p["severity"] == "high" and len(p["fixes"]) == 1:
                self.apply(p, p["fixes"][0])
                self.notify("log", f"Fixed automatically: {p['title']} -> {p['fixes'][0]['label']}")
            else:
                ask.append(p)
        self.notify("problems", {"list": ask, "crashed": crashed})

    def apply(self, p, f):
        verb, *args = f["action"]
        st, undo = self.store, None
        with st.lock:
            if verb in ("disable", "enable", "disable_many"):
                ids = args[0] if verb == "disable_many" else [args[0]]
                undo = ["states", {m["id"]: m["enabled"] for m in st.mods if m["id"] in ids}]
                self.set_enabled(ids, verb == "enable")
            elif verb == "order":  # args[0]: mod ids in the new order
                undo = ["order", [m["id"] for m in st.mods]]
                self._order(args[0])
            elif verb == "prefer":  # let args[0] win over args[1]: move it just below, and keep the choice for Auto-sort
                pid = f"conflict-{args[1]}-{args[0]}"
                undo = ["prefer", [[m["id"] for m in st.mods], pid]]
                lo = st.mod(args[0])
                st.mods.remove(lo)
                st.mods.insert(st.mods.index(st.mod(args[1])) + 1, lo)
                st.doctor.setdefault("ignored", []).append(pid)
            elif verb == "ignore":
                st.doctor.setdefault("ignored", []).append(args[0])
                undo = ["unignore", args[0]]
            elif verb == "update":
                self.bg(self.update_mod, args[0])
            elif verb == "install_fw":
                self.bg(self.add_framework, args[0])
            elif verb == "queue_nexus":
                self.bg(self._add, {"type": "nexus", "mod_id": args[0]})
            elif verb == "bisect_start":
                self.bisect_begin()
            st.doctor.setdefault("history", []).append({"time": time.time(), "problem": p["title"], "fix": f["label"], "undo": undo})
            st.save()
        self.notify("mods", None)
        if st.game():
            self.redeploy()

    def undo(self, i):
        st = self.store
        with st.lock:
            h = st.doctor["history"].pop(i)
            kind, arg = h["undo"] or (None, None)
            if kind == "states":
                for m in st.mods:
                    m["enabled"] = arg.get(m["id"], m["enabled"])
            elif kind == "order":
                self._order(arg)
            elif kind == "unignore":
                st.doctor["ignored"].remove(arg)
            elif kind == "prefer":
                self._order(arg[0])
                st.doctor["ignored"].remove(arg[1])
            st.save()
        self.notify("mods", None)
        if st.game():
            self.redeploy()

    def _order(self, ids):
        """Sort the mod list by `ids`; a mod not in `ids` (added since) goes last, in its old order."""
        pos = {mid: n for n, mid in enumerate(ids)}
        self.store.mods.sort(key=lambda m: pos.get(m["id"], len(pos)))

    def auto_sort(self):
        """Put the mods in the best load order (the Crash doctor can undo it), then check them for problems."""
        order = doctor.best_order(self.store)
        moved = doctor.moved([m["id"] for m in self.store.mods], order)
        if moved:
            self.apply({"title": "Load order"}, doctor.fix(f"Sorted automatically ({moved} mod(s) changed place)", "order", order))
        msg = (f"Load order sorted: {moved} mod(s) changed place." if moved else
               "The mods are already in the best load order I can find.")
        self.notify("log", msg)
        self.check_now()
        return msg

    # ---------- local AI ----------
    def setup_ai(self):
        """Download the engine and the model when they are missing (a run from source), then start the engine once."""
        try:
            if self.game_on:
                raise RuntimeError(GAME_ON)
            if not llm.ready():
                llm.fetch(lambda what, done, total: self.notify(
                    "ai_status", f"Downloading the local AI ({what}): {done * 100 // total if total else 0}%"))
            self.notify("ai_status", "Starting the local AI…")
            llm.start()
            self.notify("ai_status", "✔ Ready: the local AI runs on this PC. It stops when the game starts or this window closes.")
        except Exception as e:  # noqa: BLE001 - shown to the user
            self.notify("ai_status", f"✖ {e}")

    def ask_ai(self, report=""):
        """Check the mods, let the local AI pick fixes and load-order rules, and send the advice to the window.
        `report`: the player's own words about a bug; the AI then also names the cause and changes for those mods."""
        try:
            if self.game_on:
                raise RuntimeError(GAME_ON)
            llm.start()
            probs = self.check_now()
            # One copy of the list: the AI's mod numbers map back to it, and a framework install can shift the live one.
            mods = list(self.store.mods)
            a = llm.advise(mods, probs, report.strip())
            a["fixes"] = a["fixes"][:10]  # the window lists 10: apply only fixes the user saw
            order = doctor.best_order(self.store, a["after"])
            moved = doctor.moved([m["id"] for m in mods], order)
            base = order if moved else [m["id"] for m in mods]
            mine = {"title": "Your bug report"}
            for m, act, why in a.pop("mod_fixes", [])[:5]:
                n = m["name"]
                if act == "turn_on" and not m["enabled"]:
                    a["fixes"].append((mine, doctor.fix(f"Turn on {n}", "enable", m["id"]), why))
                elif act == "turn_off" and m["enabled"]:
                    a["fixes"].append((mine, doctor.fix(f"Turn off {n}", "disable", m["id"]), why))
                elif act == "update" and m.get("source", {}).get("type") in ("nexus", "github"):
                    a["fixes"].append((mine, doctor.fix(f"Update {n} to its newest version", "update", m["id"]), why))
                elif act == "load_last" and base[-1] != m["id"] and not any(  # a picked "Let it win" fix does it already
                        f["action"][:2] == ["prefer", m["id"]] for _, f, _ in a["fixes"]):  # applied after the order
                    base = [i for i in base if i != m["id"]] + [m["id"]]
                    a["fixes"].append((mine, doctor.fix(f"Load {n} last, so it wins", "order", base), why))
            self.notify("ai", {**a, "order": order if moved else None, "moved": moved})
        except Exception as e:  # noqa: BLE001 - shown to the user
            self.notify("ai", {"error": str(e)})

    def apply_ai(self, a):
        """The order first, so that a fix that moves one mod (prefer) keeps the last word."""
        if a["order"]:
            self.apply({"title": "Load order"}, doctor.fix(f"Sorted by the local AI ({a['moved']} mod(s) changed place)", "order", a["order"]))
        for p, f, _ in a["fixes"]:
            self.apply(p, f)
        self.check_now()

    # ---------- find mods from a video ----------
    def find_in_video(self, entry):
        try:
            title, rows = finder.find(entry)
        except Exception as e:  # noqa: BLE001 - shown to the user (page blocked or gone, no network, Nexus down)
            return self.notify("video", {"error": str(e)})
        self.notify("video", {"title": title, "rows": rows})

    # ---------- bad-mod search (bisect) ----------
    def bisect_begin(self):
        state = doctor.bisect_start(self.store)
        if len(state["suspects"]) < 2:
            return self.notify("log", "Fewer than two mods are on, so there is nothing to search.")
        with self.store.lock:
            self.store.doctor["bisect"] = state
        self._bisect_apply(state)
        self.notify("log", f"Bad-mod search: {len(state['suspects'])} suspects. I turned half of them off. "
                           "Start the game and play until it crashes, or quit normally if it does not.")

    def _bisect_apply(self, state):
        off = set(state["suspects"]) - set(state["testing"])
        with self.store.lock:
            for m in self.store.mods:
                if m["id"] in state["restore"]:
                    m["enabled"] = m["id"] not in off
            self.store.save()
        self.notify("mods", None)
        if self.store.game():
            self.redeploy()  # in the background, so the window does not freeze

    def bisect_cancel(self):
        with self.store.lock:
            state = self.store.doctor.pop("bisect", None)
        if state:
            self._bisect_apply({"suspects": [], "testing": [], "restore": state["restore"]})
            self.notify("log", "Bad-mod search stopped. Your mods are back as they were.")

    def _bisect_result(self, crashed):
        with self.store.lock:
            state = doctor.bisect_step(self.store.doctor["bisect"], crashed)
            if "culprit" not in state:
                self.store.doctor["bisect"] = state
            else:
                self.store.doctor.pop("bisect")
        if "culprit" not in state:
            self._bisect_apply(state)
            if state.get("verifying"):
                m = self.store.mod(state["suspects"][0])
                return self.notify("log", f"No crash. One last check: every mod is on again. Start the game once more; "
                                          f"if it crashes, {m['name'] if m else 'the last suspect'} is the bad mod.")
            return self.notify("log", f"{'Crash' if crashed else 'No crash'}. {len(state['suspects'])} suspects left. Start the game again.")
        self._bisect_apply({"suspects": [], "testing": [], "restore": state["restore"]})
        c = self.store.mod(state["culprit"]) if state["culprit"] else None
        if not c:
            return self.notify("log", "The search ended without one bad mod: " + (
                "the game did not crash during the search." if not state.get("crashes") else
                "the cause may be two mods together.") + " Your mods are back as they were.")
        self.handle_problems([doctor.problem(f"bisect-{c['id']}", f"Found it: {c['name']} makes the game crash",
                                             "Found by turning mods off half at a time.", doctor.mod_fixes(c, self.has_update))],
                             crashed=True)
