"""Watch for the game process, and after it exits collect crash evidence: its exit code, Windows crash
events, and REDEngine crash reports (whose minidump names the module that crashed)."""
import ctypes
import os
import re
import struct
import subprocess
import time
from pathlib import Path

EXE = "Cyberpunk2077.exe"
NO_WINDOW = getattr(subprocess, "CREATE_NO_WINDOW", 0)
SYS32 = Path(os.environ.get("SystemRoot", r"C:\Windows")) / "System32"  # full paths: Windows looks next to our exe first
SYNCHRONIZE, QUERY_LIMITED = 0x00100000, 0x1000
WAIT_TIMEOUT = 0x102


def find_pid():
    try:
        r = subprocess.run([str(SYS32 / "tasklist.exe"), "/FI", f"IMAGENAME eq {EXE}", "/NH", "/FO", "CSV"], capture_output=True, text=True,
                           errors="replace", creationflags=NO_WINDOW, timeout=60)
    except subprocess.TimeoutExpired:
        return None
    m = re.search(rf'"{re.escape(EXE)}","(\d+)"', r.stdout, re.I)
    return int(m[1]) if m else None


def watch(on_start, on_exit, stop, interval=5):
    """Call on_start(t) when the game starts and on_exit(t_started, exit_code) when it stops, until stop is set.
    exit_code is None when Windows would not let us open the process."""
    k32 = ctypes.windll.kernel32
    k32.OpenProcess.restype = ctypes.c_void_p
    k32.WaitForSingleObject.argtypes = [ctypes.c_void_p, ctypes.c_uint32]
    k32.GetExitCodeProcess.argtypes = [ctypes.c_void_p, ctypes.POINTER(ctypes.c_uint32)]
    k32.CloseHandle.argtypes = [ctypes.c_void_p]
    while not stop.wait(interval):
        try:
            pid = find_pid()
            if not pid:
                continue
            h = k32.OpenProcess(SYNCHRONIZE | QUERY_LIMITED, False, pid)
            started, code = time.time(), None
            on_start(started)
            if h:
                while k32.WaitForSingleObject(h, 1000) == WAIT_TIMEOUT and not stop.is_set():
                    pass
                c = ctypes.c_uint32()
                if k32.GetExitCodeProcess(h, ctypes.byref(c)):
                    code = c.value
                k32.CloseHandle(h)
            else:
                while find_pid() == pid and not stop.wait(interval):
                    pass
            on_exit(started, code)
        except Exception:  # noqa: BLE001 - one bad cycle (a report deleted mid-read, a failed save) must not end crash watching
            continue


def parse_events(text):
    """[(faulting module path or '' for hangs, process start as unix time or None)] for game crash events
    in `wevtutil /f:text` output."""
    out = []
    for block in text.split("Event[")[1:]:
        if f"application name: {EXE.lower()}" in block.lower() or f"program {EXE.lower()}" in block.lower():
            m = re.search(r"Faulting module path:\s*(.+)", block)
            s = re.search(r"(?:application start time|Start Time):\s*(?:0x)?([0-9a-fA-F]{8,})", block)
            started = (int(s[1], 16) - 116444736000000000) / 1e7 if s else None  # FILETIME -> unix
            out.append((m[1].strip() if m else "", started))
    return out


def dump_module(path):
    """Path of the module the crash happened in, read from a minidump (.dmp), or None."""
    try:
        b = Path(path).read_bytes()
        sig, _, n, rva = struct.unpack_from("<4sIII", b)
        if sig != b"MDMP":
            return None
        streams = {t: off for t, _, off in (struct.unpack_from("<III", b, rva + 12 * i) for i in range(n))}
        if 6 not in streams or 4 not in streams:  # 6 = exception, 4 = module list
            return None
        addr = struct.unpack_from("<Q", b, streams[6] + 24)[0]  # ThreadId, pad, code, flags, record, address
        count = struct.unpack_from("<I", b, streams[4])[0]
        for i in range(count):
            base, size, _, _, name_rva = struct.unpack_from("<QIIII", b, streams[4] + 4 + 108 * i)
            if base <= addr < base + size:
                length = struct.unpack_from("<I", b, name_rva)[0]
                return b[name_rva + 4:name_rva + 4 + length].decode("utf-16-le")
    except (OSError, struct.error, UnicodeDecodeError):
        return None
    return None


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
        events = [mod for mod, started in parse_events(out) if started is None or started >= since - lag]
        rq = Path(os.environ.get("LOCALAPPDATA", ""), "REDEngine/ReportQueue")
        reports = [p for p in rq.glob("*") if _mtime(p) >= since] if rq.is_dir() else []
        module = next((e for e in events if e), None) or next(
            (m for rep in reports for d in rep.rglob("*.dmp") if (m := dump_module(d))), None)
        crashed = bad_exit or bool(events or reports)
        if module or attempt == tries - 1 or (not crashed and attempt >= 1):
            return {"crashed": crashed, "module": module, "report": str(reports[0]) if reports else None, "exit_code": exit_code}
        time.sleep(2.5)
