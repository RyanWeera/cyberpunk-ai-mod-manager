//! The window. egui runs on the main thread; controller events arrive on a channel that `drain` reads each
//! frame. All state and every action live in `Window` as plain fields and methods, so the tests drive them
//! without a screen; `view` only draws them.

mod serve;
pub mod theme;
pub mod util;
mod view;

use crate::controller::{CORE, Controller, Deps, lock, shell_open};
use crate::pipe::OwnerListener;
use downloads::nexus;
use serde_json::{Map, Value, json};
use shared::{Mod, Settings, Store, VERSION, kind_of};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, TryLockError};
use eframe::egui;
use std::thread;
use std::time::{Duration, Instant};
pub use serve::Serve;
pub use theme::{BAD, OK, THEMES, Theme, WARN};
pub use util::{ago, short};

pub const KEY_PAGE: &str = "https://next.nexusmods.com/settings/api-keys";
pub const EXAMPLES: &str = "https://www.nexusmods.com/cyberpunk2077/mods/107\nhttps://www.nexusmods.com/games/cyberpunk2077/collections/iszwwe\nhttps://github.com/psiberx/cp2077-codeware";
pub const ACTIVE: [&str; 4] = ["queued", "running", "waiting", "needkey"];
/// The pack picker item that launches the list as it is.
pub const CURRENT: &str = "Current mod list";

pub fn level(sev: &str) -> &str {
    match sev {
        "high" => "Serious",
        "medium" => "Warning",
        "info" => "Info",
        s => s,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Page {
    Browse,
    Mods,
    MyPacks,
    Add,
    Find,
    Downloads,
    Crash,
    Ai,
    Settings,
}

impl Page {
    pub const ALL: [Page; 9] = [Page::Browse, Page::Mods, Page::MyPacks, Page::Add, Page::Find, Page::Downloads, Page::Crash, Page::Ai, Page::Settings];

    pub fn name(self) -> &'static str {
        match self {
            Page::Browse => "Browse",
            Page::Mods => "Mods",
            Page::MyPacks => "My mod packs",
            Page::Add => "Add mods",
            Page::Find => "Find mod from video",
            Page::Downloads => "Downloads",
            Page::Crash => "Crash doctor",
            Page::Ai => "AI helpers",
            Page::Settings => "Settings",
        }
    }
}

/// The rail pages and their icon glyphs, top to bottom (None = a line); Settings sits at the bottom.
pub const RAIL: [Option<(Page, &str)>; 9] = [
    Some((Page::Browse, "\u{e774}")),
    Some((Page::Mods, "\u{e8f1}")),
    Some((Page::MyPacks, "\u{e7b8}")),
    Some((Page::Downloads, "\u{e896}")),
    Some((Page::Crash, "\u{e90f}")),
    Some((Page::Find, "\u{e714}")),
    Some((Page::Ai, "\u{e99a}")),
    None,
    Some((Page::Add, "\u{e710}")),
];

/// The ways to add mods in the Add mods dialog: (glyph, title, text).
pub const ADD_WAYS: [(&str, &str, &str); 5] = [
    ("\u{e774}", "Browse Nexus Mods", "Find mods by name or category, and install them with one click."),
    ("\u{e71b}", "Paste links", "Nexus mods and collections, GitHub pages, or direct .zip / .7z / .rar links."),
    ("\u{e8e5}", "Add files from this PC", "Mod archives you downloaded yourself."),
    ("\u{e896}", "Install the core frameworks", "RED4ext, Cyber Engine Tweaks, redscript, ArchiveXL, TweakXL and Codeware. Most mods need them."),
    ("\u{e714}", "Find mods from a video", "Paste a YouTube, TikTok or Instagram link to get the mods it shows."),
];

/// The message boxes and file pickers; the tests give a scripted one.
pub trait Ask {
    fn yes_no(&self, title: &str, text: &str, default_no: bool) -> bool;
    fn info(&self, title: &str, text: &str);
    fn error(&self, title: &str, text: &str);
    fn open_files(&self) -> Vec<PathBuf>;
    fn save_file(&self, title: &str, name: &str) -> Option<PathBuf>;
    fn folder(&self, title: &str) -> Option<PathBuf>;
}

/// Windows message boxes (so "No" can be the default button) and rfd file pickers.
pub struct NativeAsk;

fn message_box(title: &str, text: &str, flags: u32) -> i32 {
    use windows_sys::Win32::UI::WindowsAndMessaging::{MB_SETFOREGROUND, MB_TOPMOST, MessageBoxW};
    let (t, c) = (shared::win::wide(text), shared::win::wide(title));
    // SAFETY: NUL-ended wide strings; no owner window.
    unsafe { MessageBoxW(std::ptr::null_mut(), t.as_ptr(), c.as_ptr(), flags | MB_TOPMOST | MB_SETFOREGROUND) }
}

impl Ask for NativeAsk {
    fn yes_no(&self, title: &str, text: &str, default_no: bool) -> bool {
        use windows_sys::Win32::UI::WindowsAndMessaging::{IDYES, MB_DEFBUTTON2, MB_ICONQUESTION, MB_YESNO};
        message_box(title, text, MB_YESNO | MB_ICONQUESTION | if default_no { MB_DEFBUTTON2 } else { 0 }) == IDYES
    }
    fn info(&self, title: &str, text: &str) {
        message_box(title, text, windows_sys::Win32::UI::WindowsAndMessaging::MB_ICONINFORMATION);
    }
    fn error(&self, title: &str, text: &str) {
        message_box(title, text, windows_sys::Win32::UI::WindowsAndMessaging::MB_ICONERROR);
    }
    fn open_files(&self) -> Vec<PathBuf> {
        rfd::FileDialog::new().add_filter("Mod archives", &["zip", "7z", "rar"]).add_filter("All files", &["*"]).pick_files().unwrap_or_default()
    }
    fn save_file(&self, title: &str, name: &str) -> Option<PathBuf> {
        rfd::FileDialog::new().set_title(title).set_file_name(name).add_filter("Text", &["txt"]).save_file()
    }
    fn folder(&self, title: &str) -> Option<PathBuf> {
        rfd::FileDialog::new().set_title(title).pick_folder()
    }
}

/// The nxm:// registration (HKCU); the tests give fakes.
pub struct Nxm {
    pub register: Option<Box<dyn Fn() -> std::io::Result<()>>>,
    pub registered: Box<dyn Fn() -> bool>,
    pub owner: Box<dyn Fn() -> String>,
}

impl Nxm {
    pub fn real() -> Nxm {
        Nxm { register: Some(Box::new(crate::app::register_nxm)), registered: Box::new(crate::app::nxm_registered), owner: Box::new(crate::app::nxm_owner) }
    }

    pub fn none() -> Nxm {
        Nxm { register: None, registered: Box::new(|| false), owner: Box::new(String::new) }
    }
}

/// A controller call that the window starts. The tests record these instead of running them.
#[derive(Clone, Debug, PartialEq)]
pub enum Act {
    Launch(Option<String>),
    CheckNow,
    AskAi(String),
    ApplyAi(Value),
    SetupAi,
    FindInVideo(String),
    UpdateApp(Option<Value>),
    UpdateMod(String),
    AutoSort,
    AddFramework(String),
    CheckKey,
    HandleNxm(String),
    Thumbs(u64, Vec<String>),
    AddLinks(String),
    AddLink(Value),
    Reopen(Value),
    Apply(Value, Value),
    CompletePack(String),
    Redeploy,
    /// A Nexus search (request number, page); only recorded, the window runs it itself.
    Browse(u64, i64),
}

/// The parts of the store the window draws, copied when the store is free (a copy into the game holds it).
#[derive(Clone)]
pub struct View {
    pub settings: Settings,
    pub mods: Vec<Mod>,
    pub packs: Vec<(String, Vec<(String, bool)>)>,
    pub pack: String,
    pub pack_info: Map<String, Value>,
    pub doctor: Map<String, Value>,
    pub root: PathBuf,
    pub staging: PathBuf,
    pub game: bool,
}

impl View {
    fn of(st: &Store) -> View {
        View {
            settings: st.settings.clone(),
            mods: st.mods.clone(),
            packs: st.packs.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            pack: st.pack.clone(),
            pack_info: st.pack_info.clone(),
            doctor: st.doctor.clone(),
            root: st.root.clone(),
            staging: st.staging.clone(),
            game: st.game().is_some(),
        }
    }

    pub fn has_pack(&self, name: &str) -> bool {
        self.packs.iter().any(|(n, _)| n == name)
    }

    pub fn pack(&self, name: &str) -> Option<&Vec<(String, bool)>> {
        self.packs.iter().find(|(n, _)| n == name).map(|(_, p)| p)
    }
}

/// A Browse install button: free, already in the mod list, or just added.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Mark {
    Install,
    Installed,
    Added,
}

pub struct Browse {
    pub req: u64,
    pub page: i64,
    pub total: i64,
    pub per: i64,
    pub kind: String,
    pub q: String,
    pub sort: usize,
    pub view: usize,
    pub adult: bool,
    /// (name, count, ticked), sorted by name.
    pub cats: Vec<(String, i64, bool)>,
    pub mods: Vec<Value>,
    pub marks: Vec<(String, Mark)>,
    pub pack_names: Vec<(String, String)>,
    pub thumbs: Vec<(String, PathBuf)>,
    pub info: (String, egui::Color32),
    pub soon: Option<Instant>,
}

pub const VIEWS: [i64; 3] = [20, 50, 100];

/// One picked file in the mod pack dialog: (file id, name, why it may not work).
pub type PackFile = (i64, String, String);

pub enum Dialog {
    Add { q: String },
    SaveName { text: String },
    Merge { names: Vec<String>, picked: Vec<bool>, new: String },
    PackChoice { link: Value, name: String, optional: Vec<PackFile>, old: Vec<PackFile>, picks: Vec<(i64, bool)> },
    Outdated { link: Value, name: String, why: String },
}

/// What the download banner button does.
#[derive(Clone, Debug, PartialEq)]
pub enum BannerBtn {
    Reopen(Value),
    Settings,
}

/// What the window needs; `Opts::real` for the exe.
pub struct Opts {
    pub server: Option<OwnerListener>,
    pub first_url: Option<String>,
    /// A secret: never print or log it.
    pub token: Vec<u8>,
    pub nxm: Nxm,
    pub ask: Rc<dyn Ask>,
    pub deps: Deps,
    pub workers: usize,
    pub watch_game: bool,
    pub find_game: fn() -> String,
    /// What a link from the pipe starts; None = the controller's handle_nxm in the background.
    pub on_nxm: Option<OnLink>,
}

/// What a link taken from the pipe starts.
pub type OnLink = Box<dyn Fn(&str) + Send + Sync>;

/// A download's id as the Downloads table keys it.
fn job_key(j: &Value) -> String {
    j["id"].to_string()
}

/// One row of the mod list.
#[derive(Clone, Debug, PartialEq)]
pub struct ModRow {
    pub id: String,
    pub on: bool,
    pub name: String,
    pub sub: String,
    pub version: String,
    pub kind: String,
    pub src: String,
}

/// One card of the My mod packs page.
#[derive(Clone, Debug, PartialEq)]
pub struct PackCard {
    pub name: String,
    pub avatar: String,
    pub chip: &'static str,
    pub in_use: bool,
    pub sub: String,
    pub missing: usize,
    pub url: Option<String>,
    pub open: bool,
    pub lines: Vec<String>,
}

pub struct Window {
    pub ctl: Arc<Controller>,
    pub v: View,
    stale: bool,
    tx: Sender<(String, Value)>,
    rx: Receiver<(String, Value)>,
    repaint: Arc<OnceLock<egui::Context>>,
    pub ask: Rc<dyn Ask>,
    pub nxm: Nxm,
    find_game: fn() -> String,
    pub serve: Option<Arc<Serve>>,
    /// Record mode (tests): the calls the window would start.
    pub acts: Option<Vec<Act>>,
    pub page: Page,
    pub status: String,
    pub dialogs: Vec<Dialog>,
    pub copied: Option<String>,
    pub raise: bool,
    pub closing: bool,
    pub closed: bool,
    pub theme_name: String,
    applied_theme: String,
    // header
    pub pack_pick: String,
    pub game_running: bool,
    pub banner_hidden: bool,
    // Browse
    pub browse: Browse,
    // Mods
    pub search: String,
    pub mods_show: &'static str,
    pub sel: Vec<String>,
    anchor: Option<String>,
    // My mod packs
    pub packs_q: String,
    pub packs_show: &'static str,
    pub pack_open: Vec<String>,
    // Add mods
    pub links: String,
    // Find mod from video
    pub video: String,
    pub video_info: (String, egui::Color32),
    /// (url, name, found by, endorsements)
    pub found: Vec<(String, String, String, String)>,
    pub found_sel: Vec<String>,
    // Downloads
    pub job_sel: Option<String>,
    pub log: Vec<String>,
    was_busy: bool,
    asked_nxm: bool,
    pub dl_banner: Option<(String, String, BannerBtn)>,
    // Crash doctor
    pub problems: Vec<Value>,
    pub prob_sel: Option<String>,
    pub fix_choice: usize,
    pub hist_sel: Option<usize>,
    // AI helpers
    pub ai_info: (String, egui::Color32),
    pub bug_text: String,
    // Settings
    pub game_var: String,
    pub key_var: String,
    pub show_key: bool,
    pub key_info: (String, egui::Color32),
    pub nxm_state: (bool, String),
    pub update_busy: bool,
}

fn mutex_try<T>(m: &Mutex<T>) -> Option<MutexGuard<'_, T>> {
    match m.try_lock() {
        Ok(g) => Some(g),
        Err(TryLockError::Poisoned(p)) => Some(p.into_inner()),
        Err(TryLockError::WouldBlock) => None,
    }
}

fn s(v: &Value) -> &str {
    v.as_str().unwrap_or("")
}

fn avatar(name: &str) -> String {
    let a: String = name.split_whitespace().take(2).filter_map(|w| w.chars().next()).collect::<String>().to_uppercase();
    if a.is_empty() { "M".into() } else { a }
}

fn first_upper(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

impl Window {
    pub fn new(mut store: Store, o: Opts) -> Window {
        if store.game().is_none() {
            let found = (o.find_game)();
            if !found.is_empty() {
                store.settings.game_dir = found;
            }
            let _ = store.save();
        }
        let (tx, rx) = channel::<(String, Value)>();
        let repaint: Arc<OnceLock<egui::Context>> = Arc::default();
        let (t, r) = (tx.clone(), repaint.clone());
        let notify = Arc::new(move |k: &str, p: Value| {
            let _ = t.send((k.to_string(), p));
            if let Some(c) = r.get() {
                c.request_repaint();
            }
        });
        let ctl = Controller::new(store, notify, o.workers, o.watch_game, o.deps);
        let v = View::of(&lock(&ctl.store));
        let serve = o.server.map(|server| {
            let c = ctl.clone();
            let nxm = o.on_nxm.unwrap_or_else(|| {
                Box::new(move |url: &str| {
                    let url = url.to_string();
                    c.bg(move |c| c.handle_nxm(&url));
                })
            });
            let s = Arc::new(Serve { token: o.token, ctl: ctl.clone(), events: tx.clone(), nxm, timeout: Mutex::new(Duration::from_secs(5)), busy: AtomicUsize::new(0) });
            let run = s.clone();
            thread::spawn(move || run.run(server)); // take links at once, before the window is built
            s
        });
        let st = &v.settings;
        let pick = if v.has_pack(&v.pack) { v.pack.clone() } else { CURRENT.to_string() };
        let mut w = Window {
            ctl,
            stale: false,
            tx,
            rx,
            repaint,
            ask: o.ask,
            nxm: o.nxm,
            find_game: o.find_game,
            serve,
            acts: None,
            page: if v.game { Page::Browse } else { Page::Settings },
            status: "Ready.".into(),
            dialogs: vec![],
            copied: None,
            raise: false,
            closing: false,
            closed: false,
            theme_name: st.theme.clone(),
            applied_theme: String::new(),
            pack_pick: pick,
            game_running: false,
            banner_hidden: false,
            browse: Browse {
                req: 0,
                page: 0,
                total: 0,
                per: 20,
                kind: "mods".into(),
                q: String::new(),
                sort: 0,
                view: 0,
                adult: false,
                cats: vec![],
                mods: vec![],
                marks: vec![],
                pack_names: vec![],
                thumbs: vec![],
                info: ("Loading mods from Nexus…".into(), theme::THEMES[0].dim),
                soon: None,
            },
            search: String::new(),
            mods_show: "All",
            sel: vec![],
            anchor: None,
            packs_q: String::new(),
            packs_show: "all",
            pack_open: vec![],
            links: String::new(),
            video: String::new(),
            video_info: (String::new(), theme::THEMES[0].dim),
            found: vec![],
            found_sel: vec![],
            job_sel: None,
            log: vec![],
            was_busy: false,
            asked_nxm: false,
            dl_banner: None,
            problems: vec![],
            prob_sel: None,
            fix_choice: 0,
            hist_sel: None,
            ai_info: if local_ai::ready() {
                ("The local AI is ready to use.".into(), theme::THEMES[0].dim)
            } else {
                ("The local AI files are missing. Click 'Check local AI' to download them (about 1.3 GB).".into(), theme::THEMES[0].dim)
            },
            bug_text: String::new(),
            game_var: st.game_dir.clone(),
            key_var: st.api_key.clone(),
            show_key: false,
            key_info: ("Free and Premium accounts both have a personal API key.".into(), theme::THEMES[0].dim),
            nxm_state: (false, String::new()),
            update_busy: false,
            v,
        };
        w.refresh_settings();
        w.refresh_mods();
        if let Some(url) = o.first_url {
            w.act(Act::HandleNxm(url)); // a bad link (e.g. another game) becomes a message, not a crash
        }
        w.page_changed();
        w
    }

    /// The exe's window options: the controller with its real parts, the real message boxes and finder.
    pub fn opts(server: OwnerListener, first_url: Option<String>, token: String, nxm: Nxm) -> Opts {
        Opts {
            server: Some(server),
            first_url,
            token: token.into_bytes(),
            nxm,
            ask: Rc::new(NativeAsk),
            deps: Deps::default(),
            workers: 3,
            watch_game: true,
            find_game: shared::find_game,
            on_nxm: None,
        }
    }

    pub fn title(&self) -> String {
        format!("Cyberpunk 2077 AI Mod Manager {VERSION} (manual click)")
    }

    pub fn theme(&self) -> &'static Theme {
        theme::theme(&self.theme_name)
    }

    fn st(&self) -> MutexGuard<'_, Store> {
        lock(&self.ctl.store)
    }

    /// Copy the store for drawing; waits for a running copy (actions do the same in Python).
    pub fn sync(&mut self) {
        let v = View::of(&self.st());
        self.v = v;
        self.stale = false;
    }

    /// Copy the store when it is free; else try again next frame (the window does not wait).
    fn try_sync(&mut self) {
        let fresh = mutex_try(&self.ctl.store).map(|st| View::of(&st));
        match fresh {
            Some(v) => {
                self.v = v;
                self.stale = false;
            }
            None => self.stale = true,
        }
    }

    pub fn act(&mut self, a: Act) -> usize {
        if let Some(log) = &mut self.acts {
            let n = usize::from(matches!(a, Act::CompletePack(_)));
            log.push(a);
            return n;
        }
        let c = &self.ctl;
        match a {
            Act::Launch(p) => c.bg(move |c| c.launch(p.as_deref())),
            Act::CheckNow => c.bg(|c| c.check_now().map(|_| ())),
            Act::AskAi(t) => c.bg(move |c| {
                c.ask_ai(&t);
                Ok(())
            }),
            Act::ApplyAi(p) => c.bg(move |c| c.apply_ai(&p)),
            Act::SetupAi => c.bg(|c| {
                c.setup_ai();
                Ok(())
            }),
            Act::FindInVideo(u) => c.bg(move |c| {
                c.find_in_video(&u);
                Ok(())
            }),
            Act::UpdateApp(r) => c.bg(move |c| {
                c.update_app(r);
                Ok(())
            }),
            Act::UpdateMod(m) => c.bg(move |c| c.update_mod(&m)),
            Act::AutoSort => c.bg(|c| c.auto_sort().map(|_| ())),
            Act::AddFramework(f) => c.bg(move |c| c.add_framework(&f)),
            Act::CheckKey => c.bg(|c| {
                c.check_key();
                Ok(())
            }),
            Act::HandleNxm(u) => c.bg(move |c| c.handle_nxm(&u)),
            Act::Thumbs(req, urls) => {
                let (tx, dir) = (self.tx.clone(), self.v.root.join("thumbs"));
                c.bg(move |_| {
                    let paths = downloads::fetch::thumbnails(&urls, &dir, 160, 90);
                    let paths: Map<String, Value> = paths.into_iter().map(|(u, p)| (u, json!(p.to_string_lossy()))).collect();
                    let _ = tx.send(("thumbs".into(), json!({"req": req, "paths": paths})));
                    Ok(())
                })
            }
            Act::AddLinks(t) => c.add_links(&t),
            Act::AddLink(l) => c.add_link(l),
            Act::Reopen(j) => c.reopen(&j),
            Act::Apply(p, f) => {
                if let Err(e) = c.apply(&p, &f) {
                    self.ask.error("Mod manager", &e);
                }
            }
            Act::CompletePack(n) => return c.complete_pack(&n),
            Act::Redeploy => c.redeploy(false, false),
            Act::Browse(..) => {}
        }
        0
    }

    pub fn say(&mut self, text: &str) {
        self.status = text.to_string();
    }

    fn safe(&self, r: Result<(), String>) {
        if let Err(e) = r {
            self.ask.error("Mod manager", &e);
        }
    }

    fn jobs(&self) -> Vec<Value> {
        lock(&self.ctl.jobs).clone()
    }

    // ---------- pages ----------
    pub fn tab_text(&self, p: Page) -> String {
        let active = self.jobs().iter().filter(|j| ACTIVE.contains(&s(&j["state"]))).count();
        match p {
            Page::Mods => format!("Mods ({})", self.v.mods.len()),
            Page::Downloads if active > 0 => format!("Downloads ({active})"),
            Page::Crash if !self.problems.is_empty() => format!("Crash doctor ({})", self.problems.len()),
            p => p.name().to_string(),
        }
    }

    pub fn badge(&self, p: Page) -> usize {
        match p {
            Page::Downloads => self.jobs().iter().filter(|j| ACTIVE.contains(&s(&j["state"]))).count(),
            Page::Crash => self.problems.len(),
            _ => 0,
        }
    }

    pub fn page_title(&self) -> String {
        self.tab_text(self.page)
    }

    pub fn select(&mut self, p: Page) {
        self.page = p;
        self.page_changed();
    }

    /// A rail click: Add mods opens the dialog, any other item shows its page.
    pub fn rail_click(&mut self, p: Page) {
        if p == Page::Add { self.add_dialog() } else { self.select(p) }
    }

    fn page_changed(&mut self) {
        if self.page == Page::Browse && self.browse.req == 0 {
            self.browse_go(0);
        }
    }

    // ---------- Browse ----------
    pub fn browse_kind(&mut self, kind: &str) {
        // Mods <-> Mod packs: each has its own categories, so the list is built again on the next load
        self.browse.kind = kind.into();
        self.browse.cats.clear();
        self.browse_go(0);
    }

    pub fn browse_hint(&self) -> &'static str {
        if self.browse.kind == "packs" { "Search mod packs…" } else { "Search mods…" }
    }

    /// Search while the user types, once they pause.
    pub fn browse_soon(&mut self) {
        self.browse.soon = Some(Instant::now() + Duration::from_millis(450));
    }

    pub fn browse_go(&mut self, page: i64) {
        let b = &mut self.browse;
        b.soon = None;
        b.req += 1;
        let (req, words, kind) = (b.req, b.q.trim().to_string(), b.kind.clone());
        let cats: Vec<String> = b.cats.iter().filter(|c| c.2).map(|c| c.0.clone()).collect();
        let (sort, page, adult, per) = (nexus::SORTS[b.sort].1, page.max(0), b.adult, VIEWS[b.view]);
        b.info = ("Searching…".into(), theme::theme(&self.theme_name).dim);
        if let Some(log) = &mut self.acts {
            return log.push(Act::Browse(req, page));
        }
        let tx = self.tx.clone();
        thread::spawn(move || {
            let c: Vec<&str> = cats.iter().map(String::as_str).collect();
            let r = if kind == "packs" { nexus::browse_packs(&words, &c, sort, page, adult, per) } else { nexus::browse(&words, &c, sort, page, adult, per) };
            let p = match r {
                Ok(Value::Object(mut o)) => {
                    o.extend([("req".into(), json!(req)), ("page".into(), json!(page)), ("per".into(), json!(per)), ("kind".into(), json!(kind))]);
                    Value::Object(o)
                }
                Ok(_) => json!({"req": req, "error": "no answer"}),
                Err(e) => json!({"req": req, "error": e}), // network boundary: shown in the tab
            };
            let _ = tx.send(("browse".into(), p));
        });
    }

    /// The page buttons: Some(page index) or None for "…" (1 2 3 … 965).
    pub fn pager(&self) -> Vec<Option<i64>> {
        let b = &self.browse;
        let pages = self.pages();
        let mut ns: Vec<i64> = vec![0, pages - 1];
        ns.extend((b.page - 1).max(0)..(b.page + 2).min(pages));
        ns.sort_unstable();
        ns.dedup();
        let (mut out, mut last) = (vec![], -1);
        for n in ns {
            if n - last > 1 {
                out.push(None);
            }
            out.push(Some(n));
            last = n;
        }
        out
    }

    pub fn pages(&self) -> i64 {
        (self.browse.total + self.browse.per - 1).div_euclid(self.browse.per.max(1)).max(1)
    }

    pub fn show_browse(&mut self, p: &Value) {
        if p["req"].as_u64() != Some(self.browse.req) {
            return; // an older search finished after a newer one started
        }
        if let Some(e) = p.get("error") {
            self.browse.info = (format!("Nexus Mods did not answer: {}", e.as_str().map_or_else(|| e.to_string(), String::from)), BAD);
            return;
        }
        let cats = p["categories"].as_object().cloned().unwrap_or_default();
        if self.browse.cats.is_empty() && !cats.is_empty() {
            // counts come first, from an unfiltered load: the whole game
            let mut c: Vec<(String, i64, bool)> = cats.iter().map(|(k, n)| (k.clone(), n.as_i64().unwrap_or(0), false)).collect();
            c.sort_by(|a, b| a.0.cmp(&b.0));
            self.browse.cats = c;
        }
        let (total, per) = (p["total"].as_i64().unwrap_or(0), p["per"].as_i64().unwrap_or(20));
        self.browse.page = p["page"].as_i64().unwrap_or(0);
        self.browse.total = total;
        self.browse.per = per;
        let what = if s(&p["kind"]) == "packs" { "mod packs" } else { "mods" };
        self.browse.info = if total > 0 {
            (format!("{} {what}", util::commas(total)), self.theme().fg)
        } else {
            (format!("No {} matches. Try fewer words or other categories.", &what[..what.len() - 1]), WARN)
        };
        let mods = p["mods"].as_array().cloned().unwrap_or_default();
        self.browse.marks = mods.iter().map(|m| (m["id"].to_string(), Mark::Install)).collect();
        self.browse.pack_names = mods.iter().filter(|m| m.get("slug").is_some()).map(|m| (m["id"].to_string(), s(&m["name"]).trim().to_string())).collect();
        self.browse.thumbs.clear();
        self.browse.mods = mods;
        self.browse_marks();
        let urls: Vec<String> = self.browse.mods.iter().map(|m| s(&m["thumb"]).to_string()).filter(|u| !u.is_empty()).collect();
        if !urls.is_empty() {
            self.act(Act::Thumbs(self.browse.req, urls)); // thumbnails load in the background
        }
    }

    pub fn show_thumbs(&mut self, p: &Value) {
        if p["req"].as_u64() != Some(self.browse.req) {
            return;
        }
        for (url, path) in p["paths"].as_object().into_iter().flatten() {
            self.browse.thumbs.push((url.clone(), PathBuf::from(s(path))));
        }
    }

    /// Install buttons say "Installed" for mods in the mod list.
    fn browse_marks(&mut self) {
        let mut ids: Vec<String> = self.v.mods.iter().filter_map(|m| m.source.get("mod_id")).map(Value::to_string).collect();
        let repos: Vec<String> = self.v.mods.iter().map(|m| s(&m.source["repo"]).to_lowercase()).collect();
        ids.extend(shared::FRAMEWORKS.iter().filter(|f| repos.contains(&f.1.to_lowercase())).map(|f| f.2.to_string()));
        ids.extend(self.browse.pack_names.iter().filter(|(_, n)| self.v.has_pack(n)).map(|(id, _)| id.clone()));
        for (id, m) in &mut self.browse.marks {
            if ids.contains(id) {
                *m = Mark::Installed;
            }
        }
    }

    pub fn mark(&self, id: &Value) -> Mark {
        let id = id.to_string();
        self.browse.marks.iter().find(|(k, _)| *k == id).map_or(Mark::Install, |(_, m)| *m)
    }

    pub fn browse_page_url(m: &Value) -> String {
        if m.get("slug").is_some() {
            format!("https://www.nexusmods.com/games/{}/collections/{}", nexus::GAME, s(&m["slug"]))
        } else {
            format!("https://www.nexusmods.com/{}/mods/{}", nexus::GAME, m["id"])
        }
    }

    pub fn browse_install(&mut self, m: &Value) {
        let name = s(&m["name"]).to_string();
        let msg = if m.get("slug").is_some() {
            self.act(Act::AddLinks(Self::browse_page_url(m)));
            format!(
                "Added the mod pack {name} ({} mods). I download and set up each mod, and save the pack so you can pick it next to 'Launch game'. If it has optional mods, I ask which ones you want first.",
                util::commas(m["mod_count"].as_i64().unwrap_or(0))
            )
        } else {
            self.act(Act::AddLinks(Self::browse_page_url(m)));
            format!("Added {name}. I download and set it up with the mods it needs. Follow it in the Downloads tab.")
        };
        let id = m["id"].to_string();
        if let Some(e) = self.browse.marks.iter_mut().find(|(k, _)| *k == id) {
            e.1 = Mark::Added;
        }
        self.say(&msg);
    }

    pub fn open_url(&self, url: &str) {
        let _ = shell_open(Path::new(url), &[], None);
    }

    // ---------- Mods ----------
    pub fn mod_rows(&self) -> Vec<ModRow> {
        let q = self.search.trim().to_lowercase();
        self.v
            .mods
            .iter()
            .filter(|m| !(!q.is_empty() && !m.name.to_lowercase().contains(&q) || self.mods_show == "On" && !m.enabled || self.mods_show == "Off" && m.enabled))
            .map(|m| {
                let src = &m.source;
                let src = match s(&src["type"]) {
                    "nexus" => format!("Nexus #{}", src["mod_id"]),
                    "github" => format!("GitHub {}", s(&src["repo"])),
                    "file" => "Local file".into(),
                    _ => "Web link".into(),
                };
                let kind = kind_of(m);
                ModRow {
                    id: m.id.clone(),
                    on: m.enabled,
                    name: m.name.clone(),
                    sub: format!("{kind}  ·  {src}"),
                    version: if m.version.is_empty() { "–".into() } else { m.version.clone() },
                    kind,
                    src,
                }
            })
            .collect()
    }

    /// The selected mods, in list order.
    pub fn selected(&self) -> Vec<String> {
        self.mod_rows().into_iter().map(|r| r.id).filter(|id| self.sel.contains(id)).collect()
    }

    /// A row click: plain picks one, Ctrl adds or drops one, Shift picks a range.
    pub fn click_row(&mut self, id: &str, ctrl: bool, shift: bool) {
        if shift && let Some(a) = self.anchor.clone() {
            let ids: Vec<String> = self.mod_rows().into_iter().map(|r| r.id).collect();
            let (i, j) = (ids.iter().position(|x| *x == a), ids.iter().position(|x| x == id));
            if let (Some(i), Some(j)) = (i, j) {
                self.sel = ids[i.min(j)..=i.max(j)].to_vec();
                return;
            }
        }
        if ctrl {
            if let Some(i) = self.sel.iter().position(|x| x == id) {
                self.sel.remove(i);
            } else {
                self.sel.push(id.into());
            }
        } else {
            self.sel = vec![id.into()];
        }
        self.anchor = Some(id.into());
    }

    pub fn select_all(&mut self) {
        self.sel = self.mod_rows().into_iter().map(|r| r.id).collect();
    }

    pub fn mods_empty(&self) -> Option<(&'static str, &'static str, bool)> {
        if !self.mod_rows().is_empty() {
            return None;
        }
        let any = !self.v.mods.is_empty();
        Some(if any {
            ("No match", "No mod matches your search or filter.", false)
        } else {
            ("Ready to mod Night City?", "No mods yet. Find mods on Nexus, paste links, or start with the core frameworks.", true)
        })
    }

    pub fn pack_sub(&self) -> String {
        let on = self.v.mods.iter().filter(|m| m.enabled).count();
        format!("{} mods  ·  {on} on  ·  lower in the list wins when two mods change the same file", self.v.mods.len())
    }

    pub fn pack_avatar(&self) -> String {
        avatar(&self.pack_pick)
    }

    pub fn refresh_mods(&mut self) {
        self.sync();
        let ids: Vec<String> = self.mod_rows().into_iter().map(|r| r.id).collect();
        self.sel.retain(|i| ids.contains(i));
        self.refresh_packs();
        self.browse_marks();
    }

    pub fn redeploy(&mut self) {
        if self.v.game {
            self.say("Setting up your mods in the game folder…");
            self.act(Act::Redeploy);
        } else {
            self.say("Set the game folder in Settings first.");
        }
    }

    fn set_on(&mut self, ids: &[String], on: bool) {
        let r = self.ctl.set_enabled(ids, on);
        self.safe(r);
        self.sync();
        self.redeploy();
    }

    pub fn set_one(&mut self, id: &str, on: bool) {
        self.set_on(&[id.to_string()], on);
    }

    pub fn toggle(&mut self) {
        let ids = self.selected();
        if !ids.is_empty() {
            let all_on = ids.iter().all(|i| self.v.mods.iter().any(|m| m.id == *i && m.enabled));
            self.set_on(&ids, !all_on);
        }
    }

    pub fn move_sel(&mut self, d: i64) {
        if let Some(id) = self.selected().first().cloned() {
            let r = self.ctl.move_mod(&id, d);
            self.safe(r);
            self.sync();
            self.redeploy();
        }
    }

    pub fn auto_sort(&mut self) {
        self.say("Finding the best load order…");
        self.act(Act::AutoSort);
    }

    pub fn update(&mut self) {
        let ids = self.selected();
        for id in &ids {
            self.act(Act::UpdateMod(id.clone()));
        }
        if !ids.is_empty() {
            self.say("Looking for newer versions…");
        }
    }

    pub fn remove(&mut self) {
        let ids = self.selected();
        if ids.is_empty() {
            return;
        }
        let what = if ids.len() == 1 {
            self.v.mods.iter().find(|m| m.id == ids[0]).map(|m| m.name.clone()).unwrap_or_default()
        } else {
            format!("these {} mods", ids.len())
        };
        if self.ask.yes_no("Remove", &format!("Remove {what} from the game and from this app?"), false) {
            for id in &ids {
                let r = self.ctl.remove(id);
                self.safe(r);
            }
            self.refresh_mods();
            self.redeploy();
        }
    }

    pub fn open_folder(&self) {
        if let Some(id) = self.selected().first() {
            let _ = shell_open(&self.v.staging.join(id), &[], None);
        }
    }

    pub fn install_core(&mut self) {
        for fw in CORE {
            self.act(Act::AddFramework(fw.into()));
        }
        self.say("Installing the core frameworks from GitHub…");
        self.select(Page::Downloads);
    }

    // ---------- mod packs ----------
    pub fn pack_values(&self) -> Vec<String> {
        std::iter::once(CURRENT.to_string()).chain(self.v.packs.iter().map(|(n, _)| n.clone())).collect()
    }

    pub fn picked_pack(&self) -> Option<String> {
        self.v.has_pack(&self.pack_pick).then(|| self.pack_pick.clone())
    }

    pub fn refresh_packs(&mut self) {
        if self.pack_pick != CURRENT && !self.v.has_pack(&self.pack_pick) {
            self.pack_pick = CURRENT.into(); // its pack was deleted
        }
    }

    pub fn save_pack(&mut self) {
        let text = self.picked_pack().unwrap_or_else(|| self.v.pack.clone());
        self.dialogs.push(Dialog::SaveName { text });
    }

    /// OK in the "Name of the mod pack" dialog.
    pub fn save_pack_ok(&mut self) {
        let Some(Dialog::SaveName { text }) = self.dialogs.pop() else { return };
        let name = text.trim().to_string();
        if name.is_empty() {
            return;
        }
        if self.v.has_pack(&name) && !self.ask.yes_no("Save mod pack", &format!("Replace the mod pack '{name}'?"), false) {
            return;
        }
        let r = self.ctl.save_pack(&name);
        self.safe(r);
        self.sync();
        self.refresh_packs();
        self.pack_pick = name.clone();
        self.say(&format!("Saved the mod pack '{name}'. Pick it next to 'Launch game' to launch it."));
    }

    pub fn use_pack(&mut self, pack: Option<&str>) {
        let Some(pack) = pack.map(String::from).or_else(|| self.picked_pack()) else {
            return self.ask.info("Mod packs", "Pick a mod pack next to 'Launch game' first.");
        };
        match self.ctl.use_pack(&pack) {
            Ok(msg) => self.say(&msg),
            Err(e) => self.ask.error("Mod manager", &e),
        }
        self.sync();
        self.redeploy();
    }

    pub fn complete_pack(&mut self, pack: &str) {
        let n = self.act(Act::CompletePack(pack.into()));
        if n == 0 {
            return self.say(&format!("I cannot download the missing mods of '{pack}': they did not come from Nexus."));
        }
        self.say(&format!("Installing {n} mod(s) of '{pack}'. When they are done, I turn the pack on in the game. Follow them in the Downloads tab."));
        self.select(Page::Downloads);
    }

    pub fn delete_pack(&mut self, pack: Option<&str>) {
        let pack = pack.map(String::from).or_else(|| self.picked_pack());
        if let Some(pack) = pack
            && self.ask.yes_no("Mod packs", &format!("Forget the mod pack '{pack}'? Its mods stay installed."), false)
        {
            let r = self.ctl.delete_pack(&pack);
            self.safe(r);
            self.sync();
            self.refresh_packs(); // resets the pick if it was this pack
        }
    }

    pub fn merge_packs(&mut self) {
        let names: Vec<String> = self.v.packs.iter().map(|(n, _)| n.clone()).collect();
        if names.len() < 2 {
            return self.ask.info("Merge mod packs", "Save or add two or more mod packs first. Each Nexus collection you add is a mod pack.");
        }
        let picked = vec![false; names.len()];
        self.dialogs.push(Dialog::Merge { names, picked, new: "Merged pack".into() });
    }

    pub fn merge_ok(&mut self) {
        let Some(Dialog::Merge { names, picked, new }) = self.dialogs.last() else { return };
        let chosen: Vec<String> = names.iter().zip(picked).filter(|(_, p)| **p).map(|(n, _)| n.clone()).collect();
        let new = new.trim().to_string();
        match self.ctl.merge_packs(&chosen, &new) {
            Err(e) => self.ask.info("Merge mod packs", &e),
            Ok(n) => {
                self.dialogs.pop();
                self.sync();
                self.refresh_packs();
                self.pack_pick = new.clone();
                self.say(&format!("Made the mod pack '{new}' with {n} mods. Launch it, or pick 'Use the picked pack' in the ••• menu to see it."));
            }
        }
    }

    pub fn pack_cards(&self) -> Vec<PackCard> {
        let q = self.packs_q.trim().to_lowercase();
        self.v
            .packs
            .iter()
            .filter(|(n, _)| n.to_lowercase().contains(&q) && (self.packs_show == "all" || (self.packs_show == "nexus") == self.v.pack_info.contains_key(n.as_str())))
            .map(|(name, pack)| {
                let info = self.v.pack_info.get(name);
                let have = |mid: &str| self.v.mods.iter().find(|m| m.id == mid);
                let installed = pack.iter().filter(|(mid, _)| have(mid).is_some()).count();
                let on = pack.iter().filter(|(_, e)| *e).count();
                let missing = pack.len() - installed;
                let mut sub = format!("{} mods  ·  {installed} installed  ·  {on} on", pack.len());
                if missing > 0 {
                    sub += &format!("  ·  {missing} not installed yet");
                }
                let open = self.pack_open.contains(name);
                let lines = if open {
                    let names = info.and_then(|i| i.get("names"));
                    pack.iter()
                        .map(|(mid, e)| {
                            let n = have(mid).map(|m| m.name.clone()).or_else(|| names.and_then(|n| n.get(mid)).map(|n| s(n).to_string())).unwrap_or_else(|| mid.clone());
                            format!("{}  {n}{}", if *e { "●" } else { "○" }, if have(mid).is_some() { "" } else { "   (not installed yet)" })
                        })
                        .collect()
                } else {
                    vec![]
                };
                PackCard {
                    name: name.clone(),
                    avatar: avatar(name),
                    chip: if info.is_some() { "Nexus mod pack" } else { "My own pack" },
                    in_use: self.v.pack == *name,
                    sub,
                    missing,
                    url: info.map(|i| s(&i["url"]).to_string()),
                    open,
                    lines,
                }
            })
            .collect()
    }

    pub fn packs_empty(&self) -> &'static str {
        if self.v.packs.is_empty() {
            "No mod packs yet. Install one from 'Find mod packs', or save your mod list as a pack."
        } else {
            "No mod pack matches."
        }
    }

    pub fn toggle_pack_open(&mut self, name: &str) {
        match self.pack_open.iter().position(|n| n == name) {
            Some(i) => {
                self.pack_open.remove(i);
            }
            None => self.pack_open.push(name.into()),
        }
    }

    pub fn find_packs(&mut self) {
        self.select(Page::Browse);
        if self.browse.kind != "packs" {
            self.browse_kind("packs");
        }
    }

    pub fn launch_pack(&mut self, name: &str) {
        self.pack_pick = name.into();
        self.launch();
    }

    pub fn launch(&mut self) {
        self.sync();
        if !self.v.game {
            self.ask.info("Mod manager", "Pick the Cyberpunk 2077 folder in Settings first.");
            return self.select(Page::Settings);
        }
        let busy = self.jobs().iter().filter(|j| matches!(s(&j["state"]), "queued" | "running")).count();
        if busy > 0 && !self.ask.yes_no("Downloads running", &format!("{busy} download(s) are still running. Start the game anyway?"), false) {
            return;
        }
        let pack = self.picked_pack();
        self.act(Act::Launch(pack.clone())); // copying mods can take a while; errors arrive as messages
        self.say(&match pack {
            Some(p) => format!("Starting the game with the mod pack '{p}'. I watch it for crashes."),
            None => "Starting the game. I watch it for crashes.".into(),
        });
    }

    // ---------- Add mods ----------
    pub fn add_dialog(&mut self) {
        self.dialogs.push(Dialog::Add { q: String::new() });
    }

    /// A pick in the Add mods dialog (index into ADD_WAYS); it closes the dialog first.
    pub fn add_go(&mut self, i: usize) {
        if matches!(self.dialogs.last(), Some(Dialog::Add { .. })) {
            self.dialogs.pop();
        }
        match i {
            0 => self.select(Page::Browse),
            1 => self.select(Page::Add),
            2 => self.add_files(),
            3 => self.install_core(),
            _ => self.select(Page::Find),
        }
    }

    /// Enter in the Add mods dialog searches Nexus.
    pub fn add_search(&mut self) {
        if let Some(Dialog::Add { q }) = self.dialogs.pop() {
            self.browse.q = q.trim().to_string();
            self.browse.soon = None;
            self.select(Page::Browse);
            self.browse_go(0);
        }
    }

    pub fn links_text(&self) -> &str {
        &self.links
    }

    pub fn link_count(&self) -> String {
        let n = self.links.lines().filter(|l| !l.trim().is_empty() && !l.trim().starts_with('#')).count();
        if n > 0 { format!("{n} link(s) ready") } else { String::new() }
    }

    pub fn paste(&mut self) {
        let Some(text) = util::clipboard_text() else { return self.say("The clipboard is empty.") };
        let sep = if self.links.trim().is_empty() { "" } else { "\n" };
        self.links = format!("{}{sep}{}", self.links, text.trim());
    }

    pub fn add_links(&mut self) {
        let text = self.links.clone();
        let n = text.lines().filter(|l| !l.trim().is_empty()).count();
        if n == 0 {
            return self.say("Paste at least one link first.");
        }
        self.act(Act::AddLinks(text));
        self.links.clear();
        self.say(&format!("Added {n} link(s). Follow them in the Downloads tab."));
        self.select(Page::Downloads);
    }

    pub fn add_files(&mut self) {
        let files = self.ask.open_files();
        for p in &files {
            self.act(Act::AddLinks(p.to_string_lossy().into_owned()));
        }
        if !files.is_empty() {
            self.say(&format!("Installing {} file(s)…", files.len()));
            self.select(Page::Downloads);
        }
    }

    // ---------- Find mod from video ----------
    pub fn find_video(&mut self) {
        let entry = self.video.trim().to_string();
        if entry.is_empty() {
            return self.say("Paste a video link first.");
        }
        self.video_info = ("Reading the video page…".into(), self.theme().dim);
        self.act(Act::FindInVideo(entry));
    }

    pub fn show_found(&mut self, p: &Value) {
        self.found.clear();
        self.found_sel.clear();
        if let Some(e) = p.get("error") {
            self.video_info = (format!("I could not read that page: {}", e.as_str().map_or_else(|| e.to_string(), String::from)), BAD);
            return;
        }
        let rows = p["rows"].as_array().cloned().unwrap_or_default();
        for r in &rows {
            let e = match &r["endorsements"] {
                Value::Null => String::new(),
                Value::Number(n) if n.as_i64() == Some(0) => String::new(),
                v => v.to_string(),
            };
            self.found.push((s(&r["url"]).into(), s(&r["name"]).into(), s(&r["why"]).into(), e));
        }
        let n = rows.len();
        let linked = rows.iter().filter(|r| s(&r["why"]) == "Linked in the video").count();
        let text = if n == 0 {
            "I found no mod. If you know the mod's name, paste the name.".to_string()
        } else if linked == n {
            format!("{n} mod(s) linked in the video.")
        } else {
            format!("{n} mod(s): {linked} linked in the video, {} found by name. Open a page to check a mod before you add it.", n - linked)
        };
        let title = s(&p["title"]);
        let head = if title.is_empty() { String::new() } else { format!("\"{title}\"   ") };
        self.video_info = (head + &text, if n > 0 { self.theme().fg } else { WARN });
    }

    pub fn add_found(&mut self) {
        let urls: Vec<String> = self.found.iter().map(|f| f.0.clone()).filter(|u| self.found_sel.contains(u)).collect();
        if urls.is_empty() {
            return self.say("Select one or more mods in the list first.");
        }
        self.act(Act::AddLinks(urls.join("\n")));
        self.say(&format!("Added {} mod(s). Follow them in the Downloads tab.", urls.len()));
        self.select(Page::Downloads);
    }

    pub fn open_found(&self) {
        if let Some(u) = self.found_sel.last() {
            self.open_url(u);
        }
    }

    // ---------- Downloads ----------
    /// (id, name, status, progress, state) of each download.
    pub fn job_rows(&self) -> Vec<(String, String, String, String, String)> {
        self.jobs().iter().map(|j| (job_key(j), s(&j["name"]).into(), s(&j["status"]).into(), s(&j["progress"]).into(), s(&j["state"]).into())).collect()
    }

    pub fn show_job(&mut self, _j: &Value) {
        self.refresh_progress();
    }

    /// (share done 0..1, "2 of 5 downloads finished") while downloads run.
    pub fn progress(&self) -> Option<(f32, String)> {
        let jobs = self.jobs();
        let active = jobs.iter().filter(|j| ACTIVE.contains(&s(&j["state"]))).count();
        let done = jobs.len() - active;
        (active > 0).then(|| (done as f32 / jobs.len() as f32, format!("{done} of {} downloads finished", jobs.len())))
    }

    pub fn refresh_progress(&mut self) {
        let jobs = self.jobs();
        let active = jobs.iter().filter(|j| ACTIVE.contains(&s(&j["state"]))).count();
        if active == 0 && self.was_busy {
            let failed = jobs.iter().filter(|j| j["state"] == "failed").count();
            let tail = if failed > 0 { format!(" {failed} failed: see the Downloads tab.") } else { String::new() };
            self.say(&format!("All downloads finished.{tail}"));
        }
        self.was_busy = active > 0;
        let waiting = jobs.iter().find(|j| j["state"] == "waiting").cloned();
        let needkey = jobs.iter().any(|j| j["state"] == "needkey");
        let auto = waiting.as_ref().is_some_and(|w| self.v.settings.auto_click && !w["manual"].as_bool().unwrap_or(false)); // the hidden browser clicks
        if waiting.is_some() && !auto && self.nxm.register.is_some() && !(self.nxm.registered)() && !self.asked_nxm {
            self.asked_nxm = true; // once per run: the user may want the other app to keep Nexus links
            if self.ask.yes_no(
                "Nexus download button",
                "Your 'Slow download' clicks go to another app (such as Vortex), not to this one, so nothing happens here.\n\nSend Nexus downloads to this app?",
                false,
            ) {
                self.register_nxm(); // also opens the waiting page again
            }
        }
        self.dl_banner = if let Some(w) = waiting {
            let left = jobs.iter().filter(|j| j["state"] == "waiting").count();
            let text = if auto {
                format!(
                    "I click 'Slow download' on Nexus for you in a hidden browser, with a {}-second pause between files. {left} file{} left.",
                    downloads::nexus_browser::GAP.as_secs(),
                    if left > 1 { "s" } else { "" }
                )
            } else {
                format!(
                    "Your browser shows the Nexus page for '{}'. Click 'Slow download' there and I do the rest. {left} click{} left.",
                    s(&w["name"]),
                    if left > 1 { "s" } else { "" }
                )
            };
            Some((text, "Open page again".into(), BannerBtn::Reopen(w)))
        } else if needkey {
            Some(("Nexus downloads wait for your API key. Add it in Settings and they continue by themselves.".into(), "Open Settings".into(), BannerBtn::Settings))
        } else {
            None
        };
    }

    pub fn banner_click(&mut self, b: BannerBtn) {
        match b {
            BannerBtn::Reopen(j) => {
                self.act(Act::Reopen(j));
            }
            BannerBtn::Settings => self.select(Page::Settings),
        }
    }

    fn job_sel(&mut self) -> Option<Value> {
        let j = self.job_sel.as_ref().and_then(|id| self.jobs().into_iter().find(|j| job_key(j) == *id));
        if j.is_none() {
            self.say("Select a download first.");
        }
        j
    }

    pub fn job_reopen(&mut self) {
        if let Some(j) = self.job_sel() {
            if j["src"]["type"] != "nexus" {
                return self.say("That download is not from Nexus.");
            }
            self.act(Act::Reopen(j));
        }
    }

    pub fn job_skip(&mut self, retry: bool) {
        if let Some(j) = self.job_sel() {
            let id = j["id"].as_u64().unwrap_or(0);
            if !(if retry { self.ctl.retry(id) } else { self.ctl.skip(id) }) {
                self.say("Skip works only before a download starts, and Retry only after it failed.");
            }
        }
    }

    pub fn retry_failed(&mut self) {
        let mut n = 0;
        for j in self.jobs() {
            if j["state"] == "failed" && !s(&j["status"]).starts_with("Blocked") && !s(&j["status"]).starts_with("Skipped") {
                self.ctl.retry(j["id"].as_u64().unwrap_or(0));
                n += 1;
            }
        }
        self.say(&if n > 0 { format!("Trying {n} download(s) again.") } else { "Nothing to retry.".into() });
    }

    pub fn clear_jobs(&mut self) {
        lock(&self.ctl.jobs).retain(|j| !matches!(s(&j["state"]), "done" | "failed"));
        self.refresh_progress();
    }

    pub fn log_line(&mut self, text: &str) {
        self.log.push(format!("{}{text}", util::strftime("%H:%M:%S  ", util::now())));
        self.say(text);
    }

    // ---------- Crash doctor ----------
    pub fn problem(&self) -> Option<&Value> {
        self.prob_sel.as_ref().and_then(|id| self.problems.iter().find(|p| p["id"].as_str() == Some(id)))
    }

    pub fn select_problem(&mut self, id: Option<String>) {
        self.prob_sel = id;
        let best = self.problem().map(|p| p["fixes"].as_array().into_iter().flatten().position(|x| x["recommended"].as_bool().unwrap_or(false)).unwrap_or(0));
        self.fix_choice = best.unwrap_or(0);
    }

    pub fn problem_detail(&self) -> String {
        match self.problem() {
            Some(p) => format!("{}\n\n{}", s(&p["title"]), s(&p["detail"])),
            None => "No problem selected.\n\nPress 'Check my mods now' to look for problems. When the game crashes, I check it by myself and show the problems here.".into(),
        }
    }

    /// The fix choices of the selected problem: (label, recommended mark).
    pub fn fix_labels(&self) -> Vec<String> {
        let Some(p) = self.problem() else { return vec![] };
        let fixes: Vec<&Value> = p["fixes"].as_array().into_iter().flatten().collect();
        let best = fixes.iter().position(|x| x["recommended"].as_bool().unwrap_or(false)).unwrap_or(0);
        fixes.iter().enumerate().map(|(i, x)| format!("{}{}", s(&x["label"]), if i == best { "   (recommended)" } else { "" })).collect()
    }

    pub fn show_problems(&mut self, payload: &Value) {
        self.problems = payload["list"].as_array().cloned().unwrap_or_default();
        let first = self.problems.first().and_then(|p| p["id"].as_str()).map(String::from);
        self.select_problem(first);
        self.sync();
        if payload["crashed"].as_bool().unwrap_or(false) {
            self.select(Page::Crash);
            self.raise = true;
            let n = self.problems.iter().filter(|p| p["severity"] == "high").count();
            let text = if n > 0 { format!("I checked the crash. {n} problem(s) need you to pick a fix.") } else { "I fixed what I found. Start the game again.".into() };
            self.ask.info("The game crashed", &text);
        } else {
            let text = if self.problems.is_empty() { "No problems found.".into() } else { format!("Found {} problem(s). See the Crash doctor tab.", self.problems.len()) };
            self.say(&text);
        }
    }

    pub fn apply_fix(&mut self) {
        let Some(p) = self.problem().cloned() else { return };
        let Some(f) = p["fixes"].get(self.fix_choice).cloned() else { return };
        self.act(Act::Apply(p.clone(), f.clone()));
        self.problems.retain(|x| x["id"] != p["id"]);
        let first = self.problems.first().and_then(|p| p["id"].as_str()).map(String::from);
        self.select_problem(first);
        self.sync();
        self.say(&format!("Done: {}.", s(&f["label"])));
    }

    /// (when, what) of each fix done.
    pub fn history(&self) -> Vec<(String, String)> {
        let h = self.v.doctor.get("history").and_then(Value::as_array).cloned().unwrap_or_default();
        h.iter().map(|h| (util::strftime("%d %b %H:%M", h["time"].as_f64().unwrap_or(0.0) as i64), format!("{}  →  {}", s(&h["problem"]), s(&h["fix"])))).collect()
    }

    pub fn undo(&mut self) {
        if let Some(i) = self.hist_sel {
            let r = self.ctl.undo(i);
            self.safe(r);
            self.hist_sel = None;
            self.sync();
            self.say("Fix reversed.");
        }
    }

    pub fn bisect_text(&self) -> Option<String> {
        let b = self.v.doctor.get("bisect").filter(|b| !b.is_null())?;
        let n = b["suspects"].as_array().map_or(0, Vec::len);
        Some(format!("Bad-mod search running: {n} suspects left. Start the game; play until it crashes or quit normally."))
    }

    pub fn game_state(&self) -> &'static str {
        if self.game_running {
            "●  The game is running. I am watching for a crash."
        } else {
            "●  The game is not running. I watch for crashes in the background."
        }
    }

    pub fn save_report(&mut self) {
        let name = format!("cp2077-crash-report-{}.txt", util::strftime("%Y%m%d-%H%M", util::now()));
        let Some(path) = self.ask.save_file("Save crash report", &name) else { return };
        if let Err(e) = std::fs::write(&path, self.ctl.crash_report(false)) {
            return self.ask.error("Save crash report", &format!("Could not save the report: {e}"));
        }
        self.say(&format!("Crash report saved: {}", path.display()));
    }

    pub fn copy_report(&mut self) {
        self.copied = Some(self.ctl.crash_report(true));
        self.say("Report copied. Paste it into an AI chat.");
    }

    pub fn open_crash_logs(&mut self) {
        let d = self.v.root.join("crash_logs");
        if !d.is_dir() {
            return self.say("No crash logs are saved yet. I save them after each crash.");
        }
        let _ = shell_open(&d, &[], None);
    }

    pub fn bisect_begin(&mut self) {
        if self.ask.yes_no(
            "Find the bad mod",
            "I will turn half of your mods off. Start the game and play until it crashes, or quit normally if it does not. I repeat this until I find the bad mod.\n\nStart?",
            false,
        ) {
            let r = self.ctl.bisect_begin();
            self.safe(r);
            self.sync();
        }
    }

    pub fn bisect_cancel(&mut self) {
        let r = self.ctl.bisect_cancel();
        self.safe(r);
        self.sync();
    }

    // ---------- AI helpers ----------
    pub fn setup_ai(&mut self) {
        if !local_ai::ready()
            && !self.ask.yes_no(
                "Local AI",
                "The local AI files are not next to this app. I download the llama.cpp engine (MIT license, 36 MB, from GitHub) and the Qwen3.5 model (Apache 2.0 license, 1.3 GB, from this app's GitHub releases), and check both files.\n\nGo on?",
                false,
            )
        {
            return;
        }
        self.ai_info = ("Setting up the local AI…".into(), self.theme().dim);
        self.act(Act::SetupAi);
    }

    pub fn ask_ai(&mut self) {
        self.say("The local AI is reading your mods and problems… The first answer can take a minute.");
        self.act(Act::AskAi(String::new()));
    }

    pub fn ask_bug(&mut self) {
        let text = self.bug_text.trim().to_string();
        if text.is_empty() {
            return self.ask.info("Local AI", "First write what goes wrong in the game.");
        }
        self.say("The local AI is reading your bug report, mods and problems… The first answer can take a minute.");
        self.act(Act::AskAi(text));
    }

    pub fn show_ai(&mut self, p: &Value) {
        if let Some(e) = p.get("error") {
            self.say("The local AI could not help.");
            return self.ask.error("Local AI", e.as_str().unwrap_or_default());
        }
        let cut = |t: &str, n: usize| t.chars().take(n).collect::<String>();
        let sum = s(&p["summary"]);
        let mut text = cut(if sum.is_empty() { "The local AI gave no summary." } else { sum }, 900);
        if let Some(c) = p["cause"].as_str().filter(|c| !c.is_empty()) {
            text = format!("Why it happens: {}\n\n{text}", cut(c, 600));
        }
        let order = p["order"].as_array().is_some_and(|o| !o.is_empty());
        if order {
            text += &format!("\n\nLoad order: {} mod(s) change place.", p["moved"]);
        }
        let fixes = p["fixes"].as_array().cloned().unwrap_or_default();
        if !fixes.is_empty() {
            let lines: Vec<String> = fixes
                .iter()
                .map(|f| {
                    let why = s(&f[2]);
                    format!("•  {}  →  {}{}", s(&f[0]["title"]), s(&f[1]["label"]), if why.is_empty() { String::new() } else { format!("\n    {}", cut(why, 200)) })
                })
                .collect();
            text += &format!("\n\nFixes:\n{}", lines.join("\n"));
        }
        if !(order || !fixes.is_empty()) {
            self.say("The local AI found nothing to change.");
            return self.ask.info("Local AI", &format!("{text}\n\nThe local AI found nothing to change."));
        }
        if self.ask.yes_no("Local AI", &format!("{text}\n\nMake these changes? You can undo each one in the Crash doctor tab."), false) {
            self.act(Act::ApplyAi(p.clone()));
            self.say("Done. You can undo each change in the Crash doctor tab.");
        }
    }

    /// The MCP setup that starts this exe with `--mcp`.
    pub fn copy_setup(&mut self, desktop: bool) {
        let exe = std::env::current_exe().unwrap_or_default().to_string_lossy().into_owned();
        let cmd = [exe, "--mcp".to_string()];
        let text = if desktop {
            serde_json::to_string_pretty(&json!({"mcpServers": {"cp2077-ai-mod-manager": {"command": cmd[0], "args": cmd[1..]}}})).unwrap_or_default()
        } else {
            // quoted: Git Bash drops the backslashes of an unquoted path
            format!("claude mcp add --scope user cp2077-ai-mod-manager -- {}", cmd.iter().map(|c| format!("\"{c}\"")).collect::<Vec<_>>().join(" "))
        };
        self.copied = Some(text);
        self.say("Copied. Paste it as the text in the AI helpers tab says.");
    }

    // ---------- Settings ----------
    pub fn game_info(&self) -> (&'static str, egui::Color32) {
        if self.v.game {
            ("✔ Cyberpunk 2077 found", OK)
        } else if !self.game_var.trim().is_empty() {
            ("✖ No bin\\x64\\Cyberpunk2077.exe in this folder. Pick the game's main folder.", BAD)
        } else {
            ("Not set. Press 'Find it for me' or 'Browse…'.", WARN)
        }
    }

    pub fn nxm_info(&self) -> (String, egui::Color32) {
        let (on, other) = &self.nxm_state;
        if *on {
            ("✔ 'Slow download' on Nexus sends files to this app".into(), OK)
        } else if !other.is_empty() {
            (format!("✖ 'Slow download' on Nexus sends files to {other}, not to this app"), WARN)
        } else {
            let tail = if self.v.settings.auto_click { " (not needed while I click for you)" } else { "" };
            (format!("✖ 'Slow download' on Nexus does nothing yet{tail}"), WARN)
        }
    }

    /// (button text, main colour)
    pub fn nxm_btn(&self) -> (&'static str, bool) {
        if self.nxm_state.0 { ("Set it up again", false) } else { ("▶  Fix it: use this app", true) }
    }

    pub fn refresh_settings(&mut self) {
        self.nxm_state = ((self.nxm.registered)(), (self.nxm.owner)());
    }

    /// The setup banner under the header, or None when all is set (or it was closed).
    pub fn setup_banner(&self) -> Option<String> {
        let steps = [("Game folder", self.v.game), ("Nexus API key", !self.v.settings.api_key.is_empty()), ("Nexus download button", self.nxm_state.0)];
        if steps.iter().all(|s| s.1) || self.banner_hidden {
            return None;
        }
        let parts: Vec<String> = steps.iter().map(|(n, ok)| format!("{} {n}", if *ok { "✔" } else { "✖" })).collect();
        Some(format!("Finish setup:   {}", parts.join("    ")))
    }

    pub fn save_settings(&mut self) {
        let (key, game) = {
            let mut st = self.st();
            let (old_key, old_game) = (st.settings.api_key.clone(), st.settings.game_dir.clone());
            st.settings.game_dir = self.game_var.trim().to_string();
            st.settings.api_key = self.key_var.trim().to_string();
            let _ = st.save();
            (st.settings.api_key != old_key && !st.settings.api_key.is_empty(), st.settings.game_dir != old_game && st.game().is_some())
        };
        self.sync();
        if key {
            self.check_key();
        }
        if game {
            self.redeploy();
        }
        self.refresh_settings();
        self.refresh_mods();
    }

    /// A ticked option in Settings: saved at once.
    pub fn set_flag(&mut self, key: &str, on: bool) {
        {
            let mut st = self.st();
            let f = &mut st.settings;
            match key {
                "virus_scan" => f.virus_scan = on,
                "auto_fix" => f.auto_fix = on,
                "include_optional" => f.include_optional = on,
                _ => {} // manual-click version: nothing turns the automatic click on
            }
            let _ = st.save();
        }
        self.save_settings();
    }

    pub fn set_theme(&mut self, name: &str) {
        {
            let mut st = self.st();
            st.settings.theme = name.into();
            let _ = st.save();
        }
        self.theme_name = name.into();
        self.sync();
    }

    pub fn check_key(&mut self) {
        if self.key_var.trim() != self.v.settings.api_key {
            return self.save_settings(); // saving a new key runs the check
        }
        if self.v.settings.api_key.is_empty() {
            self.key_info = ("Paste your key first.".into(), WARN);
            return;
        }
        self.key_info = ("Checking…".into(), self.theme().dim);
        self.act(Act::CheckKey);
    }

    pub fn browse_game(&mut self) {
        if let Some(d) = self.ask.folder("Pick the Cyberpunk 2077 folder") {
            self.game_var = d.to_string_lossy().into_owned();
            self.save_settings();
        }
    }

    pub fn detect_game(&mut self) {
        let g = (self.find_game)();
        if g.is_empty() {
            self.ask.info("Mod manager", "I could not find Cyberpunk 2077. Use Browse to pick its folder.");
        } else {
            self.game_var = g;
            self.save_settings();
        }
    }

    pub fn register_nxm(&mut self) {
        if let Some(r) = &self.nxm.register
            && let Err(e) = r()
        {
            self.ask.error("Mod manager", &e.to_string());
        }
        self.refresh_settings();
        self.say("Nexus download buttons now send files to this app.");
        let waiting = self.jobs().into_iter().find(|j| j["state"] == "waiting");
        // its last click went to the other app: show its page again (not while the hidden browser clicks)
        if let Some(w) = waiting
            && (!self.v.settings.auto_click || w["manual"].as_bool().unwrap_or(false))
        {
            self.act(Act::Reopen(w));
        }
    }

    pub fn update_app(&mut self) {
        self.update_busy = true; // one update at a time; every "update" event says when it may click again
        self.say("Looking for a new version on GitHub…");
        self.act(Act::UpdateApp(None));
    }

    pub fn show_update(&mut self, p: &Value) {
        let mut p = p.clone();
        if let Some(rel) = p.get("ask").cloned() {
            if self.ask.yes_no(
                "Update",
                &format!("Version {} is on GitHub (you have {VERSION}).\n\nDownload it, scan it with Microsoft Defender and restart the app with it?", s(&rel["tag"])),
                false,
            ) {
                self.act(Act::UpdateApp(Some(rel)));
                return;
            }
            p = json!({"text": "Update skipped."});
        }
        self.say(s(&p["text"]));
        let restart = p["restart"].as_bool().unwrap_or(false);
        self.update_busy = p["busy"].as_bool().unwrap_or(false) || restart;
        if restart {
            self.close();
        }
    }

    // ---------- questions from the controller ----------
    /// A mod pack has optional mods, or mods that may not work with the game any more: the user picks what to download.
    pub fn pack_choice(&mut self, p: &Value) {
        let files = p["files"].as_array().cloned().unwrap_or_default();
        let entry = |f: &Value| (f["file_id"].as_i64().unwrap_or(0), s(&f["name"]).to_string(), s(&f["outdated"]).to_string());
        let old: Vec<PackFile> = files.iter().filter(|f| !s(&f["outdated"]).is_empty()).map(entry).collect();
        let optional: Vec<PackFile> = files.iter().filter(|f| s(&f["outdated"]).is_empty()).map(entry).collect();
        let inc = self.v.settings.include_optional;
        let picks = files.iter().filter(|f| s(&f["outdated"]).is_empty()).chain(files.iter().filter(|f| !s(&f["outdated"]).is_empty()));
        let picks = picks.map(|f| (f["file_id"].as_i64().unwrap_or(0), inc || !f["optional"].as_bool().unwrap_or(false))).collect();
        self.dialogs.push(Dialog::PackChoice { link: p["link"].clone(), name: s(&p["name"]).into(), optional, old, picks });
    }

    pub fn set_pick(&mut self, file_id: i64, on: bool) {
        for d in &mut self.dialogs {
            if let Dialog::PackChoice { picks, .. } = d
                && let Some(p) = picks.iter_mut().find(|p| p.0 == file_id)
            {
                p.1 = on;
            }
        }
    }

    pub fn choice_ok(&mut self) {
        let Some(i) = self.dialogs.iter().rposition(|d| matches!(d, Dialog::PackChoice { .. })) else { return };
        let Dialog::PackChoice { link, name, picks, .. } = self.dialogs.remove(i) else { return };
        let chosen: Vec<i64> = picks.iter().filter(|p| p.1).map(|p| p.0).collect();
        let mut link = link;
        link["chosen"] = json!(chosen);
        self.act(Act::AddLink(link));
        self.select(Page::Downloads);
        self.say(&format!("Downloading the mod pack {name} with {} of the {} mods you could pick.", chosen.len(), picks.len()));
    }

    pub fn ask_outdated(&mut self, p: &Value) {
        self.dialogs.push(Dialog::Outdated { link: p["link"].clone(), name: s(&p["name"]).into(), why: s(&p["why"]).into() });
    }

    pub fn outdated_text(why: &str) -> String {
        format!("This mod may not work with the game any more: {why}.")
    }

    /// Download anyway, or skip, in the top "This mod may not work" dialog.
    pub fn outdated_pick(&mut self, get: bool) {
        let Some(i) = self.dialogs.iter().rposition(|d| matches!(d, Dialog::Outdated { .. })) else { return };
        let Dialog::Outdated { mut link, name, .. } = self.dialogs.remove(i) else { return };
        if get {
            link["anyway"] = json!(true);
            self.act(Act::AddLink(link));
        }
        self.say(&if get { format!("Downloading {name}.") } else { format!("Skipped {name}.") });
    }

    pub fn pick_text(why: &str) -> String {
        first_upper(why) + "."
    }

    /// A Nexus link this app did not ask for: it may come from any web page, so "No" is the default.
    pub fn confirm(&mut self, p: &Value) {
        self.raise = true;
        let text: String = s(&p["text"]).split_whitespace().collect::<Vec<_>>().join(" ").chars().take(300).collect();
        if self.ask.yes_no("Nexus download", &format!("{text}\n\nSay yes only if you clicked this download yourself on Nexus Mods."), true) {
            self.act(Act::AddLink(p["link"].clone()));
            self.select(Page::Downloads);
        }
    }

    // ---------- events ----------
    pub fn drain(&mut self) {
        while let Ok((kind, p)) = self.rx.try_recv() {
            self.on_event(&kind, &p);
        }
    }

    pub fn on_event(&mut self, kind: &str, p: &Value) {
        match kind {
            "job" => self.show_job(p),
            "mods" => self.refresh_mods(),
            "log" => self.log_line(s(p)),
            "key" => self.key_info = (s(p).into(), if s(p).starts_with("Key OK") { OK } else { BAD }),
            "game" => {
                self.game_running = p.as_bool().unwrap_or(false);
                self.sync();
            }
            "problems" => self.show_problems(p),
            "video" => self.show_found(p),
            "browse" => self.show_browse(p),
            "thumbs" => self.show_thumbs(p),
            "ai_status" => {
                let t = s(p);
                let c = if t.starts_with('✔') { OK } else if t.starts_with('✖') { BAD } else { self.theme().dim };
                self.ai_info = (t.into(), c);
            }
            "ai" => self.show_ai(p),
            "update" => self.show_update(p),
            "pack_choice" => self.pack_choice(p),
            "outdated" => self.ask_outdated(p),
            "show" => self.raise = true,
            "confirm" => self.confirm(p),
            _ => {}
        }
    }

    // ---------- close ----------
    /// No new copy starts. The window shows a message and takes no input (it does not freeze) while a
    /// running copy finishes, then it closes by itself.
    pub fn close(&mut self) {
        if self.ctl.stop.load(SeqCst) {
            return;
        }
        self.ctl.close(); // stop, the hidden Nexus browser and the local AI end at once
        self.say("Finishing the current copy into the game folder…");
        self.closing = true;
        self.close_when_free();
    }

    /// True once the window may go: it took the store lock and keeps it until the process ends, so no thread
    /// can start a copy or swap mod files now.
    pub fn close_when_free(&mut self) -> bool {
        if self.closed {
            return true;
        }
        if let Some(g) = mutex_try(&self.ctl.store) {
            std::mem::forget(g);
            self.closed = true;
        }
        self.closed
    }

    #[cfg(test)]
    pub(crate) fn kinds(&self) -> Vec<String> {
        self.rx.try_iter().map(|(k, _)| k).collect()
    }
}

/// Runs the window until it closes: (pipe that this process owns, nxm:// link it was started with, token).
pub fn run(server: OwnerListener, url: Option<String>, token: String, nxm: Nxm) {
    let store = match Store::open_default() {
        Ok(s) => s,
        Err(e) => {
            NativeAsk.error("Mod manager", &format!("Could not open the data folder: {e}"));
            return;
        }
    };
    let opts = Window::opts(server, url, token, nxm);
    let w = Window::new(store, opts);
    let mut vp = egui::ViewportBuilder::default().with_title(w.title()).with_inner_size([1180.0, 760.0]).with_min_inner_size([900.0, 560.0]);
    if let Ok(icon) = eframe::icon_data::from_png_bytes(include_bytes!("../../../manager_ui/icon.png")) {
        vp = vp.with_icon(icon);
    }
    let options = eframe::NativeOptions { viewport: vp, ..Default::default() };
    let _ = eframe::run_native(
        "CP2077 AI Mod Manager",
        options,
        Box::new(move |cc| {
            egui_extras::install_image_loaders(&cc.egui_ctx);
            theme::fonts(&cc.egui_ctx);
            let _ = w.repaint.set(cc.egui_ctx.clone());
            Ok(Box::new(w))
        }),
    );
}

#[cfg(test)]
mod tests;
