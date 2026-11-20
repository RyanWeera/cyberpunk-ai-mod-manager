//! End-to-end crash check with a REAL crash (tests/e2e_crash.py; Windows only, about 1 minute, ignored by
//! default: run with `cargo test -p manager_ui -- --ignored`).
//!
//! The fake_game example, copied as Cyberpunk2077.exe into a fake game folder, crashes inside a mod DLL (a
//! copy of vcruntime140.dll, made to read address 8) and writes a minidump like the game's crash reporter.
//! The real watcher (exit code), the minidump reader and the real pipeline must then:
//!   1. find the mod and turn it off by itself (one fix only),
//!   2. on a second crash with an update available, ask the user instead (two fixes),
//!   3. after a normal exit, save the mod set as the last good one.
use manager_ui::controller::{Controller, Deps};
use serde_json::Value;
use shared::{Mod, Store};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::sleep;
use std::time::{Duration, Instant};
use testkit::{TempDir, make_zip};

type Events = Arc<Mutex<Vec<(String, Value)>>>;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn until(what: &str, f: impl Fn() -> bool) {
    let end = Instant::now() + Duration::from_secs(120);
    while !f() {
        assert!(Instant::now() < end, "FAIL: timed out waiting for {what}");
        sleep(Duration::from_millis(500));
    }
}

fn enabled(c: &Controller, id: &str) -> bool {
    lock(&c.store).mods.iter().any(|m| m.id == id && m.enabled)
}

/// Start the 'game' like a launcher would; the manager only watches.
fn play(bin64: &Path, plugin: &Path, events: &Events, mode: &str) -> Vec<(String, Value)> {
    let n = lock(events).len();
    let code = Command::new(bin64.join("Cyberpunk2077.exe")).args([mode, &plugin.to_string_lossy()]).current_dir(bin64).status().unwrap();
    println!("game exit code: {:#x}", code.code().unwrap_or(0) as u32);
    let end = Instant::now() + Duration::from_secs(40);
    while Instant::now() < end {
        let new: Vec<(String, Value)> = lock(events)[n..].to_vec();
        let closed = |(k, p): &(String, Value)| k == "log" && p.as_str().is_some_and(|s| s.contains("closed normally"));
        if new.iter().any(|(k, _)| k == "problems") || new.iter().any(closed) {
            sleep(Duration::from_millis(1500)); // let the redeploy finish
            return new;
        }
        sleep(Duration::from_millis(500));
    }
    panic!("FAIL: no reaction after the game exited: {:?}", &lock(events)[n..]);
}

#[test]
#[ignore = "real crash, about 1 minute"]
fn e2e_crash() {
    let tmp = TempDir::new();
    let game = tmp.path().join("Cyberpunk 2077");
    let bin64 = game.join("bin/x64");
    std::fs::create_dir_all(&bin64).unwrap();
    std::fs::create_dir_all(game.join("archive/pc/mod")).unwrap();
    let me = std::env::current_exe().unwrap();
    let fake: PathBuf = me.ancestors().map(|d| d.join("examples/fake_game.exe")).find(|p| p.exists()).expect("build the fake_game example first");
    std::fs::copy(&fake, bin64.join("Cyberpunk2077.exe")).unwrap();

    // SAFETY: set before any thread of this test reads the environment; crash reports go to a temp REDEngine\ReportQueue.
    unsafe { std::env::set_var("LOCALAPPDATA", tmp.path().join("local")) };
    let mut store = Store::open(tmp.path().join("data")).unwrap();
    store.settings.game_dir = game.to_string_lossy().into_owned();
    store.settings.virus_scan = true;
    store.settings.auto_fix = true;
    let mut fw = Mod::new("fw-red4ext", &[]); // pretend it is installed
    (fw.name, fw.version, fw.framework) = ("RED4ext".into(), "x".into(), Some("RED4ext".into()));
    store.mods.push(fw);

    let events: Events = Arc::default();
    let rec = events.clone();
    let update = Arc::new(AtomicBool::new(false));
    let up = update.clone();
    let deps = Deps { has_update: Some(Arc::new(move |_: &Mod| up.load(SeqCst))), ..Deps::default() };
    let ctl = Controller::new(store, Arc::new(move |k: &str, p: Value| lock(&rec).push((k.into(), p))), 3, true, deps);

    let vcrt = std::fs::read(r"C:\Windows\System32\vcruntime140.dll").unwrap();
    ctl.add_links(&make_zip(tmp.path().join("Bad Plugin.zip"), &[("BadPlugin/BadPlugin.dll", &vcrt)]).to_string_lossy());
    ctl.add_links(&make_zip(tmp.path().join("Good Mod.zip"), &[("good.archive", b"x")]).to_string_lossy());
    let plugin = game.join("red4ext/plugins/BadPlugin/BadPlugin.dll");
    until("the installs", || lock(&ctl.store).mods.len() == 3 && plugin.exists());
    let bad = lock(&ctl.store).mods.iter().find(|m| m.name == "Bad Plugin").unwrap().id.clone();
    println!("installed: {:?} | plugin deployed", lock(&ctl.store).mods.iter().map(|m| m.name.clone()).collect::<Vec<_>>());

    // 1. Real crash, one possible fix -> fixed without asking.
    let new = play(&bin64, &plugin, &events, "crash");
    println!("1: {:?}", new.iter().filter(|(k, _)| k == "log").map(|(_, p)| p).collect::<Vec<_>>());
    assert!(!enabled(&ctl, &bad), "bad mod should be off");
    assert!(!plugin.exists(), "plugin should be removed from the game");
    assert!(game.join("archive/pc/mod/good.archive").exists(), "good mod must stay");
    println!("1 OK: crash traced to Bad Plugin and fixed by itself");

    // 2. Real crash with an update available -> two fixes -> the user picks.
    let last = lock(&ctl.store).doctor["history"].as_array().unwrap().len() - 1;
    ctl.undo(last).unwrap();
    update.store(true, SeqCst);
    until("the plugin to come back", || plugin.exists());
    let new = play(&bin64, &plugin, &events, "crash");
    let asked = new.iter().find(|(k, _)| k == "problems").unwrap().1.clone();
    let prob = asked["list"].as_array().unwrap().iter().find(|p| p["id"] == format!("crash-{bad}")).unwrap().clone();
    let labels: Vec<&Value> = prob["fixes"].as_array().unwrap().iter().map(|f| &f["label"]).collect();
    println!("2: asked the user: {} -> {labels:?}", prob["title"]);
    assert!(enabled(&ctl, &bad), "with two fixes nothing is applied without the user");
    ctl.apply(&prob, &prob["fixes"][1]).unwrap(); // the user picks "Disable"
    assert!(!enabled(&ctl, &bad));
    println!("2 OK: user picked {}", prob["fixes"][1]["label"]);

    // 3. Normal exit -> this setup becomes the last good one.
    sleep(Duration::from_secs(2));
    play(&bin64, &plugin, &events, "clean");
    let good = lock(&ctl.store).doctor.get("last_good").cloned().unwrap_or_default();
    assert!(good.as_object().is_some_and(|o| !o.is_empty()), "last good setup should be saved");
    println!("3 OK: normal exit saved as last good: {good}");
    ctl.close();
    println!("E2E PASSED");
}
