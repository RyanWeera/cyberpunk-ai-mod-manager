"""Everything the window does, without Tk: links -> jobs -> download -> virus scan -> install -> deploy,
plus game launch, crash handling, fixes and undo. Worker threads report through notify(kind, payload)."""
import itertools
import queue
import re
import shutil
import subprocess
import tempfile
import threading
import time
import webbrowser
from pathlib import Path

from crash_doctor import doctor, watch
from downloads import nexus
from downloads.fetch import FRAMEWORKS, FW_BY_NEXUS, FW_BY_REPO, download, github_latest
from installer import deploy, install
from shared.store import GAME_EXE
from virus_scan import scan

CORE = ["RED4ext", "Cyber Engine Tweaks", "redscript", "ArchiveXL", "TweakXL", "Codeware"]


class Controller:
    def __init__(self, store, notify, workers=3, watch_game=True):
        self.store, self.notify = store, notify
        self.jobs, self.lock, self.work, self.ids = [], threading.RLock(), queue.Queue(), itertools.count(1)
        self.premium, self.clicking, self.stop = False, None, threading.Event()
        for _ in range(workers):
            threading.Thread(target=self._worker, daemon=True).start()
        if watch_game:
            threading.Thread(target=watch.watch, args=(self._game_started, self._game_exited, self.stop), daemon=True).start()
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
            files = [f for f in c["files"] if self.store.settings["include_optional"] or not f["optional"]]
            self.notify("log", f"Collection '{c['name']}' revision {c['revision']}: {len(files)} files")
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
        elif t == "github":
            fw = FW_BY_REPO.get(link["repo"].lower())
            if fw:
                return self.add_framework(fw)
            if not self._have(lambda s: s.get("repo") == link["repo"]):
                self._job(link["repo"].split("/")[1], {"type": "github", "repo": link["repo"]})
        else:
            self._job(Path(link.get("path") or link["url"].split("?")[0]).stem, link)

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
        Any other link may come from any web page, so the user is asked before anything is installed."""
        link = nexus.parse_link(url)
        if link["type"] != "nexus":
            return self.notify("confirm", {"url": url, "text": "Download and install this Nexus collection?"})
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
        self.notify("confirm", {"url": url, "text": f"Download and install '{name}' (file {link['file_id']}) from Nexus?"})

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
                self._set(j, f"Failed: {e}")
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
                j["state"] = "waiting"
                return self._set(j, "Waiting for your click on Nexus")
        # One new folder per job: no two jobs share a file, and nothing here is ever the user's own file.
        work = Path(tempfile.mkdtemp(prefix="job-", dir=self.store.downloads))
        try:
            if s["type"] == "nexus":
                self._set(j, "Getting download link")
                path = download(nexus.download_url(self.key, s["mod_id"], s["file_id"], s.get("key"), s.get("expires")),
                                work, prog)
            elif s["type"] == "github":
                rel = github_latest(s["repo"])
                j["version"] = rel["tag"]
                path = download(rel["url"], work, prog)
            elif s["type"] == "url":
                path = download(s["url"], work, prog)
            else:  # a file from the PC: copied under a fixed plain name (for Defender's command line); the original stays
                self._set(j, "Copying")
                suffix = Path(s["path"]).suffix.lower()
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
               "source": {k: v for k, v in s.items() if k not in ("key", "expires")}, "files": rels,
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
    def launch(self):
        game = self.store.game()
        if not game:
            raise RuntimeError("Set the Cyberpunk 2077 folder in Settings first.")
        with self.store.lock:
            if self.stop.is_set():  # the window is closing
                return
            deploy.deploy(self.store)
        redmod = any(r.lower().startswith("mods/") for m in self.store.enabled() for r in m["files"])
        subprocess.Popen([str(game / GAME_EXE)] + (["-modded"] if redmod else []), cwd=str(game / "bin/x64"))

    def _game_started(self, t):
        with self.store.lock:
            self.store.doctor["last_launch"] = t
            self.store.save()
        self.notify("game", True)

    def _game_exited(self, started, exit_code=None):
        self.notify("game", False)
        ev = watch.crash_evidence(started, exit_code)
        if self.store.doctor.get("bisect"):
            return self._bisect_result(ev["crashed"])
        if not ev["crashed"]:
            with self.store.lock:
                self.store.doctor["last_good"] = {m["id"]: m.get("version", "") for m in self.store.enabled()}
                self.store.save()
            return self.notify("log", "The game closed normally. This mod setup is saved as the last good one.")
        self.notify("log", "The game crashed. Looking for the cause...")
        self.handle_problems(doctor.diagnose(self.store, started, ev, self.has_update), crashed=True)

    def check_now(self):
        since = self.store.doctor.get("last_launch", 0)
        self.handle_problems(doctor.diagnose(self.store, since, None, self.has_update))

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
            elif verb == "prefer":  # let args[0] win over args[1]: move it just below
                undo = ["order", [m["id"] for m in st.mods]]
                lo = st.mod(args[0])
                st.mods.remove(lo)
                st.mods.insert(st.mods.index(st.mod(args[1])) + 1, lo)
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
                pos = {mid: n for n, mid in enumerate(arg)}
                st.mods.sort(key=lambda m: pos.get(m["id"], len(pos)))
            elif kind == "unignore":
                st.doctor["ignored"].remove(arg)
            st.save()
        self.notify("mods", None)
        if st.game():
            self.redeploy()

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
