"""Cyberpunk 2077 AI Mod Manager. Run: python app.py  (Nexus starts it as: app.py "nxm://...")

Only one window runs. A second start (for example from a Nexus "Mod Manager Download" click)
hands its nxm:// link to the running window over a per-user named pipe and exits. Both sides
prove they know a random token that the running window writes to the user's own data folder,
so other users cannot send links to it or pose as it."""
import getpass
import hashlib
import secrets
import sys
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


def hand_over(msg):
    """Give msg to the running window. True when it took it."""
    try:
        with Client(PIPE, family="AF_PIPE", authkey=TOKEN.read_bytes().strip()) as c:
            c.send_bytes(msg.encode())
        return True
    except (OSError, EOFError, AuthenticationError):
        return False


def main():
    url = next((a for a in sys.argv[1:] if a.lower().startswith("nxm://")), None)
    if hand_over(url or "show"):
        return
    try:
        server = Listener(PIPE, family="AF_PIPE")  # fails while any window of ours (or anyone) holds the name
    except OSError:
        for _ in range(6):  # a window that is just starting: its token appears in a moment
            time.sleep(0.5)
            if hand_over(url or "show"):
                return
        import ctypes
        ctypes.windll.user32.MessageBoxW(None, "The mod manager is already running, but it did not answer. If it runs as "
                                               "administrator, close it and start it normally.", "CP2077 AI Mod Manager", 0x30)
        return
    token = secrets.token_hex(16).encode()  # written only once this process owns the pipe
    TOKEN.parent.mkdir(parents=True, exist_ok=True)
    TOKEN.write_bytes(token)
    from manager_ui.ui import run
    run(server, url, register_nxm, nxm_registered, token)


if __name__ == "__main__":
    main()
