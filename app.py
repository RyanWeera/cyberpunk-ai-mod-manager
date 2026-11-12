"""Cyberpunk 2077 AI Mod Manager. Run: python app.py  (Nexus starts it as: app.py "nxm://...";
AI apps start it as: app.py --mcp)

Only one window runs. A second start (for example from a Nexus "Mod Manager Download" click)
hands its nxm:// link to the running window over a per-user named pipe and exits. Only the
account that owns the pipe can open it, and both sides prove they know a random token that the
running window writes to the user's own data folder, so other users cannot send links to it or
pose as it."""
import _winapi
import ctypes
import getpass
import hashlib
import json
import os
import secrets
import subprocess
import sys
import threading
import time
from ctypes import wintypes
from multiprocessing import AuthenticationError
from multiprocessing.connection import BUFSIZE, Client, Listener, PipeListener
from pathlib import Path

from shared.store import DATA

PIPE = r"\\.\pipe\CP2077ModManager-" + hashlib.sha256(getpass.getuser().encode()).hexdigest()[:16]
TOKEN = DATA / "ipc.token"
NXM_KEY = r"Software\Classes\nxm\shell\open\command"


def nxm_command():
    if getattr(sys, "frozen", False):
        return f'"{sys.executable}" "%1"'
    py = Path(sys.executable)
    pyw = py.with_name("pythonw.exe") if py.with_name("pythonw.exe").exists() else py
    return f'"{pyw}" "{Path(__file__).resolve()}" "%1"'


def register_nxm():
    """Make Windows open nxm:// links with this app (current user only, no admin)."""
    import winreg
    with winreg.CreateKey(winreg.HKEY_CURRENT_USER, r"Software\Classes\nxm") as k:
        winreg.SetValueEx(k, "", 0, winreg.REG_SZ, "URL:NXM Protocol")
        winreg.SetValueEx(k, "URL Protocol", 0, winreg.REG_SZ, "")
    with winreg.CreateKey(winreg.HKEY_CURRENT_USER, NXM_KEY) as k:
        winreg.SetValueEx(k, "", 0, winreg.REG_SZ, nxm_command())


def nxm_registered():
    import winreg
    try:
        with winreg.OpenKey(winreg.HKEY_CURRENT_USER, NXM_KEY) as k:
            return winreg.QueryValueEx(k, "")[0] == nxm_command()
    except OSError:
        return False


class _SA(ctypes.Structure):  # SECURITY_ATTRIBUTES
    _fields_ = [("nLength", wintypes.DWORD), ("lpSecurityDescriptor", ctypes.c_void_p), ("bInheritHandle", wintypes.BOOL)]


class _OwnerOnlyPipe(PipeListener):
    """PipeListener whose pipe only its owner (the account that started the window) can open. The default
    also lets every other account and anonymous users open it for reading and hold a connection slot."""

    def __init__(self, address):
        sd = ctypes.c_void_p()  # kept for the life of the process
        if not ctypes.windll.advapi32.ConvertStringSecurityDescriptorToSecurityDescriptorW("D:P(A;;GA;;;OW)", 1,
                                                                                         ctypes.byref(sd), None):
            raise ctypes.WinError()
        self._sa = _SA(ctypes.sizeof(_SA), sd, False)
        super().__init__(address)

    def _new_handle(self, first=False):  # PipeListener._new_handle with the security attributes above
        flags = _winapi.PIPE_ACCESS_DUPLEX | _winapi.FILE_FLAG_OVERLAPPED
        if first:
            flags |= _winapi.FILE_FLAG_FIRST_PIPE_INSTANCE
        return _winapi.CreateNamedPipe(self._address, flags,
                                       _winapi.PIPE_TYPE_MESSAGE | _winapi.PIPE_READMODE_MESSAGE | _winapi.PIPE_WAIT,
                                       _winapi.PIPE_UNLIMITED_INSTANCES, BUFSIZE, BUFSIZE, _winapi.NMPWAIT_WAIT_FOREVER,
                                       ctypes.addressof(self._sa))


class OwnerListener(Listener):
    """Listener(address, family="AF_PIPE") on an owner-only pipe."""

    def __init__(self, address):
        self._listener, self._authkey = _OwnerOnlyPipe(address), None


class HandOver(threading.Thread):
    """One attempt to give msg to the running window, on a helper thread so that a pipe holder that never
    answers cannot block start-up. `ok` turns True only when the window confirms it took the link."""

    def __init__(self, msg):
        super().__init__(daemon=True)
        self.msg, self.ok = msg, False
        self.start()

    def run(self):
        try:
            with Client(PIPE, family="AF_PIPE", authkey=TOKEN.read_bytes().strip()) as c:
                c.send_bytes(self.msg.encode())
                self.ok = c.recv_bytes(16) == b"ok"
        except (OSError, EOFError, AuthenticationError):
            pass


def ask_window(tool, args, wait=40):
    """Run one AI-app tool in the window and return its text. A window that is not running is started first.
    Only connecting is retried: a command that was sent is never sent twice."""
    deadline, started = time.monotonic() + wait, False
    while True:
        try:
            c = Client(PIPE, family="AF_PIPE", authkey=TOKEN.read_bytes().strip())
            break
        except (OSError, EOFError, AuthenticationError) as e:
            if time.monotonic() > deadline:
                raise RuntimeError("The mod manager window did not start or did not answer. Start it normally (not as "
                                   "administrator), or wait until it is free, then try again.") from None
            # no pipe or no token: no window yet. A busy pipe or a refused token means one runs, and a second copy
            # would only show the "still running" message.
            if not started and isinstance(e, FileNotFoundError):
                started = True
                cmd = [sys.executable] if getattr(sys, "frozen", False) else [sys.executable, str(Path(__file__).resolve())]
                # a new copy of the one-file exe, not one that shares this process's unpacked files: else this
                # process cannot clean them up and exit while the window runs
                subprocess.Popen(cmd, creationflags=subprocess.DETACHED_PROCESS, close_fds=True,
                                 env={**os.environ, "PYINSTALLER_RESET_ENVIRONMENT": "1"},
                                 stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            time.sleep(0.5)
    with c:
        c.send_bytes(json.dumps({"tool": tool, "args": args}).encode())
        r = json.loads(c.recv_bytes())
    if "error" in r:
        raise RuntimeError(r["error"])
    return r["text"]


def main(wait=15):
    if "--mcp" in sys.argv[1:]:  # started by an AI app: no window here, just the MCP server
        if sys.stdin:
            from ai_link import mcp
            from shared.store import VERSION
            mcp.serve(ask_window, VERSION)
        return
    url = next((a for a in sys.argv[1:] if a.lower().startswith("nxm://")), None)
    deadline = time.monotonic() + wait  # a window that is starting (token appears) or closing (pipe frees)
    attempt, started = HandOver(url or "show"), time.monotonic()
    while True:
        attempt.join(0.5)
        done = not attempt.is_alive()  # read once: an attempt that ends between two reads would be started again
        if done:  # only one attempt at a time, so a link is delivered at most once
            if attempt.ok:
                return
            try:
                server = OwnerListener(PIPE)  # fails while any window of ours (or anyone) holds the name
                break
            except OSError:
                pass
        if time.monotonic() > deadline:
            attempt.join(1)  # one last chance for an answer that is on its way
            if attempt.ok:
                return
            ctypes.windll.user32.MessageBoxW(None, "The mod manager is still running and did not answer. If it is closing, "
                                                   "try again in a moment. If it runs as administrator, close it and start it "
                                                   "normally.", "CP2077 AI Mod Manager", 0x30)
            return
        if done:
            time.sleep(max(0.0, started + 0.5 - time.monotonic()))  # at most 2 tries a second, never a busy loop
            attempt, started = HandOver(url or "show"), time.monotonic()
    token = secrets.token_hex(16).encode()  # written only once this process owns the pipe
    TOKEN.parent.mkdir(parents=True, exist_ok=True)
    TOKEN.write_bytes(token)
    from manager_ui.ui import run
    run(server, url, register_nxm, nxm_registered, token)


if __name__ == "__main__":
    main()
