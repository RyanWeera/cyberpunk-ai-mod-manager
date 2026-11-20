//! Port of tests/test_ui.py: a real window state on a throwaway store, driven through its methods, plus a
//! headless frame of every page and dialog. Controller calls are recorded (`acts`) instead of mocked.

use super::*;
use crate::pipe::{Conn, Wire, answer_challenge, deliver_challenge, token_hex};
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::io;
use std::sync::atomic::AtomicBool;
use testkit::{TempDir, fake_game, mod_rec, stage};

/// Scripted message boxes: yes/no answers in order (default no), and every call recorded.
#[derive(Default)]
struct Script {
    answers: RefCell<VecDeque<bool>>,
    /// (kind, title, text, default_no)
    calls: RefCell<Vec<(&'static str, String, String, bool)>>,
    files: RefCell<Vec<PathBuf>>,
}

impl Script {
    fn answer(&self, yes: bool) {
        self.answers.borrow_mut().push_back(yes);
    }
    fn last(&self, kind: &str) -> Option<(String, String, bool)> {
        self.calls.borrow().iter().rev().find(|c| c.0 == kind).map(|c| (c.1.clone(), c.2.clone(), c.3))
    }
    fn count(&self, kind: &str) -> usize {
        self.calls.borrow().iter().filter(|c| c.0 == kind).count()
    }
}

impl Ask for Script {
    fn yes_no(&self, title: &str, text: &str, default_no: bool) -> bool {
        self.calls.borrow_mut().push(("yes_no", title.into(), text.into(), default_no));
        self.answers.borrow_mut().pop_front().unwrap_or(false)
    }
    fn info(&self, title: &str, text: &str) {
        self.calls.borrow_mut().push(("info", title.into(), text.into(), false));
    }
    fn error(&self, title: &str, text: &str) {
        self.calls.borrow_mut().push(("error", title.into(), text.into(), false));
    }
    fn open_files(&self) -> Vec<PathBuf> {
        self.files.borrow_mut().drain(..).collect()
    }
    fn save_file(&self, _: &str, _: &str) -> Option<PathBuf> {
        None
    }
    fn folder(&self, _: &str) -> Option<PathBuf> {
        None
    }
}

struct Fx {
    tmp: TempDir,
    ask: Rc<Script>,
    w: Window,
    nxm_calls: Arc<AtomicUsize>,
}

fn deps() -> Deps {
    Deps {
        scan: Arc::new(|_| (Some(true), String::new())),
        open_url: Arc::new(|_| {}),
        spawn: Arc::new(|_| Ok(())),
        run_as: Arc::new(|_, _, _| Ok(())),
        block_ai: Arc::new(|_| {}),
        start_ai: Arc::new(|| Err("no local AI in tests".into())),
        advise: Arc::new(|_, _, _| Err("no advice in tests".into())),
        timer: Arc::new(|_, _| {}),
        ..Deps::default()
    }
}

fn pipe() -> String {
    format!(r"\\.\pipe\cp2077mm-test-{}-{}", std::process::id(), token_hex(8))
}

fn fx_with(server: Option<OwnerListener>, nxm: Nxm) -> Fx {
    let tmp = TempDir::new();
    let store = Store::open(tmp.path().join("data")).unwrap();
    let ask = Rc::new(Script::default());
    let nxm_calls = Arc::new(AtomicUsize::new(0));
    let n = nxm_calls.clone();
    let opts = Opts {
        server,
        first_url: None,
        token: b"t0k3n".to_vec(),
        nxm,
        ask: ask.clone(),
        deps: deps(),
        workers: 0,
        watch_game: false,
        find_game: String::new, // never point a test at the real game on this PC
        on_nxm: Some(Box::new(move |_| {
            n.fetch_add(1, SeqCst);
        })),
    };
    let mut w = Window::new(store, opts);
    w.acts = Some(vec![]);
    Fx { tmp, ask, w, nxm_calls }
}

fn fx() -> Fx {
    fx_with(None, Nxm::none())
}

impl Fx {
    fn acts(&mut self) -> Vec<Act> {
        self.w.acts.replace(vec![]).unwrap_or_default()
    }
    fn store(&self) -> MutexGuard<'_, Store> {
        lock(&self.w.ctl.store)
    }
    fn set_game(&mut self) {
        let g = fake_game(self.tmp.path());
        self.store().settings.game_dir = g.to_string_lossy().into_owned();
        self.w.sync();
    }
    /// Python's `events()`: wait a moment, then the kinds of the queued window events.
    fn events(&self) -> Vec<String> {
        thread::sleep(Duration::from_millis(300));
        self.w.kinds()
    }
}

fn named(id: &str, files: &[&str], name: &str, on: bool) -> Mod {
    let mut m = mod_rec(id, files);
    m.name = name.into();
    m.enabled = on;
    m
}

/// One headless frame of the whole window.
fn render(w: &mut Window) -> egui::Context {
    let ctx = egui::Context::default();
    let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
        w.step(ui.ctx());
        w.draw(ui);
    });
    out.textures_delta.clear(); // no painter here: drop the font textures on purpose
    ctx
}

/// The token handshake from the client side (Python's `Client(..., authkey=...)`).
fn client(pipe: &str, key: &[u8]) -> io::Result<Conn> {
    let c = Conn::connect(pipe)?;
    answer_challenge(&c, key)?;
    deliver_challenge(&c, key)?;
    Ok(c)
}

// test_style_switch_recolors_and_is_kept
#[test]
fn style_switch_recolors_and_is_kept() {
    let mut f = fx();
    assert_eq!(f.store().settings.theme, "Cyberpunk");
    f.w.set_theme("Arasaka");
    assert_eq!(f.store().settings.theme, "Arasaka");
    assert_eq!(f.w.theme().accent, THEMES[1].accent);
    let ctx = render(&mut f.w);
    assert_eq!(ctx.global_style().visuals.window_fill, THEMES[1].panel);
    assert_eq!(Store::open(f.tmp.path().join("data")).unwrap().settings.theme, "Arasaka"); // saved for the next start
}

// test_first_run_shows_setup_and_empty_state
#[test]
fn first_run_shows_setup_and_empty_state() {
    let mut f = fx();
    assert_eq!(f.w.tab_text(Page::Mods), "Mods (0)");
    assert_eq!(f.w.mods_empty().map(|e| e.0), Some("Ready to mod Night City?"));
    assert!(f.w.setup_banner().unwrap().contains("✖ Game folder"));
    assert_eq!(f.w.page, Page::Settings);
    f.w.game_var = fake_game(f.tmp.path()).to_string_lossy().into_owned();
    f.w.save_settings();
    assert!(f.w.setup_banner().unwrap().contains("✔ Game folder"));
    assert!(f.w.game_info().0.contains("found"));
    f.w.game_var = f.tmp.path().to_string_lossy().into_owned();
    f.w.save_settings();
    assert!(f.w.game_info().0.contains("No bin"));
}

// test_mod_list_search_toggle_remove
#[test]
fn mod_list_search_toggle_remove() {
    let mut f = fx();
    stage(&mut f.store(), named("a", &["archive/pc/mod/a.archive"], "Alpha HUD", true), &[]);
    stage(&mut f.store(), named("b", &["r6/scripts/b/b.reds"], "Beta Cars", false), &[]);
    f.w.refresh_mods();
    assert_eq!(f.w.tab_text(Page::Mods), "Mods (2)");
    let rows = f.w.mod_rows();
    assert!(!rows.iter().find(|r| r.id == "b").unwrap().on);
    assert_eq!(rows.iter().find(|r| r.id == "a").unwrap().kind, "Archive");
    let ids = |w: &Window| w.mod_rows().into_iter().map(|r| r.id).collect::<Vec<_>>();
    f.w.search = "beta".into();
    assert_eq!(ids(&f.w), ["b"]);
    f.w.search = "zzz".into();
    assert!(f.w.mods_empty().unwrap().1.contains("No mod matches"));
    f.w.search.clear();
    f.w.mods_show = "Off";
    assert_eq!(ids(&f.w), ["b"]);
    f.w.mods_show = "All";
    assert!(f.w.pack_sub().contains("2 mods  ·  1 on"));
    f.w.sel = vec!["a".into(), "b".into()];
    f.w.toggle(); // one is off -> both on
    assert!(f.store().mods.iter().all(|m| m.enabled));
    f.w.toggle(); // all on -> both off
    assert!(f.store().mods.iter().all(|m| !m.enabled));
    f.ask.answer(true);
    f.w.sel = vec!["a".into()];
    f.w.remove();
    assert!(f.store().mod_by_id("a").is_none());
    assert!(!f.store().staging.join("a").exists());
}

// test_rail_and_add_dialog
#[test]
fn rail_and_add_dialog() {
    let mut f = fx();
    f.w.rail_click(Page::Downloads);
    assert_eq!(f.w.page, Page::Downloads);
    assert_eq!(f.w.page_title(), "Downloads");
    f.w.rail_click(Page::Add);
    assert!(matches!(f.w.dialogs.last(), Some(Dialog::Add { .. })));
    render(&mut f.w);
    assert_eq!(ADD_WAYS.len(), 5);
    f.w.add_go(1); // Paste links
    assert_eq!(f.w.page, Page::Add);
    assert!(f.w.dialogs.is_empty());
}

// test_add_links_placeholder_and_count
#[test]
fn add_links_placeholder_and_count() {
    let mut f = fx();
    assert_eq!(f.w.links_text(), ""); // the example text is a hint, not a link
    f.w.links = "https://www.nexusmods.com/cyberpunk2077/mods/107\n\nhttps://github.com/a/b\n".into();
    assert_eq!(f.w.link_count(), "2 link(s) ready");
    f.w.add_links();
    assert_eq!(f.acts().iter().filter(|a| matches!(a, Act::AddLinks(_))).count(), 1);
    assert_eq!(f.w.page, Page::Downloads);
    assert!(f.w.status.contains("Added 2 link(s)"));
}

// test_browse_tab_cards_pages_and_install
#[test]
fn browse_tab_cards_pages_and_install() {
    let mut f = fx();
    assert_eq!(f.w.browse.req, 0); // no game folder yet: Settings shows, and nothing loads from the web
    let mut cet = mod_rec("cet", &[]);
    cet.source = json!({"type": "github", "repo": "maximegmd/CyberEngineTweaks"});
    stage(&mut f.store(), cet, &[]);
    f.w.refresh_mods();
    let mods = json!([
        {"id": 107, "name": "Cyber Engine Tweaks", "summary": "Scripting", "author": "yamashi", "downloads": 20381596,
         "endorsements": 248536, "updated": "2026-10-26T01:28:58Z", "thumb": "https://staticdelivery.nexusmods.com/a.png",
         "category": "Modders Resources"},
        {"id": 5, "name": "Nova LUT", "summary": "", "author": "", "downloads": 9, "endorsements": 0, "updated": "",
         "thumb": "", "category": ""}]);
    f.w.browse.req = 3;
    f.w.show_browse(&json!({"req": 2, "page": 0, "total": 1, "categories": {}, "mods": []})); // an older search: ignored
    assert!(f.w.browse.mods.is_empty());
    f.w.show_browse(&json!({"req": 3, "page": 0, "total": 41, "categories": {"Weapons": 1464, "Audio": 1438}, "mods": mods}));
    assert_eq!(f.acts().iter().filter(|a| matches!(a, Act::Thumbs(..))).count(), 1); // thumbnails load in the background
    assert_eq!(f.w.browse.cats.iter().map(|c| c.0.as_str()).collect::<Vec<_>>(), ["Audio", "Weapons"]);
    assert_eq!(f.w.browse.mods.len(), 2);
    assert_eq!(f.w.browse.info.0, "41 mods");
    let pager: Vec<String> = f.w.pager().into_iter().map(|p| p.map_or("…".into(), |n| util::commas(n + 1))).collect();
    assert_eq!(pager, ["1", "2", "3"]);
    assert!(f.w.browse.page == 0); // the back button is off
    assert!(f.w.browse.page < f.w.pages() - 1); // the next button is on
    assert_eq!(f.w.mark(&json!(107)), Mark::Installed); // the framework came from GitHub
    render(&mut f.w);
    f.w.browse_install(&mods[1]);
    assert_eq!(f.acts(), [Act::AddLinks("https://www.nexusmods.com/cyberpunk2077/mods/5".into())]);
    assert_eq!(f.w.mark(&json!(5)), Mark::Added);
    f.w.show_browse(&json!({"req": 3, "error": "timed out"}));
    assert!(f.w.browse.info.0.contains("timed out"));
}

// test_find_mod_from_video
#[test]
fn find_mod_from_video() {
    let mut f = fx();
    f.w.find_video();
    assert!(f.acts().is_empty()); // nothing pasted
    f.w.video = " https://youtu.be/8Yexbiw0T28 ".into();
    f.w.find_video();
    assert_eq!(f.acts(), [Act::FindInVideo("https://youtu.be/8Yexbiw0T28".into())]);
    let (a, b) = ("https://www.nexusmods.com/cyberpunk2077/mods/1", "https://github.com/a/b");
    f.w.show_found(&json!({"title": "Top mods", "rows": [{"name": "Nova LUT", "url": a, "why": "Linked in the video", "endorsements": 5},
                                                          {"name": "a/b", "url": b, "why": "Linked in the video", "endorsements": null}]}));
    assert_eq!(f.w.found.iter().map(|r| r.0.as_str()).collect::<Vec<_>>(), [a, b]);
    assert_eq!(f.w.video_info.0, "\"Top mods\"   2 mod(s) linked in the video.");
    render(&mut f.w);
    f.w.found_sel = vec![b.into()];
    f.w.add_found();
    assert_eq!(f.acts(), [Act::AddLinks(b.into())]);
    assert_eq!(f.w.page, Page::Downloads);
    f.w.show_found(&json!({"error": "HTTP Error 404: Not Found"}));
    assert!(f.w.found.is_empty());
    assert!(f.w.video_info.0.contains("404"));
}

// test_download_banner_progress_and_tabs
#[test]
fn download_banner_progress_and_tabs() {
    let mut f = fx();
    let j = json!({"id": 1, "name": "HUD", "src": {"type": "nexus", "mod_id": 1, "file_id": 2}, "state": "needkey",
                   "status": "Needs key", "progress": ""});
    lock(&f.w.ctl.jobs).push(j.clone());
    f.w.show_job(&j);
    assert!(f.w.dl_banner.as_ref().unwrap().0.contains("API key"));
    assert_eq!(f.w.tab_text(Page::Downloads), "Downloads (1)");
    assert_eq!(f.w.job_rows()[0].4, "needkey");
    render(&mut f.w);
    {
        let mut jobs = lock(&f.w.ctl.jobs);
        jobs[0]["state"] = json!("done");
        jobs[0]["status"] = json!("Done");
    }
    f.w.show_job(&j);
    assert!(f.w.dl_banner.is_none());
    assert_eq!(f.w.tab_text(Page::Downloads), "Downloads");
    assert!(f.w.status.contains("All downloads finished"));
}

// test_link_pipe_needs_the_token_both_ways
#[test]
fn link_pipe_needs_the_token_both_ways() {
    let name = pipe();
    let f = fx_with(Some(OwnerListener::new(&name).unwrap()), Nxm::none());
    let link = b"nxm://cyberpunk2077/mods/1/files/2?key=a&expires=1";
    assert!(client(&name, b"wrong").and_then(|c| c.send(link)).is_err()); // wrong token: the window refuses, nothing is sent
    assert!(!f.events().contains(&"show".to_string()));
    let c = client(&name, b"t0k3n").unwrap();
    c.send(link).unwrap();
    assert_eq!(c.recv(1 << 20).unwrap(), b"ok"); // the window confirms it took the link
    drop(c);
    assert!(f.events().contains(&"show".to_string()));
    f.w.ctl.stop.store(true, SeqCst); // closing: even the right token is refused, so the sender keeps its link
    let r = client(&name, b"t0k3n").and_then(|c| {
        c.send(b"nxm://cyberpunk2077/mods/1/files/3?key=a&expires=1")?;
        c.recv(1 << 20)
    });
    assert!(r.is_err());
    assert_eq!(f.nxm_calls.load(SeqCst), 1);
}

// test_ai_app_tool_call_over_the_pipe
#[test]
fn ai_app_tool_call_over_the_pipe() {
    let name = pipe();
    let _f = fx_with(Some(OwnerListener::new(&name).unwrap()), Nxm::none());
    let c = client(&name, b"t0k3n").unwrap();
    c.send(json!({"tool": "status", "args": {}}).to_string().as_bytes()).unwrap();
    let r: Value = serde_json::from_slice(&c.recv(1 << 20).unwrap()).unwrap();
    assert!(s(&r["text"]).contains("Game folder: not set"));
    let c = client(&name, b"t0k3n").unwrap();
    c.send(br#"{"tool": "nope"}"#).unwrap();
    let r: Value = serde_json::from_slice(&c.recv(1 << 20).unwrap()).unwrap();
    assert!(r.get("error").is_some());
}

// test_ai_tab_setup_text_and_advice
#[test]
fn ai_tab_setup_text_and_advice() {
    let mut f = fx();
    assert_eq!(Page::Ai.name(), "AI helpers");
    f.w.copy_setup(true);
    let v: Value = serde_json::from_str(f.w.copied.as_deref().unwrap()).unwrap();
    assert_eq!(v["mcpServers"]["cp2077-ai-mod-manager"]["args"].as_array().unwrap().last().unwrap(), "--mcp");
    f.w.copy_setup(false);
    assert!(f.w.copied.as_deref().unwrap().starts_with("claude mcp add --scope user cp2077-ai-mod-manager -- "));
    render(&mut f.w);
    assert!(f.w.copied.is_none()); // the frame put it on the clipboard
    let advice = json!({"summary": "s", "fixes": [[{"title": "t"}, {"label": "Turn A off"}, "w"]], "order": ["a"], "moved": 1});
    for answer in [false, true] {
        f.ask.answer(answer);
        f.w.show_ai(&advice);
        let (_, text, _) = f.ask.last("yes_no").unwrap();
        assert!(text.contains("1 mod(s) change place"));
        assert!(text.contains("t  →  Turn A off"));
        assert_eq!(f.acts().len(), usize::from(answer));
    }
    f.w.show_ai(&json!({"summary": "", "fixes": [], "order": null, "moved": 0}));
    assert!(f.ask.last("info").unwrap().1.contains("nothing to change"));
    f.w.show_ai(&json!({"error": "The local AI did not start."}));
    assert_eq!(f.ask.last("error").unwrap(), ("Local AI".into(), "The local AI did not start.".into(), false));
    assert_eq!(f.ask.count("error"), 1);
}

// test_bug_report_goes_to_the_local_ai
#[test]
fn bug_report_goes_to_the_local_ai() {
    let mut f = fx();
    f.w.ask_bug(); // nothing written
    assert!(f.acts().is_empty());
    assert_eq!(f.ask.count("info"), 1);
    f.w.bug_text = "  My modded car does not show up \n".into();
    f.w.ask_bug();
    assert_eq!(f.acts(), [Act::AskAi("My modded car does not show up".into())]);
    f.w.show_ai(&json!({"summary": "s", "cause": "The car mod is off.", "fixes": [], "order": null, "moved": 0}));
    assert!(f.ask.last("info").unwrap().1.starts_with("Why it happens: The car mod is off."));
}

// test_stalled_pipe_callers_are_cut_off
#[test]
fn stalled_pipe_callers_are_cut_off() {
    let name = pipe();
    let f = fx_with(Some(OwnerListener::new(&name).unwrap()), Nxm::none());
    *lock(&f.w.serve.as_ref().unwrap().timeout) = Duration::from_millis(500);
    let stalls: Vec<Conn> = (0..6).filter_map(|_| Conn::connect(&name).ok()).collect(); // connect, never answer the challenge
    thread::sleep(Duration::from_millis(1500)); // every stalled check has been cut off by now
    let c = client(&name, b"t0k3n").unwrap();
    c.send(b"show").unwrap();
    assert_eq!(c.recv(1 << 20).unwrap(), b"ok");
    drop(stalls);
}

// test_cut_off_caller_is_closed_once
#[test]
fn cut_off_caller_is_closed_once() {
    /// Answers nothing; reading ends only when the time limit closes it.
    struct Stalled {
        closes: Mutex<Vec<String>>,
        cut: (Mutex<bool>, std::sync::Condvar),
    }
    impl Wire for Stalled {
        fn send(&self, _: &[u8]) -> io::Result<()> {
            Ok(())
        }
        fn recv(&self, _: usize) -> io::Result<Vec<u8>> {
            let g = lock(&self.cut.0);
            let _g = self.cut.1.wait_timeout_while(g, Duration::from_secs(5), |cut| !*cut).unwrap();
            Err(io::Error::other("closed"))
        }
        fn close(&self) {
            lock(&self.closes).push(thread::current().name().unwrap_or("").to_string());
            *lock(&self.cut.0) = true;
            self.cut.1.notify_all();
        }
    }
    let f = fx();
    let serve = Serve {
        token: b"t0k3n".to_vec(),
        ctl: f.w.ctl.clone(),
        events: f.w.tx.clone(),
        nxm: Box::new(|_| {}),
        timeout: Mutex::new(Duration::from_millis(200)),
        busy: AtomicUsize::new(0),
    };
    let wire = Arc::new(Stalled { closes: Mutex::new(vec![]), cut: (Mutex::new(false), std::sync::Condvar::new()) });
    serve.take(wire.clone());
    assert_eq!(lock(&wire.closes).len(), 1); // the timer closed it; the handshake thread must not close it again
}

// test_unrequested_link_needs_a_yes
#[test]
fn unrequested_link_needs_a_yes() {
    let mut f = fx();
    for answer in [false, true] {
        let link = json!({"type": "nexus", "mod_id": 9, "file_id": 8});
        f.ask.answer(answer);
        f.w.tx.send(("confirm".into(), json!({"link": link, "text": "Install?"}))).unwrap();
        f.w.drain();
        assert!(f.ask.last("yes_no").unwrap().2); // "No" is the default button
        let adds: Vec<Act> = f.acts().into_iter().filter(|a| matches!(a, Act::AddLink(_))).collect();
        assert_eq!(adds, if answer { vec![Act::AddLink(link)] } else { vec![] });
    }
}

// test_pack_choice_and_outdated_questions
#[test]
fn pack_choice_and_outdated_questions() {
    let mut f = fx();
    let link = json!({"type": "collection", "slug": "abc", "revision": null});
    let files = json!([{"mod_id": 6, "file_id": 60, "optional": true, "name": "Extra", "outdated": ""},
                       {"mod_id": 7, "file_id": 70, "optional": false, "name": "Old", "outdated": "it is old"}]);
    f.w.pack_choice(&json!({"link": link, "name": "P", "files": files}));
    render(&mut f.w);
    f.w.set_pick(70, false); // skip the old mod
    f.w.choice_ok();
    let mut want = link.clone();
    want["chosen"] = json!([60]);
    assert_eq!(f.acts(), [Act::AddLink(want)]);
    let one = json!({"type": "nexus", "mod_id": 3});
    f.w.ask_outdated(&json!({"link": one, "name": "Old", "why": "it is old"}));
    render(&mut f.w);
    f.w.outdated_pick(true);
    f.w.ask_outdated(&json!({"link": one, "name": "Old", "why": "it is old"}));
    f.w.outdated_pick(false);
    let mut want = one.clone();
    want["anyway"] = json!(true);
    assert_eq!(f.acts(), [Act::AddLink(want)]);
}

// test_pipe_squatter_gets_nothing
/// A program that holds the pipe name but does not know the token learns no link.
#[test]
fn pipe_squatter_gets_nothing() {
    let name = pipe();
    let got = Arc::new(Mutex::new(vec![]));
    let ls = OwnerListener::new(&name).unwrap();
    let g = got.clone();
    let t = thread::spawn(move || {
        if let Ok(c) = ls.accept()
            && deliver_challenge(&c, b"guess").and_then(|()| answer_challenge(&c, b"guess")).is_ok()
            && let Ok(m) = c.recv(1 << 20)
        {
            lock(&g).push(m);
        }
    });
    thread::sleep(Duration::from_millis(200));
    assert!(client(&name, b"real-token").and_then(|c| c.send(b"nxm://secret")).is_err());
    t.join().unwrap();
    assert!(lock(&got).is_empty());
}

// test_close_waits_for_a_running_copy_and_select_all
#[test]
fn close_waits_for_a_running_copy_and_select_all() {
    let mut f = fx();
    f.set_game();
    stage(&mut f.store(), mod_rec("a", &["archive/pc/mod/a.archive"]), &[]);
    stage(&mut f.store(), mod_rec("b", &["archive/pc/mod/b.archive"]), &[]);
    let _ = f.store().save();
    f.w.refresh_mods();
    f.w.select_all();
    assert_eq!(f.w.sel, ["a", "b"]);
    assert!(f.w.title().contains(VERSION));
    let released = Arc::new(AtomicBool::new(false));
    let (store, r) = (f.w.ctl.store.clone(), released.clone());
    thread::spawn(move || {
        let _g = lock(&store); // what a deploy holds while it copies
        thread::sleep(Duration::from_millis(500));
        r.store(true, SeqCst);
    });
    thread::sleep(Duration::from_millis(100));
    f.w.close();
    assert!(f.w.ctl.stop.load(SeqCst));
    assert!(f.w.closing && !f.w.closed); // still open (repaints, takes no input) while the copy runs
    render(&mut f.w);
    let end = Instant::now() + Duration::from_secs(5);
    while !f.w.close_when_free() && Instant::now() < end {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(f.w.closed);
    assert!(released.load(SeqCst)); // it closed only after the copy finished
    let store = f.w.ctl.store.clone();
    let other = thread::spawn(move || matches!(store.try_lock(), Err(TryLockError::WouldBlock))).join().unwrap();
    assert!(other); // the window keeps the lock: no other thread can copy now
    let ctl = f.w.ctl.clone();
    let late = thread::spawn(move || {
        ctl.redeploy(true, true); // after close: no new copy starts, not even here
        let _ = ctl.launch(None);
    });
    thread::sleep(Duration::from_millis(300));
    assert!(!late.is_finished());
    let saved: Value = serde_json::from_slice(&std::fs::read(f.tmp.path().join("data/state.json")).unwrap()).unwrap();
    assert!(saved["deployed"].as_object().is_none_or(|d| d.is_empty()));
}

// test_mod_packs_in_the_window
#[test]
fn mod_packs_in_the_window() {
    let mut f = fx();
    f.store().mods = vec![mod_rec("a", &[]), named("b", &[], "b", false)];
    f.w.sync();
    f.w.save_pack();
    if let Some(Dialog::SaveName { text }) = f.w.dialogs.last_mut() {
        *text = " Night ".into();
    }
    render(&mut f.w);
    f.w.save_pack_ok();
    assert_eq!((f.w.pack_pick.as_str(), f.w.pack_values()), ("Night", vec![CURRENT.to_string(), "Night".into()]));
    f.set_game();
    for (pick, pack) in [("Night", Some("Night")), (CURRENT, None)] {
        f.w.pack_pick = pick.into();
        f.w.launch();
        assert_eq!(f.acts(), [Act::Launch(pack.map(String::from))]);
    }
    f.w.ctl.save_pack("Day").unwrap();
    f.w.sync();
    f.w.merge_packs();
    if let Some(Dialog::Merge { picked, .. }) = f.w.dialogs.last_mut() {
        picked.iter_mut().for_each(|p| *p = true);
    }
    render(&mut f.w);
    f.w.merge_ok();
    let merged = f.store().packs.get("Merged pack").cloned();
    assert_eq!((f.w.pack_pick.as_str(), merged), ("Merged pack", Some(vec![("a".into(), true), ("b".into(), false)])));
    f.ask.answer(true);
    f.w.delete_pack(None);
    assert!(!f.store().packs.contains_key("Merged pack"));
    assert_eq!(f.w.pack_pick, CURRENT);
}

// test_settings_names_the_other_app_and_fixes_it_in_one_click
#[test]
fn settings_names_the_other_app_and_fixes_it_in_one_click() {
    let on = Rc::new(Cell::new(false));
    let (a, b) = (on.clone(), on.clone());
    let nxm = Nxm {
        register: Some(Box::new(move || {
            a.set(true);
            Ok(())
        })),
        registered: Box::new(move || b.get()),
        owner: Box::new(|| "Vortex".into()),
    };
    let mut f = fx_with(None, nxm);
    f.w.refresh_settings();
    assert!(f.w.nxm_info().0.contains("sends files to Vortex"));
    assert!(f.w.nxm_btn().1); // the big button
    let job = json!({"state": "waiting", "name": "Nx", "src": {"mod_id": 5, "file_id": 6}, "manual": true}); // the hidden click failed
    lock(&f.w.ctl.jobs).push(job.clone());
    f.w.register_nxm();
    assert_eq!(f.acts(), [Act::Reopen(job)]); // its last click went to Vortex
    assert!(f.w.nxm_info().0.contains("sends files to this app"));
    assert_eq!(f.w.nxm_btn().0, "Set it up again");
}

// test_update_asks_first_and_turns_the_button_back_on
#[test]
fn update_asks_first_and_turns_the_button_back_on() {
    let mut f = fx();
    let rel = json!({"tag": "v9.0.0", "url": "https://x/a.exe"});
    f.w.update_app();
    assert_eq!(f.acts(), [Act::UpdateApp(None)]);
    assert!(f.w.update_busy); // no second update while one runs
    f.ask.answer(false);
    f.w.show_update(&json!({"ask": rel}));
    assert!(!f.w.update_busy);
    f.ask.answer(true);
    f.w.show_update(&json!({"ask": rel}));
    assert_eq!(f.acts(), [Act::UpdateApp(Some(rel))]);
    f.w.show_update(&json!({"text": "Update failed: offline"}));
    assert!(!f.w.update_busy);
}

// test_my_mod_packs_page
#[test]
fn my_mod_packs_page() {
    let mut f = fx();
    {
        let mut st = f.store();
        st.mods = vec![named("a", &[], "Alpha", true)];
        st.packs = [("Mine".to_string(), vec![("a".to_string(), true)]), ("Neon City".into(), vec![("a".into(), false), ("nexus-9-90".into(), true)])]
            .into_iter()
            .collect();
        st.pack_info = json!({"Neon City": {"url": "https://www.nexusmods.com/games/cyberpunk2077/collections/x", "names": {"nexus-9-90": "Neon Signs"}}})
            .as_object()
            .cloned()
            .unwrap();
    }
    f.w.refresh_mods();
    assert_eq!(f.w.pack_cards().len(), 2);
    f.w.packs_show = "nexus";
    let [card] = <[PackCard; 1]>::try_from(f.w.pack_cards()).unwrap();
    assert!(card.sub.contains("1 not installed yet"));
    f.w.toggle_pack_open("Neon City");
    assert!(f.w.pack_cards()[0].lines.iter().any(|l| l.contains("Neon Signs   (not installed yet)")));
    f.w.page = Page::MyPacks;
    render(&mut f.w);
    f.w.complete_pack("Neon City");
    assert_eq!(f.acts(), [Act::CompletePack("Neon City".into())]);
    f.w.packs_show = "all";
    f.w.packs_q = "mine".into();
    f.set_game();
    f.w.use_pack(Some("Mine"));
    assert_eq!(f.acts(), [Act::Redeploy]);
    assert!(f.w.pack_cards()[0].in_use);
    f.w.launch_pack("Mine");
    assert_eq!(f.acts(), [Act::Launch(Some("Mine".into()))]);
}

// test_problem_picker_applies_chosen_fix
#[test]
fn problem_picker_applies_chosen_fix() {
    let mut f = fx();
    let p = json!({"id": "x", "title": "Bad mod crashed", "detail": "d", "severity": "high", "auto": true,
                   "fixes": [{"label": "Update", "action": ["update", "m"], "recommended": true},
                             {"label": "Disable", "action": ["disable", "m"], "recommended": false}]});
    f.w.show_problems(&json!({"list": [p], "crashed": false}));
    assert_eq!(f.w.tab_text(Page::Crash), "Crash doctor (1)");
    assert_eq!(f.w.fix_labels().len(), 2);
    assert_eq!(f.w.fix_choice, 0);
    f.w.page = Page::Crash;
    render(&mut f.w);
    f.w.fix_choice = 1;
    f.w.apply_fix();
    assert_eq!(f.acts(), [Act::Apply(p.clone(), p["fixes"][1].clone())]);
    assert_eq!(f.w.tab_text(Page::Crash), "Crash doctor");
    assert!(f.w.status.contains("Disable"));
}

// New in Rust: a mod picture on disk (here in a folder name with a space) loads in the window.
#[test]
fn mod_picture_loads_from_disk() {
    let t = TempDir::new();
    let dir = t.path().join("thumb s");
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("a_160x90.png");
    image::RgbImage::new(160, 90).save(&file).unwrap();
    let ctx = egui::Context::default();
    egui_extras::install_image_loaders(&ctx);
    let uri = super::util::file_uri(&file);
    for _ in 0..100 {
        match ctx.try_load_image(&uri, Default::default()) {
            Ok(egui::load::ImagePoll::Ready { image }) => return assert_eq!(image.size, [160, 90]),
            Ok(_) => std::thread::sleep(std::time::Duration::from_millis(20)),
            Err(e) => panic!("{uri}: {e}"),
        }
    }
    panic!("the picture did not load");
}

// test_links.py Browse.test_numbers_and_dates
#[test]
fn numbers_and_dates() {
    let got: Vec<String> = [999.0, 1000.0, 248536.0, 20381596.0, 1.5e9].into_iter().map(short).collect();
    assert_eq!(got, ["999", "1K", "248.5K", "20.4M", "1.5B"]);
    let now = util::parse_iso("2026-11-20T00:00:00Z");
    let got: Vec<String> =
        ["2026-11-20T01:00:00Z", "2026-11-19T00:00:00Z", "2026-11-10", "2026-08-01", "2023-01-01", "bad"].into_iter().map(|d| ago(d, now)).collect();
    assert_eq!(got, ["today", "1 day ago", "10 days ago", "3 months ago", "3 years ago", ""]);
}

/// Every page and every dialog draws without a panic, in each style, also while closing.
#[test]
fn every_page_and_dialog_renders() {
    let mut f = fx();
    f.set_game();
    stage(&mut f.store(), named("a", &["archive/pc/mod/a.archive"], "Alpha HUD", true), &[]);
    f.store().packs.insert("Mine".into(), vec![("a".into(), true)]);
    f.w.refresh_mods();
    f.w.show_problems(&json!({"list": [{"id": "x", "title": "T", "detail": "d", "severity": "medium", "fixes": [{"label": "L"}]}], "crashed": false}));
    for t in &THEMES {
        f.w.set_theme(t.name);
        for p in Page::ALL {
            f.w.page = p;
            render(&mut f.w);
        }
    }
    let link = json!({"type": "nexus", "mod_id": 3});
    f.w.add_dialog();
    f.w.save_pack();
    f.w.ctl.save_pack("Day").unwrap();
    f.w.sync();
    f.w.merge_packs();
    f.w.pack_choice(&json!({"link": link, "name": "P", "files": [{"file_id": 1, "name": "E", "optional": true, "outdated": ""}]}));
    f.w.ask_outdated(&json!({"link": link, "name": "Old", "why": "it is old"}));
    while !f.w.dialogs.is_empty() {
        render(&mut f.w);
        f.w.dialogs.pop();
    }
    f.w.closing = true;
    render(&mut f.w);
}
