//! Find why the game crashed (or will), and list every way to fix each problem.
//!
//! A problem is {"id", "title", "detail", "severity", "auto", "fixes": [{"label", "action", "recommended"}]}.
//! `action` is a list the controller runs, e.g. ["disable", mod_id]. A problem with exactly one fix and
//! auto=true is fixed without asking; any other problem is shown to the user to pick a fix.
use regex::{NoExpand, Regex};
use serde_json::{Map, Value, json};
use shared::win::wide;
use shared::{FRAMEWORKS, Mod, Store, fw_by_nexus};
use std::cmp::Reverse;
use std::collections::{BTreeSet, BinaryHeap, HashMap, HashSet};
use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::{SystemTime, UNIX_EPOCH};
use windows_sys::Win32::Foundation::{FILETIME, SYSTEMTIME};
use windows_sys::Win32::Storage::FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW};
use windows_sys::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};

fn re(p: &str) -> Regex {
    Regex::new(p).expect("regex")
}

static LEVEL_OK: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)\[(info|debug|trace)\b"));
static ERR: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)\b(error|critical|fatal|exception|failed)\b"));
static PATH: LazyLock<Regex> =
    LazyLock::new(|| re(r#"(?:^|[\s'"(\[=])((?:archive|bin|engine|r6|red4ext|mods)/[^:'"()<>|*?,\r\n]+)"#));
static ARCH: LazyLock<Regex> = LazyLock::new(|| re(r#"([^\s/:'"]+\.archive)\b"#));
static STAMP: LazyLock<Regex> = LazyLock::new(|| re(r"^\[(\d{4}-\d\d-\d\d[ T]\d\d:\d\d:\d\d(?:\.\d+)?)"));
static LOADING: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)loading plugin"));

/// Reports of the Advanced Crash Reporter mod, when it is installed.
pub const ACR: &str = "bin/x64/plugins/AdvancedCrashReporter/reports";
/// Its Nexus mod id: offered after a crash with no clue.
pub const ACR_NEXUS: u64 = 34403;
/// Windows exception codes in plain words.
pub const CODES: &[(u64, &str)] = &[
    (0xC000_0005, "Code 0xC0000005: the game read or wrote memory it does not own. A DLL mod made for another game version often causes this."),
    (0xC000_0409, "Code 0xC0000409: the game found damaged memory on its call stack and stopped itself."),
    (0xC000_00FD, "Code 0xC00000FD: a function called itself again and again until the stack was full."),
    (0xC000_0374, "Code 0xC0000374: the game's memory heap became damaged, often by a mod that wrote past its memory."),
    (0xC000_001D, "Code 0xC000001D: the processor found an instruction it does not know."),
    (0x8000_0003, "Code 0x80000003: the game or a mod stopped on purpose after a fatal error."),
    (0xE06D_7363, "Code 0xE06D7363: a C++ error was thrown and nothing caught it."),
];
/// Where players put mods by hand.
pub const HAND: &[&str] =
    &["archive/pc/mod", "mods", "red4ext/plugins", "bin/x64/plugins/cyber_engine_tweaks/mods", "bin/x64/plugins", "r6/scripts", "r6/tweaks"];
/// Seconds: a DLL mod that loads slower than this slows every game start.
pub const SLOW: f64 = 3.0;
pub const LOGS: &[&str] = &["r6/logs", "red4ext/logs", "red4ext/plugins", "bin/x64/plugins/cyber_engine_tweaks"];
/// Folders that hold the game's own files and many mods' files: a crash or log there never blames one mod.
pub const SHARED: &[&str] = &[
    "archive", "archive/pc", "archive/pc/mod", "archive/pc/content", "bin", "bin/x64", "bin/x64/plugins",
    "bin/x64/plugins/cyber_engine_tweaks/mods", "engine", "engine/config", "engine/config/base", "engine/config/platform",
    "engine/config/platform/pc", "engine/tools", "mods", "r6", "r6/cache", "r6/config", "r6/input", "r6/logs", "r6/scripts",
    "r6/tweaks", "red4ext", "red4ext/logs", "red4ext/plugins",
];
pub const AI_ASK: &str = "My modded Cyberpunk 2077 crashes. Below is what my mod manager found. Tell me which mod to test first and why. \
                          Use only the mod names in this report, and do not invent mods or fixes.";
/// `keep_logs` defaults: how many crash zips stay, and the most bytes kept of one file.
pub const KEEP_LOGS: usize = 10;
pub const LOG_LIMIT: u64 = 20_000_000;

/// The plain-words text of an exception code.
pub fn code_text(code: Option<&Value>) -> Option<&'static str> {
    let c = code?.as_u64()?;
    CODES.iter().find(|x| x.0 == c).map(|x| x.1)
}

pub fn problem(pid: &str, title: &str, detail: &str, fixes: Vec<Value>, severity: &str, auto: bool) -> Value {
    json!({"id": pid, "title": title, "detail": detail, "fixes": fixes, "severity": severity, "auto": auto})
}

/// `action` is a JSON list: [verb, *args].
pub fn fix(label: &str, action: Value, recommended: bool) -> Value {
    json!({"label": label, "action": action, "recommended": recommended})
}

// ---- small helpers shared with watch.rs ----

/// Python truthiness of a JSON value.
pub(crate) fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// A JSON value as Python's `str()` prints it.
fn py_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "None".into(),
        Value::Bool(b) => if *b { "True" } else { "False" }.into(),
        v => v.to_string(),
    }
}

/// A lookup key for a JSON id (1 and "1" stay different, as in Python).
fn key(v: Option<&Value>) -> String {
    v.map(Value::to_string).unwrap_or_else(|| "null".into())
}

fn arr(v: Option<&Value>) -> &[Value] {
    v.and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[])
}

/// The strings in a JSON list.
fn strs(v: Option<&Value>) -> Vec<&str> {
    arr(v).iter().filter_map(Value::as_str).collect()
}

/// A non-empty string field.
fn get_str<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// Where in `module` an event happened ("Name.dll+0x10"), from its "places".
fn place<'a>(ev: &'a Value, module: &str) -> Option<&'a str> {
    ev.get("places").and_then(|p| p.get(module)).and_then(Value::as_str)
}

fn take_chars(s: &str, n: usize) -> &str {
    s.char_indices().nth(n).map_or(s, |(i, _)| &s[..i])
}

fn append_detail(p: &mut Value, more: &str) {
    if let Some(Value::String(d)) = p.get_mut("detail") {
        d.push_str(more);
    }
}

fn ignored(store: &Store) -> HashSet<String> {
    arr(store.doctor.get("ignored")).iter().map(py_str).collect()
}

fn names(mods: &[&Mod], n: usize) -> String {
    mods.iter().take(n).map(|m| m.name.as_str()).collect::<Vec<_>>().join(", ")
}

fn ids(mods: &[&Mod]) -> Vec<String> {
    mods.iter().map(|m| m.id.clone()).collect()
}

fn is_framework(m: &Mod) -> bool {
    m.framework.as_deref().is_some_and(|f| !f.is_empty())
}

/// The source of the Advanced Crash Reporter mod (its Nexus id may come as a number or a string).
pub fn is_acr(src: &Value) -> bool {
    let id = &src["mod_id"];
    src["type"] == "nexus" && (id.as_u64() == Some(ACR_NEXUS) || id.as_str().and_then(|s| s.trim().parse().ok()) == Some(ACR_NEXUS))
}

/// Advanced Crash Reporter is in the game folder: its folder holds more than the `reports` it writes (deploy cannot
/// remove those, so they stay after the mod is removed).
pub fn acr_installed(game: &Path) -> bool {
    let dir = gpath(game, ACR);
    let Some(dir) = dir.parent() else { return false };
    fs::read_dir(dir).into_iter().flatten().flatten().any(|e| !e.file_name().eq_ignore_ascii_case("reports"))
}

/// A game folder path from a posix game-relative path, with Windows separators.
fn gpath(game: &Path, rel: &str) -> PathBuf {
    game.join(rel.replace('/', "\\"))
}

/// A relative path with `/` separators.
fn posix(rel: &Path) -> String {
    rel.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/")
}

pub(crate) fn unix(t: SystemTime) -> f64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs_f64(),
        Err(e) => -e.duration().as_secs_f64(),
    }
}

pub(crate) fn now() -> f64 {
    unix(SystemTime::now())
}

pub(crate) fn mtime(p: &Path) -> io::Result<f64> {
    Ok(unix(fs::metadata(p)?.modified()?))
}

fn has_name(p: &Path, f: impl Fn(&str) -> bool) -> bool {
    p.file_name().is_some_and(|n| f(&n.to_string_lossy().to_lowercase()))
}

/// Local time of unix time `t`.
fn local(t: f64) -> SYSTEMTIME {
    let ft = ((t * 1e7) as i64 + 116_444_736_000_000_000) as u64;
    let ft = FILETIME { dwLowDateTime: ft as u32, dwHighDateTime: (ft >> 32) as u32 };
    let (mut utc, mut loc) = (SYSTEMTIME::default(), SYSTEMTIME::default());
    // SAFETY: plain value structs on the stack.
    unsafe {
        FileTimeToSystemTime(&ft, &mut utc);
        if SystemTimeToTzSpecificLocalTime(std::ptr::null(), &utc, &mut loc) == 0 {
            loc = utc;
        }
    }
    loc
}

/// Python `time.strftime(fmt, time.localtime(t))` for %Y %m %d %H %M %S %b.
pub(crate) fn strftime(fmt: &str, t: f64) -> String {
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let st = local(t);
    let mut out = String::new();
    let mut chars = fmt.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('Y') => out += &format!("{:04}", st.wYear),
            Some('m') => out += &format!("{:02}", st.wMonth),
            Some('d') => out += &format!("{:02}", st.wDay),
            Some('H') => out += &format!("{:02}", st.wHour),
            Some('M') => out += &format!("{:02}", st.wMinute),
            Some('S') => out += &format!("{:02}", st.wSecond),
            Some('b') => out += MONTHS[(st.wMonth.clamp(1, 12) - 1) as usize],
            Some(x) => {
                out.push('%');
                out.push(x);
            }
            None => out.push('%'),
        }
    }
    out
}

/// Python `str.splitlines()`.
pub(crate) fn splitlines(s: &str) -> Vec<&str> {
    let (mut out, mut start) = (vec![], 0);
    let mut it = s.char_indices().peekable();
    while let Some((i, c)) = it.next() {
        if matches!(c, '\n' | '\r' | '\x0b' | '\x0c' | '\x1c' | '\x1d' | '\x1e' | '\u{85}' | '\u{2028}' | '\u{2029}') {
            out.push(&s[start..i]);
            start = i + c.len_utf8();
            if c == '\r' && it.peek().is_some_and(|p| p.1 == '\n') {
                it.next();
                start += 1;
            }
        }
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

fn walk_dirs(d: &Path, dirs: &mut Vec<PathBuf>) {
    let Ok(rd) = fs::read_dir(d) else { return };
    let subs: Vec<PathBuf> = rd.flatten().filter(|e| e.file_type().is_ok_and(|t| t.is_dir())).map(|e| e.path()).collect();
    dirs.extend(subs.iter().cloned());
    for s in &subs {
        walk_dirs(s, dirs);
    }
}

/// Everything below `top`, in the order of Python's `Path.rglob("*")`; symlinked folders are not entered.
pub(crate) fn rglob(top: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![top.to_path_buf()];
    walk_dirs(top, &mut dirs);
    dirs.iter().filter_map(|d| fs::read_dir(d).ok()).flat_map(|rd| rd.flatten().map(|e| e.path())).collect()
}

// ---- who owns a game path ----

/// Lowercase game path -> mod id, for files and for folders that belong to one mod.
pub struct OwnerIndex {
    pub files: HashMap<String, String>,
    pub dirs: HashMap<String, String>,
}

/// A folder below the shared game folders belongs to a mod when that mod alone has files directly in it
/// (or, with no direct files, alone has files under it). Advanced Crash Reporter owns nothing: as the crash
/// handler it is on the stack of every crash, and its reports sit in its own folder, so it is never blamed.
pub fn owner_index(store: &Store) -> OwnerIndex {
    let mut files = HashMap::new();
    let mut direct: HashMap<String, HashSet<&str>> = HashMap::new();
    let mut under: HashMap<String, HashSet<&str>> = HashMap::new();
    for m in store.mods.iter().filter(|m| !is_acr(&m.source)) {
        for rel in &m.files {
            let r = rel.to_lowercase();
            let parts: Vec<&str> = r.split('/').collect();
            files.insert(r.clone(), m.id.clone());
            if r.starts_with("archive/pc/mod/") {
                files.entry(parts[parts.len() - 1].to_string()).or_insert_with(|| m.id.clone());
            }
            direct.entry(parts[..parts.len() - 1].join("/")).or_default().insert(&m.id);
            for i in 1..parts.len() {
                under.entry(parts[..i].join("/")).or_default().insert(&m.id);
            }
        }
    }
    let mut dirs = HashMap::new();
    for (d, mods) in &under {
        if SHARED.contains(&d.as_str()) {
            continue;
        }
        let own = direct.get(d);
        let own_n = own.map_or(0, HashSet::len);
        if own_n == 1 || (own_n == 0 && mods.len() == 1) {
            let set = if own_n == 1 { own.expect("own") } else { mods };
            dirs.insert(d.clone(), set.iter().next().expect("one").to_string());
        }
    }
    OwnerIndex { files, dirs }
}

pub fn owner_of(path: &str, idx: &OwnerIndex) -> Option<String> {
    let s = path.to_lowercase().replace('\\', "/");
    // Real paths are short; a huge log line must not explode.
    let s: String = s.trim_matches(|c| c == '/' || c == ' ').chars().take(300).collect();
    // Paths may hold spaces.
    let mut cands = vec![s.as_str()];
    cands.extend(s.char_indices().rev().filter(|&(i, c)| c == ' ' && i > 0).map(|(i, _)| &s[..i]));
    if let Some(c) = cands.iter().find(|c| idx.files.contains_key(**c)) {
        return idx.files.get(*c).cloned();
    }
    let mut hits: Vec<&str> = vec![];
    for c in &cands {
        let mut p = *c;
        while let Some(i) = p.rfind('/') {
            p = &p[..i];
            if idx.dirs.contains_key(p) {
                hits.push(p);
                break;
            }
        }
    }
    hits.iter().max_by_key(|h| h.len()).and_then(|h| idx.dirs.get(*h).cloned())
}

pub fn owners_in(text: &str, idx: &OwnerIndex, game: Option<&Path>) -> Option<String> {
    let mut t = text.to_lowercase().replace('\\', "/");
    if let Some(g) = game {
        let g = g.to_string_lossy().to_lowercase().replace('\\', "/");
        t = t.replace(&format!("{}/", g.trim_end_matches('/')), " ");
    }
    PATH.captures_iter(&t)
        .find_map(|c| owner_of(&c[1], idx))
        .or_else(|| ARCH.captures_iter(&t).find_map(|c| idx.files.get(&c[1]).cloned()))
}

// ---- logs ----

/// Errors one mod logged.
#[derive(Clone, Debug, Default)]
pub struct LogHit {
    pub lines: Vec<String>,
    /// A redscript or RED4ext-plugin error, which stops mods from loading; CET Lua errors are weak.
    pub strong: bool,
}

/// The last `max` bytes of a file.
fn read_tail(p: &Path, size: u64, max: u64) -> io::Result<Vec<u8>> {
    let mut f = fs::File::open(p)?;
    f.seek(SeekFrom::Start(size.saturating_sub(max)))?;
    let mut buf = vec![];
    f.read_to_end(&mut buf)?;
    Ok(buf)
}

/// [(mod id, errors)] from framework logs written since `since`, in the order first seen.
pub fn log_errors(game: &Path, since: f64, idx: &OwnerIndex) -> Vec<(String, LogHit)> {
    let mut found: Vec<(String, LogHit)> = vec![];
    for base in LOGS {
        let d = gpath(game, base);
        if !d.is_dir() {
            continue;
        }
        for log in rglob(&d).into_iter().filter(|p| has_name(p, |n| n.ends_with(".log"))) {
            let Ok(md) = fs::metadata(&log) else { continue };
            let Ok(t) = md.modified() else { continue };
            if unix(t) < since {
                continue;
            }
            // Only the end of the log: some grow to gigabytes.
            let Ok(bytes) = read_tail(&log, md.len(), 2_000_000) else { continue };
            let text = String::from_utf8_lossy(&bytes);
            let all = splitlines(&text);
            let lines = &all[all.len().saturating_sub(5000)..];
            // A log inside a mod's own folder is about that mod; shared log folders need a path in the line.
            let rel = log.strip_prefix(game).map(posix).unwrap_or_default();
            let log_owner = if base.ends_with("/logs") { None } else { owner_of(&rel, idx) };
            let log_name = log.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            for (i, line) in lines.iter().enumerate() {
                if LEVEL_OK.is_match(line) || !ERR.is_match(line) {
                    continue;
                }
                let near = lines[i..(i + 3).min(lines.len())].join(" ");
                let Some(mid) = owners_in(&near, idx, Some(game)).or_else(|| log_owner.clone()) else { continue };
                let at = match found.iter().position(|f| f.0 == mid) {
                    Some(at) => at,
                    None => {
                        found.push((mid, LogHit::default()));
                        found.len() - 1
                    }
                };
                let hit = &mut found[at].1;
                hit.strong |= !base.starts_with("bin/");
                if hit.lines.len() < 8 {
                    hit.lines.push(format!("{log_name}: {}", take_chars(line.trim(), 220)));
                }
            }
        }
    }
    found
}

pub fn mod_fixes(m: &Mod, has_update: &dyn Fn(&Mod) -> bool) -> Vec<Value> {
    // Advanced Crash Reporter is always on (owner decision 2026-11-21): never offer to turn it off
    let mut fixes = if is_acr(&m.source) { vec![] } else { vec![fix(&format!("Disable {}", m.name), json!(["disable", m.id]), false)] };
    if has_update(m) {
        fixes.insert(0, fix(&format!("Update {} to its newest version", m.name), json!(["update", m.id]), true));
    }
    fixes
}

pub fn missing_frameworks(store: &Store) -> Vec<Value> {
    let on = store.enabled();
    let have: HashSet<Option<&str>> = on.iter().map(|m| m.framework.as_deref()).collect();
    let mut out = vec![];
    for (fw, _, _) in FRAMEWORKS {
        let users: Vec<&Mod> = on.iter().copied().filter(|m| m.needs.iter().any(|n| n == fw)).collect();
        if users.is_empty() || have.contains(&Some(*fw)) {
            continue;
        }
        let first = match store.mods.iter().find(|m| m.framework.as_deref() == Some(*fw)) {
            Some(off) => fix(&format!("Turn {fw} back on"), json!(["enable", off.id]), true),
            None => fix(&format!("Install {fw} (automatic download)"), json!(["install_fw", fw]), true),
        };
        let all = fix(&format!("Disable the {} mod(s) that need it", users.len()), json!(["disable_many", ids(&users)]), false);
        out.push(problem(&format!("need-{fw}"), &format!("{fw} is missing"), &format!("Needed by: {}", names(&users, 6)), vec![first, all], "high", true));
    }
    out
}

pub fn missing_requirements(store: &Store) -> Vec<Value> {
    let on = store.enabled();
    let have: HashSet<String> = on.iter().filter(|m| m.source_type() == "nexus").map(|m| key(m.source.get("mod_id"))).collect();
    // (key, mod id, name, mods that need it), in the order first seen
    let mut need: Vec<(String, Value, String, Vec<&Mod>)> = vec![];
    for m in &on {
        for r in &m.requires {
            let rid = r.get("mod_id").cloned().unwrap_or(Value::Null);
            let k = rid.to_string();
            if have.contains(&k) || rid.as_u64().and_then(fw_by_nexus).is_some() {
                continue;
            }
            match need.iter_mut().find(|n| n.0 == k) {
                Some(n) => n.3.push(m),
                None => need.push((k, rid, r.get("name").map(py_str).unwrap_or_default(), vec![m])),
            }
        }
    }
    need.into_iter()
        .map(|(_, rid, name, users)| {
            problem(
                &format!("req-{}", py_str(&rid)),
                &format!("Required mod missing: {name}"),
                &format!("Needed by: {}", names(&users, 6)),
                vec![
                    fix(&format!("Download {name}"), json!(["queue_nexus", rid]), true),
                    fix(&format!("Disable the {} mod(s) that need it", users.len()), json!(["disable_many", ids(&users)]), false),
                ],
                "high",
                true,
            )
        })
        .collect()
}

// ---- archives and conflicts ----

/// Resource hashes inside a .archive (RDAR v12: 40-byte header, index of 56-byte file entries).
pub fn archive_hashes(path: &Path) -> BTreeSet<u64> {
    let read = || -> io::Result<BTreeSet<u64>> {
        let mut f = fs::File::open(path)?;
        let mut head = [0u8; 20];
        f.read_exact(&mut head)?;
        let le4 = |b: &[u8]| u32::from_le_bytes(b.try_into().expect("4 bytes"));
        if le4(&head[0..4]) != 0x5241_4452 {
            return Ok(BTreeSet::new());
        }
        let ipos = u64::from_le_bytes(head[8..16].try_into().expect("8 bytes"));
        f.seek(SeekFrom::Start(ipos))?;
        let mut idx = vec![];
        f.take(le4(&head[16..20]) as u64).read_to_end(&mut idx)?;
        if idx.len() < 20 {
            return Ok(BTreeSet::new());
        }
        let n = le4(&idx[16..20]) as usize;
        let count = n.min(idx.len().saturating_sub(28) / 56);
        Ok((0..count).map(|i| u64::from_le_bytes(idx[28 + i * 56..36 + i * 56].try_into().expect("8 bytes"))).collect())
    };
    read().unwrap_or_default()
}

/// One game resource: an archive resource hash or a loose game file.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum Res {
    Hash(u64),
    File(String),
}

/// (mod pair (lower, higher in the list), how many resources both change), in the order first seen.
pub type Pairs = Vec<((String, String), usize)>;

/// (sizes, pairs): how many archive resources and loose game files each enabled mod changes, and how many
/// of them each pair of mods (lower, higher in the list) both change.
pub fn overlaps(store: &Store) -> (HashMap<String, usize>, Pairs) {
    let mut owners: Vec<Vec<String>> = vec![];
    let mut at: HashMap<Res, usize> = HashMap::new();
    let mut sizes: HashMap<String, usize> = HashMap::new();
    for m in store.enabled() {
        let mut res = BTreeSet::new();
        for r in &m.files {
            let l = r.to_lowercase();
            if l.ends_with(".archive") {
                res.extend(archive_hashes(&store.staging.join(&m.id).join(r)).into_iter().map(Res::Hash));
            } else {
                res.insert(Res::File(l)); // only one copy can be in the game
            }
        }
        sizes.insert(m.id.clone(), res.len());
        for h in res {
            let i = *at.entry(h).or_insert_with(|| {
                owners.push(vec![]);
                owners.len() - 1
            });
            owners[i].push(m.id.clone());
        }
    }
    let (mut pairs, mut pi): (Pairs, HashMap<(String, String), usize>) = Default::default();
    for o in &owners {
        for i in 0..o.len() {
            for j in i + 1..o.len() {
                let k = (o[i].clone(), o[j].clone());
                match pi.get(&k) {
                    Some(&n) => pairs[n].1 += 1,
                    None => {
                        pi.insert(k.clone(), pairs.len());
                        pairs.push((k, 1));
                    }
                }
            }
        }
    }
    (sizes, pairs)
}

/// Pairs by count, highest first; ties keep their order (Python's `Counter.most_common`).
fn most_common(pairs: &Pairs) -> Pairs {
    let mut v = pairs.clone();
    v.sort_by_key(|a| Reverse(a.1));
    v
}

pub fn archive_conflicts(store: &Store) -> Vec<Value> {
    let (_, pairs) = overlaps(store);
    let ign = ignored(store);
    let mut out = vec![];
    for ((lo, hi), n) in most_common(&pairs).into_iter().take(50) {
        let pid = format!("conflict-{lo}-{hi}");
        if ign.contains(&pid) {
            continue;
        }
        let (Some(a), Some(b)) = (store.mod_by_id(&lo), store.mod_by_id(&hi)) else { continue };
        out.push(problem(
            &pid,
            &format!("{} and {} change the same {n} game file(s)", a.name, b.name),
            &format!("{} wins now (it is lower in the list).", b.name),
            vec![
                fix(&format!("Keep {} winning", b.name), json!(["ignore", pid]), true),
                fix(&format!("Let {} win", a.name), json!(["prefer", lo, hi]), false),
                fix(&format!("Disable {}", a.name), json!(["disable", lo]), false),
                fix(&format!("Disable {}", b.name), json!(["disable", hi]), false),
            ],
            "info",
            false,
        ));
    }
    out
}

pub fn changed_since_good(store: &Store) -> Value {
    let good = store.doctor.get("last_good").filter(|g| !g.is_null());
    let changed: Vec<&Mod> = store
        .enabled()
        .into_iter()
        .filter(|m| good.is_some_and(|g| g.get(&m.id) != Some(&Value::String(m.version.clone()))))
        .collect();
    let mut fixes = vec![fix(
        "Find the bad mod automatically (disables half the mods per game launch)",
        json!(["bisect_start"]),
        changed.is_empty(),
    )];
    if !changed.is_empty() {
        fixes.insert(
            0,
            fix(
                &format!("Disable the {} mod(s) added or changed since the last launch that worked", changed.len()),
                json!(["disable_many", ids(&changed)]),
                true,
            ),
        );
    }
    let detail = if changed.is_empty() {
        "No launch without a crash is on record yet.".to_string()
    } else {
        format!("Changed since the last good launch: {}", names(&changed, 8))
    };
    problem("unknown", "The crash left no clue in the logs", &detail, fixes, "high", false)
}

// ---- load order ----

/// Every mod id in the load order most likely to work (index 0 = lowest priority). Frameworks go first,
/// each mod wins over the mods it requires, and of two mods that change the same game resources the one
/// that changes fewer wins: it is usually a patch or add-on for the bigger one. A conflict the user chose
/// to keep keeps its winner. `after`: more (lo, hi) pairs where hi must win, such as the patches the local AI
/// found by name; they rank below the rules above them, above the size rule. A mod that no rule moves keeps its
/// place relative to the others.
pub fn best_order(store: &Store, after: &[(String, String)]) -> Vec<String> {
    let mods: Vec<&Mod> = store.mods.iter().collect();
    let (sizes, pairs) = overlaps(store);
    order_with(&mods, &sizes, &pairs, &ignored(store), after)
}

/// hi wins over lo, unless the rules so far already make lo win over hi.
fn wins<'a>(beats: &mut HashMap<&'a str, HashSet<&'a str>>, lo: &'a str, hi: &'a str) {
    // ponytail: one graph walk per rule; fine for hundreds of mods, keep reachability sets if it gets slow
    let (mut seen, mut todo) = (HashSet::new(), vec![hi]);
    while let Some(x) = todo.pop() {
        if x == lo {
            return;
        }
        if seen.insert(x) {
            todo.extend(beats[x].iter().copied());
        }
    }
    beats.get_mut(lo).expect("mod").insert(hi);
}

pub(crate) fn order_with(
    mods: &[&Mod],
    sizes: &HashMap<String, usize>,
    pairs: &Pairs,
    kept: &HashSet<String>,
    after: &[(String, String)],
) -> Vec<String> {
    let pos: HashMap<&str, usize> = mods.iter().enumerate().map(|(i, m)| (m.id.as_str(), i)).collect();
    let fw: HashSet<&str> = mods.iter().filter(|m| is_framework(m)).map(|m| m.id.as_str()).collect();
    // mod -> the mods that must win over it
    let mut beats: HashMap<&str, HashSet<&str>> = pos.keys().map(|&k| (k, HashSet::new())).collect();
    let mut rules = requires_pairs(mods);
    for ((a, b), _) in pairs {
        if kept.contains(&format!("conflict-{a}-{b}")) {
            rules.push((a.clone(), b.clone()));
        } else if kept.contains(&format!("conflict-{b}-{a}")) {
            rules.push((b.clone(), a.clone()));
        }
    }
    rules.extend(after.iter().filter(|(lo, hi)| pos.contains_key(lo.as_str()) && pos.contains_key(hi.as_str())).cloned());
    let size = |id: &str| sizes.get(id).copied().unwrap_or(0);
    for ((a, b), _) in most_common(pairs) {
        if size(&a) != size(&b) {
            rules.push(if size(&a) > size(&b) { (a, b) } else { (b, a) });
        }
    }
    for (lo, hi) in &rules {
        // in pos: not a mod added meanwhile
        let (Some((&lo, _)), Some((&hi, _))) = (pos.get_key_value(lo.as_str()), pos.get_key_value(hi.as_str())) else { continue };
        if lo != hi && !fw.contains(lo) && !fw.contains(hi) {
            wins(&mut beats, lo, hi);
        }
    }
    let mut waits: HashMap<&str, usize> = HashMap::new();
    for &hi in beats.values().flatten() {
        *waits.entry(hi).or_default() += 1;
    }
    let mut ready: BinaryHeap<Reverse<(bool, usize)>> =
        pos.iter().filter(|(mid, _)| !waits.contains_key(*mid)).map(|(mid, &i)| Reverse((!fw.contains(mid), i))).collect();
    let mut out = vec![];
    // the earliest free mod next, frameworks before all others
    while let Some(Reverse((_, i))) = ready.pop() {
        let mid = mods[i].id.as_str();
        out.push(mid.to_string());
        for &hi in &beats[mid] {
            let w = waits.get_mut(hi).expect("waiting");
            *w -= 1;
            if *w == 0 {
                ready.push(Reverse((!fw.contains(hi), pos[hi])));
            }
        }
    }
    out
}

/// How many mods the order `new` moves compared with `old`: all but the most that keep their order.
pub fn moved(old: &[String], new: &[String]) -> usize {
    let pos: HashMap<&str, usize> = old.iter().enumerate().map(|(i, m)| (m.as_str(), i)).collect();
    let places: Vec<usize> = new.iter().filter_map(|m| pos.get(m.as_str()).copied()).collect();
    // the longest increasing run of old places (patience sort) is what stays
    let mut tails: Vec<usize> = vec![];
    for &i in &places {
        let k = tails.partition_point(|&t| t <= i);
        if k == tails.len() {
            tails.push(i);
        } else {
            tails[k] = i;
        }
    }
    places.len() - tails.len()
}

// ---- crash evidence ----

/// (path, text) of the newest Advanced Crash Reporter report written since `since`, or None.
pub fn acr_report(game: &Path, since: f64) -> Option<(PathBuf, String)> {
    let mut reps: Vec<(f64, PathBuf)> = vec![];
    for e in fs::read_dir(gpath(game, ACR)).into_iter().flatten().flatten() {
        let f = e.path();
        if let Ok(md) = fs::metadata(&f)
            && let (true, Ok(t)) = (md.is_file(), md.modified())
            && unix(t) >= since
        {
            reps.push((unix(t), f));
        }
    }
    let f = reps.into_iter().max_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.cmp(&b.1)))?.1;
    // the summary is at the top; the full report lists every installed mod
    let mut buf = vec![];
    fs::File::open(&f).ok()?.take(6000).read_to_end(&mut buf).ok()?;
    Some((f, String::from_utf8_lossy(&buf).into_owned()))
}

/// Mods whose code is on the stack of the crashing thread (or of the main thread in a freeze). With no mod blamed yet,
/// the newest one is the main suspect.
pub fn call_path(
    ev: &Value,
    idx: &OwnerIndex,
    game: &Path,
    blamed: &mut HashSet<String>,
    store: &Store,
    has_update: &dyn Fn(&Mod) -> bool,
    what: &str,
) -> Vec<Value> {
    let mut probs = vec![];
    for module in strs(ev.get("stack")) {
        let Some(mid) = owners_in(module, idx, Some(game)) else { continue };
        if blamed.contains(&mid) {
            continue;
        }
        let Some(m) = store.mod_by_id(&mid) else { continue };
        let first = blamed.is_empty();
        blamed.insert(mid.clone());
        let (title, why) = if first {
            (format!("{} was running when the game {what}", m.name), "This is a strong hint, not proof.")
        } else {
            (format!("{} is in the call path", m.name), "An earlier call on the same thread, so it is less likely the cause.")
        };
        let at = place(ev, module).map(|w| format!("\nWhere: {w}")).unwrap_or_default();
        let kind = if what == "crashed" { "stack" } else { "freeze" };
        probs.push(problem(
            &format!("{kind}-{mid}"),
            &title,
            &format!("{module}{at}\n{why}"),
            mod_fixes(m, has_update),
            if first { "high" } else { "medium" },
            false,
        ));
    }
    probs
}

/// Problems from the snapshot of a game that stopped responding and never came back.
pub fn freeze_problems(
    hang: &Value,
    idx: &OwnerIndex,
    game: &Path,
    blamed: &mut HashSet<String>,
    store: &Store,
    has_update: &dyn Fn(&Mod) -> bool,
) -> Vec<Value> {
    let mut probs = call_path(hang, idx, game, blamed, store, has_update, "froze");
    let mut seen: Vec<Option<String>> = vec![];
    for m in strs(hang.get("others")) {
        let o = owners_in(m, idx, Some(game));
        if !seen.contains(&o) {
            seen.push(o);
        }
    }
    let others: Vec<&str> = seen
        .iter()
        .flatten()
        .filter(|mid| !blamed.contains(*mid))
        .filter_map(|mid| store.mod_by_id(mid))
        .map(|m| m.name.as_str())
        .collect();
    let note = if others.is_empty() {
        String::new()
    } else {
        format!("\nMod code on other game threads (each may be waiting, not stuck): {}", others.join(", "))
    };
    match probs.first_mut() {
        Some(p) => append_detail(p, &note),
        None => probs.push(problem(
            "freeze",
            "The game froze, with no mod code on its main thread",
            &format!(
                "The game stopped responding and did not come back. A mesh, texture or tweak mod (no code) can do this too.{note}"
            ),
            vec![fix("Find the bad mod automatically (disables half the mods per game launch)", json!(["bisect_start"]), false)],
            "high",
            false,
        )),
    }
    probs
}

/// [(game path, modified time)] of mods in the game folder that this app did not install, newest first.
pub fn hand_installed(store: &Store) -> Vec<(String, f64)> {
    let Some(game) = store.game() else { return vec![] };
    let mut ours = HashSet::new();
    for m in &store.mods {
        for rel in &m.files {
            let l = rel.to_lowercase();
            let parts: Vec<&str> = l.split('/').collect();
            for i in 1..=parts.len() {
                ours.insert(parts[..i].join("/"));
            }
        }
    }
    let mut out = vec![];
    for root in HAND {
        let Ok(rd) = fs::read_dir(gpath(&game, root)) else { continue };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let rel = format!("{root}/{name}");
            let l = rel.to_lowercase();
            if ours.contains(&l) || HAND.contains(&l.as_str()) || name.to_lowercase() == "cyber_engine_tweaks" {
                continue;
            }
            if let Ok(t) = mtime(&e.path()) {
                out.push((rel, t));
            }
        }
    }
    out.sort_by(|a, b| b.1.total_cmp(&a.1));
    out
}

pub fn recent_hand_installed(store: &Store, days: u32) -> Vec<Value> {
    let cut = now() - days as f64 * 86400.0;
    let hand: Vec<(String, f64)> = hand_installed(store).into_iter().filter(|h| h.1 >= cut).collect();
    let Some(first) = hand.first() else { return vec![] };
    let pid = format!("manual-{}", first.1 as i64);
    if ignored(store).contains(&pid) {
        return vec![];
    }
    let list: Vec<String> = hand.iter().take(20).map(|(rel, t)| format!("{}  {rel}", strftime("%Y-%m-%d", *t))).collect();
    vec![problem(
        &pid,
        &format!("{} mod(s) installed by hand changed in the last {days} days", hand.len()),
        &format!(
            "This app did not install these, so it cannot turn them off. To test one, move it out of the game folder:\n{}",
            list.join("\n")
        ),
        vec![fix("Hide this", json!(["ignore", pid]), false)],
        "medium",
        false,
    )]
}

/// File version of the game's exe, e.g. "3.0.81.8013", or its size when Windows gives no version.
pub fn game_version(game: &Path) -> Option<String> {
    let exe = game.join(r"bin\x64\Cyberpunk2077.exe").to_string_lossy().replace('/', "\\");
    let w = wide(&exe);
    // SAFETY: valid NUL-terminated strings; `buf` holds `n` bytes and outlives `ptr`, which points into it.
    unsafe {
        let n = GetFileVersionInfoSizeW(w.as_ptr(), std::ptr::null_mut());
        if n != 0 {
            let mut buf = vec![0u8; n as usize];
            let (mut ptr, mut size, root) = (std::ptr::null_mut(), 0u32, wide("\\"));
            if GetFileVersionInfoW(w.as_ptr(), 0, n, buf.as_mut_ptr().cast()) != 0
                && VerQueryValueW(buf.as_ptr().cast(), root.as_ptr(), &mut ptr, &mut size) != 0
                && size >= 16
            {
                // VS_FIXEDFILEINFO: signature, struct version, MS, LS
                let b = std::slice::from_raw_parts(ptr as *const u8, 16);
                let ms = u32::from_le_bytes(b[8..12].try_into().expect("4 bytes"));
                let ls = u32::from_le_bytes(b[12..16].try_into().expect("4 bytes"));
                return Some(format!("{}.{}.{}.{}", ms >> 16, ms & 0xFFFF, ls >> 16, ls & 0xFFFF));
            }
        }
    }
    fs::metadata(&exe).ok().map(|m| format!("size {}", m.len()))
}

/// A problem when the game changed since the last launch that worked: frameworks often break after a patch.
pub fn game_updated(store: &Store, has_update: &dyn Fn(&Mod) -> bool) -> Vec<Value> {
    game_updated_with(store, has_update, &game_version)
}

pub(crate) fn game_updated_with(
    store: &Store,
    has_update: &dyn Fn(&Mod) -> bool,
    version: &dyn Fn(&Path) -> Option<String>,
) -> Vec<Value> {
    let (game, old) = (store.game(), store.doctor.get("last_good_game"));
    let new = match (&game, old) {
        (Some(g), Some(o)) if truthy(o) => version(g),
        _ => None,
    };
    let Some(new) = new.filter(|n| !n.is_empty()) else { return vec![] };
    let pid = format!("game-{new}");
    let old = old.expect("checked above");
    if *old == Value::String(new.clone()) || ignored(store).contains(&pid) {
        return vec![];
    }
    let ups: Vec<&Mod> = store.enabled().into_iter().filter(|m| is_framework(m) && has_update(m)).collect();
    let mut fixes: Vec<Value> = ups
        .iter()
        .map(|m| fix(&format!("Update {} to its newest version", m.name), json!(["update", m.id]), true))
        .collect();
    fixes.push(fix("Hide this", json!(["ignore", pid]), false));
    let found = if ups.is_empty() {
        "No framework update is known yet. Check their Nexus pages.".to_string()
    } else {
        format!("Updates found: {}", names(&ups, usize::MAX))
    };
    vec![problem(
        &pid,
        "The game was updated since the last launch that worked",
        &format!(
            "Game version {} -> {new}. RED4ext, Cyber Engine Tweaks, redscript and other frameworks often need an update after a game patch. {found}",
            py_str(old)
        ),
        fixes,
        if ups.is_empty() { "medium" } else { "high" },
        false,
    )]
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468
}

/// Microseconds of a log time stamp "YYYY-MM-DD HH:MM:SS(.ffffff)", or None when it is no real time.
fn stamp_us(s: &str) -> Option<i64> {
    let num = |a: usize, b: usize| s.get(a..b)?.parse::<i64>().ok();
    let (y, mo, d, h, mi, se) = (num(0, 4)?, num(5, 7)?, num(8, 10)?, num(11, 13)?, num(14, 16)?, num(17, 19)?);
    let us = match s.get(19..)?.strip_prefix('.') {
        Some(f) => format!("{:0<6}", f.chars().take(6).collect::<String>()).parse::<i64>().ok()?,
        None => 0,
    };
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let dim = [31, if leap { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    if y < 1 || !(1..=12).contains(&mo) || d < 1 || d > dim[mo as usize - 1] || h > 23 || mi > 59 || se > 59 {
        return None;
    }
    Some((((days_from_civil(y, mo, d) * 24 + h) * 60 + mi) * 60 + se) * 1_000_000 + us)
}

/// [(mod id, seconds)] for DLL mods that took longer than `limit` to load, from RED4ext's log: the time from the line
/// that loads the mod to the next line.
pub fn slow_mods(game: &Path, since: f64, idx: &OwnerIndex, limit: f64) -> Vec<(String, f64)> {
    let mut logs: Vec<(f64, PathBuf)> = vec![];
    for e in fs::read_dir(gpath(game, "red4ext/logs")).into_iter().flatten().flatten() {
        let p = e.path();
        if !has_name(&p, |n| n.starts_with("red4ext") && n.ends_with(".log")) {
            continue;
        }
        match mtime(&p) {
            Ok(t) if t >= since => logs.push((t, p)),
            Ok(_) => {}
            Err(_) => return vec![],
        }
    }
    let Some(newest) = logs.iter().fold(None::<&(f64, PathBuf)>, |best, l| match best {
        Some(b) if b.0 >= l.0 => Some(b),
        _ => Some(l),
    }) else {
        return vec![];
    };
    // the log changed while read, or a bad time stamp
    let Ok(bytes) = fs::read(&newest.1) else { return vec![] };
    let text = String::from_utf8_lossy(&bytes);
    let all = splitlines(&text);
    let mut stamped: Vec<(i64, &str)> = vec![];
    for &line in &all[all.len().saturating_sub(5000)..] {
        if let Some(c) = STAMP.captures(line) {
            let Some(t) = stamp_us(&c[1]) else { return vec![] };
            stamped.push((t, line));
        }
    }
    let mut found: Vec<(String, f64)> = vec![];
    for w in stamped.windows(2) {
        let ((t, line), (t2, _)) = (w[0], w[1]);
        if !LOADING.is_match(line) {
            continue;
        }
        let Some(mid) = owners_in(line, idx, Some(game)) else { continue };
        let secs = (t2 - t) as f64 / 1e6;
        let at = found.iter().position(|f| f.0 == mid);
        // a longer gap is a quiet log, not a slow load
        if limit.max(at.map_or(0.0, |i| found[i].1)) < secs && secs < 120.0 {
            match at {
                Some(i) => found[i].1 = secs,
                None => found.push((mid, secs)),
            }
        }
    }
    found
}

fn home_dir() -> Option<String> {
    let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    var("USERPROFILE").or_else(|| Some(format!("{}{}", var("HOMEDRIVE").unwrap_or_default(), var("HOMEPATH")?)))
}

/// Minutes of a "secs" value, like Python's `secs // 60`.
fn minutes(v: Option<&Value>) -> String {
    match v {
        Some(Value::Number(n)) if n.is_i64() || n.is_u64() => n.as_i64().unwrap_or(i64::MAX).div_euclid(60).to_string(),
        Some(Value::Number(n)) => format!("{:?}", (n.as_f64().unwrap_or(0.0) / 60.0).floor()),
        _ => "0".into(),
    }
}

/// Plain-text report to share with a mod author, or (for_ai) to paste into an AI chat. Folder paths of this PC are hidden.
pub fn report(store: &Store, probs: &[Value], crash: Option<&Value>, for_ai: bool) -> String {
    let game = store.game();
    let mut out: Vec<String> = if for_ai { vec![AI_ASK.into(), String::new()] } else { vec![] };
    out.push(format!("Cyberpunk 2077 AI Mod Manager crash report, {}", strftime("%Y-%m-%d %H:%M", now())));
    if let Some(g) = &game {
        out.push(format!("Game version: {}", game_version(g).unwrap_or_else(|| "None".into())));
    }
    if let Some(c) = crash.filter(|c| truthy(c)) {
        out.push(String::new());
        out.push("== LAST GAME EXIT ==".into());
        out.push(format!("Crashed: {}", if c.get("crashed").is_some_and(truthy) { "yes" } else { "no" }));
        if let Some(e) = c.get("exit_code").filter(|e| !e.is_null()) {
            out.push(format!("Exit code: 0x{:08X}", e.as_u64().unwrap_or(0)));
        }
        if let Some(t) = code_text(c.get("code")) {
            out.push(t.into());
        }
        if let Some(m) = get_str(c, "module") {
            out.push(format!("Crashed in: {m}  {}", place(c, m).unwrap_or("")));
        }
        let empty = json!({});
        let hang = c.get("hang").filter(|h| truthy(h)).unwrap_or(&empty);
        for (name, ev) in [("Crashing thread", c), ("Main thread at the freeze", hang)] {
            let stack = strs(ev.get("stack"));
            if !stack.is_empty() {
                out.push(format!("{name}, newest call first:"));
                out.extend(stack.iter().map(|m| format!("  {}  {m}", place(ev, m).unwrap_or(""))));
            }
        }
        let others = strs(hang.get("others"));
        if !others.is_empty() {
            out.push(format!("Other threads at the freeze: {}", others.join(", ")));
        }
    }
    let crashes = arr(store.doctor.get("crashes"));
    let recent = &crashes[crashes.len().saturating_sub(5)..];
    if !recent.is_empty() {
        out.push(String::new());
        out.push("== RECENT CRASHES ==".into());
        for c in recent {
            let fault = c.get("fault").filter(|f| truthy(f)).map(py_str).unwrap_or_else(|| "no crash ID".into());
            let when = strftime("%Y-%m-%d %H:%M", c.get("time").and_then(Value::as_f64).unwrap_or(0.0));
            out.push(format!("{when}  {fault}, after {} min", minutes(c.get("secs"))));
        }
    }
    out.push(String::new());
    out.push("== PROBLEMS FOUND ==".into());
    if probs.is_empty() {
        out.push("None".into());
    }
    for p in probs {
        let detail = p.get("detail").map(py_str).unwrap_or_default().replace('\n', "\n  ");
        out.push(format!("[{}] {}\n  {detail}", p.get("severity").map(py_str).unwrap_or_default(), p.get("title").map(py_str).unwrap_or_default()));
    }
    out.push(String::new());
    out.push("== MODS IN LOAD ORDER (lower wins) ==".into());
    out.extend(store.mods.iter().map(|m| format!("{}  {}  {}", if m.enabled { "on " } else { "off" }, m.name, m.version)));
    let hand = hand_installed(store);
    if !hand.is_empty() {
        out.push(String::new());
        out.push("== MODS INSTALLED BY HAND ==".into());
        out.extend(hand.iter().map(|(rel, t)| format!("{}  {rel}", strftime("%Y-%m-%d", *t))));
    }
    let mut text = out.join("\n") + "\n";
    let hide = [
        (game.map(|g| g.to_string_lossy().into_owned()), "<game>"),
        (Some(store.root.to_string_lossy().into_owned()), "<data folder>"),
        (home_dir(), "%USERPROFILE%"),
    ];
    for (path, name) in hide {
        let Some(path) = path.filter(|p| !p.is_empty()) else { continue };
        for p in [path.replace('/', "\\"), path.replace('\\', "/")] {
            // Windows paths may differ in case
            let r = Regex::new(&format!("(?i){}", regex::escape(&p))).expect("regex");
            text = r.replace_all(&text, NoExpand(name)).into_owned();
        }
    }
    text
}

/// All problems, worst first. `crash` is `watch::crash_evidence()` output when the game just crashed.
pub fn diagnose(store: &Store, since: f64, crash: Option<&Value>, has_update: &dyn Fn(&Mod) -> bool) -> Vec<Value> {
    let game = store.game();
    let idx = owner_index(store);
    let crash = crash.filter(|c| truthy(c));
    let crashed = crash.is_some_and(|c| c.get("crashed").is_some_and(truthy));
    // `since` is when polling saw the game (up to one poll late): look a little further back for early log lines
    let back = if since != 0.0 { since - 15.0 } else { 0.0 };
    let mut probs = missing_frameworks(store);
    probs.extend(missing_requirements(store));
    probs.extend(game_updated(store, has_update));
    let mut blamed = HashSet::new();
    if let (Some(c), Some(g)) = (crash, &game) {
        if let Some(module) = get_str(c, "module")
            && let Some(m) = owners_in(module, &idx, Some(g)).and_then(|mid| store.mod_by_id(&mid))
        {
            blamed.insert(m.id.clone());
            let mut detail = module.to_string();
            if let Some(w) = place(c, module).filter(|w| !w.is_empty()) {
                detail += &format!("\nWhere: {w}");
            }
            if let Some(why) = code_text(c.get("code")) {
                detail += &format!("\n{why}");
            }
            probs.push(problem(&format!("crash-{}", m.id), &format!("The game crashed inside {}", m.name), &detail, mod_fixes(m, has_update), "high", true));
        }
        probs.extend(call_path(c, &idx, g, &mut blamed, store, has_update, "crashed"));
        if let Some(h) = c.get("hang").filter(|h| truthy(h)) {
            probs.extend(freeze_problems(h, &idx, g, &mut blamed, store, has_update));
        }
        if let Some((path, text)) = if crashed { acr_report(g, back) } else { None } {
            // the full mod list comes after the verdict
            let head = text.split("INSTALLED MODS").next().unwrap_or("");
            let m = owners_in(head, &idx, Some(g)).and_then(|mid| store.mod_by_id(&mid));
            let detail = format!("Report: {}\n\n{}", path.display(), take_chars(&text, 1500));
            let pid = format!("acr-{}", path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
            match m {
                Some(m) if !blamed.contains(&m.id) => {
                    blamed.insert(m.id.clone());
                    probs.push(problem(&format!("acr-{}", m.id), &format!("Advanced Crash Reporter names {}", m.name), &detail, mod_fixes(m, has_update), "high", false));
                }
                None if !ignored(store).contains(&pid) => {
                    probs.push(problem(&pid, "Advanced Crash Reporter wrote a report", &detail, vec![fix("Hide this report", json!(["ignore", pid]), false)], "info", false));
                }
                _ => {}
            }
        }
    }
    if let Some(g) = &game {
        let clean = arr(store.doctor.get("clean_errors"));
        // two clean launches before ruling out
        let noise: HashSet<String> = if clean.len() >= 2 { clean.iter().flat_map(|run| arr(Some(run))).map(py_str).collect() } else { HashSet::new() };
        for (mid, hit) in log_errors(g, back, &idx) {
            let Some(m) = store.mod_by_id(&mid) else { continue };
            if blamed.contains(&mid) {
                continue;
            }
            let lines = hit.lines.join("\n");
            if noise.contains(&mid) {
                probs.push(problem(
                    &format!("log-{mid}"),
                    &format!("{} reported errors, also in launches without a crash", m.name),
                    &format!("Probably not the cause: this mod also logs errors in launches that ended normally.\n\n{lines}"),
                    mod_fixes(m, has_update),
                    "info",
                    false,
                ));
                continue;
            }
            probs.push(problem(
                &format!("log-{mid}"),
                &format!("{} reported errors", m.name),
                &lines,
                mod_fixes(m, has_update),
                if hit.strong { "high" } else { "medium" },
                hit.strong,
            ));
        }
        for (mid, secs) in slow_mods(g, back, &idx, SLOW) {
            if let Some(m) = store.mod_by_id(&mid) {
                probs.push(problem(
                    &format!("slow-{mid}"),
                    &format!("{} took {secs:.0} seconds to load", m.name),
                    "Seen in RED4ext's log. A slow DLL mod makes every game start slow, and can make the game freeze at startup.",
                    mod_fixes(m, has_update),
                    "medium",
                    false,
                ));
            }
        }
    }
    if crashed {
        probs.extend(recent_hand_installed(store, 7));
    }
    if crashed && !probs.iter().any(|p| p["severity"] == "high") {
        let mut p = changed_since_good(store);
        if game.as_ref().is_some_and(|g| !acr_installed(g))
            && let Some(Value::Array(f)) = p.get_mut("fixes")
        {
            f.push(fix(
                "Install Advanced Crash Reporter (its next crash report names the mod whose code ran)",
                json!(["queue_nexus", ACR_NEXUS]),
                false,
            ));
        }
        probs.push(p);
    }
    if let Some(c) = crash.filter(|_| crashed) {
        let notes = crash_notes(store, c);
        if !notes.is_empty() && !probs.is_empty() {
            let kinds = ["crash", "stack", "freeze", "acr", "log", "unknown"];
            let i = probs
                .iter()
                .position(|p| p["id"].as_str().and_then(|id| id.split('-').next()).is_some_and(|k| kinds.contains(&k)))
                .unwrap_or(0);
            append_detail(&mut probs[i], &format!("\n\n{notes}"));
        }
    }
    probs.extend(archive_conflicts(store));
    probs
}

/// The crash ID and how often it happened, and where the crash logs were saved.
pub fn crash_notes(store: &Store, crash: &Value) -> String {
    let mut notes = vec![];
    if let Some(f) = crash.get("fault").filter(|f| truthy(f)) {
        let same: Vec<f64> = arr(store.doctor.get("crashes"))
            .iter()
            .filter(|c| c.get("fault") == Some(f))
            .map(|c| c.get("time").and_then(Value::as_f64).unwrap_or(0.0))
            .collect();
        let tail = if same.len() > 1 {
            let first = same.iter().copied().fold(f64::INFINITY, f64::min);
            format!("The same crash happened {} times since {}.", same.len(), strftime("%d %b", first))
        } else {
            "This crash is new.".into()
        };
        notes.push(format!("Crash ID: {}. {tail}", py_str(f)));
    }
    if let Some(s) = crash.get("saved").filter(|s| truthy(s)) {
        notes.push(format!("Crash logs saved in {}", py_str(s)));
    }
    notes.join("\n\n")
}

/// Ids of mods that logged errors in a launch that ended without a crash.
pub fn noisy_mods(store: &Store, since: f64) -> Vec<String> {
    let Some(game) = store.game() else { return vec![] };
    let back = if since != 0.0 { since - 15.0 } else { 0.0 };
    let mut out: Vec<String> = log_errors(&game, back, &owner_index(store)).into_iter().map(|f| f.0).collect();
    out.sort();
    out
}

/// Zip this crash's REDEngine report and the mod logs written since `since` into the data folder, because the
/// game deletes old logs. Keeps the newest `keep` zips (`KEEP_LOGS`); of a file over `limit` bytes (`LOG_LIMIT`)
/// only the end of a log is kept. Returns the zip path, or None.
pub fn keep_logs(store: &Store, since: f64, report: Option<&str>, keep: usize, limit: u64) -> Option<PathBuf> {
    let game = store.game();
    let out = store.root.join("crash_logs");
    let run = || -> io::Result<Option<PathBuf>> {
        let mut files: Vec<(PathBuf, String)> = vec![];
        if let Some(r) = report.filter(|r| !r.is_empty()) {
            let r = Path::new(r);
            let parent = r.parent().unwrap_or(Path::new(""));
            let cands = if r.is_file() { vec![r.to_path_buf()] } else { rglob(r) };
            for p in cands {
                if p.is_file()
                    && !p.is_symlink()
                    && let Ok(rel) = p.strip_prefix(parent)
                {
                    files.push((p.clone(), format!("report/{}", posix(rel))));
                }
            }
        }
        if let Some(g) = &game {
            for base in LOGS {
                let d = gpath(g, base);
                if !d.is_dir() {
                    continue;
                }
                for p in rglob(&d) {
                    if has_name(&p, |n| n.ends_with(".log"))
                        && !p.is_symlink()
                        && mtime(&p)? >= since - 15.0
                        && let Ok(rel) = p.strip_prefix(g)
                    {
                        files.push((p.clone(), posix(rel)));
                    }
                }
            }
        }
        if files.is_empty() {
            return Ok(None);
        }
        if let Err(e) = fs::create_dir(&out)
            && e.kind() != io::ErrorKind::AlreadyExists
        {
            return Err(e);
        }
        let t = now();
        let z = out.join(strftime("crash-%Y%m%d-%H%M%S.zip", t));
        let st = local(t);
        let when = zip::DateTime::from_date_and_time(st.wYear, st.wMonth as u8, st.wDay as u8, st.wHour as u8, st.wMinute as u8, st.wSecond as u8)
            .unwrap_or_default();
        let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated).last_modified_time(when);
        let mut zf = zip::ZipWriter::new(fs::File::create(&z)?);
        for (p, name) in &files {
            // the game or the reporter can still hold or delete a file
            let Ok(size) = fs::metadata(p).map(|m| m.len()) else { continue };
            if size > limit && p.extension().is_none_or(|e| e != "log") {
                continue;
            }
            let Ok(data) = read_tail(p, size, limit) else { continue };
            if zf.start_file(name.as_str(), opts).is_err() || zf.write_all(&data).is_err() {
                continue;
            }
        }
        zf.finish().map_err(io::Error::other)?;
        let mut old: Vec<PathBuf> =
            fs::read_dir(&out)?.flatten().map(|e| e.path()).filter(|p| has_name(p, |n| n.starts_with("crash-") && n.ends_with(".zip"))).collect();
        old.sort_by_key(|p| p.to_string_lossy().to_lowercase());
        if keep > 0 && old.len() > keep {
            for p in &old[..old.len() - keep] {
                if let Err(e) = fs::remove_file(p)
                    && e.kind() != io::ErrorKind::NotFound
                {
                    return Err(e);
                }
            }
        }
        Ok(Some(z))
    };
    run().ok().flatten()
}

// ---- bisect ----

/// (required mod id, mod id that needs it) from the Nexus requirements of `mods`.
pub fn requires_pairs(mods: &[&Mod]) -> Vec<(String, String)> {
    let mut nexus: HashMap<String, Vec<&str>> = HashMap::new();
    for m in mods.iter().filter(|m| m.source_type() == "nexus") {
        nexus.entry(key(m.source.get("mod_id"))).or_default().push(&m.id);
    }
    let mut out = vec![];
    for m in mods {
        for r in &m.requires {
            for rid in nexus.get(&key(r.get("mod_id"))).into_iter().flatten() {
                out.push((rid.to_string(), m.id.clone()));
            }
        }
    }
    out
}

fn visit(mid: &str, needs: &Map<String, Value>, seen: &mut HashSet<String>, order: &mut Vec<String>) {
    if seen.insert(mid.to_string()) {
        for r in strs(needs.get(mid)) {
            visit(r, needs, seen, order);
        }
        order.push(mid.to_string());
    }
}

/// Suspects come after the mods they require, so every first half (`testing`) holds what its mods need.
pub fn bisect_start(store: &Store) -> Value {
    let on = store.enabled();
    let mut needs = Map::new();
    for (req, mid) in requires_pairs(&on) {
        if let Value::Array(a) = needs.entry(mid).or_insert_with(|| json!([])) {
            a.push(json!(req));
        }
    }
    // frameworks and Advanced Crash Reporter stay on in every round
    let fw: HashSet<&str> = on.iter().filter(|m| is_framework(m) || is_acr(&m.source)).map(|m| m.id.as_str()).collect();
    let (mut order, mut seen) = (vec![], HashSet::new());
    for m in &on {
        visit(&m.id, &needs, &mut seen, &mut order);
    }
    let suspects: Vec<String> = order.into_iter().filter(|x| !fw.contains(x.as_str())).collect();
    let secs = arr(store.doctor.get("crashes")).last().and_then(|c| c.get("secs")).cloned().unwrap_or(json!(0));
    json!({"suspects": suspects, "testing": suspects[..suspects.len().div_ceil(2)], "restore": ids(&on), "needs": needs,
           "crash_secs": secs})
}

/// Mods to turn off for this launch: the suspects not tested, and every mod that needs one of them,
/// so the game can still start.
pub fn bisect_off(state: &Value) -> BTreeSet<String> {
    let testing: HashSet<&str> = strs(state.get("testing")).into_iter().collect();
    let mut off: BTreeSet<String> = strs(state.get("suspects")).into_iter().filter(|s| !testing.contains(s)).map(String::from).collect();
    let empty = Map::new();
    let needs = state.get("needs").and_then(Value::as_object).unwrap_or(&empty);
    loop {
        let more: Vec<String> = needs
            .iter()
            .filter(|(m, rs)| !off.contains(*m) && strs(Some(rs)).iter().any(|r| off.contains(*r)))
            .map(|(m, _)| m.clone())
            .collect();
        if more.is_empty() {
            return off;
        }
        off.extend(more);
    }
}

/// Seconds as JSON: a whole number stays an integer.
fn secs_value(s: f64) -> Value {
    if s.fract() == 0.0 && s.abs() < 9e15 { json!(s as i64) } else { json!(s) }
}

/// Next state after one launch. Only `testing` suspects are on; a crash means the bad mod is among them.
/// A mod found only by elimination (no crash seen during the search) is confirmed by one more launch with
/// every mod on; if that launch does not crash either, there is no single bad mod (culprit None).
/// `crash_secs` is the longest time to a crash seen: a launch without a crash must run longer to count.
pub fn bisect_step(state: &Value, crashed: bool, secs: f64) -> Value {
    let mut st = state.as_object().cloned().unwrap_or_default();
    let old = st.get("crash_secs").cloned().unwrap_or(json!(0));
    let cand = if crashed { secs_value(secs) } else { json!(0) };
    let keep_old = old.as_f64().unwrap_or(0.0) >= cand.as_f64().unwrap_or(0.0);
    st.insert("crash_secs".into(), if keep_old { old } else { cand });
    let crashes = st.get("crashes").and_then(Value::as_i64).unwrap_or(0) + crashed as i64;
    let suspects: Vec<Value> = arr(st.get("suspects")).to_vec();
    let testing: Vec<Value> = arr(st.get("testing")).to_vec();
    let done = |st: &mut Map<String, Value>, pairs: Vec<(&str, Value)>| {
        for (k, v) in pairs {
            st.insert(k.into(), v);
        }
        Value::Object(std::mem::take(st))
    };
    if st.get("verifying").is_some_and(truthy) {
        let culprit = if crashed { suspects.first().cloned().unwrap_or(Value::Null) } else { Value::Null };
        return done(&mut st, vec![("crashes", json!(crashes)), ("culprit", culprit)]);
    }
    let s: Vec<Value> = if crashed { testing } else { suspects.into_iter().filter(|x| !testing.contains(x)).collect() };
    if s.len() <= 1 {
        if !s.is_empty() && crashes == 0 {
            return done(&mut st, vec![("suspects", json!(s)), ("testing", json!(s)), ("crashes", json!(0)), ("verifying", json!(true))]);
        }
        let culprit = s.first().cloned().unwrap_or(Value::Null);
        return done(&mut st, vec![("suspects", json!(s)), ("testing", json!(s)), ("crashes", json!(crashes)), ("culprit", culprit)]);
    }
    let half = s[..s.len().div_ceil(2)].to_vec();
    done(&mut st, vec![("suspects", json!(s)), ("testing", json!(half)), ("crashes", json!(crashes))])
}
