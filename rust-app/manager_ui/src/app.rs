//! Start-up. Only one window runs. A second start (for example from a Nexus "Mod Manager Download"
//! click) hands its nxm:// link to the running window over a per-user named pipe and exits. Only the
//! account that owns the pipe can open it, and both sides prove they know a random token that the running
//! window writes to the user's own data folder, so other users cannot send links to it or pose as it.
//! AI apps start the exe with `--mcp`: then it is only the MCP server on stdio.

use crate::pipe::{Conn, OwnerListener, Wire, answer_challenge, deliver_challenge, token_hex};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use shared::win::{HKCU, reg_get, reg_set, wide};
use std::fs;
use std::io;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::thread;
use std::time::{Duration, Instant};

pub const NXM_KEY: &str = r"Software\Classes\nxm\shell\open\command";
const DETACHED_PROCESS: u32 = 0x8;
pub const NOT_STARTED: &str =
    "The mod manager window did not start or did not answer. Start it normally (not as administrator), or wait until it is free, then try again.";
pub const STILL_RUNNING: &str = "The mod manager is still running and did not answer. If it is closing, try again in a moment. If it runs as administrator, close it and start it normally.";

/// Python's `getpass.getuser()` on Windows.
fn user() -> String {
    ["LOGNAME", "USER", "LNAME", "USERNAME"].iter().find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty())).unwrap_or_default()
}

/// `\\.\pipe\CP2077ModManager-` + the first 16 hex digits of sha256(user name), as in Python.
pub fn pipe_name() -> String {
    let h: String = Sha256::digest(user().as_bytes()).iter().take(8).map(|b| format!("{b:02x}")).collect();
    format!(r"\\.\pipe\CP2077ModManager-{h}")
}

/// The token file in the data folder. A secret: never print or log what it holds.
pub fn token_path() -> PathBuf {
    shared::data_dir().join("ipc.token")
}

fn exe() -> PathBuf {
    std::env::current_exe().unwrap_or_default()
}

pub fn nxm_command() -> String {
    format!("\"{}\" \"%1\"", exe().display())
}

/// Make Windows open nxm:// links with this app (current user only, no admin).
pub fn register_nxm() -> io::Result<()> {
    reg_set(HKCU, r"Software\Classes\nxm", "", "URL:NXM Protocol")?;
    reg_set(HKCU, r"Software\Classes\nxm", "URL Protocol", "")?;
    reg_set(HKCU, NXM_KEY, "", &nxm_command())
}

/// The program name in an nxm:// open command (such as "Vortex").
pub fn owner_of(cmd: &str) -> String {
    let exe = match cmd.strip_prefix('"') {
        Some(rest) => rest.split('"').next().unwrap_or(""),
        None => cmd.split(' ').next().unwrap_or(""),
    };
    Path::new(exe).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
}

/// The name of the program Windows now gives nxm:// links to, or "" when none is set.
pub fn nxm_owner() -> String {
    reg_get(HKCU, NXM_KEY, "").map(|c| owner_of(&c)).unwrap_or_default()
}

pub fn nxm_registered() -> bool {
    reg_get(HKCU, NXM_KEY, "").is_some_and(|c| c == nxm_command())
}

/// Connect and prove the token both ways. A missing pipe or token file gives `NotFound`.
pub(crate) fn connect(pipe: &str, token: &Path) -> io::Result<Conn> {
    let key = fs::read_to_string(token)?.trim().as_bytes().to_vec();
    let c = Conn::connect(pipe)?;
    answer_challenge(&c, &key)?;
    deliver_challenge(&c, &key)?;
    Ok(c)
}

/// One attempt to give msg to the running window, on a helper thread so that a pipe holder that never
/// answers cannot block start-up. `ok` turns true only when the window confirms it took the link.
pub struct HandOver {
    ok: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
}

impl HandOver {
    pub fn start(pipe: &str, token: &Path, msg: &str) -> HandOver {
        let (ok, done) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
        let (o, d, pipe, token, msg) = (ok.clone(), done.clone(), pipe.to_string(), token.to_path_buf(), msg.to_string());
        thread::spawn(move || {
            let r = (|| -> io::Result<bool> {
                let c = connect(&pipe, &token)?;
                c.send(msg.as_bytes())?;
                Ok(c.recv(16)? == b"ok")
            })();
            o.store(r.unwrap_or(false), SeqCst);
            d.store(true, SeqCst);
        });
        HandOver { ok, done }
    }

    /// Wait at most `d` for the attempt to end.
    pub fn join(&self, d: Duration) {
        let end = Instant::now() + d;
        while !self.done() && Instant::now() < end {
            thread::sleep(Duration::from_millis(10));
        }
    }

    pub fn done(&self) -> bool {
        self.done.load(SeqCst)
    }

    pub fn ok(&self) -> bool {
        self.ok.load(SeqCst)
    }
}

/// Run one AI-app tool in the window and return its text. A window that is not running is started first
/// (`spawn`). Only connecting is retried: a command that was sent is never sent twice.
pub fn ask_window_with(pipe: &str, token: &Path, tool: &str, args: &Value, wait: Duration, spawn: &dyn Fn()) -> Result<String, String> {
    let (deadline, mut started) = (Instant::now() + wait, false);
    let c = loop {
        match connect(pipe, token) {
            Ok(c) => break c,
            Err(e) => {
                if Instant::now() > deadline {
                    return Err(NOT_STARTED.into());
                }
                // no pipe or no token: no window yet. A busy pipe or a refused token means one runs, and a
                // second copy would only show the "still running" message.
                if !started && e.kind() == io::ErrorKind::NotFound {
                    started = true;
                    spawn();
                }
                thread::sleep(Duration::from_millis(500));
            }
        }
    };
    let send = || -> io::Result<Vec<u8>> {
        c.send(json!({"tool": tool, "args": args}).to_string().as_bytes())?;
        c.recv(64 << 20)
    };
    let r: Value = send().ok().and_then(|b| serde_json::from_slice(&b).ok()).ok_or_else(|| NOT_STARTED.to_string())?;
    if let Some(e) = r.get("error") {
        return Err(e.as_str().map_or_else(|| e.to_string(), str::to_string));
    }
    Ok(r["text"].as_str().unwrap_or_default().to_string())
}

/// Start a new window: this exe again, detached, with no console and no stdio.
fn spawn_window() {
    let _ = Command::new(exe())
        .creation_flags(DETACHED_PROCESS)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

pub fn ask_window(tool: &str, args: &Value) -> Result<String, String> {
    ask_window_with(&pipe_name(), &token_path(), tool, args, Duration::from_secs(40), &spawn_window)
}

/// What `start` needs; the tests give fakes.
pub struct Start<'a> {
    pub pipe: &'a str,
    pub token: &'a Path,
    pub args: &'a [String],
    pub wait: Duration,
    /// Runs the window: (pipe that this process now owns, nxm:// link, new token).
    pub run_window: Box<dyn FnOnce(OwnerListener, Option<String>, String) + 'a>,
    pub message_box: &'a dyn Fn(),
}

/// Hand the link (or "show") to a running window, or take the pipe and run the window here.
pub fn start(s: Start) {
    let url = s.args.iter().find(|a| a.to_lowercase().starts_with("nxm://")).cloned();
    let msg = url.clone().unwrap_or_else(|| "show".into());
    let deadline = Instant::now() + s.wait; // a window that is starting (token appears) or closing (pipe frees)
    let (mut attempt, mut started) = (HandOver::start(s.pipe, s.token, &msg), Instant::now());
    let server = loop {
        attempt.join(Duration::from_millis(500));
        let done = attempt.done(); // read once: an attempt that ends between two reads would be started again
        if done {
            // only one attempt at a time, so a link is delivered at most once
            if attempt.ok() {
                return;
            }
            if let Ok(server) = OwnerListener::new(s.pipe) {
                break server; // fails while any window of ours (or anyone) holds the name
            }
        }
        if Instant::now() > deadline {
            attempt.join(Duration::from_secs(1)); // one last chance for an answer that is on its way
            if !attempt.ok() {
                (s.message_box)();
            }
            return;
        }
        if done {
            thread::sleep((started + Duration::from_millis(500)).saturating_duration_since(Instant::now())); // at most 2 tries a second
            (attempt, started) = (HandOver::start(s.pipe, s.token, &msg), Instant::now());
        }
    };
    let token = token_hex(16); // written only once this process owns the pipe
    if let Some(d) = s.token.parent() {
        let _ = fs::create_dir_all(d);
    }
    let _ = fs::write(s.token, &token);
    (s.run_window)(server, url, token)
}

fn message_box(text: &str) {
    let (t, c) = (wide(text), wide("CP2077 AI Mod Manager"));
    // SAFETY: NUL-ended wide strings; no owner window.
    unsafe {
        windows_sys::Win32::UI::WindowsAndMessaging::MessageBoxW(
            std::ptr::null_mut(),
            t.as_ptr(),
            c.as_ptr(),
            windows_sys::Win32::UI::WindowsAndMessaging::MB_ICONWARNING,
        )
    };
}

/// A GUI exe started from a console has no stdio: attach to the parent's console. Started by an AI app,
/// stdin and stdout are already its pipes. False when there is nothing to talk to.
fn stdio() -> bool {
    use windows_sys::Win32::Storage::FileSystem::{CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING};
    use windows_sys::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole, GetStdHandle, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle};
    // SAFETY: plain handle queries and opens of the console devices.
    unsafe {
        let h = GetStdHandle(STD_INPUT_HANDLE);
        if !h.is_null() && h != windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
            return true;
        }
        if AttachConsole(ATTACH_PARENT_PROCESS) == 0 {
            return false;
        }
        for (name, std) in [("CONIN$", STD_INPUT_HANDLE), ("CONOUT$", STD_OUTPUT_HANDLE)] {
            let w = wide(name);
            let access = windows_sys::Win32::Foundation::GENERIC_READ | windows_sys::Win32::Foundation::GENERIC_WRITE;
            let h = CreateFileW(w.as_ptr(), access, FILE_SHARE_READ | FILE_SHARE_WRITE, std::ptr::null(), OPEN_EXISTING, 0, std::ptr::null_mut());
            if h == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
                return false;
            }
            SetStdHandle(std, h);
        }
        true
    }
}

/// The exe's start: `--mcp` serves AI apps on stdio, anything else shows the one window.
pub fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--mcp") {
        // started by an AI app: no window here, just the MCP server
        if stdio() {
            ai_link::mcp::serve(&ask_window, shared::VERSION, io::stdin().lock(), io::stdout().lock());
        }
        return;
    }
    let (pipe, token) = (pipe_name(), token_path());
    start(Start {
        pipe: &pipe,
        token: &token,
        args: &args,
        wait: Duration::from_secs(15),
        run_window: Box::new(|server, url, token| crate::ui::run(server, url, token, crate::ui::Nxm::real())),
        message_box: &|| message_box(STILL_RUNNING),
    });
}

#[cfg(test)]
mod tests;
