"""Turn a downloaded archive into game-relative files: unpack, run a FOMOD installer with its
default choices, and put loose files in the folder the game (or its frameworks) loads them from."""
import os
import re
import shutil
import subprocess
import tempfile
import xml.etree.ElementTree as ET
from pathlib import Path, PureWindowsPath

# Windows 10+ ships bsdtar, which unpacks zip, 7z and rar.
TAR = Path(os.environ.get("SystemRoot", r"C:\Windows")) / "System32" / "tar.exe"
NO_WINDOW = getattr(subprocess, "CREATE_NO_WINDOW", 0)
ROOTS = {"archive", "bin", "engine", "r6", "red4ext", "mods"}
CET_MODS = "bin/x64/plugins/cyber_engine_tweaks/mods"


def extract(archive, dest):
    try:
        r = subprocess.run([str(TAR), "-xf", str(archive), "-C", str(dest)], capture_output=True, text=True,
                           errors="replace", creationflags=NO_WINDOW, timeout=1800)
    except subprocess.TimeoutExpired:
        raise RuntimeError(f"Unpacking {Path(archive).name} took more than 30 minutes") from None
    if r.returncode:
        hint = (" The tar.exe in Windows 10 cannot open some .7z and .rar files: update to Windows 11, or unpack it "
                "yourself, zip it, and add the .zip.") if Path(archive).suffix.lower() in (".7z", ".rar") else ""
        raise RuntimeError(f"Could not unpack {Path(archive).name}: {r.stderr.strip()[-300:]}{hint}")


def safe(name):
    return re.sub(r"[^\w\- .]", "", name).strip(" .") or "mod"


def _inside(path):
    """A relative path that stays inside its folder. Refuses `..`, and any root or drive: on Windows
    `\\x`, `/x` and `D:x` are not "absolute", but joining them still leaves the folder."""
    p = PureWindowsPath(path)
    return not p.anchor and ".." not in p.parts


def fomod_tree(tree, out):
    """If the mod has a FOMOD installer, copy the default choices into `out` and return it, else None.
    Defaults: Required/Recommended options, else the first option of a pick-one group."""
    cfg = next((p for p in tree.rglob("*") if p.name.lower() == "moduleconfig.xml" and p.parent.name.lower() == "fomod"), None)
    raw = cfg.read_bytes() if cfg else b""
    if not cfg or b"<!ENTITY" in raw or "<!ENTITY".encode("utf-16-le") in raw:
        return None  # no installer, or one using XML entities (never needed, only used for attacks)
    base, root = cfg.parent.parent, ET.fromstring(raw)
    for el in root.iter():
        el.tag = el.tag.split("}")[-1]
    picks, flags = [], {}

    def take(node):
        for f in (node if node is not None else []):
            if f.tag in ("file", "folder"):
                picks.append((f.get("source", ""), f.get("destination", f.get("source", "")), f.tag))

    def kind(p):
        t = p.find("typeDescriptor/type")
        t = t if t is not None else p.find("typeDescriptor/dependencyType/defaultType")
        return t.get("name") if t is not None else "Optional"

    take(root.find("requiredInstallFiles"))
    for group in root.iter("group"):
        plugins = [p for p in group.iter("plugin") if kind(p) != "NotUsable"]
        gtype = group.get("type", "SelectAny")
        chosen = plugins if gtype == "SelectAll" else [p for p in plugins if kind(p) in ("Required", "Recommended")]
        if not chosen and gtype in ("SelectExactlyOne", "SelectAtLeastOne"):
            chosen = plugins[:1]
        if gtype in ("SelectExactlyOne", "SelectAtMostOne"):
            chosen = chosen[:1]
        for p in chosen:
            take(p.find("files"))
            for fl in p.iter("flag"):
                flags[fl.get("name")] = (fl.text or "").strip()
    for pat in root.iterfind("conditionalFileInstalls/patterns/pattern"):
        deps = pat.find("dependencies")
        conds = [flags.get(d.get("flag")) == d.get("value", "") for d in (deps.iter("flagDependency") if deps is not None else [])]
        if (any(conds) if deps is not None and deps.get("operator") == "Or" else all(conds)):
            take(pat.find("files"))

    out.mkdir(parents=True, exist_ok=True)  # exists even when the default choices pick no file
    base_r, out_r = base.resolve(), out.resolve()

    def within(p, root):  # after following any link: still inside?
        return p.resolve().is_relative_to(root)

    for src, dst, tag in picks:
        if not (_inside(src) and _inside(dst)):
            continue  # never let an installer read or write outside the mod folders
        s, d = base / src.replace("\\", "/"), out / dst.replace("\\", "/")
        if not (within(s, base_r) and within(d, out_r)):
            continue
        if tag == "folder" and s.is_dir():
            shutil.copytree(s, d, dirs_exist_ok=True, symlinks=True)  # links stay links; layout() drops them
        elif s.is_file() and not s.is_symlink():
            target = d / s.name if not dst or dst.endswith(("/", "\\")) or d.is_dir() else d
            if within(target, out_r):
                target.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(s, target)
    return out


def layout(tree, name):
    """[(source file, game-relative posix path)] for everything the game can use in `tree`."""
    files = [p for p in tree.rglob("*") if p.is_file() and not p.is_symlink()  # a link could point at any file on the PC
             and "fomod" not in (x.lower() for x in p.relative_to(tree).parts[:-1])]
    for d in sorted([tree] + [p for p in tree.rglob("*") if p.is_dir()], key=lambda d: len(d.parts)):  # shallowest folder that holds game folders (archive/, r6/, ...) is the game root
        kids = {c.name.lower() for c in d.iterdir() if c.is_dir()}
        if not kids & ROOTS:
            continue
        cet_style = "mods" in kids and any(p.name.lower() == "init.lua" for p in (d / "mods").glob("*/init.lua"))
        out = []
        for f in files:
            if not f.is_relative_to(d):
                continue
            rel = f.relative_to(d).as_posix()
            top = rel.split("/")[0].lower()
            if top == "mods" and cet_style:
                out.append((f, f"{CET_MODS}/{rel.split('/', 1)[1]}"))
            elif top in ROOTS:
                out.append((f, rel))
        if out:
            return out

    # Loose files: place each kind where it loads from.
    cet = [p.parent for p in files if p.name.lower() == "init.lua"]
    cet = [d for d in cet if not any(o != d and o in d.parents for o in cet)]
    redmod = [p.parent for p in files if p.name.lower() == "info.json" and (p.parent / "archives").is_dir()]
    dlls = {p.parent for p in files if p.suffix.lower() == ".dll"}
    reds = [p.parent for p in files if p.suffix.lower() == ".reds"]
    reds_root = Path(os.path.commonpath(reds)) if reds else None
    out = []
    for f in files:
        ext = f.suffix.lower()
        home = next((d for d in cet if f.is_relative_to(d)), None)
        rm = next((d for d in redmod if f.is_relative_to(d)), None)
        if home:
            out.append((f, f"{CET_MODS}/{safe(home.name if home != tree else name)}/{f.relative_to(home).as_posix()}"))
        elif rm:
            out.append((f, f"mods/{safe(rm.name if rm != tree else name)}/{f.relative_to(rm).as_posix()}"))
        elif f.parent in dlls:
            out.append((f, f"red4ext/plugins/{safe(f.parent.name if f.parent != tree else name)}/{f.name}"))
        elif ext in (".archive", ".xl"):
            out.append((f, f"archive/pc/mod/{f.name}"))
        elif ext == ".reds":
            out.append((f, f"r6/scripts/{safe(name)}/{f.relative_to(reds_root).as_posix()}"))
        elif ext in (".yaml", ".yml", ".tweak"):
            out.append((f, f"r6/tweaks/{safe(name)}/{f.name}"))
        elif ext == ".xml":
            out.append((f, f"r6/input/{f.name}"))
        elif ext == ".ini":
            out.append((f, f"engine/config/platform/pc/{f.name}"))
    return out


def needs_for(rels):
    """Framework names the files need, from where they install and what kind they are."""
    need = set()
    for r in (x.lower() for x in rels):
        if r.startswith("red4ext/plugins/"):
            need.add("RED4ext")
        if r.endswith(".reds"):
            need.add("redscript")
        if r.endswith(".xl"):
            need.add("ArchiveXL")
        if r.startswith("r6/tweaks/") and r.endswith((".yaml", ".yml", ".tweak")):
            need.add("TweakXL")
        if r.startswith(CET_MODS + "/"):
            need.add("Cyber Engine Tweaks")
        if r.startswith("r6/input/"):
            need.add("Input Loader")
    return need


def install(archive, stage, name):
    """Unpack `archive` into the staging folder `stage` in game layout. Returns the game-relative paths."""
    with tempfile.TemporaryDirectory() as tmp:
        tree = Path(tmp) / "x"
        tree.mkdir()
        extract(archive, tree)
        src = fomod_tree(tree, Path(tmp) / "fomod") or tree
        pairs = layout(src, name)
        if not pairs:
            raise RuntimeError(f"{Path(archive).name} has no files the game can load")
        shutil.rmtree(stage, ignore_errors=True)
        for f, rel in pairs:
            d = Path(stage) / rel
            d.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(f, d)
    return sorted({rel for _, rel in pairs})
