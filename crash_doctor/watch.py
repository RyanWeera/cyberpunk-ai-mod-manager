"""Watch for the game process, and after it exits collect crash evidence: its exit code, Windows crash
events, and REDEngine crash reports (whose minidump names the module that crashed). While the game runs, a window that
stays "Not responding" gets a snapshot of every thread, so a freeze can be explained too."""
import ctypes
import msvcrt
import os
import re
import struct
import subprocess
import time
from bisect import bisect_right
from pathlib import Path

EXE = "Cyberpunk2077.exe"
NO_WINDOW = getattr(subprocess, "CREATE_NO_WINDOW", 0)
SYS32 = Path(os.environ.get("SystemRoot", r"C:\Windows")) / "System32"  # full paths: Windows looks next to our exe first
SYNCHRONIZE, QUERY_LIMITED = 0x00100000, 0x1000
WAIT_TIMEOUT = 0x102
HANG_AFTER = 20  # seconds a window stays "Not responding" (Windows says so after 5 s) before the snapshot: 25 s in all
VM_READ, QUERY_INFO = 0x0010, 0x0400


class SymbolInfo(ctypes.Structure):  # SYMBOL_INFOW
    _fields_ = [("SizeOfStruct", ctypes.c_uint32), ("TypeIndex", ctypes.c_uint32), ("Reserved", ctypes.c_uint64 * 2),
                ("Index", ctypes.c_uint32), ("Size", ctypes.c_uint32), ("ModBase", ctypes.c_uint64), ("Flags", ctypes.c_uint32),
                ("Value", ctypes.c_uint64), ("Address", ctypes.c_uint64), ("Register", ctypes.c_uint32), ("Scope", ctypes.c_uint32),
                ("Tag", ctypes.c_uint32), ("NameLen", ctypes.c_uint32), ("MaxNameLen", ctypes.c_uint32), ("Name", ctypes.c_wchar * 256)]


def kernel32():
    k32 = ctypes.windll.kernel32
    k32.OpenProcess.restype = ctypes.c_void_p
    k32.WaitForSingleObject.argtypes = [ctypes.c_void_p, ctypes.c_uint32]
    k32.GetExitCodeProcess.argtypes = [ctypes.c_void_p, ctypes.POINTER(ctypes.c_uint32)]
    k32.CloseHandle.argtypes = [ctypes.c_void_p]
    return k32


def hung(pid):
    """True when a visible window of process `pid` is "Not responding"."""
    u = ctypes.windll.user32
    u.GetWindowThreadProcessId.argtypes = [ctypes.c_void_p, ctypes.POINTER(ctypes.c_uint32)]
    u.IsWindowVisible.argtypes = u.IsHungAppWindow.argtypes = [ctypes.c_void_p]
    wins = []

    def each(hwnd, _):
        owner = ctypes.c_uint32()
        u.GetWindowThreadProcessId(hwnd, ctypes.byref(owner))
        if owner.value == pid and u.IsWindowVisible(hwnd):
            wins.append(hwnd)
        return True

    u.EnumWindows(ctypes.WINFUNCTYPE(ctypes.c_bool, ctypes.c_void_p, ctypes.c_void_p)(each), 0)
    return any(u.IsHungAppWindow(w) for w in wins)


def write_dump(pid, folder, keep=5):
    """Snapshot (threads, their stacks, modules) of running process `pid` as a minidump in `folder`: its path, or None.
    Only the newest `keep` snapshots stay."""
    k32 = kernel32()
    h = k32.OpenProcess(QUERY_INFO | VM_READ, False, pid)
    if not h:  # e.g. the game runs as administrator and this app does not
        return None
    try:
        folder.mkdir(parents=True, exist_ok=True)
        path = folder / f"freeze-{time.strftime('%Y%m%d-%H%M%S')}.dmp"
        dbg = ctypes.WinDLL(str(SYS32 / "dbghelp.dll"))
        dbg.MiniDumpWriteDump.argtypes = [ctypes.c_void_p, ctypes.c_uint32, ctypes.c_void_p, ctypes.c_uint32,
                                          ctypes.c_void_p, ctypes.c_void_p, ctypes.c_void_p]
        with open(path, "wb") as f:
            ok = dbg.MiniDumpWriteDump(h, pid, msvcrt.get_osfhandle(f.fileno()), 0, None, None, None)  # 0 = MiniDumpNormal
        if not ok:
            path.unlink(missing_ok=True)
            return None
        for old in sorted(folder.glob("freeze-*.dmp"))[:-keep]:
            old.unlink(missing_ok=True)
        return path
    except OSError:
        return None
    finally:
        k32.CloseHandle(h)


def find_pid():
    try:
        r = subprocess.run([str(SYS32 / "tasklist.exe"), "/FI", f"IMAGENAME eq {EXE}", "/NH", "/FO", "CSV"], capture_output=True, text=True,
                           errors="replace", creationflags=NO_WINDOW, timeout=60)
    except subprocess.TimeoutExpired:
        return None
    m = re.search(rf'"{re.escape(EXE)}","(\d+)"', r.stdout, re.I)
    return int(m[1]) if m else None


def watch(on_start, on_exit, stop, interval=5, hang_dir=None):
    """Call on_start(t) when the game starts and on_exit(t_started, exit_code, freeze) when it stops, until stop is set.
    exit_code is None when Windows would not let us open the process. freeze is the snapshot (in hang_dir) of a freeze
    the game never came back from, or None."""
    k32 = kernel32()
    while not stop.wait(interval):
        try:
            pid = find_pid()
            if not pid:
                continue
            h = k32.OpenProcess(SYNCHRONIZE | QUERY_LIMITED, False, pid)
            started, code, freeze, hung_since = time.time(), None, None, None
            on_start(started)
            if h:
                while k32.WaitForSingleObject(h, 1000) == WAIT_TIMEOUT and not stop.is_set():
                    if hang_dir is None:
                        continue
                    try:
                        if not hung(pid):
                            hung_since = None
                            if freeze:  # it came back: a long load, not a freeze
                                freeze.unlink(missing_ok=True)
                            freeze = None
                        elif hung_since is None:
                            hung_since = time.time()
                        elif freeze is None and time.time() - hung_since >= HANG_AFTER:
                            freeze = write_dump(pid, hang_dir) or False  # False: no second try in this freeze
                    except Exception:  # noqa: BLE001 - a failed hang check must not skip the exit report below
                        freeze = freeze or False
                c = ctypes.c_uint32()
                if k32.GetExitCodeProcess(h, ctypes.byref(c)):
                    code = c.value
                k32.CloseHandle(h)
                if freeze and code == 0:  # a clean exit: it was a slow quit, not a freeze
                    freeze.unlink(missing_ok=True)
                    freeze = None
            else:
                while find_pid() == pid and not stop.wait(interval):
                    pass
            on_exit(started, code, freeze or None)
        except Exception:  # noqa: BLE001 - one bad cycle (a report deleted mid-read, a failed save) must not end crash watching
            continue


def parse_events(text):
    """[(faulting module path or '' for hangs, process start as unix time or None, fault offset or None)] for game
    crash events in `wevtutil /f:text` output."""
    out = []
    for block in text.split("Event[")[1:]:
        if f"application name: {EXE.lower()}" in block.lower() or f"program {EXE.lower()}" in block.lower():
            m = re.search(r"Faulting module path:\s*(.+)", block)
            s = re.search(r"(?:application start time|Start Time):\s*(?:0x)?([0-9a-fA-F]{8,})", block)
            started = (int(s[1], 16) - 116444736000000000) / 1e7 if s else None  # FILETIME -> unix
            o = re.search(r"Fault offset:\s*0x([0-9a-fA-F]+)", block)
            out.append((m[1].strip() if m else "", started, int(o[1], 16) if o else None))
    return out


def symbol(path, base, size, addr):
    """Function name at `addr` from the .pdb file next to a module, or None. Mods rarely ship one."""
    if len(Path(path).drive) != 2 or not Path(path).with_suffix(".pdb").is_file():  # "C:" only: no \\host\share paths
        return None
    try:
        dbg = ctypes.WinDLL(str(SYS32 / "dbghelp.dll"))
        dbg.SymInitializeW.argtypes = [ctypes.c_void_p, ctypes.c_wchar_p, ctypes.c_bool]
        dbg.SymLoadModuleExW.argtypes = [ctypes.c_void_p, ctypes.c_void_p, ctypes.c_wchar_p, ctypes.c_wchar_p, ctypes.c_uint64,
                                         ctypes.c_uint32, ctypes.c_void_p, ctypes.c_uint32]
        dbg.SymLoadModuleExW.restype = ctypes.c_uint64
        dbg.SymFromAddrW.argtypes = [ctypes.c_void_p, ctypes.c_uint64, ctypes.POINTER(ctypes.c_uint64), ctypes.POINTER(SymbolInfo)]
        dbg.SymCleanup.argtypes = [ctypes.c_void_p]
        h = ctypes.c_void_p(0x5CB0)  # any unique value: no live process is read
        dbg.SymSetOptions(0x2 | 0x200 | 0x1000 | 0x80000)  # undecorate names, no error boxes, ignore _NT_SYMBOL_PATH, no prompts
        if not dbg.SymInitializeW(h, str(Path(path).parent), False):  # search only the mod's folder, never a symbol server
            return None
        try:
            info, disp = SymbolInfo(SizeOfStruct=88, MaxNameLen=255), ctypes.c_uint64()
            if dbg.SymLoadModuleExW(h, None, str(path), None, base, size, None, 0) and \
                    dbg.SymFromAddrW(h, addr, ctypes.byref(disp), ctypes.byref(info)) and not info.Flags & 0x200:  # 0x200: export only
                return info.Name
        finally:
            dbg.SymCleanup(h)
    except (OSError, AttributeError):
        return None
    return None


def read_dump(path):
    """From a minidump (.dmp):
    module: path of the module the crash happened in (None for a freeze snapshot, which has no crash)
    code: exception code
    stack: modules on the crashing thread's stack (on the main thread in a freeze snapshot), newest call first
    others: modules on the other threads' stacks (freeze snapshots only)
    places: {module path: "Name.dll+0xOFFSET (function)"} where the crash happened and where each stack module was found
    fault: crash ID "Name.dll+0xOFFSET" of the crashing instruction, or None
    or None when the file is no minidump."""
    try:
        b = Path(path).read_bytes()
        sig, _, n, rva = struct.unpack_from("<4sIII", b)
        if sig != b"MDMP":
            return None
        streams = {t: off for t, _, off in (struct.unpack_from("<III", b, rva + 12 * i) for i in range(n))}
        if 4 not in streams or not ({3, 6} & streams.keys()):  # 6 = exception, 4 = module list, 3 = thread list
            return None
        tid = code = addr = None
        if 6 in streams:
            tid, _, code = struct.unpack_from("<III", b, streams[6])
            addr = struct.unpack_from("<Q", b, streams[6] + 24)[0]  # ThreadId, pad, code, flags, record, address
        mods = []
        for i in range(struct.unpack_from("<I", b, streams[4])[0]):
            base, size, _, _, name_rva = struct.unpack_from("<QIIII", b, streams[4] + 4 + 108 * i)
            length = struct.unpack_from("<I", b, name_rva)[0]
            mods.append((base, base + size, b[name_rva + 4:name_rva + 4 + length].decode("utf-16-le")))
        mods.sort()
        bases = [m[0] for m in mods]
        places = {}

        def at(a):
            i = bisect_right(bases, a) - 1
            m = mods[i] if i >= 0 and a < mods[i][1] else None
            if m and m[2] not in places:
                sym = symbol(m[2], m[0], m[1] - m[0], a)
                places[m[2]] = f"{Path(m[2].replace(chr(92), '/')).name}+0x{a - m[0]:X}" + (f" ({sym})" if sym else "")
            return m and m[2]

        module = at(addr) if addr is not None else None
        threads = [struct.unpack_from("<IIIIQQII", b, streams[3] + 4 + 48 * i)
                   for i in range(min(struct.unpack_from("<I", b, streams[3])[0], 1024) if 3 in streams else 0)]
        if tid is not None:
            threads = [t for t in threads if t[0] == tid]  # a crash: only the crashing thread matters
        # ponytail: every stack slot that points into a module counts, so an old return address can show up too;
        # walk the unwind data if this lists the wrong mods. The first thread of a snapshot is taken as the main thread.
        stacks = []
        for t in threads:
            hits = []
            for word in struct.unpack_from(f"<{t[6] // 8}Q", b, t[7]):
                if (m := at(word)) and m not in hits:
                    hits.append(m)
            stacks.append(hits)
        stack = stacks[0] if stacks else []
        others = list(dict.fromkeys(m for hits in stacks[1:] for m in hits if m not in stack))
        fault = places[module].split(" (")[0] if module else None
        return {"module": module, "code": code, "stack": stack, "others": others, "places": places, "fault": fault}
    except (OSError, struct.error, UnicodeDecodeError):
        return None


def dump_module(path):
    """Path of the module the crash happened in, read from a minidump (.dmp), or None."""
    return (read_dump(path) or {}).get("module")


def fault_id(module, offset):
    """Short crash ID, the same for every crash at the same instruction: 'Bad.dll+0x10'."""
    return f"{Path(module.replace(chr(92), '/')).name}+0x{offset:X}"


def _mtime(p):
    try:
        return p.stat().st_mtime
    except OSError:  # the crash reporter can delete a report while we look
        return 0


def crash_evidence(since, exit_code=None, tries=4, lag=10):
    """`since` is when polling saw the game start (up to one poll late); events of older game processes are ignored.
    Windows writes its crash event a moment after the process ends, so look a few times when the exit code says crash."""
    bad_exit = exit_code is not None and exit_code >= 0xC0000000  # NTSTATUS error, e.g. 0xC0000005 access violation
    for attempt in range(tries):
        ms = int((time.time() - since) * 1000) + 60_000
        q = (f"*[System[(Provider[@Name='Application Error'] or Provider[@Name='Application Hang'])"
             f" and TimeCreated[timediff(@SystemTime) <= {ms}]]]")
        try:
            out = subprocess.run([str(SYS32 / "wevtutil.exe"), "qe", "Application", f"/q:{q}", "/f:text", "/rd:true", "/c:20"], capture_output=True,
                                 text=True, errors="replace", creationflags=NO_WINDOW, timeout=60).stdout
        except subprocess.TimeoutExpired:
            out = ""
        events = [(mod, off) for mod, started, off in parse_events(out) if started is None or started >= since - lag]
        rq = Path(os.environ.get("LOCALAPPDATA", ""), "REDEngine/ReportQueue")
        reports = [p for p in rq.glob("*") if _mtime(p) >= since] if rq.is_dir() else []
        dump = next((d for rep in reports for f in rep.rglob("*.dmp") if (d := read_dump(f))), {})
        hit = next((e for e in events if e[0]), None)
        module = hit[0] if hit else dump.get("module")
        fault = fault_id(*hit) if hit and hit[1] is not None else dump.get("fault") if module == dump.get("module") else None
        crashed = bad_exit or bool(events or reports)
        if module or attempt == tries - 1 or (not crashed and attempt >= 1):
            return {"crashed": crashed, "module": module, "report": str(reports[0]) if reports else None, "exit_code": exit_code,
                    "code": dump.get("code") or (exit_code if bad_exit else None), "stack": dump.get("stack", []),
                    "places": dump.get("places", {}), "fault": fault}
        time.sleep(2.5)
