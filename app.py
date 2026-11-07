"""Cyberpunk 2077 AI Mod Manager. Run: python app.py  (Nexus starts it as: app.py "nxm://...")

Only one window runs. A second start (for example from a Nexus "Mod Manager Download" click)
hands its nxm:// link to the running window over a localhost socket and exits. Every message
starts with a random token that the running window writes to the user's own data folder, so
other users and web pages cannot send it links."""
import secrets
import socket
import sys
from pathlib import Path

from shared.store import DATA

PORT = 47077
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


def main():
    url = next((a for a in sys.argv[1:] if a.lower().startswith("nxm://")), None)
    try:
        with socket.create_connection(("127.0.0.1", PORT), timeout=1) as s:
            s.sendall(f"{TOKEN.read_text('utf-8').strip()} {url or 'show'}".encode())
        return
    except OSError:
        pass
    server = socket.create_server(("127.0.0.1", PORT))
    token = secrets.token_hex(16)
    TOKEN.parent.mkdir(parents=True, exist_ok=True)
    TOKEN.write_text(token, "utf-8")
    from manager_ui.ui import run
    run(server, url, register_nxm, nxm_registered, token)


if __name__ == "__main__":
    main()
