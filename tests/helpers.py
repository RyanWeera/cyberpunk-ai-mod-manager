import struct
import zipfile
from pathlib import Path


def make_zip(path, files):
    """files: {"inner/path": "text" or bytes}"""
    with zipfile.ZipFile(path, "w") as z:
        for name, body in files.items():
            z.writestr(name, body)
    return Path(path)


def rdar(hashes):
    """Bytes of a minimal .archive holding the given resource hashes."""
    entries = b"".join(struct.pack("<QqIIIII", h, 0, 0, 0, 0, 0, 0) + b"\0" * 20 for h in hashes)
    idx = struct.pack("<IIQIII", 8, len(entries) + 20, 0, len(hashes), 0, 0) + entries
    return struct.pack("<IIQIQIQ", 0x52414452, 12, 40, len(idx), 0, 0, 40 + len(idx)) + idx


def fake_game(root):
    g = Path(root) / "Cyberpunk 2077"
    (g / "bin/x64").mkdir(parents=True)
    (g / "bin/x64/Cyberpunk2077.exe").write_bytes(b"MZ")
    (g / "archive/pc/mod").mkdir(parents=True)
    return g


def mod(mid, files, **kw):
    return {"id": mid, "name": kw.pop("name", mid), "version": kw.pop("version", "1"), "enabled": kw.pop("enabled", True),
            "source": kw.pop("source", {"type": "file"}), "files": files, "needs": kw.pop("needs", []),
            "requires": kw.pop("requires", []), "framework": kw.pop("framework", None), **kw}


def stage(store, m, bodies=None):
    """Write staging files for mod m and add it to the store."""
    for rel in m["files"]:
        p = store.staging / m["id"] / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_bytes((bodies or {}).get(rel, f"{m['id']}:{rel}".encode()))
    store.mods.append(m)
    return m
