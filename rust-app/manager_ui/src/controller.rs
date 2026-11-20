//! Everything the window does, without the GUI: links -> jobs -> download -> virus scan -> install -> deploy,
//! plus game launch, crash handling, fixes and undo. Worker threads report through `notify(kind, payload)`.
//!
//! Locks: the store lock is not re-entrant, so no method locks it twice on one thread. The store lock and the
//! job lock are never held at the same time. The job lock comes before `clicking` and `finish_pack`.
use crash_doctor::{doctor, watch};
use downloads::nexus_browser::Clicker;
use downloads::{fetch, nexus};
use installer::{deploy, install};
use local_ai::Advice;
use regex::Regex;
use serde_json::{Map, Value, json};
use shared::{FRAMEWORKS, GAME_EXE, Mod, Store, VERSION, fw_by_nexus, fw_by_repo};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::io;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::SeqCst};
use std::sync::{Arc, Condvar, LazyLock, Mutex, MutexGuard, Weak};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Swaps in the new exe once the old one has quit (a running exe is locked), then starts it. If the swap never
/// works, the old exe starts again.
pub const SWAP: &str = r#"for ($i = 0; $i -lt 240; $i++) {
  try { Move-Item -LiteralPath $env:CP_NEW -Destination $env:CP_EXE -Force -ErrorAction Stop; break } catch { Start-Sleep -Milliseconds 500 } }
Start-Process -FilePath $env:CP_EXE"#;

/// A compatibility patch's name; NOT_COMPAT: names that only speak of a game patch or say "compatible with" an item.
static COMPAT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)patch|compat|integration|bridge").unwrap());
static NOT_COMPAT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)deleted|obsolete|deprecated|incompatible|not compatible|patch \d|\d compatible").unwrap());
pub const CORE: [&str; 6] = ["RED4ext", "Cyber Engine Tweaks", "redscript", "ArchiveXL", "TweakXL", "Codeware"];
pub const GAME_ON: &str = local_ai::BLOCKED;

const NO_WINDOW: u32 = 0x0800_0000;
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
const ACTIVE: [&str; 4] = ["queued", "running", "waiting", "needkey"];

/// The window's event sink: (kind, payload). It runs on worker threads, so it must only queue the event.
pub type Notify = Arc<dyn Fn(&str, Value) + Send + Sync>;

/// What the controller calls outside itself. `Deps::default()` is the real thing; tests replace single fields.
#[allow(clippy::type_complexity)] // each field is one plain function signature
#[derive(Clone)]
pub struct Deps {
    /// Microsoft Defender: (Some(true) clean, Some(false) virus, None not scanned, detail).
    pub scan: Arc<dyn Fn(&Path) -> (Option<bool>, String) + Send + Sync>,
    /// (archive, stage folder, name) -> game-relative files.
    pub install: Arc<dyn Fn(&Path, &Path, &str) -> Result<Vec<String>, String> + Send + Sync>,
    pub deploy: Arc<dyn Fn(&mut Store) -> Result<usize, String> + Send + Sync>,
    /// Opens a web page in the user's browser.
    pub open_url: Arc<dyn Fn(&str) + Send + Sync>,
    /// Starts a process (the game, the update script).
    pub spawn: Arc<dyn Fn(&mut Command) -> io::Result<()> + Send + Sync>,
    /// (exe, args, cwd): start through the Windows shell, which asks the user first when the exe needs admin rights.
    pub run_as: Arc<dyn Fn(&Path, &[String], &Path) -> Result<(), String> + Send + Sync>,
    /// `local_ai::block`.
    pub block_ai: Arc<dyn Fn(bool) + Send + Sync>,
    pub start_ai: Arc<dyn Fn() -> Result<(), String> + Send + Sync>,
    pub advise: Arc<dyn Fn(&[Mod], &[Value], &str) -> Result<Advice, String> + Send + Sync>,
    /// Runs the closure once after the delay, on its own thread.
    pub timer: Arc<dyn Fn(Duration, Box<dyn FnOnce() + Send>) + Send + Sync>,
    pub crash_evidence: Arc<dyn Fn(f64, Option<u32>) -> Value + Send + Sync>,
    pub read_dump: Arc<dyn Fn(&Path) -> Option<Value> + Send + Sync>,
    /// Replaces `Controller::has_update` when set.
    pub has_update: Option<Arc<dyn Fn(&Mod) -> bool + Send + Sync>>,
    /// Replaces the hidden browser's click when set.
    pub click: Option<Arc<dyn Fn(&Value) + Send + Sync>>,
}

impl Default for Deps {
    fn default() -> Self {
        Deps {
            scan: Arc::new(|p: &Path| virus_scan::scan(p)),
            install: Arc::new(|a: &Path, s: &Path, n: &str| install::install(a, s, n)),
            deploy: Arc::new(|s: &mut Store| deploy::deploy(s)),
            open_url: Arc::new(|url: &str| {
                let _ = shell_open(Path::new(url), &[], None);
            }),
            spawn: Arc::new(|c: &mut Command| c.spawn().map(|_| ())),
            run_as: Arc::new(|exe: &Path, args: &[String], cwd: &Path| shell_open(exe, args, Some(cwd))),
            block_ai: Arc::new(local_ai::block),
            start_ai: Arc::new(|| local_ai::start(Duration::from_secs(180))),
            advise: Arc::new(|m: &[Mod], p: &[Value], r: &str| local_ai::advise(m, p, r)),
            timer: Arc::new(|d: Duration, f: Box<dyn FnOnce() + Send>| {
                thread::spawn(move || {
                    thread::sleep(d);
                    f()
                });
            }),
            crash_evidence: Arc::new(watch::crash_evidence),
            read_dump: Arc::new(watch::read_dump),
            has_update: None,
            click: None,
        }
    }
}

/// ShellExecuteW "open": a web page in the browser, or an exe the way Explorer starts it.
pub(crate) fn shell_open(file: &Path, args: &[String], cwd: Option<&Path>) -> Result<(), String> {
    use shared::win::wide;
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let (op, f, p) = (wide("open"), wide(&file.to_string_lossy()), wide(&args.join(" ")));
    let d = cwd.map(|c| wide(&c.to_string_lossy()));
    // SAFETY: every pointer is a NUL-terminated UTF-16 buffer that lives until the call returns.
    let r = unsafe {
        ShellExecuteW(std::ptr::null_mut(), op.as_ptr(), f.as_ptr(), p.as_ptr(), d.as_ref().map_or(std::ptr::null(), |d| d.as_ptr()), SW_SHOWNORMAL)
    };
    if r as isize > 32 { Ok(()) } else { Err(format!("Windows could not open {} (error {})", file.display(), r as isize)) }
}

pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64())
}

/// Python truthiness.
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Python str() of a JSON value.
fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "None".into(),
        v => v.to_string(),
    }
}

/// A Nexus id that may come as a number or a string.
fn int(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
}

fn strs(v: &Value) -> Vec<String> {
    v.as_array().into_iter().flatten().filter_map(|x| x.as_str().map(String::from)).collect()
}

fn job_id(j: &Value) -> u64 {
    j["id"].as_u64().unwrap_or(0)
}

fn ids(mods: &[Mod]) -> Vec<String> {
    mods.iter().map(|m| m.id.clone()).collect()
}

/// A copy of the store for slow readers (diagnose, report, logs), so the lock is not held during network calls.
fn snapshot(s: &Store) -> Store {
    Store {
        root: s.root.clone(),
        downloads: s.downloads.clone(),
        staging: s.staging.clone(),
        backups: s.backups.clone(),
        file: s.file.clone(),
        settings: s.settings.clone(),
        mods: s.mods.clone(),
        deployed: s.deployed.clone(),
        deploy_game: s.deploy_game.clone(),
        modlist_tracked: s.modlist_tracked,
        doctor: s.doctor.clone(),
        packs: s.packs.clone(),
        pack: s.pack.clone(),
        pack_info: s.pack_info.clone(),
    }
}

/// Sort the mod list by `order`; a mod not in it (added since) goes last, in its old order.
fn order_mods(st: &mut Store, order: &[String]) {
    let pos: HashMap<&str, usize> = order.iter().enumerate().map(|(n, m)| (m.as_str(), n)).collect();
    let end = pos.len();
    st.mods.sort_by_key(|m| pos.get(m.id.as_str()).copied().unwrap_or(end));
}

fn remove_first(list: Option<&mut Value>, x: &Value) {
    if let Some(Value::Array(a)) = list
        && let Some(i) = a.iter().position(|v| v == x)
    {
        a.remove(i);
    }
}

fn push_to(doc: &mut Map<String, Value>, key: &str, v: Value) {
    let e = doc.entry(key).or_insert_with(|| json!([]));
    if !e.is_array() {
        *e = json!([]);
    }
    if let Value::Array(a) = e {
        a.push(v);
    }
}

/// Keeps only the last `n` items of `doc[key]`.
fn keep_last(doc: &mut Map<String, Value>, key: &str, n: usize) {
    if let Some(Value::Array(a)) = doc.get_mut(key)
        && a.len() > n
    {
        a.drain(..a.len() - n);
    }
}

/// Turn on the pack's mods in the pack's order and turn off the rest. A framework the pack does not list stays on: the
/// pack's mods need it. Advanced Crash Reporter is always on, also when the pack lists it as off (crash reports need
/// it). Returns (mods on, mods of the pack not installed yet).
fn use_pack_in(st: &mut Store, name: &str) -> Result<(usize, usize), String> {
    let pack = st.packs.get(name).cloned().ok_or_else(|| format!("No mod pack '{name}'"))?;
    let pos: HashMap<&str, i64> = pack.iter().enumerate().map(|(n, (m, _))| (m.as_str(), n as i64)).collect();
    let on: HashSet<&str> = pack.iter().filter(|(_, e)| *e).map(|(m, _)| m.as_str()).collect();
    st.mods.sort_by_key(|m| pos.get(m.id.as_str()).copied().unwrap_or(-1)); // mods outside the pack go first (lowest)
    for m in st.mods.iter_mut() {
        m.enabled = if doctor::is_acr(&m.source) {
            true
        } else if pos.contains_key(m.id.as_str()) {
            on.contains(m.id.as_str())
        } else {
            m.framework.as_deref().is_some_and(|f| !f.is_empty()) && m.enabled
        };
    }
    st.pack = name.to_string();
    st.save().map_err(|e| e.to_string())?;
    let have: HashSet<&str> = st.mods.iter().map(|m| m.id.as_str()).collect();
    let missing = pos.keys().filter(|m| !have.contains(*m)).count();
    Ok((st.enabled().len(), missing))
}

fn pack_message(name: &str, on: usize, missing: usize) -> String {
    format!("Mod pack '{name}': {on} mod(s) on") + &if missing > 0 { format!(", {missing} not installed yet.") } else { ".".into() }
}

struct Work {
    queue: VecDeque<u64>,
    /// Jobs put and not yet finished (Python's `Queue.unfinished_tasks`).
    unfinished: usize,
}

pub struct Controller {
    pub store: Arc<Mutex<Store>>,
    /// Every job: {"id", "name", "src", "state", "status", "progress", ...}. States: queued, running, waiting
    /// (free-account click), needkey, done, failed.
    pub jobs: Mutex<Vec<Value>>,
    /// Set when the window closes: no new browser, deploy or game watch starts.
    pub stop: AtomicBool,
    premium: AtomicBool,
    /// An Advanced Crash Reporter add runs; a second one would queue it twice (its Nexus lookup is slow).
    acr_busy: AtomicBool,
    /// The local AI does not run while the game runs: the game gets the whole PC.
    game_on: AtomicBool,
    ids: AtomicU64,
    /// The waiting job the click queue works on.
    clicking: Mutex<Option<u64>>,
    /// The pack that Complete installation uses once its downloads end.
    finish_pack: Mutex<Option<String>>,
    work: Mutex<Work>,
    wake: Condvar,
    clicker: Clicker,
    deps: Deps,
    notify: Notify,
    root: PathBuf,
    downloads: PathBuf,
    staging: PathBuf,
    me: Weak<Controller>,
}

impl Controller {
    pub fn new(store: Store, notify: Notify, workers: usize, watch_game: bool, deps: Deps) -> Arc<Controller> {
        let (root, downloads, staging) = (store.root.clone(), store.downloads.clone(), store.staging.clone());
        let has_key = !store.settings.api_key.trim().is_empty();
        let c = Arc::new_cyclic(|me: &Weak<Controller>| {
            // Free accounts only: a hidden browser clicks 'Slow download' so no file needs the user (owner decision
            // 2026-11-20; Nexus's terms disallow the automation, so every failure falls back to the manual click).
            let mut clicker = Clicker::new(&root.join("browser"));
            let w = me.clone();
            clicker.log = Box::new(move |m| {
                if let Some(c) = w.upgrade() {
                    c.log(m)
                }
            });
            let w = me.clone();
            clicker.on_link = Box::new(move |url| w.upgrade().map_or(Ok(()), |c| c.handle_nxm(url)));
            let w = me.clone();
            clicker.on_fail = Box::new(move |j, why| {
                if let Some(c) = w.upgrade() {
                    c.manual_click(j, why)
                }
            });
            let w = me.clone();
            clicker.waiting = Box::new(move |j| w.upgrade().is_some_and(|c| c.state_of(job_id(j)).as_deref() == Some("waiting")));
            Controller {
                store: Arc::new(Mutex::new(store)),
                jobs: Mutex::new(vec![]),
                stop: AtomicBool::new(false),
                premium: AtomicBool::new(false),
                acr_busy: AtomicBool::new(false),
                game_on: AtomicBool::new(false),
                ids: AtomicU64::new(1),
                clicking: Mutex::new(None),
                finish_pack: Mutex::new(None),
                work: Mutex::new(Work { queue: VecDeque::new(), unfinished: 0 }),
                wake: Condvar::new(),
                clicker,
                deps,
                notify,
                root: root.clone(),
                downloads: downloads.clone(),
                staging,
                me: me.clone(),
            }
        });
        // left by jobs that the app's close cut off (only one window runs)
        for e in std::fs::read_dir(&downloads).into_iter().flatten().flatten() {
            if e.file_name().to_string_lossy().starts_with("job-") {
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
        for _ in 0..workers {
            let w = c.clone();
            thread::spawn(move || w.worker());
        }
        if watch_game {
            let w = c.clone();
            thread::spawn(move || {
                let hang = w.root.join("freezes");
                let (a, b) = (w.clone(), w.clone());
                watch::watch(move |t| a.game_started(t), move |s, code, f| b.game_exited(s, code, f.as_deref()), &w.stop, 5.0, Some(&hang));
            });
        }
        if has_key {
            c.bg(|c| {
                c.check_key();
                Ok(())
            });
        }
        c
    }

    /// The Nexus API key. Secret: never print or log it.
    fn key(&self) -> String {
        lock(&self.store).settings.api_key.trim().to_string()
    }

    pub fn is_premium(&self) -> bool {
        self.premium.load(SeqCst)
    }

    pub fn game_on(&self) -> bool {
        self.game_on.load(SeqCst)
    }

    fn notify(&self, kind: &str, payload: Value) {
        (self.notify)(kind, payload)
    }

    fn log(&self, m: &str) {
        self.notify("log", json!(m));
    }

    fn game(&self) -> Option<PathBuf> {
        lock(&self.store).game()
    }

    fn has_update_fn(&self) -> impl Fn(&Mod) -> bool + '_ {
        move |m: &Mod| self.has_update(m)
    }

    /// Runs `f` on its own thread; an error is shown in the log.
    pub fn bg(&self, f: impl FnOnce(&Arc<Controller>) -> Result<(), String> + Send + 'static) {
        let Some(c) = self.me.upgrade() else { return };
        thread::spawn(move || {
            if let Err(e) = f(&c) {
                c.log(&format!("Error: {e}"));
            }
        });
    }

    /// Ends the worker threads, the game watch, the hidden browser and the local AI engine (the window closes).
    pub fn close(&self) {
        self.stop.store(true, SeqCst);
        let _w = lock(&self.work);
        self.wake.notify_all();
        drop(_w);
        self.clicker.close();
        (self.deps.block_ai)(true);
    }

    pub fn check_key(&self) {
        let key = self.key();
        match nexus::validate(&key) {
            Ok(u) => {
                let premium = truthy(u.get("is_premium").unwrap_or(&Value::Null));
                self.premium.store(premium, SeqCst);
                let kind = if premium {
                    "Premium: downloads need no clicks"
                } else if lock(&self.store).settings.auto_click {
                    "Free: I click 'Slow download' for you after one Nexus login"
                } else {
                    "Free: one click per Nexus file, the rest is automatic"
                };
                self.notify("key", json!(format!("Key OK for {}. {kind}.", text(u.get("name").unwrap_or(&Value::Null)))));
            }
            Err(e) => {
                self.premium.store(false, SeqCst);
                return self.notify("key", json!(format!("Key check failed: {e}")));
            }
        }
        // Nexus jobs that waited for the key (or for a click a Premium account does not need)
        let premium = self.is_premium();
        let js: Vec<Value> = {
            let mut jobs = lock(&self.jobs);
            jobs.iter_mut()
                .filter(|j| j["state"] == "needkey" || (premium && j["state"] == "waiting"))
                .map(|j| {
                    j["state"] = json!("queued");
                    j["status"] = json!("Queued");
                    j["progress"] = json!("");
                    j.clone()
                })
                .collect()
        };
        for j in js {
            self.notify("job", j.clone());
            self.put(job_id(&j));
        }
        self.ensure_acr();
    }

    /// Advanced Crash Reporter is always installed and on (owner decision 2026-11-21): its report names the mod whose
    /// code ran at a crash. A job queued for it is not queued again until restart, also when it is skipped or fails.
    pub fn ensure_acr(&self) {
        let Some(game) = self.game() else { return };
        {
            let mut st = lock(&self.store);
            if let Some(i) = st.mods.iter().position(|m| doctor::is_acr(&m.source)) {
                if !st.mods[i].enabled {
                    st.mods[i].enabled = true; // the next deploy puts it back in the game
                    if let Err(e) = st.save() {
                        drop(st);
                        return self.log(&format!("Could not turn on Advanced Crash Reporter: {e}"));
                    }
                    drop(st);
                    self.log("Turned Advanced Crash Reporter on again, so a crash report names the mod that caused it.");
                    self.notify("mods", Value::Null);
                }
                return;
            }
        }
        if doctor::acr_installed(&game)
            || lock(&self.jobs).iter().any(|j| doctor::is_acr(&j["src"]))
            || self.acr_busy.swap(true, SeqCst)
        {
            return;
        }
        self.log("Adding Advanced Crash Reporter, so a crash report names the mod that caused it.");
        let link = json!({"type": "nexus", "mod_id": doctor::ACR_NEXUS});
        self.bg(move |c| {
            let r = c.add(&link);
            c.acr_busy.store(false, SeqCst);
            r
        });
    }

    // ---------- links and jobs ----------
    pub fn add_links(&self, text: &str) {
        let lines: Vec<String> = text.lines().filter(|l| !l.trim().is_empty()).map(String::from).collect();
        self.bg(move |c| {
            c.expand(&lines);
            Ok(())
        });
    }

    /// One link that is parsed already, such as the nxm link the user said yes to.
    pub fn add_link(&self, link: Value) {
        self.bg(move |c| c.add(&link));
    }

    fn expand(&self, lines: &[String]) {
        for line in lines {
            // one bad link must not stop the others
            let r = nexus::parse_link(line).and_then(|l| l.map_or(Ok(()), |l| self.add(&l)));
            if let Err(e) = r {
                self.log(&format!("{}: {e}", line.trim()));
            }
        }
    }

    fn have(&self, pred: impl Fn(&Value) -> bool) -> bool {
        if lock(&self.store).mods.iter().any(|m| pred(&m.source)) {
            return true;
        }
        lock(&self.jobs).iter().any(|j| j["state"] != "failed" && pred(&j["src"]))
    }

    fn job(&self, name: &str, src: Value, meta: Value) -> Value {
        let id = self.ids.fetch_add(1, SeqCst);
        let mut j = json!({"id": id, "name": name, "src": src, "state": "queued", "status": "Queued", "progress": ""});
        if let (Value::Object(o), Value::Object(m)) = (&mut j, meta) {
            o.extend(m);
        }
        lock(&self.jobs).push(j.clone());
        self.notify("job", j.clone());
        self.put(id);
        j
    }

    /// Changes one job under the job lock; its new value, or None when the job is gone.
    fn edit(&self, id: u64, f: impl FnOnce(&mut Value)) -> Option<Value> {
        let mut jobs = lock(&self.jobs);
        let j = jobs.iter_mut().find(|j| job_id(j) == id)?;
        f(j);
        Some(j.clone())
    }

    fn set(&self, id: u64, state: Option<&str>, status: &str, progress: &str) {
        let j = self.edit(id, |j| {
            if let Some(s) = state {
                j["state"] = json!(s);
            }
            j["status"] = json!(status);
            j["progress"] = json!(progress);
        });
        if let Some(j) = j {
            self.notify("job", j);
        }
    }

    fn state_of(&self, id: u64) -> Option<String> {
        lock(&self.jobs).iter().find(|j| job_id(j) == id).and_then(|j| j["state"].as_str().map(String::from))
    }

    fn add(&self, link: &Value) -> Result<(), String> {
        let t = link["type"].as_str().unwrap_or("");
        if t == "collection" {
            let slug = link["slug"].as_str().unwrap_or("");
            let c = nexus::collection(slug, link.get("revision").and_then(Value::as_i64), &self.key())?;
            let all = c["files"].as_array().cloned().unwrap_or_default();
            let fw = |f: &Value| int(&f["mod_id"]).and_then(|i| fw_by_nexus(i as u64)).is_some();
            let ask = |f: &Value| (truthy(&f["optional"]) || truthy(f.get("outdated").unwrap_or(&Value::Null))) && !fw(f);
            let name = text(&c["name"]).trim().to_string();
            if link.get("chosen").is_none() && all.iter().any(ask) {
                // the window asks which ones to download, then adds it again
                let asked: Vec<&Value> = all.iter().filter(|f| ask(f)).collect();
                self.notify("pack_choice", json!({"link": link, "name": name, "files": asked}));
                return Ok(());
            }
            let chosen: Vec<i64> = link["chosen"].as_array().into_iter().flatten().filter_map(int).collect();
            let files: Vec<&Value> = all.iter().filter(|f| !ask(f) || int(&f["file_id"]).is_some_and(|i| chosen.contains(&i))).collect();
            self.log(&format!("Collection '{}' revision {}: {} files", text(&c["name"]), text(&c["revision"]), files.len()));
            let mid = |f: &Value| format!("nexus-{}-{}", text(&f["mod_id"]), text(&f["file_id"]));
            {
                // the collection is also a mod pack (a mod's id is known before its download)
                let mut st = lock(&self.store);
                st.packs.insert(name.clone(), files.iter().filter(|f| !fw(f)).map(|f| (mid(f), true)).collect());
                let names: Map<String, Value> = files.iter().map(|f| (mid(f), f["name"].clone())).collect();
                st.pack_info.insert(
                    name.clone(),
                    json!({"url": format!("https://www.nexusmods.com/games/{}/collections/{slug}", nexus::GAME), "names": names}),
                );
                st.save().map_err(|e| e.to_string())?;
            }
            self.notify("mods", Value::Null);
            for f in files {
                let mut l = json!({"type": "nexus"});
                if let (Value::Object(o), Value::Object(m)) = (&mut l, f) {
                    o.extend(m.clone());
                }
                self.add(&l)?;
            }
        } else if t == "nexus" {
            let mod_id = int(&link["mod_id"]).ok_or("Nexus link without a mod id")?;
            let file_id = int(link.get("file_id").unwrap_or(&Value::Null));
            if let Some(fw) = fw_by_nexus(mod_id as u64) {
                return self.add_framework(fw);
            }
            if self.have(|s| s["type"] == "nexus" && int(&s["mod_id"]) == Some(mod_id) && (file_id.is_none() || int(&s["file_id"]) == file_id)) {
                return Ok(());
            }
            let get = |k: &str| link.get(k).cloned().unwrap_or(Value::Null);
            let mut src = json!({"type": "nexus", "mod_id": mod_id, "file_id": get("file_id"), "key": get("key"), "expires": get("expires")});
            if file_id.is_some_and(|i| i != 0) && truthy(&get("name")) {
                // collection entry: already known, no lookup needed
                let version = link.get("version").cloned().unwrap_or(json!(""));
                self.job(&text(&link["name"]), src, json!({"version": version}));
                return Ok(());
            }
            let info = nexus::mod_info(mod_id, &self.key())?;
            if truthy(&info["outdated"]) && !truthy(&get("anyway")) {
                // the window asks: Download (adds it again with "anyway") or Skip
                self.notify("outdated", json!({"link": link, "name": info["name"], "why": info["outdated"]}));
                return Ok(());
            }
            let files = info["files"].as_array().cloned().unwrap_or_default();
            let f = nexus::pick_file(&files, file_id).ok_or_else(|| format!("Nexus mod {mod_id} has no file to download"))?;
            src["file_id"] = json!(int(&f["fileId"]).ok_or("Nexus answered a bad file id")?);
            let name = text(&info["name"]);
            self.job(&name, src, json!({"version": f["version"], "requires": info["requires"]}));
            for r in info["requires"].as_array().into_iter().flatten() {
                // a broken requirement must not cancel the mod
                if let Err(e) = self.add(&json!({"type": "nexus", "mod_id": r["mod_id"]})) {
                    self.log(&format!("Requirement {}: {e}", text(&r["name"])));
                }
            }
            if let Err(e) = self.compat(mod_id, &name) {
                // network boundary: the mod itself is added already
                self.log(&format!("Compatibility check for {name}: {e}"));
            }
        } else if t == "github" {
            let repo = link["repo"].as_str().unwrap_or("");
            if let Some(fw) = fw_by_repo(repo) {
                return self.add_framework(fw);
            }
            if !self.have(|s| s.get("repo").and_then(Value::as_str) == Some(repo)) {
                self.job(repo.split('/').nth(1).unwrap_or(repo), json!({"type": "github", "repo": repo}), json!({}));
            }
        } else {
            let p = match link.get("path").and_then(Value::as_str).filter(|p| !p.is_empty()) {
                Some(p) => p.to_string(),
                None => link["url"].as_str().unwrap_or("").split('?').next().unwrap_or("").to_string(),
            };
            let stem = Path::new(&p).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            self.job(&stem, link.clone(), json!({}));
        }
        Ok(())
    }

    /// Add the compatibility patches for mod_id and the user's other mods. A patch is a Nexus mod that requires
    /// mod_id, has a patch-like name, needs or names another of the user's mods (not a framework), and needs no mod
    /// the user does not have. Collections bring their own patches, so their files skip this.
    fn compat(&self, mod_id: i64, name: &str) -> Result<(), String> {
        let mut have: Vec<(i64, String)> = vec![]; // a Python dict: first place kept, last name wins
        let mut put = |s: &Value, n: &str| {
            if s["type"] != "nexus" {
                return;
            }
            let Some(i) = int(&s["mod_id"]) else { return };
            match have.iter_mut().find(|(k, _)| *k == i) {
                Some(e) => e.1 = n.to_string(),
                None => have.push((i, n.to_string())),
            }
        };
        for m in lock(&self.store).mods.iter() {
            put(&m.source, &m.name);
        }
        for j in lock(&self.jobs).iter().filter(|j| j["state"] != "failed") {
            put(&j["src"], &text(&j["name"]));
        }
        let is_fw = |i: i64| fw_by_nexus(i as u64).is_some();
        let others: Vec<&(i64, String)> = have.iter().filter(|(i, _)| *i != mod_id && !is_fw(*i)).collect();
        if others.is_empty() {
            return Ok(());
        }
        let have_ids: HashSet<i64> = have.iter().map(|(i, _)| *i).collect();
        let found: Vec<Value> = nexus::mods_requiring(mod_id, 1200)?
            .into_iter()
            .filter(|d| {
                let n = text(&d["name"]);
                int(&d["mod_id"]).is_some_and(|i| !have_ids.contains(&i)) && COMPAT.is_match(&n) && !NOT_COMPAT.is_match(&n)
            })
            .take(24) // ponytail: 24 candidates cap, enough for all but the biggest hub mods
            .collect();
        let key = |n: &str| n.to_lowercase().chars().filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit()).collect::<String>();
        let api_key = self.key();
        let partner = |d: &Value| -> Option<String> {
            let info = nexus::mod_info(int(&d["mod_id"])?, &api_key).ok()?; // a patch page that fails to load is skipped
            let reqs: HashSet<i64> = info["requires"].as_array().into_iter().flatten().filter_map(|r| int(&r["mod_id"])).filter(|i| !is_fw(*i)).collect();
            if !reqs.is_subset(&have_ids) {
                return None; // it needs a mod that is not in this list: it is not for this list
            }
            let dn = key(&text(&d["name"]));
            others.iter().find(|(i, n)| reqs.contains(i) || (key(n).len() >= 6 && dn.contains(&key(n)))).map(|(_, n)| n.clone())
        };
        let mut partners = vec![];
        for batch in found.chunks(6) {
            let got: Vec<Option<String>> = thread::scope(|s| {
                let hs: Vec<_> = batch.iter().map(|d| s.spawn(|| partner(d))).collect();
                hs.into_iter().map(|h| h.join().unwrap_or(None)).collect()
            });
            partners.extend(got);
        }
        for (d, other) in found.iter().zip(partners) {
            if let Some(other) = other {
                self.log(&format!("Adding the compatibility patch '{}' for {name} and {other}.", text(&d["name"])));
                self.add(&json!({"type": "nexus", "mod_id": d["mod_id"]}))?;
            }
        }
        Ok(())
    }

    pub fn add_framework(&self, fw: &str) -> Result<(), String> {
        let have = {
            let mut st = lock(&self.store);
            let i = st.mods.iter().position(|m| m.framework.as_deref() == Some(fw));
            let turned_on = match i {
                Some(i) if !st.mods[i].enabled => {
                    st.mods[i].enabled = true;
                    st.save().map_err(|e| e.to_string())?;
                    true
                }
                _ => false,
            };
            (i.is_some(), turned_on)
        };
        if have.1 {
            self.notify("mods", Value::Null);
        }
        if have.0 || lock(&self.jobs).iter().any(|j| j["framework"] == fw && j["state"] != "failed") {
            return Ok(());
        }
        let repo = FRAMEWORKS.iter().find(|f| f.0 == fw).map(|f| f.1).ok_or_else(|| format!("Unknown framework {fw}"))?;
        self.job(fw, json!({"type": "github", "repo": repo}), json!({"framework": fw}));
        Ok(())
    }

    /// An nxm:// link from the browser. A link that a waiting download asked for continues that download.
    /// Any other link may come from any web page, so the user is asked before anything is installed. The question
    /// carries the parsed link, never the text: on "yes" only what the question names is queued.
    pub fn handle_nxm(&self, url: &str) -> Result<(), String> {
        let link = nexus::parse_link(url)?.ok_or_else(|| format!("Not a mod link I know: {url}"))?;
        if link["type"] != "nexus" {
            let slug = link["slug"].as_str().unwrap_or("");
            // offline or gone: ask with the slug instead
            let name = nexus::collection_info(slug).ok().and_then(|c| c["name"].as_str().map(String::from)).unwrap_or_else(|| slug.to_string());
            self.notify("confirm", json!({"link": link, "text": format!("Download and install the Nexus collection '{name}'?")}));
            return Ok(());
        }
        let j = {
            let mut jobs = lock(&self.jobs);
            let j = jobs.iter_mut().find(|j| {
                j["state"] == "waiting" && int(&j["src"]["mod_id"]) == int(&link["mod_id"]) && int(&j["src"]["file_id"]) == int(&link["file_id"])
            });
            j.map(|j| {
                j["src"]["key"] = link["key"].clone();
                j["src"]["expires"] = link["expires"].clone();
                j["state"] = json!("queued");
                j["status"] = json!("Queued");
                j["progress"] = json!("");
                j.clone()
            })
        };
        if let Some(j) = j {
            self.notify("job", j.clone());
            self.put(job_id(&j));
            self.next_click();
            return Ok(());
        }
        let mod_id = int(&link["mod_id"]).unwrap_or(0);
        // offline or unknown: ask with the number instead
        let name = nexus::mod_info(mod_id, &self.key()).ok().map(|i| text(&i["name"])).unwrap_or_else(|| format!("Nexus mod {mod_id}"));
        self.notify("confirm", json!({"link": link, "text": format!("Download and install '{name}' (file {}) from Nexus?", text(&link["file_id"]))}));
        Ok(())
    }

    /// Free accounts: the next waiting file, one at a time. With automatic clicks on, the hidden browser clicks
    /// 'Slow download'; otherwise the Nexus page is opened for a manual click.
    fn next_click(&self) {
        let j = {
            let jobs = lock(&self.jobs);
            let mut clicking = lock(&self.clicking);
            if let Some(id) = *clicking
                && jobs.iter().any(|j| job_id(j) == id && j["state"] == "waiting")
            {
                return;
            }
            let j = jobs.iter().find(|j| j["state"] == "waiting").cloned();
            *clicking = j.as_ref().map(job_id);
            j
        };
        let Some(j) = j else { return };
        if lock(&self.store).settings.auto_click {
            self.set(job_id(&j), None, "Clicking 'Slow download' for you", "");
            self.bg(move |c| {
                match &c.deps.click {
                    Some(f) => f(&j),
                    None => c.clicker.click(&j),
                }
                Ok(())
            });
        } else {
            self.set(job_id(&j), None, "Click 'Slow download' in your browser", "");
            self.reopen(&j);
        }
    }

    /// The hidden browser could not click (no login, Nexus changed its page, ...): back to the manual way.
    fn manual_click(&self, job: &Value, why: &str) {
        if self.stop.load(SeqCst) {
            return; // the window is closing: no browser may open now
        }
        let j = {
            let mut jobs = lock(&self.jobs);
            match jobs.iter_mut().find(|j| job_id(j) == job_id(job)) {
                Some(j) if j["state"] == "waiting" => {
                    j["manual"] = json!(true); // the window now asks the user to click
                    j["status"] = json!("Click 'Slow download' in your browser");
                    j["progress"] = json!("");
                    j.clone()
                }
                _ => return,
            }
        };
        self.notify("job", j.clone());
        self.log(&format!("Automatic click failed ({why}). I opened the Nexus page again: click 'Slow download' there."));
        self.reopen(&j);
    }

    pub fn reopen(&self, j: &Value) {
        (self.deps.open_url)(&nexus::files_page(int(&j["src"]["mod_id"]).unwrap_or(0), int(&j["src"]["file_id"]).unwrap_or(0)));
    }

    /// Only a download that has not started can be skipped.
    pub fn skip(&self, id: u64) -> bool {
        let mut ok = false;
        let j = self.edit(id, |j| {
            if ["queued", "waiting", "needkey"].contains(&j["state"].as_str().unwrap_or("")) {
                ok = true;
                j["state"] = json!("failed");
                j["status"] = json!("Skipped");
                j["progress"] = json!("");
            }
        });
        let Some(j) = j.filter(|_| ok) else { return false };
        self.notify("job", j);
        self.next_click();
        if self.finish_pack() && self.game().is_some() {
            self.redeploy(false, false);
        }
        true
    }

    pub fn retry(&self, id: u64) -> bool {
        let mut ok = false;
        let j = self.edit(id, |j| {
            if j["state"] == "failed" {
                ok = true;
                j["state"] = json!("queued");
                j["status"] = json!("Queued");
                j["progress"] = json!("");
            }
        });
        match j {
            Some(j) if ok => {
                self.notify("job", j);
                self.put(id);
                true
            }
            _ => false,
        }
    }

    fn put(&self, id: u64) {
        let mut w = lock(&self.work);
        w.queue.push_back(id);
        w.unfinished += 1;
        self.wake.notify_one();
    }

    fn take(&self) -> Option<u64> {
        let mut w = lock(&self.work);
        loop {
            if self.stop.load(SeqCst) {
                return None;
            }
            if let Some(id) = w.queue.pop_front() {
                return Some(id);
            }
            w = self.wake.wait(w).unwrap_or_else(|e| e.into_inner());
        }
    }

    fn work_done(&self) {
        let mut w = lock(&self.work);
        w.unfinished = w.unfinished.saturating_sub(1);
    }

    /// True when every job put on the work queue is finished.
    pub fn idle(&self) -> bool {
        lock(&self.work).unfinished == 0
    }

    fn worker(&self) {
        while let Some(id) = self.take() {
            // a job skipped while it sat in the queue is dropped here
            let j = self.edit(id, |j| {
                if j["state"] == "queued" {
                    j["state"] = json!("running");
                }
            });
            let Some(j) = j.filter(|j| j["state"] == "running") else {
                self.work_done();
                continue;
            };
            if let Err(e) = self.run(j) {
                // job boundary: mark failed, keep the worker alive
                let virus = e.contains("(os error 225)") || e.contains("(os error 226)"); // real-time protection stopped or removed the file
                let refused = e.contains("(os error 87)") && e.contains(": '"); // Windows refused to open a named file
                let status = if virus {
                    "Blocked: virus found (Microsoft Defender stopped the file)".to_string()
                } else if refused {
                    "Blocked: Windows refused to read the file. An antivirus may have stopped it: see Windows Security > Protection history"
                        .to_string()
                } else {
                    format!("Failed: {e}")
                };
                self.set(id, Some("failed"), &status, "");
            }
            self.after_job();
            self.work_done();
        }
    }

    fn run(&self, mut j: Value) -> Result<(), String> {
        let id = job_id(&j);
        let premium = self.is_premium();
        if j["src"]["type"] == "nexus" {
            if self.key().is_empty() {
                self.set(id, Some("needkey"), "Needs your Nexus API key (Settings). I continue when you add it.", "");
                return Ok(());
            }
            if !(premium || truthy(&j["src"]["key"])) {
                if j["src"].get("github").is_none() {
                    // looked up once: a retry or a later click does not ask GitHub again
                    self.set(id, None, "Looking for the author's GitHub release", "");
                    let gh = match nexus::github_copy(int(&j["src"]["mod_id"]).unwrap_or(0), j["version"].as_str().unwrap_or("")) {
                        Ok(g) => g.unwrap_or(Value::Null),
                        Err(e) => {
                            // network boundary: the Nexus click still works
                            self.log(&format!("{}: GitHub check failed ({e})", text(&j["name"])));
                            Value::Null
                        }
                    };
                    j["src"]["github"] = gh.clone();
                    self.edit(id, |x| x["src"]["github"] = gh);
                }
                if !truthy(&j["src"]["github"]) {
                    self.set(id, Some("waiting"), "Waiting for the Nexus download link", "");
                    return Ok(());
                }
            }
        }
        // One new folder per job: no two jobs share a file, and nothing here is ever the user's own file.
        let work = self.job_dir(id)?;
        let r = self.fetch_install(&mut j, &work, premium);
        let _ = std::fs::remove_dir_all(&work);
        if r? {
            self.set(id, Some("done"), "Done", "");
        }
        Ok(())
    }

    fn job_dir(&self, id: u64) -> Result<PathBuf, String> {
        for n in 0u32.. {
            let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
            let p = self.downloads.join(format!("job-{id}-{nanos:x}{n}"));
            match std::fs::create_dir(&p) {
                Ok(()) => return Ok(p),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists && n < 100 => continue,
                Err(e) => return Err(format!("{e}: '{}'", p.display())),
            }
        }
        unreachable!()
    }

    /// Download (or copy), scan and install one job. False when the virus scan blocked it.
    fn fetch_install(&self, j: &mut Value, work: &Path, premium: bool) -> Result<bool, String> {
        let id = job_id(j);
        let s = j["src"].clone();
        let ty = s["type"].as_str().unwrap_or("");
        let name = text(&j["name"]);
        let prog = |done: u64, total: u64| {
            let p = (done * 100).checked_div(total).map_or_else(|| format!("{} MB", done >> 20), |p| format!("{p}%"));
            self.set(id, None, "Downloading", &p);
        };
        let virus_scan = lock(&self.store).settings.virus_scan;
        let path = if ty == "nexus" && truthy(&s["github"]) && !(premium || truthy(&s["key"])) {
            self.log(&format!("{name}: same version on the author's GitHub ({}), so no click is needed.", text(&s["github"]["repo"])));
            fetch::download(s["github"]["url"].as_str().unwrap_or(""), work, Some(&prog), &[])?
        } else if ty == "nexus" {
            self.set(id, None, "Getting download link", "");
            let (m, f) = (int(&s["mod_id"]).unwrap_or(0), int(&s["file_id"]).unwrap_or(0));
            let url = nexus::download_url(&self.key(), m, f, s["key"].as_str(), s["expires"].as_str())?;
            fetch::download(&url, work, Some(&prog), &[])?
        } else if ty == "github" {
            let rel = fetch::github_latest(s["repo"].as_str().unwrap_or(""))?;
            j["version"] = rel["tag"].clone();
            self.edit(id, |x| x["version"] = rel["tag"].clone());
            fetch::download(rel["url"].as_str().unwrap_or(""), work, Some(&prog), &[])?
        } else if ty == "url" {
            fetch::download(s["url"].as_str().unwrap_or(""), work, Some(&prog), &[])?
        } else {
            // A file from the PC is only read. Defender gets a copy under a fixed plain name (its command line).
            let mut path = PathBuf::from(s["path"].as_str().unwrap_or(""));
            if virus_scan {
                self.set(id, None, "Copying", "");
                let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
                let plain = (1..=5).contains(&ext.len()) && ext.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
                let copy = work.join(if plain { format!("archive.{ext}") } else { "archive".into() });
                // only the content: a read-only file must not make a read-only copy that cannot be removed
                let mut from = std::fs::File::open(&path).map_err(|e| format!("{e}: '{}'", path.display()))?;
                let mut to = std::fs::File::create(&copy).map_err(|e| format!("{e}: '{}'", copy.display()))?;
                io::copy(&mut from, &mut to).map_err(|e| format!("{e}: '{}'", path.display()))?;
                path = copy;
            }
            path
        };
        if virus_scan {
            self.set(id, None, "Scanning for viruses", "");
            let (clean, detail) = (self.deps.scan)(&path);
            if clean == Some(false) {
                self.set(id, Some("failed"), &format!("Blocked: virus found ({detail})"), "");
                return Ok(false);
            }
            if clean.is_none() {
                self.log(&format!("{name}: virus scan skipped ({detail})"));
            }
        }
        self.set(id, None, "Installing", "");
        self.install_job(j, &path)?;
        Ok(true)
    }

    fn install_job(&self, j: &Value, path: &Path) -> Result<(), String> {
        let s = &j["src"];
        let ty = s["type"].as_str().unwrap_or("");
        let name = text(&j["name"]);
        let mid = match j.get("replace").and_then(Value::as_str).filter(|r| !r.is_empty()) {
            Some(r) => r.to_string(),
            None if ty == "nexus" => format!("nexus-{}-{}", text(&s["mod_id"]), text(&s["file_id"])),
            None if ty == "github" => format!("gh-{}", install::safe(&s["repo"].as_str().unwrap_or("").replace('/', "-"))),
            None => format!("{ty}-{}", install::safe(&name)),
        };
        // unpack beside the old copy, swap under the lock (a deploy may be reading it)
        let new = self.staging.join(format!("{mid}.new"));
        let rels = (self.deps.install)(path, &new, &name)?;
        let fw: Option<String> = j
            .get("framework")
            .and_then(Value::as_str)
            .filter(|f| !f.is_empty())
            .map(String::from)
            .or_else(|| s.get("repo").and_then(Value::as_str).and_then(fw_by_repo).map(String::from))
            .or_else(|| int(s.get("mod_id").unwrap_or(&Value::Null)).and_then(|i| fw_by_nexus(i as u64)).map(String::from));
        let needs: Vec<String> = install::needs_for(&rels).into_iter().filter(|n| Some(*n) != fw.as_deref()).map(String::from).collect();
        let source: Map<String, Value> =
            s.as_object().into_iter().flatten().filter(|(k, _)| !["key", "expires", "github"].contains(&k.as_str())).map(|(k, v)| (k.clone(), v.clone())).collect();
        let mut rec = Mod {
            id: mid.clone(),
            name,
            version: j.get("version").and_then(Value::as_str).unwrap_or("").to_string(),
            enabled: true,
            source: Value::Object(source),
            files: rels,
            needs,
            requires: j.get("requires").and_then(Value::as_array).cloned().unwrap_or_default(),
            framework: fw.clone(),
            installed: Some(now()),
            extra: Map::new(),
        };
        let needs = rec.needs.clone();
        {
            let mut st = lock(&self.store);
            let dest = self.staging.join(&mid);
            let _ = std::fs::remove_dir_all(&dest);
            std::fs::rename(&new, &dest).map_err(|e| format!("{e}: '{}'", new.display()))?;
            match st.mods.iter().position(|m| m.id == mid) {
                Some(i) => {
                    let old = &st.mods[i];
                    rec.enabled = old.enabled;
                    if rec.requires.is_empty() {
                        rec.requires = old.requires.clone();
                    }
                    st.mods[i] = rec;
                    for v in st.deployed.values_mut() {
                        // updated files: copy them into the game again
                        if v.as_deref() == Some(mid.as_str()) {
                            *v = None;
                        }
                    }
                }
                None if fw.is_some() => st.mods.insert(0, rec), // frameworks first: other mods should win any overlap
                None => st.mods.push(rec),
            }
            st.save().map_err(|e| e.to_string())?;
        }
        for need in &needs {
            // automatic setup: fetch every framework the new files need
            self.add_framework(need)?;
        }
        self.notify("mods", Value::Null);
        Ok(())
    }

    fn after_job(&self) {
        let busy = lock(&self.jobs).iter().any(|j| j["state"] == "queued" || j["state"] == "running");
        self.next_click(); // open the next Nexus page before the slower deploy
        self.finish_pack(); // before the deploy below, so the game gets the pack
        if !busy && self.game().is_some() {
            self.redeploy(true, true);
        }
    }

    // ---------- mod list ----------
    /// Copy the mods into the game folder: in the background, or now (`sync`). `quiet` logs nothing when nothing changed.
    pub fn redeploy(&self, sync: bool, quiet: bool) {
        let run = move |c: &Controller| -> Result<(), String> {
            let n = {
                let mut st = lock(&c.store);
                if c.stop.load(SeqCst) {
                    return Ok(()); // the window is closing: start no new copy
                }
                (c.deps.deploy)(&mut st)?
            };
            if n > 0 || !quiet {
                c.log(&if n > 0 {
                    format!("Mods are set up in the game folder ({n} file change(s)).")
                } else {
                    "Everything is already set up in the game folder.".to_string()
                });
            }
            Ok(())
        };
        if sync {
            if let Err(e) = run(self) {
                self.log(&format!("Error: {e}"));
            }
        } else {
            self.bg(move |c| run(c));
        }
    }

    pub fn set_enabled(&self, ids: &[String], on: bool) -> Result<(), String> {
        {
            let mut st = lock(&self.store);
            for m in st.mods.iter_mut().filter(|m| ids.contains(&m.id)) {
                m.enabled = on;
            }
            st.save().map_err(|e| e.to_string())?;
        }
        self.notify("mods", Value::Null);
        Ok(())
    }

    /// Move a mod `delta` places (up = toward index 0, the lowest priority).
    pub fn move_mod(&self, mid: &str, delta: i64) -> Result<(), String> {
        {
            let mut st = lock(&self.store);
            let i = st.mods.iter().position(|m| m.id == mid).ok_or_else(|| format!("No mod {mid}"))?;
            let k = (i as i64 + delta).clamp(0, st.mods.len() as i64 - 1) as usize;
            let m = st.mods.remove(i);
            st.mods.insert(k, m);
            st.save().map_err(|e| e.to_string())?;
        }
        self.notify("mods", Value::Null);
        Ok(())
    }

    // ---------- mod packs ----------
    /// Keep the mod list as it is now (which mods are on, and their order) as the mod pack `name`.
    pub fn save_pack(&self, name: &str) -> Result<(), String> {
        let name = name.trim();
        if name.is_empty() {
            return Err("Give the mod pack a name.".into());
        }
        {
            let mut st = lock(&self.store);
            let pack = st.mods.iter().map(|m| (m.id.clone(), m.enabled)).collect();
            st.packs.insert(name.to_string(), pack);
            st.pack = name.to_string();
            st.save().map_err(|e| e.to_string())?;
        }
        self.notify("mods", Value::Null);
        Ok(())
    }

    /// Turn on the pack's mods in the pack's order and turn off the rest; the message for the user.
    pub fn use_pack(&self, name: &str) -> Result<String, String> {
        let (on, missing) = use_pack_in(&mut lock(&self.store), name)?;
        self.notify("mods", Value::Null);
        let msg = pack_message(name, on, missing);
        self.log(&msg);
        Ok(msg)
    }

    /// Download the pack's Nexus mods that are not installed yet. Returns how many it queued. Every link gets a
    /// name, so each mod downloads straight away with no per-mod lookup (a merged pack can hold thousands).
    pub fn complete_pack(&self, name: &str) -> usize {
        static NX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^nexus-(\d+)-(\d+)$").unwrap());
        let mut links: Vec<Value> = {
            let st = lock(&self.store);
            let have: HashSet<&str> = st.mods.iter().map(|m| m.id.as_str()).collect();
            let mut names: HashMap<&str, &Value> = HashMap::new();
            for info in st.pack_info.values() {
                for (k, v) in info["names"].as_object().into_iter().flatten() {
                    names.insert(k, v);
                }
            }
            st.packs
                .get(name)
                .into_iter()
                .flatten()
                .filter(|(mid, _)| !have.contains(mid.as_str()))
                .filter_map(|(mid, _)| {
                    let c = NX.captures(mid)?;
                    let (m, f) = (c[1].parse::<i64>().ok()?, c[2].parse::<i64>().ok()?);
                    Some(json!({"type": "nexus", "mod_id": m, "file_id": f, "name": names.get(mid.as_str()).map_or(Value::Null, |v| (*v).clone())}))
                })
                .collect()
        };
        let n = links.len();
        let name = name.to_string();
        self.bg(move |c| {
            let mut unnamed: Vec<i64> = links.iter().filter(|l| !truthy(&l["name"])).filter_map(|l| l["mod_id"].as_i64()).collect();
            unnamed.sort();
            unnamed.dedup();
            let mut found: BTreeMap<i64, Value> = BTreeMap::new();
            for chunk in unnamed.chunks(100) {
                match nexus::mod_names(chunk) {
                    Ok(f) => found.extend(f),
                    Err(e) => c.log(&format!("Mod names: {e}")), // network boundary: the mod id stands in for the name
                }
            }
            for l in links.iter_mut() {
                if !truthy(&l["name"]) {
                    let id = l["mod_id"].as_i64().unwrap_or(0);
                    let n = found.get(&id).map(|f| f["name"].clone()).filter(truthy).unwrap_or_else(|| json!(format!("Nexus mod {id}")));
                    l["name"] = n;
                }
            }
            for l in &links {
                // one bad mod must not stop the others
                if let Err(e) = c.add(l) {
                    c.log(&format!("{}: {e}", text(&l["name"])));
                }
            }
            {
                // set only now: a fast first download must not use the pack before the rest is queued
                let _jobs = lock(&c.jobs);
                *lock(&c.finish_pack) = Some(name);
            }
            if c.finish_pack() && c.game().is_some() {
                c.redeploy(false, false);
            }
            Ok(())
        });
        n
    }

    /// After Complete installation: once none of the downloads is left (done, failed or skipped), use the pack:
    /// its mods on, in its order, the rest off. True when it did.
    fn finish_pack(&self) -> bool {
        let name = {
            let jobs = lock(&self.jobs);
            let mut fp = lock(&self.finish_pack);
            if fp.is_none() || jobs.iter().any(|j| ACTIVE.contains(&j["state"].as_str().unwrap_or(""))) {
                return false;
            }
            fp.take().unwrap_or_default()
        };
        let exists = lock(&self.store).packs.contains_key(&name);
        if exists {
            // the user may have deleted it meanwhile
            if let Err(e) = self.use_pack(&name) {
                self.log(&format!("Error: {e}"));
            }
        }
        true
    }

    // ---------- app update ----------
    /// Get this app's newest version from GitHub. With no rel it only looks: the window asks, then calls this again
    /// with the release. The exe is downloaded and must pass a Defender scan; a script swaps it in once the window
    /// has closed (Windows cannot overwrite a running exe) and starts it. Every outcome, errors too, is an "update"
    /// event, so the window can turn its button back on.
    pub fn update_app(&self, rel: Option<Value>) {
        let r = (|| -> Result<(), String> {
            let Some(rel) = rel else {
                let latest = fetch::app_latest()?;
                self.notify("update", match latest {
                    Some(rel) => json!({"ask": rel}),
                    None => json!({"text": format!("You have the newest version ({VERSION}).")}),
                });
                return Ok(());
            };
            let tag = text(&rel["tag"]);
            self.notify("update", json!({"text": format!("Downloading version {tag}…"), "busy": true}));
            let folder = self.root.join("update");
            std::fs::create_dir_all(&folder).map_err(|e| e.to_string())?;
            let new = fetch::download(rel["url"].as_str().unwrap_or(""), &folder, None, &[])?;
            let (ok, why) = (self.deps.scan)(&new);
            if ok != Some(true) {
                // a file Defender could not scan does not run either
                let _ = std::fs::remove_file(&new);
                return Err(if ok == Some(false) {
                    format!("Microsoft Defender found {why}")
                } else {
                    format!("Microsoft Defender could not scan the new version ({why})")
                });
            }
            let exe = std::env::current_exe().map_err(|e| e.to_string())?;
            let mut cmd = Command::new(shared::sys32().join("WindowsPowerShell/v1.0/powershell.exe"));
            cmd.args(["-NoProfile", "-NonInteractive", "-Command", SWAP])
                .env("CP_NEW", &new)
                .env("CP_EXE", &exe)
                .creation_flags(NO_WINDOW | CREATE_NEW_PROCESS_GROUP); // DETACHED_PROCESS: PowerShell runs nothing
            (self.deps.spawn)(&mut cmd).map_err(|e| e.to_string())?;
            self.notify("update", json!({"text": format!("Installing version {tag}. The app restarts by itself."), "restart": true}));
            Ok(())
        })();
        if let Err(e) = r {
            self.notify("update", json!({"text": format!("Update failed: {e}")}));
        }
    }

    /// One pack with every mod of `names`. A mod in several packs is on if one of them turns it on, and takes its
    /// place from the last pack, so a later pack wins when two mods change the same file.
    pub fn merge_packs(&self, names: &[String], new: &str) -> Result<usize, String> {
        let new = new.trim();
        if names.len() < 2 || new.is_empty() {
            return Err("Pick two or more mod packs and give the new pack a name.".into());
        }
        let n = {
            let mut st = lock(&self.store);
            let (mut last, mut on, mut k) = (HashMap::<String, usize>::new(), HashSet::<String>::new(), 0);
            for n in names {
                for (mid, e) in st.packs.get(n).ok_or_else(|| format!("No mod pack '{n}'"))? {
                    last.insert(mid.clone(), k);
                    k += 1;
                    if *e {
                        on.insert(mid.clone());
                    }
                }
            }
            let mut order: Vec<(String, usize)> = last.into_iter().collect();
            order.sort_by_key(|(_, k)| *k);
            let pack: Vec<(String, bool)> = order.into_iter().map(|(m, _)| (m.clone(), on.contains(&m))).collect();
            let n = pack.len();
            st.packs.insert(new.to_string(), pack);
            st.save().map_err(|e| e.to_string())?;
            n
        };
        self.notify("mods", Value::Null);
        Ok(n)
    }

    pub fn delete_pack(&self, name: &str) -> Result<(), String> {
        {
            let mut st = lock(&self.store);
            st.packs.shift_remove(name);
            st.pack_info.shift_remove(name);
            if st.pack == name {
                st.pack.clear();
            }
            st.save().map_err(|e| e.to_string())?;
        }
        self.notify("mods", Value::Null);
        Ok(())
    }

    pub fn remove(&self, mid: &str) -> Result<(), String> {
        {
            let mut st = lock(&self.store);
            st.mods.retain(|m| m.id != mid);
            st.save().map_err(|e| e.to_string())?;
            let _ = std::fs::remove_dir_all(self.staging.join(mid));
        }
        self.notify("mods", Value::Null);
        Ok(())
    }

    pub fn update_mod(&self, mid: &str) -> Result<(), String> {
        let m = lock(&self.store).mod_by_id(mid).cloned().ok_or_else(|| format!("No mod {mid}"))?;
        let s = &m.source;
        match m.source_type() {
            "github" => {
                self.job(&m.name, s.clone(), json!({"replace": mid, "framework": m.framework}));
            }
            "nexus" => {
                let info = nexus::mod_info(int(&s["mod_id"]).unwrap_or(0), &self.key())?;
                let files = info["files"].as_array().cloned().unwrap_or_default();
                let Some(f) = nexus::newer_file(&files, int(&s["file_id"]).unwrap_or(0)) else {
                    self.log(&format!("{} is already the newest version.", m.name));
                    return Ok(());
                };
                let src = json!({"type": "nexus", "mod_id": s["mod_id"], "file_id": int(&f["fileId"])});
                self.job(&m.name, src, json!({"replace": mid, "version": f["version"], "requires": info["requires"]}));
            }
            _ => self.log(&format!("{} came from a file or plain link. Add the new file to update it.", m.name)),
        }
        Ok(())
    }

    pub fn has_update(&self, m: &Mod) -> bool {
        if let Some(f) = &self.deps.has_update {
            return f(m);
        }
        let s = &m.source;
        // offline or API down: offer no update
        match m.source_type() {
            "nexus" => nexus::mod_info(int(&s["mod_id"]).unwrap_or(0), &self.key())
                .is_ok_and(|i| nexus::newer_file(i["files"].as_array().map_or(&[], Vec::as_slice), int(&s["file_id"]).unwrap_or(0)).is_some()),
            "github" => fetch::github_latest(s["repo"].as_str().unwrap_or("")).is_ok_and(|r| r["tag"].as_str() != Some(m.version.as_str())),
            _ => false,
        }
    }

    // ---------- game and crashes ----------
    /// Start the game with the mods that are on, or first set the mod list to the mod pack `pack`.
    pub fn launch(&self, pack: Option<&str>) -> Result<(), String> {
        let game = self.game().ok_or("Set the Cyberpunk 2077 folder in Settings first.")?;
        self.ensure_acr();
        let snap = snapshot(&lock(&self.store));
        for p in doctor::game_updated(&snap, &self.has_update_fn()) {
            // warn only: the game may run fine
            self.log(&format!("Warning: {}. Click 'Check my mods now' for framework updates.", text(&p["title"])));
        }
        let (used, redmod, replaced) = {
            let mut st = lock(&self.store);
            if self.stop.load(SeqCst) {
                return Ok(()); // the window is closing
            }
            let used = match pack {
                Some(p) => Some((p, use_pack_in(&mut st, p)?)),
                None => None,
            };
            (self.deps.deploy)(&mut st)?;
            let redmod = st.enabled().iter().any(|m| m.files.iter().any(|r| r.to_lowercase().starts_with("mods/")));
            (used, redmod, st.deployed.contains_key(&GAME_EXE.to_lowercase()))
        };
        if let Some((p, (on, missing))) = used {
            self.notify("mods", Value::Null);
            self.log(&pack_message(p, on, missing));
        }
        let (exe, cwd) = (game.join(GAME_EXE), game.join("bin/x64"));
        let args: Vec<String> = if redmod { vec!["-modded".into()] } else { vec![] };
        (self.deps.block_ai)(true); // free the local AI's memory before the game loads, and keep it off while the game runs
        let mut cmd = Command::new(&exe);
        cmd.args(&args).current_dir(&cwd);
        let r = match (self.deps.spawn)(&mut cmd) {
            Ok(()) => Ok(()),
            // "Run as administrator" is set: Windows asks the user first. Never elevate an exe that a mod put there.
            Err(e) if e.raw_os_error() == Some(740) && !replaced => (self.deps.run_as)(&exe, &args, &cwd),
            Err(e) => Err(e.to_string()),
        };
        if let Err(e) = r {
            if !(self.game_on() || self.stop.load(SeqCst)) {
                (self.deps.block_ai)(false); // no game started, and the window is not closing
            }
            return Err(e);
        }
        // The game watch sees the game within seconds; a game that never showed up must not keep the AI off.
        let w = self.me.clone();
        (self.deps.timer)(
            Duration::from_secs(120),
            Box::new(move || {
                if let Some(c) = w.upgrade()
                    && !(c.game_on() || c.stop.load(SeqCst))
                {
                    (c.deps.block_ai)(false);
                }
            }),
        );
        Ok(())
    }

    fn game_started(&self, t: f64) {
        self.game_on.store(true, SeqCst); // also when the game starts from Steam or GOG
        (self.deps.block_ai)(true);
        self.notify("game", json!(true));
        let r = {
            let mut st = lock(&self.store);
            st.doctor.insert("last_launch".into(), json!(t));
            st.save()
        };
        if let Err(e) = r {
            // an error here would make the game watch skip this game's exit: the AI would stay off
            self.log(&format!("Could not save the launch time: {e}"));
        }
    }

    fn game_exited(&self, started: f64, exit_code: Option<u32>, freeze: Option<&Path>) {
        self.game_on.store(false, SeqCst);
        if !self.stop.load(SeqCst) {
            (self.deps.block_ai)(false); // the window is not closing
        }
        self.notify("game", json!(false));
        let mut ev = (self.deps.crash_evidence)(started, exit_code);
        if !ev.is_object() {
            ev = json!({"crashed": false});
        }
        if let Some(f) = freeze {
            // the game stopped responding and never came back: the user closed it, or the game's own timer did
            ev["crashed"] = json!(true);
            ev["hang"] = (self.deps.read_dump)(f).unwrap_or(Value::Null);
            self.log(&format!("The game froze. I saved a snapshot of its threads: {}", f.display()));
        }
        let secs = now() - started;
        let crashed = truthy(&ev["crashed"]);
        if crashed {
            let snap = snapshot(&lock(&self.store));
            let saved = doctor::keep_logs(&snap, started, ev.get("report").and_then(Value::as_str), doctor::KEEP_LOGS, doctor::LOG_LIMIT);
            ev["saved"] = saved.map_or(Value::Null, |p| json!(p.to_string_lossy())); // the game deletes old logs
        }
        let (r, bisect) = {
            let mut st = lock(&self.store);
            if crashed {
                let fault = ev.get("fault").cloned().unwrap_or(Value::Null);
                push_to(&mut st.doctor, "crashes", json!({"time": now(), "fault": fault, "secs": secs as i64}));
                keep_last(&mut st.doctor, "crashes", 50);
            }
            st.doctor.insert("last_exit".into(), ev.clone()); // for the crash report, also after the app restarts
            (st.save(), st.doctor.get("bisect").is_some_and(truthy))
        };
        if let Err(e) = r {
            self.log(&format!("Could not save the crash details: {e}"));
        }
        if bisect {
            if let Err(e) = self.bisect_result(crashed, secs) {
                self.log(&format!("Error: {e}"));
            }
            return;
        }
        if !crashed {
            let snap = snapshot(&lock(&self.store));
            let noisy = doctor::noisy_mods(&snap, started); // errors in a launch that worked are not what crashes the game
            let version = snap.game().map(|g| doctor::game_version(&g));
            let r = {
                let mut st = lock(&self.store);
                let good: Map<String, Value> = st.enabled().iter().map(|m| (m.id.clone(), json!(m.version))).collect();
                st.doctor.insert("last_good".into(), Value::Object(good));
                push_to(&mut st.doctor, "clean_errors", json!(noisy));
                keep_last(&mut st.doctor, "clean_errors", 3);
                if let Some(v) = version {
                    st.doctor.insert("last_good_game".into(), json!(v));
                }
                st.save()
            };
            if let Err(e) = r {
                self.log(&format!("Error: {e}"));
            }
            return self.log("The game closed normally. This mod setup is saved as the last good one.");
        }
        self.log("The game crashed. Looking for the cause...");
        let snap = snapshot(&lock(&self.store));
        let probs = doctor::diagnose(&snap, started, Some(&ev), &self.has_update_fn());
        if let Err(e) = self.handle_problems(probs, true) {
            self.log(&format!("Error: {e}"));
        }
    }

    /// Text of a report about the last game exit and the current mods, to share with a mod author or an AI chat.
    pub fn crash_report(&self, for_ai: bool) -> String {
        let snap = snapshot(&lock(&self.store));
        let since = snap.doctor.get("last_launch").and_then(Value::as_f64).unwrap_or(0.0);
        let ev = snap.doctor.get("last_exit").cloned();
        let probs = doctor::diagnose(&snap, since, ev.as_ref(), &self.has_update_fn());
        doctor::report(&snap, &probs, ev.as_ref(), for_ai)
    }

    pub fn check_now(&self) -> Result<Vec<Value>, String> {
        let snap = snapshot(&lock(&self.store));
        let since = snap.doctor.get("last_launch").and_then(Value::as_f64).unwrap_or(0.0);
        let probs = doctor::diagnose(&snap, since, None, &self.has_update_fn());
        self.handle_problems(probs.clone(), false)?;
        Ok(probs)
    }

    /// After a crash with auto-fix on, a high-severity problem with one fix is fixed at once; the rest go to the window.
    pub fn handle_problems(&self, probs: Vec<Value>, crashed: bool) -> Result<(), String> {
        let auto_fix = lock(&self.store).settings.auto_fix;
        let mut ask = vec![];
        for p in probs {
            let fixes = p["fixes"].as_array().map_or(&[][..], Vec::as_slice);
            if crashed && auto_fix && truthy(&p["auto"]) && p["severity"] == "high" && fixes.len() == 1 {
                self.apply(&p, &fixes[0])?;
                self.log(&format!("Fixed automatically: {} -> {}", text(&p["title"]), text(&fixes[0]["label"])));
            } else {
                ask.push(p);
            }
        }
        self.notify("problems", json!({"list": ask, "crashed": crashed}));
        Ok(())
    }

    /// Apply fix `f` of problem `p` and keep an undo step in the Crash doctor's history.
    pub fn apply(&self, p: &Value, f: &Value) -> Result<(), String> {
        let action = f["action"].as_array().cloned().unwrap_or_default();
        let verb = action.first().and_then(Value::as_str).unwrap_or("").to_string();
        let arg = |i: usize| action.get(i + 1).cloned().unwrap_or(Value::Null);
        {
            let mut guard = lock(&self.store);
            let st = &mut *guard;
            let undo = match verb.as_str() {
                "disable" | "enable" | "disable_many" => {
                    let ids = if verb == "disable_many" { strs(&arg(0)) } else { vec![text(&arg(0))] };
                    let was: Map<String, Value> = st.mods.iter().filter(|m| ids.contains(&m.id)).map(|m| (m.id.clone(), json!(m.enabled))).collect();
                    for m in st.mods.iter_mut().filter(|m| ids.contains(&m.id)) {
                        m.enabled = verb == "enable";
                    }
                    json!(["states", was])
                }
                "order" => {
                    // arg 0: mod ids in the new order
                    let undo = json!(["order", ids(&st.mods)]);
                    order_mods(st, &strs(&arg(0)));
                    undo
                }
                "prefer" => {
                    // let arg 0 win over arg 1: move it just below, and keep the choice for Auto-sort
                    let (lo, hi) = (text(&arg(0)), text(&arg(1)));
                    let pid = format!("conflict-{hi}-{lo}");
                    let undo = json!(["prefer", [ids(&st.mods), pid]]);
                    let i = st.mods.iter().position(|m| m.id == lo).ok_or_else(|| format!("No mod {lo}"))?;
                    if !st.mods.iter().any(|m| m.id == hi) {
                        return Err(format!("No mod {hi}"));
                    }
                    let m = st.mods.remove(i);
                    let k = st.mods.iter().position(|m| m.id == hi).unwrap_or(0) + 1;
                    st.mods.insert(k, m);
                    push_to(&mut st.doctor, "ignored", json!(pid));
                    undo
                }
                "ignore" => {
                    push_to(&mut st.doctor, "ignored", arg(0));
                    json!(["unignore", arg(0)])
                }
                _ => Value::Null,
            };
            push_to(&mut st.doctor, "history", json!({"time": now(), "problem": p["title"], "fix": f["label"], "undo": undo}));
            st.save().map_err(|e| e.to_string())?;
        }
        match verb.as_str() {
            "update" => {
                let mid = text(&arg(0));
                self.bg(move |c| c.update_mod(&mid));
            }
            "install_fw" => {
                let fw = text(&arg(0));
                self.bg(move |c| c.add_framework(&fw));
            }
            "queue_nexus" => {
                let link = json!({"type": "nexus", "mod_id": arg(0)});
                self.bg(move |c| c.add(&link));
            }
            "bisect_start" => self.bisect_begin()?,
            _ => {}
        }
        self.notify("mods", Value::Null);
        if self.game().is_some() {
            self.redeploy(false, false);
        }
        Ok(())
    }

    /// Undo history step `i`.
    pub fn undo(&self, i: usize) -> Result<(), String> {
        {
            let mut guard = lock(&self.store);
            let st = &mut *guard;
            let h = match st.doctor.get_mut("history") {
                Some(Value::Array(a)) if i < a.len() => a.remove(i),
                _ => return Err(format!("No undo step {i}")),
            };
            let u = &h["undo"];
            let (kind, arg) = (u[0].as_str().unwrap_or(""), &u[1]);
            match kind {
                "states" => {
                    for m in st.mods.iter_mut() {
                        m.enabled = arg.get(&m.id).and_then(Value::as_bool).unwrap_or(m.enabled);
                    }
                }
                "order" => order_mods(st, &strs(arg)),
                "unignore" => remove_first(st.doctor.get_mut("ignored"), arg),
                "prefer" => {
                    order_mods(st, &strs(&arg[0]));
                    remove_first(st.doctor.get_mut("ignored"), &arg[1]);
                }
                _ => {}
            }
            st.save().map_err(|e| e.to_string())?;
        }
        self.notify("mods", Value::Null);
        if self.game().is_some() {
            self.redeploy(false, false);
        }
        Ok(())
    }

    /// Put the mods in the best load order (the Crash doctor can undo it), then check them for problems.
    pub fn auto_sort(&self) -> Result<String, String> {
        let snap = snapshot(&lock(&self.store));
        let order = doctor::best_order(&snap, &[]);
        let moved = doctor::moved(&ids(&snap.mods), &order);
        if moved > 0 {
            let f = doctor::fix(&format!("Sorted automatically ({moved} mod(s) changed place)"), json!(["order", order]), false);
            self.apply(&json!({"title": "Load order"}), &f)?;
        }
        let msg = if moved > 0 {
            format!("Load order sorted: {moved} mod(s) changed place.")
        } else {
            "The mods are already in the best load order I can find.".to_string()
        };
        self.log(&msg);
        self.check_now()?;
        Ok(msg)
    }

    // ---------- local AI ----------
    /// Download the engine and the model when they are missing, then start the engine once.
    pub fn setup_ai(&self) {
        let r = (|| -> Result<(), String> {
            if self.game_on() {
                return Err(GAME_ON.into());
            }
            if !local_ai::ready() {
                local_ai::fetch(&|what, done, total| {
                    let pct = (done * 100).checked_div(total).unwrap_or(0);
                    self.notify("ai_status", json!(format!("Downloading the local AI ({what}): {pct}%")));
                })?;
            }
            self.notify("ai_status", json!("Starting the local AI…"));
            (self.deps.start_ai)()?;
            self.notify("ai_status", json!("✔ Ready: the local AI runs on this PC. It stops when the game starts or this window closes."));
            Ok(())
        })();
        if let Err(e) = r {
            self.notify("ai_status", json!(format!("✖ {e}")));
        }
    }

    /// Check the mods, let the local AI pick fixes and load-order rules, and send the advice to the window ("ai").
    /// `report`: the player's own words about a bug; the AI then also names the cause and changes for those mods.
    pub fn ask_ai(&self, report: &str) {
        let r = (|| -> Result<Value, String> {
            if self.game_on() {
                return Err(GAME_ON.into());
            }
            (self.deps.start_ai)()?;
            let probs = self.check_now()?;
            // One copy of the list: the AI's mod numbers map back to it, and a framework install can shift the live one.
            let mods = lock(&self.store).mods.clone();
            let a = (self.deps.advise)(&mods, &probs, report.trim())?;
            let mut fixes: Vec<(Value, Value, String)> = a.fixes.into_iter().take(10).collect(); // the window lists 10: apply only fixes the user saw
            let order = doctor::best_order(&snapshot(&lock(&self.store)), &a.after);
            let moved = doctor::moved(&ids(&mods), &order);
            let mut base = if moved > 0 { order.clone() } else { ids(&mods) };
            let mine = json!({"title": "Your bug report"});
            for (m, act, why) in a.mod_fixes.into_iter().take(5) {
                let n = &m.name;
                let f = match act.as_str() {
                    "turn_on" if !m.enabled => doctor::fix(&format!("Turn on {n}"), json!(["enable", m.id]), false),
                    "turn_off" if m.enabled && !doctor::is_acr(&m.source) => doctor::fix(&format!("Turn off {n}"), json!(["disable", m.id]), false),
                    "update" if ["nexus", "github"].contains(&m.source_type()) => {
                        doctor::fix(&format!("Update {n} to its newest version"), json!(["update", m.id]), false)
                    }
                    // a picked "Let it win" fix does it already (applied after the order)
                    "load_last"
                        if base.last() != Some(&m.id)
                            && !fixes.iter().any(|(_, f, _)| f["action"][0] == "prefer" && f["action"][1] == json!(m.id)) =>
                    {
                        base.retain(|i| *i != m.id);
                        base.push(m.id.clone());
                        doctor::fix(&format!("Load {n} last, so it wins"), json!(["order", base]), false)
                    }
                    _ => continue,
                };
                fixes.push((mine.clone(), f, why));
            }
            let mut out = json!({
                "summary": a.summary,
                "fixes": fixes.into_iter().map(|(p, f, why)| json!([p, f, why])).collect::<Vec<_>>(),
                "after": a.after.iter().map(|(lo, hi)| json!([lo, hi])).collect::<Vec<_>>(),
            });
            if let Some(cause) = a.cause {
                out["cause"] = json!(cause);
            }
            out["order"] = if moved > 0 { json!(order) } else { Value::Null };
            out["moved"] = json!(moved);
            Ok(out)
        })();
        self.notify("ai", r.unwrap_or_else(|e| json!({"error": e})));
    }

    /// Apply the advice the window got in "ai". The order first, so that a fix that moves one mod (prefer) keeps
    /// the last word.
    pub fn apply_ai(&self, a: &Value) -> Result<(), String> {
        if truthy(&a["order"]) {
            let f = doctor::fix(&format!("Sorted by the local AI ({} mod(s) changed place)", text(&a["moved"])), json!(["order", a["order"]]), false);
            self.apply(&json!({"title": "Load order"}), &f)?;
        }
        for x in a["fixes"].as_array().into_iter().flatten() {
            self.apply(&x[0], &x[1])?;
        }
        self.check_now()?;
        Ok(())
    }

    // ---------- find mods from a video ----------
    pub fn find_in_video(&self, entry: &str) {
        match video_finder::find(entry) {
            // shown to the user (page blocked or gone, no network, Nexus down)
            Err(e) => self.notify("video", json!({"error": e})),
            Ok((title, rows)) => self.notify("video", json!({"title": title, "rows": rows})),
        }
    }

    // ---------- bad-mod search (bisect) ----------
    pub fn bisect_begin(&self) -> Result<(), String> {
        let state = doctor::bisect_start(&snapshot(&lock(&self.store)));
        let n = state["suspects"].as_array().map_or(0, Vec::len);
        if n < 2 {
            self.log("Fewer than two mods are on, so there is nothing to search.");
            return Ok(());
        }
        lock(&self.store).doctor.insert("bisect".into(), state.clone());
        self.bisect_apply(&state)?;
        self.log(&format!(
            "Bad-mod search: {n} suspects. I turned half of them off. Start the game and play until it crashes, or quit normally if it does not."
        ));
        Ok(())
    }

    fn bisect_apply(&self, state: &Value) -> Result<(), String> {
        let off = doctor::bisect_off(state);
        let restore = strs(&state["restore"]);
        {
            let mut st = lock(&self.store);
            for m in st.mods.iter_mut().filter(|m| restore.contains(&m.id)) {
                m.enabled = !off.contains(&m.id);
            }
            st.save().map_err(|e| e.to_string())?;
        }
        self.notify("mods", Value::Null);
        if self.game().is_some() {
            self.redeploy(false, false); // in the background, so the window does not freeze
        }
        Ok(())
    }

    pub fn bisect_cancel(&self) -> Result<(), String> {
        let state = lock(&self.store).doctor.shift_remove("bisect");
        if let Some(state) = state.filter(truthy) {
            self.bisect_apply(&json!({"suspects": [], "testing": [], "restore": state["restore"]}))?;
            self.log("Bad-mod search stopped. Your mods are back as they were.");
        }
        Ok(())
    }

    fn bisect_result(&self, crashed: bool, secs: f64) -> Result<(), String> {
        let need = lock(&self.store).doctor.get("bisect").and_then(|b| b.get("crash_secs")).and_then(Value::as_f64).unwrap_or(0.0);
        if !crashed && secs < need {
            self.log(&format!(
                "The game ran {} seconds without a crash, but the crash came after {} seconds before. This start does not count. Start the game again and play longer before you quit.",
                secs as i64, need as i64
            ));
            return Ok(());
        }
        let state = {
            let mut st = lock(&self.store);
            let state = doctor::bisect_step(st.doctor.get("bisect").unwrap_or(&Value::Null), crashed, secs);
            if state.get("culprit").is_none() {
                st.doctor.insert("bisect".into(), state.clone());
            } else {
                st.doctor.shift_remove("bisect");
            }
            state
        };
        if state.get("culprit").is_none() {
            self.bisect_apply(&state)?;
            if truthy(state.get("verifying").unwrap_or(&Value::Null)) {
                let first = text(&state["suspects"][0]);
                let name = lock(&self.store).mod_by_id(&first).map_or("the last suspect".to_string(), |m| m.name.clone());
                self.log(&format!("No crash. One last check: every mod is on again. Start the game once more; if it crashes, {name} is the bad mod."));
                return Ok(());
            }
            let n = state["suspects"].as_array().map_or(0, Vec::len);
            self.log(&format!("{}. {n} suspects left. Start the game again.", if crashed { "Crash" } else { "No crash" }));
            return Ok(());
        }
        self.bisect_apply(&json!({"suspects": [], "testing": [], "restore": state["restore"]}))?;
        let c = state["culprit"].as_str().and_then(|id| lock(&self.store).mod_by_id(id).cloned());
        let Some(c) = c else {
            self.log(&format!(
                "The search ended without one bad mod: {} Your mods are back as they were.",
                if truthy(state.get("crashes").unwrap_or(&Value::Null)) {
                    "the cause may be two mods together."
                } else {
                    "the game did not crash during the search."
                }
            ));
            return Ok(());
        };
        let p = doctor::problem(
            &format!("bisect-{}", c.id),
            &format!("Found it: {} makes the game crash", c.name),
            "Found by turning mods off half at a time.",
            doctor::mod_fixes(&c, &self.has_update_fn()),
            "high",
            true,
        );
        self.handle_problems(vec![p], true)
    }
}

impl ai_link::tools::Host for Controller {
    fn add_links(&self, text: &str) {
        Controller::add_links(self, text)
    }

    fn store(&self) -> &Mutex<Store> {
        &self.store
    }

    fn has_key(&self) -> bool {
        !self.key().is_empty()
    }

    fn premium(&self) -> bool {
        self.is_premium()
    }

    fn jobs(&self) -> Vec<Value> {
        lock(&self.jobs).clone()
    }

    fn auto_sort(&self) -> Result<String, String> {
        Controller::auto_sort(self)
    }

    fn check_now(&self) -> Result<Vec<Value>, String> {
        Controller::check_now(self)
    }
}

#[cfg(test)]
mod tests;
