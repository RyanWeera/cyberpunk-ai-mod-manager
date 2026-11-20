//! Hidden browser that clicks "Slow download" on Nexus for free accounts (owner decision 2026-11-20,
//! Settings > "Click 'Slow download' for me"). One browser process with its own profile folder keeps the Nexus
//! login. For every waiting file it opens the file's page, clicks "Slow download" and picks up the nxm:// link
//! the site answers with, so no file needs a click. Nexus's terms disallow automating that click, so any failure
//! falls back to the manual way: the manager opens the page and the user clicks.
//!
//! The browser is Brave, Edge or Chrome (all Chromium) with a hidden window, driven over its DevTools protocol:
//! a tab is opened through /json/new, the page is read and clicked with Runtime.evaluate, and the nxm:// link is
//! caught from the DevTools event stream (the site answers the click with a redirect whose Location is nxm://...).
use crate::fetch::{self, Request, quote, sha1};
use crate::nexus;
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

// Full paths only (never a PATH or current-folder lookup). Brave comes first:
// Edge is on every Windows PC, so a user who installed Brave chose it.
pub const BRAVE: &str = r"BraveSoftware\Brave-Browser\Application\brave.exe";
pub const BROWSERS: [(&str, &str); 9] = [
    ("ProgramFiles", BRAVE),
    ("ProgramFiles(x86)", BRAVE),
    ("LocalAppData", BRAVE),
    ("ProgramFiles(x86)", r"Microsoft\Edge\Application\msedge.exe"),
    ("ProgramFiles", r"Microsoft\Edge\Application\msedge.exe"),
    ("LocalAppData", r"Microsoft\Edge\Application\msedge.exe"),
    ("ProgramFiles", r"Google\Chrome\Application\chrome.exe"),
    ("ProgramFiles(x86)", r"Google\Chrome\Application\chrome.exe"),
    ("LocalAppData", r"Google\Chrome\Application\chrome.exe"),
];
pub const FLAGS: [&str; 6] = [
    "--no-first-run",
    "--no-default-browser-check",
    "--disable-gpu",
    "--lang=en-US",
    "--remote-debugging-port=0",
    "--window-size=1200,900",
];
pub const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
/// 1 when Nexus shows the page logged out (its download popup says so, or a sign-in link is on the page), 0 when the
/// popup says logged in, 2 when neither shows within 10 seconds (the page is still loading, or it is no file page).
pub const LOGIN_JS: &str = concat!(
    "(async function(){for(var n=0;n<40;n++){",
    "if(document.querySelector('mod-download-modal[user-is-logged-in=\"false\"],a[href*=\"users/login\"],a[href*=\"/auth/sign_in\"]'))return 1;",
    "if(document.querySelector('mod-download-modal[user-is-logged-in=\"true\"]'))return 0;",
    "await new Promise(function(r){setTimeout(r,250);});}return 2;})()"
);
/// For the login window: 1 when a nexusmods.com page (not Cloudflare's check, not the sign-in site) shows no
/// logged-out sign. After the login Nexus can show any page, and not every page has the download popup.
/// Before "complete" only the popup's logged-in flag counts: a script can still add the sign-in link.
pub const LOGGED_IN_JS: &str = concat!(
    "(function(){var h=location.hostname,s=document.readyState;if(s==='loading'||/^just a moment/i.test(document.title)",
    "||!/(^|\\.)nexusmods\\.com$/.test(h)||h==='users.nexusmods.com')return 0;",
    "if(document.querySelector('mod-download-modal[user-is-logged-in=\"false\"],a[href*=\"users/login\"],a[href*=\"/auth/sign_in\"]'))return 0;",
    "return s==='complete'||document.querySelector('mod-download-modal[user-is-logged-in=\"true\"]')?1:0;})()"
);
pub const HOOK_JS: &str = concat!(
    "(function(){if(window.__nxmHook)return 1;window.__nxmHook=1;window.__nxm='';var re=/^nxm:/i;",
    "var cap=function(u){if(u&&re.test(u)&&!window.__nxm)window.__nxm=u;};var o=window.open;",
    "window.open=function(u){cap(u);return o?o.apply(window,arguments):null;};",
    // composedPath also sees a link inside a shadow root (the click target is then the shadow host)
    "document.addEventListener('click',function(e){var p=e.composedPath?e.composedPath():[e.target];",
    "for(var i=0;i<p.length;i++){var a=p[i];if(a&&a.getAttribute&&re.test(a.getAttribute('href')||'')){cap(a.href);break;}}},true);",
    "return 1;})()"
);
/// Nexus draws its download popup as a web component (`mod-download-modal`), so the button is inside an open
/// shadow root and can appear after the page load: search shadow roots too, and wait up to 10 seconds.
pub const CLICK_JS: &str = concat!(
    "(async function(){var find=function(r){var els=r.querySelectorAll('*');for(var i=0;i<els.length;i++){var e=els[i];",
    "if(/^(A|BUTTON)$/.test(e.tagName)||(e.tagName==='INPUT'&&/^(button|submit)$/i.test(e.type))){var t=e.textContent||e.value||'';",
    "if(/slow\\s*download/i.test(t)&&e.getClientRects().length)return e;}",
    "if(e.shadowRoot){var f=find(e.shadowRoot);if(f)return f;}}return null;};",
    "for(var n=0;n<40;n++){var e=find(document);if(e){e.scrollIntoView({block:'center'});e.click();return 1;}",
    "await new Promise(function(r){setTimeout(r,250);});}return 0;})()"
);
pub const POLL_JS: &str = "(function(){return window.__nxm||''})()";
/// A new tab is a "complete" about:blank before it starts to load the page, so that does not count.
/// "check" while Cloudflare's "Just a moment..." page runs (it clears by itself in a normal browser window).
pub const READY_JS: &str =
    "location.href!=='about:blank'&&(/^just a moment/i.test(document.title)?'check':document.readyState)";
/// From one automatic click to the next: a big mod pack must not send Nexus a burst of clicks.
pub const GAP: Duration = Duration::from_secs(3);
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Why one hidden click did not give a link.
#[derive(Debug)]
pub enum TryError {
    /// The Nexus page shows a login link, so the hidden click cannot work until the user logs in once.
    NeedLogin,
    Failed(String),
}

impl From<String> for TryError {
    fn from(e: String) -> Self {
        TryError::Failed(e)
    }
}

/// The file name (no .exe) of the browser a Windows link-handler ProgId names: BraveHTML, MSEdgeHTM and
/// ChromeHTML (a second install adds a suffix; Beta, Dev and Canary IDs map to the stable browser), None for any
/// other browser.
pub fn browser_of(prog_id: &str) -> Option<&'static str> {
    let id = prog_id.to_ascii_lowercase();
    ["brave", "msedge", "chrome"].into_iter().find(|b| id.starts_with(b))
}

/// The user's default browser (the one that opens https links) when it is Brave, Edge or Chrome.
fn default_browser() -> Option<&'static str> {
    use shared::win::{HKCU, reg_get};
    let key = r"Software\Microsoft\Windows\Shell\Associations\UrlAssociations\https";
    // Windows 11 24H2 keeps the choice in UserChoiceLatest, older Windows in UserChoice
    let id = reg_get(HKCU, &format!(r"{key}\UserChoiceLatest\ProgId"), "ProgId")
        .or_else(|| reg_get(HKCU, &format!(r"{key}\UserChoice"), "ProgId"))?;
    browser_of(&id)
}

/// Path of the user's default browser when it is Brave, Edge or Chrome and installed, else the first of those
/// installed (in that order), or None when none is.
pub fn find_browser() -> Option<PathBuf> {
    find_browser_from(default_browser())
}

/// Like `find_browser`, with `default` ("brave", "msedge" or "chrome") in place of the PC's default browser.
fn find_browser_from(default: Option<&str>) -> Option<PathBuf> {
    let first = default.map(|b| format!(r"\{b}.exe"));
    let preferred = BROWSERS.iter().filter(|(_, tail)| first.as_ref().is_some_and(|f| tail.ends_with(f.as_str())));
    preferred.chain(BROWSERS.iter()).find_map(|(var, tail)| {
        let base = PathBuf::from(std::env::var_os(var)?);
        // an empty or relative variable would look in the current folder
        let p = base.join(tail);
        (base.is_absolute() && p.exists()).then_some(p)
    })
}

/// Ends each process tree (with its child processes) whose command line has `--user-data-dir=<CP_PROFILE>`, quoted or not.
const KILL_LEFTOVERS: &str = concat!(
    // \x22 is a double quote: a literal one could be lost when the script is passed to powershell.exe
    "$re='--user-data-dir=\\x22?'+[regex]::Escape($env:CP_PROFILE)+'(\\x22|\\s|$)';",
    "Get-CimInstance Win32_Process | Where-Object { $_.CommandLine -match $re } | ",
    "ForEach-Object { taskkill.exe /PID $_.ProcessId /T /F *> $null }"
);

/// A browser left over from an earlier run (the app crashed or was ended) keeps the profile folder,
/// and a new browser on that folder hands its work to it and quits at once. End such browsers first.
pub fn kill_leftovers(profile: &Path) {
    use std::os::windows::process::CommandExt;
    let _ = Command::new(shared::sys32().join("WindowsPowerShell/v1.0/powershell.exe"))
        .args(["-NoProfile", "-NonInteractive", "-Command", KILL_LEFTOVERS])
        .env("CP_PROFILE", profile)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .status();
}

/// The first string in a JSON value that starts with nxm:// (DevTools events nest it anywhere), or "".
pub fn scan_nxm(x: &Value) -> String {
    match x {
        Value::String(s) if s.starts_with("nxm://") => s.clone(),
        Value::Object(o) => o.values().map(scan_nxm).find(|s| !s.is_empty()).unwrap_or_default(),
        Value::Array(a) => a.iter().map(scan_nxm).find(|s| !s.is_empty()).unwrap_or_default(),
        _ => String::new(),
    }
}

/// Bytes that are hard to guess (WebSocket key and masks): std's per-process random hash keys over a counter.
fn random_bytes(n: usize) -> Vec<u8> {
    use std::hash::{BuildHasher, Hasher};
    static COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let mut out = vec![];
    while out.len() < n {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(COUNT.fetch_add(1, std::sync::atomic::Ordering::SeqCst));
        h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos());
        out.extend(h.finish().to_le_bytes());
    }
    out.truncate(n);
    out
}

fn base64(data: &[u8]) -> String {
    const ABC: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            out.push(if i <= c.len() { ABC[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' });
        }
    }
    out
}

/// One masked client WebSocket frame (RFC 6455: clients must mask).
pub fn ws_frame(payload: &[u8], opcode: u8) -> Vec<u8> {
    let n = payload.len();
    let mut out = vec![0x80 | opcode];
    if n < 126 {
        out.push(0x80 | n as u8);
    } else if n <= 0xFFFF {
        out.push(0x80 | 126);
        out.extend((n as u16).to_be_bytes());
    } else {
        out.push(0x80 | 127);
        out.extend((n as u64).to_be_bytes());
    }
    let mask = random_bytes(4);
    out.extend(&mask);
    out.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
    out
}

/// One WebSocket frame: (opcode, fin, payload).
pub type Frame = (u8, bool, Vec<u8>);

/// ((opcode, fin, payload), rest) of one complete frame in buf, (None, buf) while it is incomplete.
/// Masks are honoured when set, so the same parser reads client and server frames.
pub fn ws_message(buf: &[u8]) -> (Option<Frame>, &[u8]) {
    if buf.len() < 2 {
        return (None, buf);
    }
    let (op, fin, second) = (buf[0] & 0x0F, buf[0] & 0x80 != 0, buf[1]);
    let (mut n, mut pos): (usize, usize) = ((second & 0x7F) as usize, 2);
    if n == 126 {
        if buf.len() < 4 {
            return (None, buf);
        }
        (n, pos) = (u16::from_be_bytes([buf[2], buf[3]]) as usize, 4);
    } else if n == 127 {
        if buf.len() < 10 {
            return (None, buf);
        }
        let mut b = [0u8; 8];
        b.copy_from_slice(&buf[2..10]);
        (n, pos) = (usize::try_from(u64::from_be_bytes(b)).unwrap_or(usize::MAX), 10);
    }
    let masked = second & 0x80 != 0;
    let end = pos.saturating_add(n).saturating_add(if masked { 4 } else { 0 });
    if buf.len() < end {
        return (None, buf);
    }
    let data = if masked {
        let mask = &buf[pos..pos + 4];
        buf[pos + 4..end].iter().enumerate().map(|(i, b)| b ^ mask[i % 4]).collect()
    } else {
        buf[pos..end].to_vec()
    };
    (Some((op, fin, data)), &buf[end..])
}

/// A small WebSocket client for the DevTools protocol: text both ways, pings answered.
pub struct WS {
    sock: TcpStream,
    buf: Vec<u8>,
    parts: Vec<Vec<u8>>,
}

fn io(e: std::io::Error) -> String {
    e.to_string()
}

impl WS {
    pub fn new(url: &str) -> Result<WS, String> {
        let rest = url.strip_prefix("ws://").ok_or("the browser gave a WebSocket address I cannot use")?;
        let (hostport, path) = rest.find('/').map_or((rest, "/"), |i| (&rest[..i], &rest[i..]));
        let addr = hostport.to_socket_addrs().map_err(io)?.next().ok_or("the browser gave no WebSocket address")?;
        let ten = Some(Duration::from_secs(10));
        let mut sock = TcpStream::connect_timeout(&addr, Duration::from_secs(10)).map_err(io)?;
        sock.set_read_timeout(ten).map_err(io)?;
        sock.set_write_timeout(ten).map_err(io)?;
        let key = base64(&random_bytes(16));
        let req = format!(
            "GET {path} HTTP/1.1\r\nHost: {hostport}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
        );
        sock.write_all(req.as_bytes()).map_err(io)?;
        let mut head = vec![];
        let end = loop {
            if let Some(i) = head.windows(4).position(|w| w == b"\r\n\r\n") {
                break i;
            }
            let mut part = [0u8; 4096];
            let n = sock.read(&mut part).map_err(io)?;
            if n == 0 {
                return Err("the browser closed the WebSocket handshake".into());
            }
            head.extend(&part[..n]);
        };
        let text = String::from_utf8_lossy(&head[..end]).into_owned();
        if !text.split("\r\n").next().unwrap_or("").contains(" 101 ") {
            return Err("the browser refused the WebSocket connection".into());
        }
        if !text.contains(&base64(&sha1(format!("{key}{GUID}").as_bytes()))) {
            return Err("the browser answered a wrong WebSocket key".into());
        }
        Ok(WS { sock, buf: head[end + 4..].to_vec(), parts: vec![] })
    }

    pub fn send(&mut self, text: &str) -> Result<(), String> {
        self.sock.write_all(&ws_frame(text.as_bytes(), 1)).map_err(io)
    }

    /// One text message, or None when nothing arrived in `timeout`. Fragmented messages are
    /// collected; pings are answered and skipped; a close is an error.
    pub fn recv(&mut self, timeout: Duration) -> Result<Option<String>, String> {
        self.sock.set_read_timeout(Some(timeout.max(Duration::from_millis(1)))).map_err(io)?;
        loop {
            loop {
                let (got, rest) = ws_message(&self.buf);
                let used = self.buf.len() - rest.len();
                let Some((op, fin, data)) = got else { break };
                self.buf.drain(..used);
                if op == 0x9 {
                    // ping: answer with a pong, then keep reading
                    self.sock.write_all(&ws_frame(&data, 0xA)).map_err(io)?;
                    continue;
                }
                if op == 0x8 {
                    return Err("the browser closed the WebSocket".into());
                }
                if op == 0x1 || op == 0x2 {
                    self.parts = vec![data];
                } else if op == 0 && !self.parts.is_empty() {
                    self.parts.push(data);
                }
                if !self.parts.is_empty() && fin && (op <= 0x2) {
                    let text = String::from_utf8_lossy(&self.parts.concat()).into_owned();
                    self.parts.clear();
                    return Ok(Some(text));
                }
            }
            let mut chunk = [0u8; 65536];
            match self.sock.read(&mut chunk) {
                Ok(0) => return Err("the browser closed the WebSocket".into()),
                Ok(n) => self.buf.extend(&chunk[..n]),
                Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => return Ok(None),
                Err(e) => return Err(e.to_string()),
            }
        }
    }

    pub fn close(&mut self) {
        let _ = self.sock.write_all(&ws_frame(b"", 0x8));
        let _ = self.sock.shutdown(std::net::Shutdown::Both);
    }
}

/// What the Clicker needs from a page; `Tab` is the real one, tests use a fake.
pub trait Page {
    fn wait_ready(&mut self) -> Result<(), String>;
    /// Evaluate js and return its value.
    fn run(&mut self, js: &str) -> Result<Value, String>;
    /// The nxm:// link the site answers the click with, or None.
    fn wait_nxm(&mut self) -> Result<Option<String>, String>;
    /// Close the page in the browser.
    fn close(&mut self);
}

/// What the Clicker needs from a browser process; `Browser` is the real one, tests use a fake.
pub trait Driver: Send {
    fn alive(&mut self) -> bool;
    fn open(&mut self, url: &str) -> Result<Box<dyn Page>, String>;
    /// True when an open page no longer shows a login link. The window closing or a timeout is false.
    fn wait_login(&mut self, seconds: u64) -> bool;
    fn quit(&mut self);
}

/// One page in the browser: DevTools calls and the events they produce.
pub struct Tab {
    pub target: Value,
    pub events: Vec<Value>,
    ws: WS,
    port: u16,
    next_id: u64,
}

impl Tab {
    pub fn new(port: u16, target: Value) -> Result<Tab, String> {
        let url = target["webSocketDebuggerUrl"].as_str().ok_or("the browser gave no DevTools address for the page")?;
        Ok(Tab { ws: WS::new(url)?, target, events: vec![], port, next_id: 0 })
    }

    /// Send one DevTools command and return its result; a protocol error is an error too.
    pub fn call(&mut self, method: &str, params: Value, timeout: Duration) -> Result<Value, String> {
        self.next_id += 1;
        self.ws.send(&json!({"id": self.next_id, "method": method, "params": params}).to_string())?;
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let Some(m) = self.ws.recv(Duration::from_millis(500))? else { continue };
            let d: Value = serde_json::from_str(&m).map_err(|e| e.to_string())?;
            if d["id"].as_u64() == Some(self.next_id) {
                if let Some(e) = d.get("error") {
                    return Err(e["message"].as_str().unwrap_or("DevTools error").to_string());
                }
                return Ok(d.get("result").cloned().unwrap_or(json!({})));
            }
            self.events.push(d);
        }
        Err(format!("{method} timed out"))
    }

    pub fn pump(&mut self, timeout: Duration) -> Result<(), String> {
        if let Some(m) = self.ws.recv(timeout)? {
            self.events.push(serde_json::from_str(&m).map_err(|e| e.to_string())?);
        }
        Ok(())
    }

    /// Evaluate js and return its value (returned by value, so JSON-safe results only).
    pub fn run_js(&mut self, js: &str) -> Result<Value, String> {
        let r = self.call("Runtime.evaluate", json!({"expression": js, "returnByValue": true, "awaitPromise": true}), Duration::from_secs(15))?;
        if let Some(e) = r.get("exceptionDetails").filter(|e| !e.is_null()) {
            return Err(e["exception"]["description"].as_str().unwrap_or("the page script failed").to_string());
        }
        Ok(r["result"].get("value").cloned().unwrap_or(Value::Null))
    }

    pub fn wait_ready_for(&mut self, seconds: u64) -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_secs(seconds);
        let mut check = false;
        while Instant::now() < deadline {
            // a script error means the context reset while the page loads: keep waiting
            match self.run_js(READY_JS) {
                // "interactive" is enough: the page is built, and one slow ad or image can keep Nexus from "complete"
                // for a minute (the click script waits for the button itself)
                Ok(v) if v == "complete" || v == "interactive" => return Ok(()),
                Ok(v) => check = v == "check",
                Err(_) => {}
            }
            self.pump(Duration::from_millis(500))?;
        }
        Err(if check { "Nexus showed a Cloudflare check that did not clear" } else { "the Nexus page did not finish loading" }.into())
    }

    /// The nxm:// link the site answers the click with, from any DevTools event or the page hook, or None.
    pub fn wait_nxm_for(&mut self, seconds: u64) -> Result<Option<String>, String> {
        let deadline = Instant::now() + Duration::from_secs(seconds);
        let mut next = Instant::now();
        while Instant::now() < deadline {
            self.pump(Duration::from_millis(500))?;
            if let Some(s) = self.events.iter().map(|e| scan_nxm(e.get("params").unwrap_or(&Value::Null))).find(|s| !s.is_empty()) {
                return Ok(Some(s));
            }
            if Instant::now() > next {
                // the hook in the page caught the link itself
                next = Instant::now() + Duration::from_secs(1);
                if let Ok(Value::String(s)) = self.run_js(POLL_JS)
                    && !s.is_empty()
                {
                    return Ok(Some(s));
                }
            }
        }
        Ok(None)
    }
}

impl Page for Tab {
    fn wait_ready(&mut self) -> Result<(), String> {
        self.wait_ready_for(45)
    }

    fn run(&mut self, js: &str) -> Result<Value, String> {
        self.run_js(js)
    }

    fn wait_nxm(&mut self) -> Result<Option<String>, String> {
        self.wait_nxm_for(90)
    }

    fn close(&mut self) {
        let id = self.target["id"].as_str().unwrap_or("");
        let _ = fetch::send(&Request::get(&format!("http://127.0.0.1:{}/json/close/{id}", self.port)).timeout(3));
        self.ws.close();
    }
}

/// One browser process on its own profile folder. `visible` is for the one-time Nexus login.
pub struct Browser {
    pub exe: Option<PathBuf>,
    pub visible: bool,
    pub profile: PathBuf,
    proc: Option<Child>,
    port: u16,
}

/// Each browser gets its own profile folder: one browser cannot always read the login another one saved.
/// Edge keeps the base folder, as before.
fn profile_for(exe: Option<&Path>, base: &Path) -> PathBuf {
    let stem = exe.and_then(|e| e.file_stem()).map(|s| s.to_string_lossy().to_lowercase()).unwrap_or_default();
    if stem.is_empty() || stem == "msedge" { base.to_path_buf() } else { base.join(stem) }
}

impl Browser {
    pub fn new(profile: &Path, visible: bool) -> Browser {
        let exe = find_browser();
        let profile = profile_for(exe.as_deref(), profile);
        Browser { exe, visible, profile, proc: None, port: 0 }
    }

    pub fn start(&mut self, url: &str) -> Result<(), String> {
        use std::os::windows::process::CommandExt;
        let exe = self.exe.clone().ok_or("no Brave, Edge or Chrome found on this PC")?;
        std::fs::create_dir_all(&self.profile).map_err(io)?;
        kill_leftovers(&self.profile);
        let port_file = self.profile.join("DevToolsActivePort");
        let _ = std::fs::remove_file(&port_file); // a crashed browser leaves its old port behind
        let mut cmd = Command::new(exe);
        if !self.visible {
            // a normal window parked off-screen, not headless: Cloudflare stops a headless browser before Nexus loads
            cmd.arg("--window-position=-32000,-32000");
        }
        cmd.args(FLAGS).arg(format!("--user-data-dir={}", self.profile.display())).arg(url);
        cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).creation_flags(CREATE_NO_WINDOW);
        self.proc = Some(cmd.spawn().map_err(io)?);
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if !self.alive() {
                return Err("the browser quit right after starting".into());
            }
            let port = std::fs::read_to_string(&port_file).ok().and_then(|t| t.split_whitespace().next()?.parse().ok());
            if let Some(port) = port {
                self.port = port;
                // the window can show a moment after the port: take it off the taskbar as soon as it does
                let shown = Instant::now() + Duration::from_secs(3);
                while !self.visible && self.to_background() == 0 && Instant::now() < shown {
                    std::thread::sleep(Duration::from_millis(50));
                }
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        self.quit();
        Err("the browser did not open its DevTools port".into())
    }

    /// The browser process id while it runs.
    pub fn pid(&self) -> Option<u32> {
        self.proc.as_ref().map(Child::id)
    }

    /// Hidden mode: takes the off-screen windows off the taskbar and Alt+Tab, so the click runs in the background.
    /// Returns how many browser windows there are (0 in visible mode).
    fn to_background(&self) -> usize {
        match &self.proc {
            Some(p) if !self.visible => shared::win::to_background(p.id()),
            _ => 0,
        }
    }

    fn http(&self, path: &str, method: &str, timeout: u64) -> Result<Value, String> {
        let mut req = Request::get(&format!("http://127.0.0.1:{}{path}", self.port)).timeout(timeout);
        req.method = method.into();
        let r = fetch::send(&req)?;
        if !(200..300).contains(&r.status) {
            return Err(format!("HTTP Error {}", r.status));
        }
        r.json()
    }

    /// A new Tab loading `url` (PUT /json/new; older builds only take GET).
    pub fn open_tab(&mut self, url: &str) -> Result<Tab, String> {
        let path = format!("/json/new?{}", quote(url, "", false));
        let target = match self.http(&path, "PUT", 5) {
            Err(e) if e.starts_with("HTTP Error") => self.http(&path, "GET", 5)?,
            r => r?,
        };
        self.to_background();
        let mut tab = Tab::new(self.port, target)?;
        tab.call("Page.enable", json!({}), Duration::from_secs(15))?;
        tab.call("Network.enable", json!({}), Duration::from_secs(15))?;
        Ok(tab)
    }

    pub fn pages(&self) -> Result<Vec<Value>, String> {
        let list = self.http("/json/list", "GET", 5)?;
        Ok(list.as_array().into_iter().flatten().filter(|t| t["type"] == "page").cloned().collect())
    }
}

impl Driver for Browser {
    fn alive(&mut self) -> bool {
        self.proc.as_mut().is_some_and(|p| matches!(p.try_wait(), Ok(None)))
    }

    fn open(&mut self, url: &str) -> Result<Box<dyn Page>, String> {
        Ok(Box::new(self.open_tab(url)?))
    }

    fn wait_login(&mut self, seconds: u64) -> bool {
        let deadline = Instant::now() + Duration::from_secs(seconds);
        let mut tab: Option<Tab> = None;
        while Instant::now() < deadline {
            if !self.alive() {
                return false;
            }
            let looked = (|| -> Result<bool, String> {
                if tab.is_none()
                    && let Some(p) = self.pages()?.into_iter().next()
                {
                    tab = Some(Tab::new(self.port, p)?);
                }
                Ok(match tab.as_mut() {
                    Some(t) => t.run_js(LOGGED_IN_JS)? == 1,
                    None => false,
                })
            })();
            match looked {
                Ok(true) => return true,
                Ok(false) => {}
                // still loading, or the tab went away: look again
                Err(_) => {
                    if let Some(mut t) = tab.take() {
                        t.ws.close();
                    }
                }
            }
            std::thread::sleep(Duration::from_secs(2));
        }
        false
    }

    fn quit(&mut self) {
        if let Some(mut p) = self.proc.take() {
            let _ = p.kill();
            let deadline = Instant::now() + Duration::from_secs(5);
            while matches!(p.try_wait(), Ok(None)) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(50));
            }
            if matches!(p.try_wait(), Ok(None)) {
                use std::os::windows::process::CommandExt;
                let _ = Command::new(shared::sys32().join("taskkill.exe"))
                    .args(["/PID", &p.id().to_string(), "/T", "/F"])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .creation_flags(CREATE_NO_WINDOW)
                    .status();
            }
        }
        self.port = 0;
    }
}

/// Starts a browser: (profile folder, visible, first url).
pub type Launch = Box<dyn Fn(&Path, bool, &str) -> Result<Box<dyn Driver>, String> + Send + Sync>;

/// The real browser for `Clicker::launch`.
pub fn launch_browser(profile: &Path, visible: bool, url: &str) -> Result<Box<dyn Driver>, String> {
    let mut b = Browser::new(profile, visible);
    b.start(url)?;
    Ok(Box::new(b))
}

/// Continues a download with the nxm:// link; an error falls back to the manual click.
pub type OnLink = Box<dyn Fn(&str) -> Result<(), String> + Send + Sync>;
/// (job, why): reopens the page for a manual click.
pub type OnFail = Box<dyn Fn(&Value, &str) + Send + Sync>;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The Nexus click queue's hidden browser. Wired by the controller: `log` shows text in the window,
/// `on_link(link)` continues a download, `on_fail(job, why)` falls back to the manual click, `waiting(job)` says
/// whether the job still wants a click (the user may have skipped it while the page was loading).
/// Set the fields before sharing the Clicker between threads.
pub struct Clicker {
    pub profile: PathBuf,
    pub log: Box<dyn Fn(&str) + Send + Sync>,
    pub on_link: OnLink,
    pub on_fail: OnFail,
    pub waiting: Box<dyn Fn(&Value) -> bool + Send + Sync>,
    pub login_wait: u64,
    pub gap: Duration,
    pub launch: Launch,
    browser: Mutex<Option<Box<dyn Driver>>>,
    /// One click at a time (two would share one browser profile); holds when the last click ended.
    busy: Mutex<Option<Instant>>,
    closed: Mutex<bool>,
    wake: Condvar,
}

impl Clicker {
    pub fn new(profile: &Path) -> Clicker {
        Clicker {
            profile: profile.to_path_buf(),
            log: Box::new(|_| {}),
            on_link: Box::new(|_| Ok(())),
            on_fail: Box::new(|_, _| {}),
            waiting: Box::new(|_| true),
            login_wait: 300,
            gap: GAP,
            launch: Box::new(launch_browser),
            browser: Mutex::new(None),
            busy: Mutex::new(None),
            closed: Mutex::new(false),
            wake: Condvar::new(),
        }
    }

    /// Run on a worker thread: click for one waiting job. Never fails; any failure goes to on_fail.
    pub fn click(&self, job: &Value) {
        let (Some(mod_id), Some(file_id)) = (job["src"]["mod_id"].as_i64(), job["src"]["file_id"].as_i64()) else {
            return (self.on_fail)(job, "the job has no Nexus file to click for");
        };
        let url = nexus::files_page(mod_id, file_id);
        let mut last = lock(&self.busy);
        let closed = if let Some(t) = *last {
            let left = self.gap.saturating_sub(t.elapsed());
            let g = self.wake.wait_timeout_while(lock(&self.closed), left, |c| !*c).unwrap_or_else(|e| e.into_inner()).0;
            *g
        } else {
            *lock(&self.closed)
        };
        // the user may have skipped it while waiting
        if (self.waiting)(job) && !closed {
            self.click_one(job, &url, mod_id, file_id);
            *last = Some(Instant::now());
        }
    }

    fn click_one(&self, job: &Value, url: &str, mod_id: i64, file_id: i64) {
        let link = match self.try_click(url) {
            Err(TryError::NeedLogin) => {
                if !self.login(url) {
                    return (self.on_fail)(job, "the Nexus login window was closed or timed out");
                }
                self.try_click(url)
            }
            r => r,
        };
        let link = match link {
            Ok(l) => l,
            Err(TryError::NeedLogin) => return (self.on_fail)(job, "the Nexus page still asks for a login"),
            Err(TryError::Failed(e)) => return (self.on_fail)(job, &e),
        };
        // the link comes from page events: take it only when it is for this job's file
        if !format!("{link}?").contains(&format!("/mods/{mod_id}/files/{file_id}?")) {
            return (self.on_fail)(job, "Nexus gave a download link for another file");
        }
        if (self.waiting)(job)
            && let Err(e) = (self.on_link)(&link)
        {
            (self.on_fail)(job, &e);
        }
    }

    /// Open the file's page hidden, click, and return the nxm:// link.
    fn try_click(&self, url: &str) -> Result<String, TryError> {
        let mut tab = {
            let mut b = lock(&self.browser);
            if !b.as_mut().is_some_and(|b| b.alive()) {
                if let Some(mut old) = b.take() {
                    old.quit();
                }
                *b = Some((self.launch)(&self.profile, false, "about:blank")?);
            }
            b.as_mut().map(|b| b.open(url)).unwrap_or_else(|| Err("no browser".into()))?
        };
        let r = (|| -> Result<String, TryError> {
            tab.wait_ready()?;
            // 2 (login state not known) still tries the click: a failed click falls back to the manual one
            if tab.run(LOGIN_JS)? == 1 {
                return Err(TryError::NeedLogin);
            }
            tab.run(HOOK_JS)?;
            if !truthy(&tab.run(CLICK_JS)?) {
                return Err(TryError::Failed("no 'Slow download' button was found on the page".into()));
            }
            match tab.wait_nxm()? {
                Some(link) if !link.is_empty() => Ok(link),
                _ => Err(TryError::Failed("Nexus gave no download link after the click".into())),
            }
        })();
        tab.close();
        r
    }

    /// One visible window on the same profile so the user can log in once; the login is kept.
    fn login(&self, url: &str) -> bool {
        (self.log)(
            "To click by myself I need your Nexus login: a browser window is opening - log in there once, \
             and this app clicks every download by itself from then on.",
        );
        // a profile folder only belongs to one running browser
        if let Some(mut b) = lock(&self.browser).take() {
            b.quit();
        }
        match (self.launch)(&self.profile, true, url) {
            Err(e) => {
                (self.log)(&format!("Could not open the login window: {e}"));
                false
            }
            Ok(mut b) => {
                let ok = b.wait_login(self.login_wait);
                b.quit(); // a fresh hidden browser picks up the saved login
                ok
            }
        }
    }

    pub fn close(&self) {
        *lock(&self.closed) = true; // also ends a wait for the next click
        self.wake.notify_all();
        if let Some(mut b) = lock(&self.browser).take() {
            b.quit();
        }
    }
}

/// Python truthiness of a page script's answer.
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

#[cfg(test)]
mod tests;
