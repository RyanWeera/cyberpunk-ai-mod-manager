"""Stand-in for Cyberpunk2077.exe in tests/e2e_crash.py. Usage: fake_game.py clean|crash [mod.dll]
Runs 8 s, then exits normally or crashes inside the mod DLL (strchr on address 8, on a native thread).
Like the game's own crash reporter, it writes a minidump to %LOCALAPPDATA%\\REDEngine\\ReportQueue\\<id>\\."""
import ctypes as c
import ctypes.wintypes as w
import os
import sys
import time
import uuid
from pathlib import Path

k32, dbg = c.windll.kernel32, c.windll.dbghelp
k32.SetErrorMode(3)  # no Windows error dialog
time.sleep(8)
if sys.argv[1] == "clean":
    sys.exit(0)

k32.GetCurrentProcess.restype = k32.CreateFileW.restype = c.c_void_p
k32.CloseHandle.argtypes = [c.c_void_p]
k32.SetUnhandledExceptionFilter.argtypes = [c.c_void_p]
k32.CreateThread.argtypes = [c.c_void_p, c.c_size_t, c.c_void_p, c.c_void_p, w.DWORD, c.c_void_p]
dbg.MiniDumpWriteDump.argtypes = [c.c_void_p, w.DWORD, c.c_void_p, c.c_int, c.c_void_p, c.c_void_p, c.c_void_p]


class ExceptionInfo(c.Structure):  # MINIDUMP_EXCEPTION_INFORMATION, 4-byte packed in dbghelp.h
    _pack_ = 4
    _fields_ =[("ThreadId", w.DWORD), ("ExceptionPointers", c.c_void_p), ("ClientPointers", w.BOOL)]


@c.WINFUNCTYPE(c.c_long, c.c_void_p)
def reporter(pointers):
    d = Path(os.environ["LOCALAPPDATA"], "REDEngine/ReportQueue", uuid.uuid4().hex)
    d.mkdir(parents=True)
    f = k32.CreateFileW(str(d / "minidump.dmp"), 0x40000000, 0, None, 2, 0x80, None)
    info = ExceptionInfo(k32.GetCurrentThreadId(), pointers, False)
    dbg.MiniDumpWriteDump(k32.GetCurrentProcess(), k32.GetCurrentProcessId(), f, 0, c.byref(info), None, None)
    k32.CloseHandle(f)
    return 1  # EXCEPTION_EXECUTE_HANDLER: end the process with the exception code


k32.SetUnhandledExceptionFilter(c.cast(reporter, c.c_void_p))
strchr = c.cast(c.CDLL(sys.argv[2]).strchr, c.c_void_p)
k32.CreateThread(None, 0, strchr, c.c_void_p(8), 0, None)
time.sleep(30)
