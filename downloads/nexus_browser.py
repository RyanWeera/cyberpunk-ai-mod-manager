"""Hidden browser that clicks "Slow download" on Nexus for free accounts (owner decision 2026-11-20,
Settings > "Click 'Slow download' for me"). One browser process with its own profile folder keeps the Nexus
login. For every waiting file it opens the file's page, clicks "Slow download" and picks up the nxm:// link
the site answers with, so no file needs a click. Nexus's terms disallow automating that click, so any failure
falls back to the manual way: the manager opens the page and the user clicks.

Standard library only. The browser is Edge (Chromium) with a hidden window, driven over its DevTools protocol:
a tab is opened through /json/new, the page is read and clicked with Runtime.evaluate, and the nxm:// link is
caught from the DevTools event stream (the site answers the click with a redirect whose Location is nxm://...).
"""
import base64
import hashlib
import json
import os
import socket
import struct
import subprocess
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

# Full paths only (never shutil.which on Windows: it looks in the current folder first).
BROWSERS = [("ProgramFiles(x86)", r"Microsoft\Edge\Application\msedge.exe"), ("ProgramFiles", r"Microsoft\Edge\Application\msedge.exe"),
            ("LocalAppData", r"Microsoft\Edge\Application\msedge.exe"), ("ProgramFiles", r"Google\Chrome\Application\chrome.exe"),
            ("ProgramFiles(x86)", r"Google\Chrome\Application\chrome.exe"), ("LocalAppData", r"Google\Chrome\Application\chrome.exe")]
FLAGS = ["--no-first-run", "--no-default-browser-check", "--disable-gpu", "--lang=en-US",
         "--remote-debugging-port=0", "--window-size=1200,900"]
GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"
LOGIN_JS = "(function(){return document.querySelector('a[href*=\"users/login\"]')?1:0})()"
HOOK_JS = ("(function(){if(window.__nxmHook)return 1;window.__nxmHook=1;window.__nxm='';var re=/^nxm:/i;"
           "var cap=function(u){if(u&&re.test(u)&&!window.__nxm)window.__nxm=u;};var o=window.open;"
           "window.open=function(u){cap(u);return o?o.apply(window,arguments);};"
           "document.addEventListener('click',function(e){var a=e.target&&e.target.closest?e.target.closest('a[href]'):null;"
           "if(a&&re.test(a.getAttribute('href')||''))cap(a.href);},true);return 1;})()")
CLICK_JS = ("(function(){var els=document.querySelectorAll('a,button,input[type=\"button\"],input[type=\"submit\"]');"
            "for(var i=0;i<els.length;i++){var e=els[i],t=e.textContent||e.value||'';"
            "if(/slow\\s*download/i.test(t)&&e.offsetParent){e.scrollIntoView({block:'center'});e.click();return 1;}}return 0;})()")
POLL_JS = "(function(){return window.__nxm||''})()"
READY_JS = "document.readyState"


class NeedLogin(Exception):
    """The Nexus page shows a login link, so the hidden click cannot work until the user logs in once."""


def find_browser():
    """Path of Edge, or Chrome as a fallback, or "" when neither is installed."""
    for var, tail in BROWSERS:
        p = Path(os.environ.get(var, "")) / tail
        if p.exists():
            return str(p)
    return ""


def scan_nxm(x):
    """The first string in a JSON value that starts with nxm:// (DevTools events nest it anywhere), or ""."""
    if isinstance(x, str):
        return x if x.startswith("nxm://") else ""
    if isinstance(x, dict):
        return next((s for v in x.values() if (s := scan_nxm(v))), "")
    if isinstance(x, list):
        return next((s for v in x if (s := scan_nxm(v))), "")
    return ""


def ws_frame(payload, opcode=1):
    """One masked client WebSocket frame (RFC 6455: clients must mask)."""
    n, head, mask = len(payload), bytes([0x80 | opcode]), os.urandom(4)
    if n < 126:
        head += bytes([0x80 | n])
    elif n <= 0xFFFF:
        head += bytes([0x80 | 126]) + struct.pack(">H", n)
    else:
        head += bytes([0x80 | 127]) + struct.pack(">Q", n)
    return head + mask + bytes(b ^ mask[i % 4] for i, b in enumerate(payload))


def ws_message(buf):
    """((opcode, fin, payload), rest) of one complete frame in buf, (None, buf) while it is incomplete.
    Masks are honoured when set, so the same parser reads client and server frames."""
    if len(buf) < 2:
        return None, buf
    op, second = buf[0] & 0x0F, buf[1]
    fin, n, pos = bool(second & 0x80), second & 0x7F, 2
    if n == 126:
        if len(buf) < 4:
            return None, buf
        n, pos = struct.unpack(">H", buf[2:4])[0], 4
    elif n == 127:
        if len(buf) < 10:
            return None, buf
        n, pos = struct.unpack(">Q", buf[2:10])[0], 10
    masked = bool(second & 0x80)
    end = pos + n + (4 if masked else 0)
    if len(buf) < end:
        return None, buf
    if masked:
        mask = buf[pos:pos + 4]
        pos += 4
        data = bytes(b ^ mask[i % 4] for i, b in enumerate(buf[pos:pos + n]))
    else:
        data = buf[pos:pos + n]
    return (op, fin, data), buf[end:]


class WS:
    """A small WebSocket client for the DevTools protocol: text both ways, pings answered."""

    def __init__(self, url):
        u = urllib.parse.urlparse(url)
        self.sock = socket.create_connection((u.hostname, u.port), timeout=10)
        self.key = base64.b64encode(os.urandom(16)).decode()
        path = u.path + (f"?{u.query}" if u.query else "")
        req = (f"GET {path} HTTP/1.1\r\nHost: {u.hostname}:{u.port}\r\nUpgrade: websocket\r\n"
               f"Connection: Upgrade\r\nSec-WebSocket-Key: {self.key}\r\nSec-WebSocket-Version: 13\r\n\r\n")
        self.sock.sendall(req.encode())
        head = b""
        while b"\r\n\r\n" not in head:
            part = self.sock.recv(4096)
            if not part:
                raise ConnectionError("the browser closed the WebSocket handshake")
            head += part
        if b" 101 " not in head.split(b"\r\n", 1)[0]:
            raise ConnectionError("the browser refused the WebSocket connection")
        want = base64.b64encode(hashlib.sha1((self.key + GUID).encode()).digest()).decode()
        if want.encode() not in head:
            raise ConnectionError("the browser answered a wrong WebSocket key")
        self.buf, self.parts = head.split(b"\r\n\r\n", 1)[1], []

    def send(self, text):
        self.sock.sendall(ws_frame(text.encode()))

    def recv(self, timeout=0.5):
        """One text message, or None when nothing arrived in `timeout` seconds. Fragmented messages are
        collected; pings are answered and skipped; a close ends with ConnectionError."""
        self.sock.settimeout(timeout)
        while True:
            try:
                while True:
                    got, self.buf = ws_message(self.buf)
                    if got is None:
                        break
                    op, fin, data = got
                    if op == 0x9:  # ping: answer with a pong, then keep reading
                        self.sock.sendall(ws_frame(data, 0xA))
                        continue
                    if op == 0x8:
                        raise ConnectionError("the browser closed the WebSocket")
                    if op in (0x1, 0x2):
                        self.parts = [data]
                    elif op == 0 and self.parts:
                        self.parts.append(data)
                    if self.parts and fin and op in (0x1, 0x2, 0):
                        text = b"".join(self.parts).decode("utf-8", "replace")
                        self.parts = []
                        return text
            except TimeoutError:
                return None

    def close(self):
        try:
            self.sock.sendall(ws_frame(b"", 0x8))
            self.sock.close()
        except OSError:
            pass


class Tab:
    """One page in the browser: DevTools calls and the events they produce."""

    def __init__(self, browser, target):
        self.browser, self.target = browser, target
        self.ws = WS(target["webSocketDebuggerUrl"])
        self.events, self.next_id = [], 0

    def call(self, method, timeout=15, **params):
        """Send one DevTools command and return its result; a protocol error raises RuntimeError."""
        self.next_id += 1
        self.ws.send(json.dumps({"id": self.next_id, "method": method, "params": params}))
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            m = self.ws.recv(0.5)
            if m is None:
                continue
            d = json.loads(m)
            if d.get("id") == self.next_id:
                if "error" in d:
                    raise RuntimeError(d["error"].get("message", "DevTools error"))
                return d.get("result", {})
            self.events.append(d)
        raise RuntimeError(f"{method} timed out")

    def pump(self, timeout=0.5):
        m = self.ws.recv(timeout)
        if m is not None:
            self.events.append(json.loads(m))

    def run(self, js):
        """Evaluate js and return its value (returned by value, so JSON-safe results only)."""
        r = self.call("Runtime.evaluate", expression=js, returnByValue=True)
        if r.get("exceptionDetails"):
            raise RuntimeError(r["exceptionDetails"].get("exception", {}).get("description", "the page script failed"))
        return r.get("result", {}).get("value")

    def wait_ready(self, seconds=45):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            try:
                if self.run(READY_JS) == "complete":
                    return
            except RuntimeError:  # context reset while the page loads: keep waiting
                pass
            self.pump()
        raise RuntimeError("the Nexus page did not finish loading")

    def wait_nxm(self, seconds=90):
        """The nxm:// link the site answers the click with, from any DevTools event or the page hook, or None."""
        deadline = time.monotonic() + seconds
        nxt = 0.0
        while time.monotonic() < deadline:
            self.pump()
            for e in self.events:
                if s := scan_nxm(e.get("params", {})):
                    return s
            if time.monotonic() > nxt:  # the hook in the page caught the link itself
                nxt = time.monotonic() + 1
                try:
                    if s := self.run(POLL_JS):
                        return s
                except RuntimeError:
                    pass
        return None


class Browser:
    """One browser process on its own profile folder. `visible` is for the one-time Nexus login."""

    def __init__(self, profile, visible=False):
        self.exe, self.profile, self.visible = find_browser(), Path(profile), visible
        self.proc, self.port = None, 0

    def alive(self):
        return self.proc is not None and self.proc.poll() is None

    def start(self, url="about:blank"):
        if not self.exe:
            raise RuntimeError("no Edge or Chrome found on this PC")
        self.profile.mkdir(parents=True, exist_ok=True)
        args = [self.exe] + ([] if self.visible else ["--headless=new"]) + FLAGS + [f"--user-data-dir={self.profile}", url]
        self.proc = subprocess.Popen(args, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                                     creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0))
        port_file = self.profile / "DevToolsActivePort"
        deadline = time.monotonic() + 20
        while time.monotonic() < deadline:
            if not self.alive():
                raise RuntimeError("the browser quit right after starting")
            if port_file.exists():
                self.port = int(port_file.read_text().split()[0])
                return self
            time.sleep(0.2)
        self.quit()
        raise RuntimeError("the browser did not open its DevTools port")

    def _http(self, path, method="GET", timeout=5):
        req = urllib.request.Request(f"http://127.0.0.1:{self.port}{path}", method=method)
        return json.loads(urllib.request.urlopen(req, timeout=timeout).read().decode())

    def open(self, url):
        """A new Tab loading `url` (PUT /json/new; older builds only take GET)."""
        q = urllib.parse.quote(url, safe="")
        try:
            target = self._http(f"/json/new?{q}", method="PUT")
        except urllib.error.HTTPError:
            target = self._http(f"/json/new?{q}")
        tab = Tab(self, target)
        tab.call("Page.enable")
        tab.call("Network.enable")
        return tab

    def pages(self):
        return [t for t in self._http("/json/list") if t.get("type") == "page"]

    def wait_login(self, seconds=300):
        """True when an open page no longer shows a login link. The window closing or a timeout is False."""
        deadline, tab = time.monotonic() + seconds, None
        while time.monotonic() < deadline:
            if not self.alive():
                return False
            try:
                if tab is None and (pages := self.pages()):
                    tab = Tab(self, pages[0])
                if tab and tab.run(LOGIN_JS) == 0:
                    return True
            except (OSError, RuntimeError):  # still loading, or the tab went away: look again
                if tab:
                    tab.ws.close()
                tab = None
            time.sleep(2)
        return False

    def close(self, tab):
        try:
            self._http(f"/json/close/{tab.target['id']}", timeout=3)
        except (OSError, urllib.error.URLError):
            pass
        tab.ws.close()

    def quit(self):
        if self.proc:
            self.proc.terminate()
            try:
                self.proc.wait(5)
            except subprocess.TimeoutExpired:
                subprocess.run([os.path.join(os.environ.get("SystemRoot", r"C:\Windows"), "System32", "taskkill.exe"),
                                "/PID", str(self.proc.pid), "/T", "/F"], capture_output=True)
        self.proc, self.port = None, 0


class Clicker:
    """The Nexus click queue's hidden browser. Wired by the controller: log() shows text in the window,
    on_link(link) continues a download, on_fail(job, why) falls back to the manual click, waiting(job) says
    whether the job still wants a click (the user may have skipped it while the page was loading)."""

    def __init__(self, profile, log=lambda m: None, on_link=lambda link: None, on_fail=lambda j, why: None,
                 waiting=lambda j: True, login_wait=300):
        self.profile, self.log, self.on_link = Path(profile), log, on_link
        self.on_fail, self.waiting, self.login_wait = on_fail, waiting, login_wait
        self.browser = None

    def _hidden(self):
        if self.browser and self.browser.alive():
            return self.browser
        if self.browser:
            self.browser.quit()
        self.browser = Browser(self.profile).start()
        return self.browser

    def click(self, job):
        """Run on a worker thread: click for one waiting job. Never raises; any failure goes to on_fail."""
        from downloads import nexus  # late import: this module stays usable without the rest of the app
        url = nexus.files_page(job["src"]["mod_id"], job["src"]["file_id"])
        try:
            try:
                link = self._try(url)
            except NeedLogin:
                if not self._login(url):
                    return self.on_fail(job, "the Nexus login window was closed or timed out")
                link = self._try(url)
        except Exception as e:  # noqa: BLE001 - boundary: the manual click always stays possible
            return self.on_fail(job, str(e))
        if self.waiting(job):
            self.on_link(link)

    def _try(self, url):
        """Open the file's page hidden, click, and return the nxm:// link."""
        b = self._hidden()
        tab = b.open(url)
        try:
            tab.wait_ready()
            if tab.run(LOGIN_JS):
                raise NeedLogin()
            tab.run(HOOK_JS)
            if not tab.run(CLICK_JS):
                raise RuntimeError("no 'Slow download' button was found on the page")
            if link := tab.wait_nxm():
                return link
            raise RuntimeError("Nexus gave no download link after the click")
        finally:
            b.close(tab)

    def _login(self, url):
        """One visible window on the same profile so the user can log in once; the login is kept."""
        self.log("To click by myself I need your Nexus login: a browser window is opening - log in there once, "
                 "and this app clicks every download by itself from then on.")
        if self.browser:
            self.browser.quit()  # a profile folder only belongs to one running browser
        try:
            b = Browser(self.profile, visible=True).start(url)
        except Exception as e:  # noqa: BLE001 - boundary: the manual click stays possible
            self.browser = None
            self.log(f"Could not open the login window: {e}")
            return False
        try:
            return b.wait_login(self.login_wait)
        finally:
            b.quit()
            self.browser = None  # a fresh hidden browser picks up the saved login

    def close(self):
        if self.browser:
            self.browser.quit()
            self.browser = None
