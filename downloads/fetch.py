"""HTTP helpers, file downloads with progress, and GitHub releases for the core frameworks."""
import json
import os
import re
import time
import urllib.parse
import urllib.request
from pathlib import Path

UA = {"User-Agent": "CP2077-AI-Mod-Manager/1.0", "Application-Name": "CP2077 AI Mod Manager", "Application-Version": "1.0"}

# Frameworks most mods need. They all publish GitHub releases, so they download with no clicks and no account.
FRAMEWORKS = {
    "RED4ext": ("wopss/RED4ext", 2380),
    "Cyber Engine Tweaks": ("maximegmd/CyberEngineTweaks", 107),
    "redscript": ("jac3km4/redscript", 1511),
    "ArchiveXL": ("psiberx/cp2077-archive-xl", 4198),
    "TweakXL": ("psiberx/cp2077-tweak-xl", 4197),
    "Codeware": ("psiberx/cp2077-codeware", 7780),
    "Input Loader": ("jackhumbert/cyberpunk2077-input-loader", 4575),
    "Mod Settings": ("jackhumbert/mod_settings", 4885),
}
FW_BY_NEXUS = {nid: name for name, (_, nid) in FRAMEWORKS.items()}
FW_BY_REPO = {repo.lower(): name for name, (repo, _) in FRAMEWORKS.items()}


def http_json(url, body=None, headers=None, timeout=30):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(url, data, {**UA, "Content-Type": "application/json", **(headers or {})})
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.load(r)


def github_latest(repo):
    d = http_json(f"https://api.github.com/repos/{repo}/releases/latest")
    asset = next((a for a in d.get("assets", []) if a["name"].lower().endswith((".zip", ".7z"))
                  and not re.search(r"pdb|symbol|mac|darwin|linux", a["name"], re.I)), None)
    if not asset:
        raise RuntimeError(f"{repo} has no Windows download in its latest release")
    return {"tag": d["tag_name"], "url": asset["browser_download_url"], "name": asset["name"]}


def download(url, folder, progress=None, headers=None):
    """Download url into folder. progress(done_bytes, total_bytes) is called about twice a second."""
    req = urllib.request.Request(url, headers={**UA, **(headers or {})})
    with urllib.request.urlopen(req, timeout=60) as r:
        if urllib.parse.urlparse(r.url).scheme != "https":  # also catches an https link that redirects to http
            raise RuntimeError("Refusing a download that is not https")
        cd = r.headers.get("Content-Disposition", "")
        m = re.search(r'filename\*?=(?:UTF-8\'\')?"?([^";]+)', cd)
        name = urllib.parse.unquote(m[1] if m else os.path.basename(urllib.parse.urlparse(r.url).path)) or "download.zip"
        name = re.sub(r'[<>:"/\\|?*\x00-\x1f]', "_", name).strip(". ") or "download.zip"
        dest = Path(folder) / name
        part = dest.with_name(dest.name + ".part")
        total, done, last = int(r.headers.get("Content-Length") or 0), 0, 0.0
        with open(part, "wb") as f:
            while chunk := r.read(1 << 16):
                f.write(chunk)
                done += len(chunk)
                if progress and time.monotonic() - last > 0.5:
                    progress(done, total)
                    last = time.monotonic()
    os.replace(part, dest)
    return dest
