//! The local AI: an open model (Qwen3.5, Apache 2.0 license) that runs on this PC in llama.cpp's llama-server
//! (MIT license). Both ship in the app's local_ai folder: no install, no account, offline. The engine runs only while
//! the window is open and the game is not running. It reads the mod list and the Crash doctor's problems and picks fixes
//! from the fixes the doctor offers, and names the mods that must load after another one (patches, add-ons). It can
//! only choose; the app makes every change.
use downloads::fetch::{self, Request};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use shared::{Mod, kind_of};
use std::io::Read;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant};

#[cfg(test)]
mod tests;

const NO_WINDOW: u32 = 0x0800_0000; // CREATE_NO_WINDOW

/// A file the app downloads, with the SHA-256 it must have.
pub struct Download {
    pub url: &'static str,
    pub sha256: &'static str,
    pub file: &'static str,
}

/// Next to the exe in the release zip; the repo's `local_ai` folder when run from a cargo build, like a run from
/// source (fetch() fills it then).
pub static HOME: LazyLock<PathBuf> = LazyLock::new(|| {
    let exe = std::env::current_exe().unwrap_or_default();
    for dir in exe.ancestors() {
        if dir.file_name().is_some_and(|n| n == "target")
            && let Some(workspace) = dir.parent()
            && workspace.join("Cargo.toml").is_file()
            && let Some(repo) = workspace.parent()
        {
            return repo.join("local_ai");
        }
    }
    exe.parent().unwrap_or(Path::new(".")).join("local_ai")
});
// Vulkan: any graphics card, else the CPU
pub const ENGINE: Download = Download {
    url: "https://github.com/ggml-org/llama.cpp/releases/download/b12032/llama-b12032-bin-win-vulkan-x64.zip",
    sha256: "19a8bbf269fa683ae7bc04f886c9c2a93e0655c2606f37d2b60069bf9e8bcc32",
    file: "llama-b12032-bin-win-vulkan-x64.zip",
};
// Qwen3.5-2B tuned for mod bugs, see MODEL.md
pub const MODEL: Download = Download {
    url: "https://github.com/RyanWeera/cyberpunk-ai-mod-manager/releases/download/model-v1/Qwen3.5-2B-CP2077-Q4_K_M.gguf",
    sha256: "f39ce8f3c3531d0c06620d085f02d8256c85095b2d550c366778af1b0402d679",
    file: "Qwen3.5-2B-CP2077-Q4_K_M.gguf",
};
pub static SERVER: LazyLock<PathBuf> = LazyLock::new(|| HOME.join("engine").join("llama-server.exe"));
pub static MODEL_FILE: LazyLock<PathBuf> = LazyLock::new(|| HOME.join("model").join(MODEL.file));
pub const BLOCKED: &str = "The game is running. The local AI waits until you close the game, so the game gets all of your PC.";
pub const RULES: &str = r#"You help a player fix Cyberpunk 2077 mod problems inside a mod manager. You get the mod list in load order
and the problems the manager found, each with numbered fixes. Answer only in the JSON format you are given.
Load order: a mod lower in the list wins when two mods change the same thing. Frameworks (RED4ext, Cyber Engine Tweaks,
redscript, ArchiveXL, TweakXL, Codeware, Input Loader, Mod Settings) always go first; the manager does that itself.
"fixes": pick at most one fix for each problem, only from the numbered fixes given, and only when the problem's
details support it. After a game update, mods that hook the game's code (RED4ext plugins, Cyber Engine Tweaks mods,
redscript, frameworks) cause most crashes: prefer updating them. Turning a mod off is better than removing it.
"summary": 2 to 5 short, plain sentences for the player: what is wrong and what you change."#;
pub static SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({"type": "object", "required": ["fixes", "summary"], "properties": {
        "fixes": {"type": "array", "items": {"type": "object", "required": ["problem", "fix", "why"], "properties": {
            "problem": {"type": "integer"}, "fix": {"type": "integer"}, "why": {"type": "string"}}}},
        "summary": {"type": "string"}}})
});
pub const REPORT: &str = r#"
The player also describes a bug in their own words. "cause": 1 to 3 short, plain sentences on the most likely reason,
from the mod list and the problems. "mod_fixes": changes for the mods the bug is about, only when the list supports them:
"turn_on" a mod that is off, "update" a mod that may be too old for the game, "load_last" so a mod wins over the other
mods that change the same thing, "turn_off" a mod that breaks it. Give [] when no change fits."#;
pub const ACTIONS: [&str; 4] = ["turn_on", "turn_off", "update", "load_last"];
pub static REPORT_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    let mut s = SCHEMA.clone();
    s["required"] = json!(["fixes", "summary", "cause", "mod_fixes"]);
    s["properties"]["cause"] = json!({"type": "string"});
    s["properties"]["mod_fixes"] = json!({"type": "array", "items": {"type": "object", "required": ["mod", "action", "why"], "properties": {
        "mod": {"type": "integer"}, "action": {"type": "string", "enum": ACTIONS}, "why": {"type": "string"}}}});
    s
});
// one line: small models answer worse when the rules have line breaks
pub const ORDER: &str = "You know Cyberpunk 2077 mods. For each mod number, list the numbers of the other mods in the list that it \
builds on. A patch, add-on, fix, compatibility file, preset, translation or extra pack builds on the mod its name starts \
with and on every mod its name says it is for. Example: \"A - Patch for B\" builds on A and on B. Most mods build on no \
other mod: give them [].";

/// Asks about each of the n mods by number: small models leave a free list of pairs empty, but fill one slot per mod.
pub fn order_schema(n: usize) -> Value {
    let nums: Vec<String> = (1..=n).map(|i| i.to_string()).collect();
    let props: Map<String, Value> = nums.iter().map(|k| (k.clone(), json!({"type": "array", "items": {"type": "integer"}}))).collect();
    json!({"type": "object", "required": ["builds_on"], "properties": {"builds_on": {
        "type": "object", "required": nums, "properties": props}}})
}

pub fn ready() -> bool {
    SERVER.is_file() && MODEL_FILE.is_file()
}

/// For a run from source (the release zip has both files): download the engine and the model and check their
/// hashes. progress(what, done_bytes, total_bytes).
pub fn fetch(progress: &dyn Fn(&str, u64, u64)) -> Result<(), String> {
    let tmp = HOME.join("download");
    std::fs::create_dir_all(&tmp).map_err(|e| e.to_string())?;
    if !SERVER.is_file() {
        let z = checked(&fetch::download(ENGINE.url, &tmp, Some(&|d, t| progress("engine", d, t)), &[])?, ENGINE.sha256)?;
        unzip(&z, SERVER.parent().unwrap_or(&HOME))?;
        std::fs::remove_file(&z).map_err(|e| e.to_string())?;
    }
    if !MODEL_FILE.is_file() {
        let f = checked(&fetch::download(MODEL.url, &tmp, Some(&|d, t| progress("model", d, t)), &[])?, MODEL.sha256)?;
        std::fs::create_dir_all(MODEL_FILE.parent().unwrap_or(&HOME)).map_err(|e| e.to_string())?;
        std::fs::rename(&f, &*MODEL_FILE).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Unpack a zip with Windows' tar.exe, the archive on stdin. tar drops absolute paths and ".." parts.
fn unzip(zip: &Path, dest: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dest).map_err(|e| e.to_string())?;
    let archive = std::fs::File::open(zip).map_err(|e| e.to_string())?;
    let ok = Command::new(shared::sys32().join("tar.exe"))
        .args(["-xf", "-"])
        .current_dir(dest)
        .stdin(archive)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(NO_WINDOW)
        .status()
        .map_err(|e| e.to_string())?
        .success();
    if ok { Ok(()) } else { Err("Could not unpack the local AI engine".into()) }
}

pub fn checked(path: &Path, sha256: &str) -> Result<PathBuf, String> {
    let mut h = Sha256::new();
    let mut f = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut chunk = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut chunk).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        h.update(&chunk[..n]);
    }
    drop(f);
    let hex: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
    if hex != sha256 {
        let _ = std::fs::remove_file(path);
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        return Err(format!("{name} is not the file I expect (wrong SHA-256). Try again later."));
    }
    Ok(path.to_path_buf())
}

/// The engine process, as start() sees it.
trait Engine: Send {
    fn running(&mut self) -> bool;
    fn kill(&mut self);
}

impl Engine for Child {
    fn running(&mut self) -> bool {
        matches!(self.try_wait(), Ok(None))
    }

    fn kill(&mut self) {
        let _ = Child::kill(self);
        let end = Instant::now() + Duration::from_secs(10);
        while matches!(self.try_wait(), Ok(None)) && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// The llama-server that start() ran, its address and its key (no other program may use it), and the block flag
/// (the game runs or the window closes). `run` counts starts, so start() can tell whether its engine still runs.
struct State {
    server: Option<Box<dyn Engine>>,
    url: String,
    key: String,
    blocked: bool,
    run: u64,
}

static STATE: Mutex<State> = Mutex::new(State { server: None, url: String::new(), key: String::new(), blocked: false, run: 0 });

fn state() -> MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

fn alive(s: &mut State) -> bool {
    s.server.as_mut().is_some_and(|p| p.running())
}

pub fn running() -> bool {
    alive(&mut state())
}

/// on: the game starts or the window closes. End the engine and refuse to start it until block(false).
pub fn block(on: bool) {
    let mut s = state();
    s.blocked = on;
    if on {
        end(&mut s);
    }
}

/// Like Python's `secrets.token_urlsafe(24)`: 24 random bytes as 32 URL-safe characters. The approved crates
/// have no random number source, so the bytes come from std's randomly keyed SipHash.
fn new_key() -> String {
    use std::hash::{BuildHasher, Hasher};
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos();
    let mut bytes = vec![];
    for i in 0..3u64 {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(i);
        h.write_u128(nanos);
        h.write_u32(std::process::id());
        bytes.extend(h.finish().to_le_bytes());
    }
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    bytes
        .chunks(3)
        .flat_map(|c| {
            let n = (c[0] as u32) << 16 | (c[1] as u32) << 8 | c[2] as u32;
            [18, 12, 6, 0].map(|s| ABC[(n >> s & 63) as usize] as char)
        })
        .collect()
}

fn spawn(port: u16, key: &str) -> Result<Box<dyn Engine>, String> {
    let port = port.to_string();
    let child = Command::new(&*SERVER)
        .arg("-m")
        .arg(&*MODEL_FILE)
        .args(["--host", "127.0.0.1", "--port", &port, "-c", "16384", "--jinja", "--reasoning-budget", "0", "--no-webui", "--api-key", key])
        .current_dir(SERVER.parent().unwrap_or(&HOME))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(NO_WINDOW)
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(Box::new(child))
}

/// Start the engine with the model, on a free local port, and wait until the model is loaded.
pub fn start(wait: Duration) -> Result<(), String> {
    start_with(wait, &ready, &spawn)
}

type Spawn = dyn Fn(u16, &str) -> Result<Box<dyn Engine>, String>;

fn start_with(wait: Duration, ready: &dyn Fn() -> bool, spawn: &Spawn) -> Result<(), String> {
    let (url, run) = {
        let mut s = state(); // block() cannot slip in between the check and the start
        if s.blocked {
            return Err(BLOCKED.into());
        }
        if alive(&mut s) {
            return Ok(());
        }
        if !ready() {
            return Err("The local AI files are missing. Click 'Check local AI' in the AI helpers tab to download them.".into());
        }
        let port = std::net::TcpListener::bind("127.0.0.1:0").and_then(|l| l.local_addr()).map_err(|e| e.to_string())?.port();
        s.key = new_key();
        s.server = Some(spawn(port, &s.key)?);
        s.url = format!("http://127.0.0.1:{port}");
        s.run += 1;
        (s.url.clone(), s.run)
    };
    let end_at = Instant::now() + wait;
    loop {
        // not listening yet, or 503 while the model loads
        if fetch::http_json(&format!("{url}/health"), None, &[], 3).is_ok_and(|h| h.get("status") == Some(&json!("ok"))) {
            break;
        }
        {
            let mut s = state();
            let mine = s.run == run && s.server.is_some();
            // an engine that is no longer ours was ended by stop()
            if !mine || !alive(&mut s) || Instant::now() > end_at {
                if mine {
                    end(&mut s);
                }
                return Err(if s.blocked {
                    BLOCKED.into()
                } else {
                    "The local AI did not start. Close other big programs (it needs about 2 GB of memory) and try again.".into()
                });
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    let s = state(); // block() ran while the model loaded: it ended this engine
    if s.run != run || s.server.is_none() {
        return Err(BLOCKED.into());
    }
    Ok(())
}

fn end(s: &mut State) {
    if let Some(mut p) = s.server.take() {
        p.kill();
    }
}

/// Give the PC back (the game starts, or the window closes): end the engine, which frees the model's memory.
pub fn stop() {
    end(&mut state());
}

pub fn chat(system: &str, user: &str, schema: &Value) -> Result<Value, String> {
    let body = json!({"temperature": 0, "response_format": {"type": "json_schema", "json_schema": {"name": "answer", "schema": schema}},
                      "chat_template_kwargs": {"enable_thinking": false},
                      "messages": [{"role": "system", "content": system}, {"role": "user", "content": user}]});
    let (url, key) = {
        let s = state();
        (s.url.clone(), s.key.clone())
    };
    let mut req = Request::post(&format!("{url}/v1/chat/completions"), body.to_string().into_bytes()).timeout(900);
    for (k, v) in fetch::UA {
        req = req.header(k, v);
    }
    let req = req.header("Content-Type", "application/json").header("Authorization", &format!("Bearer {key}"));
    let r = fetch::send(&req)?;
    if !(200..300).contains(&r.status) {
        return Err(format!("The local AI failed: {}", r.text().chars().take(200).collect::<String>()));
    }
    let content = r.json()?["choices"][0]["message"]["content"].as_str().ok_or("The local AI answered without content")?.to_string();
    serde_json::from_str(&content).map_err(|e| format!("The local AI answered no JSON: {e}"))
}

/// The local AI's advice, as checked by advise().
#[derive(Clone, Debug, PartialEq)]
pub struct Advice {
    pub summary: String,
    /// (problem, fix, why)
    pub fixes: Vec<(Value, Value, String)>,
    /// (lo id, hi id): hi must load after (win over) lo.
    pub after: Vec<(String, String)>,
    /// With a bug report only.
    pub cause: Option<String>,
    /// With a bug report only: (mod, action, why).
    pub mod_fixes: Vec<(Mod, String, String)>,
}

/// Python's str() of a JSON value: text as it is, anything else as JSON.
fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        v => v.to_string(),
    }
}

/// A JSON whole number (not a bool, not 2.0) in 1..=n.
fn pick(v: Option<&Value>, n: usize) -> Option<usize> {
    v.and_then(Value::as_i64).filter(|&i| i > 0 && i as u64 <= n as u64).map(|i| i as usize)
}

/// {"summary", "fixes": [(problem, fix, why)], "after": [(lo id, hi id)]}: hi must load after (win over) lo.
/// With the player's `report` of a bug, also "cause" and "mod_fixes": [(mod, action, why)].
/// Two questions, because a small model answers each one better alone. Picks that do not exist are dropped,
/// and each problem gets at most one fix.
pub fn advise(mods: &[Mod], probs: &[Value], report: &str) -> Result<Advice, String> {
    let names: Vec<String> = mods.iter().enumerate().map(|(n, m)| format!("{}. {}", n + 1, m.name)).collect();
    let b = chat(ORDER, &format!("Mods:\n{}", names.join("\n")), &order_schema(mods.len()))?;
    let mut after = vec![];
    for (k, v) in b.get("builds_on").and_then(Value::as_object).into_iter().flatten() {
        let hi = if !k.is_empty() && k.bytes().all(|c| c.is_ascii_digit()) { k.parse::<usize>().unwrap_or(0) } else { 0 };
        if let (true, Some(v)) = (0 < hi && hi <= mods.len(), v.as_array()) {
            for lo in v.iter().filter_map(|lo| pick(Some(lo), mods.len())).filter(|&lo| lo != hi) {
                after.push((mods[lo - 1].id.clone(), mods[hi - 1].id.clone()));
            }
        }
    }
    if probs.is_empty() && report.is_empty() {
        return Ok(Advice { summary: "The Crash doctor found no problems.".into(), fixes: vec![], after, cause: None, mod_fixes: vec![] });
    }
    let mut lines = vec![];
    for (n, m) in mods.iter().enumerate() {
        let needs: Vec<String> = m.requires.iter().map(|r| text(&r["name"])).chain(m.needs.iter().cloned()).collect();
        lines.push(format!(
            "{}. {} [{}]{}{}",
            n + 1,
            m.name,
            kind_of(m),
            if m.enabled { "" } else { " (off)" },
            if needs.is_empty() { String::new() } else { format!(", needs: {}", needs.join(", ")) }
        ));
    }
    let mut plines = vec![];
    for (i, p) in probs.iter().enumerate() {
        let detail: String = text(&p["detail"]).chars().take(300).collect();
        plines.push(format!("P{} ({}) {}: {detail}", i + 1, text(&p["severity"]), text(&p["title"])));
        for (j, f) in p["fixes"].as_array().into_iter().flatten().enumerate() {
            let rec = if f["recommended"].as_bool().unwrap_or(false) { " (recommended)" } else { "" };
            plines.push(format!("  {}. {}{rec}", j + 1, text(&f["label"])));
        }
    }
    let user = format!(
        "Mods in load order (1 = loads first, the last one wins):\n{}\n\nProblems found:\n{}{}",
        lines.join("\n"),
        if plines.is_empty() { "none".to_string() } else { plines.join("\n") },
        if report.is_empty() { String::new() } else { format!("\n\nThe player says: {}", report.chars().take(1000).collect::<String>()) }
    );
    let system = if report.is_empty() { RULES.to_string() } else { format!("{RULES}{REPORT}") };
    let a = chat(&system, &user, if report.is_empty() { &SCHEMA } else { &REPORT_SCHEMA })?;
    let (mut fixes, mut done) = (vec![], vec![]);
    for x in a.get("fixes").and_then(Value::as_array).into_iter().flatten() {
        let Some(p) = pick(x.get("problem"), probs.len()) else { continue };
        let pfixes = probs[p - 1]["fixes"].as_array().map(Vec::as_slice).unwrap_or(&[]);
        if let Some(f) = pick(x.get("fix"), pfixes.len())
            && !done.contains(&p)
        {
            done.push(p);
            fixes.push((probs[p - 1].clone(), pfixes[f - 1].clone(), x.get("why").map(text).unwrap_or_default()));
        }
    }
    let summary = a.get("summary").map(text).unwrap_or_default().trim().to_string();
    let mut out = Advice { summary, fixes, after, cause: None, mod_fixes: vec![] };
    if !report.is_empty() {
        out.cause = Some(a.get("cause").map(text).unwrap_or_default().trim().to_string());
        for x in a.get("mod_fixes").and_then(Value::as_array).into_iter().flatten().filter(|x| x.is_object()) {
            let action = x["action"].as_str().unwrap_or("");
            if let Some(m) = pick(x.get("mod"), mods.len())
                && ACTIONS.contains(&action)
            {
                out.mod_fixes.push((mods[m - 1].clone(), action.to_string(), x.get("why").map(text).unwrap_or_default()));
            }
        }
    }
    Ok(out)
}
