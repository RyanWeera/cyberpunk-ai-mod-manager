"""HTTP helpers, file downloads with progress, and GitHub releases for the core frameworks."""
import hashlib
import json
import os
import re
import subprocess
import time
import urllib.parse
import urllib.request
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

from shared.store import VERSION

SYS32 = Path(os.environ.get("SystemRoot", r"C:\Windows")) / "System32"

UA = {"User-Agent": f"CP2077-AI-Mod-Manager/{VERSION}", "Application-Name": "CP2077 AI Mod Manager", "Application-Version": VERSION}

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


APP_REPO = "RyanWeera/cyberpunk-ai-mod-manager"


def app_latest():
    """{"tag", "url"} of the exe in this app's newest GitHub release, or None when this version is the newest."""
    d = http_json(f"https://api.github.com/repos/{APP_REPO}/releases/latest")
    ver = lambda s: tuple(int(n) for n in re.findall(r"\d+", s))
    if ver(d["tag_name"]) <= ver(VERSION):
        return None
    own = f"https://github.com/{APP_REPO}/releases/download/"  # only this repo's release files
    asset = next((a for a in d.get("assets", []) if a["name"].lower().endswith(".exe")
                  and a["browser_download_url"].startswith(own)), None)
    if not asset:
        raise RuntimeError(f"Release {d['tag_name']} has no exe to download")
    return {"tag": d["tag_name"], "url": asset["browser_download_url"]}


def ascii_name(name):
    """A plain ASCII file name. Windows tools that read their command line in the ANSI code page turn some
    Unicode letters into quotes ("best-fit"), so a server-chosen name must never reach them as it is."""
    return re.sub(r"[^A-Za-z0-9._ ()+-]", "_", name).strip(". ") or "download.zip"


# Tk shows only PNG and GIF, and Nexus serves its images as WebP, so the Windows image codecs (WIC, through WPF)
# scale and crop them to PNG. Windows 11 has the WebP codec built in.
SHRINK = r"""Add-Type -AssemblyName PresentationCore
foreach ($l in Get-Content -LiteralPath $env:CP_THUMBS -Encoding UTF8) {
  $src, $dst, $w, $h = $l -split '\|'; $w = [int]$w; $h = [int]$h
  try {
    $fs = [IO.File]::OpenRead($src)
    try { $f = [Windows.Media.Imaging.BitmapDecoder]::Create($fs, 'IgnoreColorProfile', 'OnLoad').Frames[0] } finally { $fs.Close() }
    $s = [Math]::Max($w / $f.PixelWidth, $h / $f.PixelHeight) + 0.001
    $t = New-Object Windows.Media.Imaging.TransformedBitmap $f, (New-Object Windows.Media.ScaleTransform $s, $s)
    $cw = [Math]::Min($w, $t.PixelWidth); $ch = [Math]::Min($h, $t.PixelHeight)
    $r = New-Object Windows.Int32Rect ([int](($t.PixelWidth - $cw) / 2)), ([int](($t.PixelHeight - $ch) / 2)), $cw, $ch
    $e = New-Object Windows.Media.Imaging.PngBitmapEncoder
    $e.Frames.Add([Windows.Media.Imaging.BitmapFrame]::Create((New-Object Windows.Media.Imaging.CroppedBitmap $t, $r)))
    $o = [IO.File]::Create($dst); try { $e.Save($o) } finally { $o.Close() }
  } catch {}
  Remove-Item -LiteralPath $src -ErrorAction SilentlyContinue
}"""


def thumbnails(urls, folder, w, h):
    """{url: png path} of w x h crops of Nexus images, cached in folder. Images that fail are left out."""
    folder = Path(folder)
    folder.mkdir(parents=True, exist_ok=True)
    out = {u: folder / f"{hashlib.sha1(u.encode()).hexdigest()[:16]}_{w}x{h}.png" for u in set(urls)
           if urllib.parse.urlparse(u).scheme == "https" and urllib.parse.urlparse(u).hostname in ("staticdelivery.nexusmods.com", "media.nexusmods.com")}

    def get(u):
        src = out[u].with_suffix(".src")
        with urllib.request.urlopen(urllib.request.Request(u, headers=UA), timeout=20) as r:
            src.write_bytes(r.read(5 << 20))  # thumbnails are small; a 5 MB cap keeps a bad file from filling memory
        return f"{src}|{out[u]}|{w}|{h}"

    with ThreadPoolExecutor(8) as pool:
        jobs = [pool.submit(get, u) for u, p in out.items() if not p.exists()]
        lines = [j.result() for j in jobs if not j.exception()]
    if lines:
        todo = folder / "todo.txt"
        todo.write_text("\n".join(lines), encoding="utf-8-sig")
        try:
            subprocess.run([str(SYS32 / "WindowsPowerShell/v1.0/powershell.exe"), "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass",
                            "-Command", SHRINK], check=False, env={**os.environ, "CP_THUMBS": str(todo)}, timeout=120, capture_output=True,
                           creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0))
        finally:  # a timeout leaves the downloaded originals behind
            for line in lines:
                Path(line.split("|")[0]).unlink(missing_ok=True)
    return {u: str(p) for u, p in out.items() if p.exists()}


def download(url, folder, progress=None, headers=None):
    """Download url into folder. progress(done_bytes, total_bytes) is called about twice a second."""
    req = urllib.request.Request(url, headers={**UA, **(headers or {})})
    with urllib.request.urlopen(req, timeout=60) as r:
        if urllib.parse.urlparse(r.url).scheme != "https":  # also catches an https link that redirects to http
            raise RuntimeError("Refusing a download that is not https")
        cd = r.headers.get("Content-Disposition", "")
        m = re.search(r'filename\*?=(?:UTF-8\'\')?"?([^";]+)', cd)
        name = urllib.parse.unquote(m[1] if m else os.path.basename(urllib.parse.urlparse(r.url).path))
        dest = Path(folder) / ascii_name(name)
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
