"""Cyberpunk 2077 AI Mod Manager. Run: python app.py  (Nexus starts it as: app.py "nxm://...")

Only one window runs. A second start (for example from a Nexus "Mod Manager Download" click)
hands its nxm:// link to the running window over a per-user named pipe and exits. Both sides
prove they know a random token that the running window writes to the user's own data folder,
so other users cannot send links to it or pose as it."""
import getpass
import hashlib
import secrets
import sys
import threading
import time
from multiprocessing import AuthenticationError
from multiprocessing.connection import Client, Listener
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


def hand_over(msg, timeout=5):
    """Give msg to the running window. True when it took it. A pipe holder that never answers cannot
    block start-up: the attempt runs on a helper thread and counts as failed after `timeout` seconds."""
    done = []

    def send():
        try:
            with Client(PIPE, family="AF_PIPE", authkey=TOKEN.read_bytes().strip()) as c:
                c.send_bytes(msg.encode())
            done.append(True)
        except (OSError, EOFError, AuthenticationError):
            pass

    t = threading.Thread(target=send, daemon=True)
    t.start()
    t.join(timeout)
    return bool(done)


def main():
    url = next((a for a in sys.argv[1:] if a.lower().startswith("nxm://")), None)
    if hand_over(url or "show"):
        return
    for _ in range(20):  # about 10 s: a window that is starting (its token appears) or closing (the pipe frees)
        try:
            server = Listener(PIPE, family="AF_PIPE")  # fails while any window of ours (or anyone) holds the name
            break
        except OSError:
            time.sleep(0.5)
            if hand_over(url or "show"):
                return
    else:
        import ctypes
        ctypes.windll.user32.MessageBoxW(None, "The mod manager is still running and did not answer. If it is closing, try "
                                               "again in a moment. If it runs as administrator, close it and start it normally.",
                                         "CP2077 AI Mod Manager", 0x30)
        return
    token = secrets.token_hex(16).encode()  # written only once this process owns the pipe
    TOKEN.parent.mkdir(parents=True, exist_ok=True)
    TOKEN.write_bytes(token)
    from manager_ui.ui import run
    run(server, url, register_nxm, nxm_registered, token)


if __name__ == "__main__":
    main()
