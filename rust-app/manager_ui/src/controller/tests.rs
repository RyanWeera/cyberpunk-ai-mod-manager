//! The controller pipeline, offline: every HTTP request goes to a fake (tests/test_controller.py, the Wiring
//! class of tests/test_browser.py and one test of tests/test_video.py).
use super::*;
use downloads::fetch::{Request, Response};
use std::collections::HashMap;
use std::sync::atomic::AtomicUsize;
use std::time::Instant;
use testkit::{TempDir, fake_game, make_zip, mod_rec};

/// (program, environment, arguments) of a started process.
type Spawned = (PathBuf, HashMap<String, String>, Vec<String>);

/// What the fake dependencies saw, and what they answer.
#[derive(Default)]
struct Rec {
    events: Mutex<Vec<(String, Value)>>,
    blocks: Mutex<Vec<bool>>,
    timers: AtomicUsize,
    opened: Mutex<Vec<String>>,
    spawned: Mutex<Vec<Spawned>>,
    spawn_err: Mutex<Option<i32>>,
    run_as: Mutex<Vec<(PathBuf, Vec<String>, PathBuf)>>,
    scan: Mutex<(Option<bool>, String)>,
    starts: AtomicUsize,
    start_err: Mutex<Option<String>>,
    advice: Mutex<Option<Advice>>,
    reports: Mutex<Vec<String>>,
    fake_deploy: AtomicBool,
    evidence: Mutex<Value>,
    dumps: Mutex<Vec<PathBuf>>,
    clicked: Mutex<Vec<Value>>,
    /// (url, body) of every HTTP request.
    asked: Mutex<Vec<(String, String)>>,
}

/// The Python setUp: a fake game, a store with virus scans off, and fakes for the AI, timers and processes.
struct Fx {
    tmp: TempDir,
    game: PathBuf,
    r: Arc<Rec>,
    ctls: Mutex<Vec<Arc<Controller>>>,
}

impl Drop for Fx {
    fn drop(&mut self) {
        for c in lock(&self.ctls).iter() {
            c.close();
        }
        // threads still running fail instead of reaching the network
        fetch::set_mock(Some(Arc::new(|r: &Request| Err(format!("no network in tests: {}", r.url)))));
        video_finder::set_resolver(None);
    }
}

fn ok(v: Value) -> Response {
    Response { status: 200, url: "https://api.test/".into(), body: v.to_string().into_bytes(), ..Default::default() }
}

fn file(url: &str, body: Vec<u8>) -> Response {
    Response { status: 200, url: url.into(), body, ..Default::default() }
}

/// (query, variables) of a Nexus GraphQL request.
fn gql_of(req: &Request) -> Option<(String, Value)> {
    if req.url != nexus::GQL {
        return None;
    }
    let b: Value = serde_json::from_slice(req.body.as_deref()?).ok()?;
    Some((b["query"].as_str()?.to_string(), b["variables"].clone()))
}

/// The GraphQL answer of `nexus::mod_info`.
fn mod_gql(name: &str, version: &str, files: Value, requires: &[i64], updated: &str) -> Value {
    let nodes: Vec<Value> =
        requires.iter().map(|r| json!({"modId": r, "modName": r.to_string(), "gameId": nexus::GAME_ID, "externalRequirement": false})).collect();
    json!({"data": {"mod": {"name": name, "version": version, "status": "published", "updatedAt": updated,
        "modRequirements": {"nexusRequirements": {"nodes": nodes}}}, "modFiles": files}})
}

/// The GraphQL answer of `nexus::collection`: (mod id, file id, optional, mod name, last update).
fn coll_gql(name: &str, files: &[(i64, i64, bool, &str, &str)]) -> Value {
    let mf: Vec<Value> = files
        .iter()
        .map(|(m, f, opt, n, up)| {
            json!({"fileId": f, "optional": opt, "file": {"name": n, "version": "1",
                "mod": {"modId": m, "name": n, "status": "published", "updatedAt": up}}})
        })
        .collect();
    json!({"data": {"collectionRevision": {"revisionNumber": 1, "collection": {"name": name}, "modFiles": mf}}})
}

fn nx_files(version: &str) -> Value {
    json!([{"fileId": 7, "name": "Nx", "version": version, "category": "MAIN", "primary": 1, "date": 1}])
}

/// Nexus for one mod: its info, the key check, the download link and the file behind it.
fn nexus_route(info: Value, premium: bool, zip: PathBuf) -> impl Fn(&Request) -> Option<Response> + Send + Sync + 'static {
    move |req| {
        if let Some((q, _)) = gql_of(req)
            && q.contains("modFiles(modId")
        {
            return Some(ok(info.clone()));
        }
        if req.url.ends_with("/users/validate.json") {
            return Some(ok(json!({"name": "u", "is_premium": premium})));
        }
        if req.url.contains("/download_link.json") {
            return Some(ok(json!([{"URI": "https://cdn.test/nx.zip"}])));
        }
        (req.url == "https://cdn.test/nx.zip").then(|| file(&req.url, std::fs::read(&zip).unwrap()))
    }
}

fn zip(path: PathBuf, files: &[(&str, &str)]) -> PathBuf {
    let f: Vec<(&str, &[u8])> = files.iter().map(|(n, b)| (*n, b.as_bytes())).collect();
    make_zip(path, &f)
}

fn m(id: &str) -> Mod {
    mod_rec(id, &[])
}

fn named(id: &str, name: &str) -> Mod {
    Mod { name: name.into(), ..m(id) }
}

fn fw_mod(id: &str, fw: &str) -> Mod {
    Mod { framework: Some(fw.into()), ..m(id) }
}

fn off(mut x: Mod) -> Mod {
    x.enabled = false;
    x
}

impl Fx {
    fn new() -> Fx {
        let tmp = TempDir::new();
        let game = fake_game(tmp.path());
        // Advanced Crash Reporter is in place, so `ensure_acr` adds nothing in the other tests.
        let acr = game.join(doctor::ACR).parent().unwrap().to_path_buf();
        std::fs::create_dir_all(acr.join("reports")).unwrap();
        std::fs::write(acr.join("AdvancedCrashReporter.dll"), b"MZ").unwrap();
        let r = Arc::new(Rec::default());
        *lock(&r.evidence) = json!({"crashed": false});
        let fx = Fx { tmp, game, r, ctls: Mutex::new(vec![]) };
        fx.web(|_| None);
        fx
    }

    fn store(&self) -> Store {
        let mut s = Store::open(self.tmp.path().join("data")).unwrap();
        s.settings.game_dir = self.game.to_string_lossy().into_owned();
        s.settings.virus_scan = false;
        s
    }

    /// Answer HTTP with `route`; GitHub copies and compatibility patches are found nowhere, the rest fails.
    fn web(&self, route: impl Fn(&Request) -> Option<Response> + Send + Sync + 'static) {
        let r = self.r.clone();
        fetch::set_mock(Some(Arc::new(move |req: &Request| {
            let body = req.body.as_deref().map(|b| String::from_utf8_lossy(b).into_owned()).unwrap_or_default();
            lock(&r.asked).push((req.url.clone(), body));
            if let Some(resp) = route(req) {
                return Ok(resp);
            }
            if let Some((q, _)) = gql_of(req) {
                if q.contains("description") {
                    return Ok(ok(json!({"data": {"mod": {"name": "x", "description": ""}}})));
                }
                if q.contains("modsRequiringThisMod") {
                    return Ok(ok(json!({"data": {"mod": {"modRequirements": {"modsRequiringThisMod": {"totalCount": 0, "nodes": []}}}}})));
                }
            }
            Err(format!("no network in tests: {}", req.url))
        })));
    }

    fn deps(&self) -> Deps {
        let r = self.r.clone();
        let (r1, r2, r3, r4, r5, r6, r7, r8, r9, r10, r11) =
            (r.clone(), r.clone(), r.clone(), r.clone(), r.clone(), r.clone(), r.clone(), r.clone(), r.clone(), r.clone(), r.clone());
        Deps {
            scan: Arc::new(move |_| lock(&r1.scan).clone()),
            deploy: Arc::new(move |s| if r2.fake_deploy.load(SeqCst) { Ok(0) } else { deploy::deploy(s) }),
            open_url: Arc::new(move |u| lock(&r3.opened).push(u.to_string())),
            spawn: Arc::new(move |cmd| {
                let envs = cmd.get_envs().filter_map(|(k, v)| Some((k.to_string_lossy().into_owned(), v?.to_string_lossy().into_owned()))).collect();
                let args = cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
                lock(&r4.spawned).push((PathBuf::from(cmd.get_program()), envs, args));
                match *lock(&r4.spawn_err) {
                    Some(code) => Err(io::Error::from_raw_os_error(code)),
                    None => Ok(()),
                }
            }),
            run_as: Arc::new(move |exe, args, cwd| {
                lock(&r5.run_as).push((exe.to_path_buf(), args.to_vec(), cwd.to_path_buf()));
                Ok(())
            }),
            block_ai: Arc::new(move |b| lock(&r6.blocks).push(b)),
            start_ai: Arc::new(move || {
                r7.starts.fetch_add(1, SeqCst);
                lock(&r7.start_err).clone().map_or(Ok(()), Err)
            }),
            advise: Arc::new(move |_, _, report| {
                lock(&r8.reports).push(report.to_string());
                lock(&r8.advice).clone().ok_or_else(|| "no advice".to_string())
            }),
            timer: Arc::new(move |_, _| {
                r9.timers.fetch_add(1, SeqCst);
            }),
            crash_evidence: Arc::new(move |_, _| lock(&r10.evidence).clone()),
            read_dump: Arc::new(move |p| {
                lock(&r11.dumps).push(p.to_path_buf());
                Some(json!({"stack": []}))
            }),
            ..Deps::default()
        }
    }

    fn ctl(&self, store: Store, workers: usize) -> Arc<Controller> {
        self.ctl_with(store, workers, self.deps())
    }

    fn ctl_with(&self, store: Store, workers: usize, deps: Deps) -> Arc<Controller> {
        let r = self.r.clone();
        let c = Controller::new(store, Arc::new(move |k: &str, p: Value| lock(&r.events).push((k.into(), p))), workers, false, deps);
        lock(&self.ctls).push(c.clone());
        c
    }

    fn events(&self, kind: &str) -> Vec<Value> {
        lock(&self.r.events).iter().filter(|(k, _)| k == kind).map(|(_, p)| p.clone()).collect()
    }
}

fn wait_for(f: impl Fn() -> bool) -> bool {
    let end = Instant::now() + Duration::from_secs(20);
    while Instant::now() < end {
        if f() {
            return true;
        }
        thread::sleep(Duration::from_millis(50));
    }
    false
}

fn statuses(c: &Controller) -> Vec<String> {
    lock(&c.jobs).iter().map(|j| text(&j["status"])).collect()
}

/// Every job settled, and every worker finished its job and the follow-up deploy.
fn wait_idle(c: &Controller) {
    let settled = || {
        let jobs = lock(&c.jobs);
        !jobs.is_empty() && jobs.iter().all(|j| ["done", "failed", "waiting", "needkey"].contains(&j["state"].as_str().unwrap_or(""))) && c.idle()
    };
    assert!(wait_for(settled), "{:?}", statuses(c));
}

fn set_mods(c: &Controller, mods: Vec<Mod>) {
    lock(&c.store).mods = mods;
}

fn states(c: &Controller) -> Vec<(String, bool)> {
    lock(&c.store).mods.iter().map(|m| (m.id.clone(), m.enabled)).collect()
}

fn mod_ids(c: &Controller) -> Vec<String> {
    ids(&lock(&c.store).mods)
}

fn enabled_ids(c: &Controller) -> Vec<String> {
    lock(&c.store).enabled().iter().map(|m| m.id.clone()).collect()
}

fn job0(c: &Controller) -> Value {
    lock(&c.jobs)[0].clone()
}

fn pairs(v: &[(&str, bool)]) -> Vec<(String, bool)> {
    v.iter().map(|(i, e)| (i.to_string(), *e)).collect()
}

fn set_readonly(p: &Path, on: bool) {
    let mut perm = std::fs::metadata(p).unwrap().permissions();
    perm.set_readonly(on);
    std::fs::set_permissions(p, perm).unwrap();
}

// ---------- tests/test_controller.py ----------

#[test]
fn local_file_installs_its_frameworks_and_deploys() {
    // test_local_file_installs_its_frameworks_and_deploys
    let fx = Fx::new();
    let t = fx.tmp.path();
    let zips: HashMap<String, Vec<u8>> = [
        ("psiberx/cp2077-tweak-xl", zip(t.join("tweak.zip"), &[("red4ext/plugins/TweakXL/TweakXL.dll", "x")])),
        ("jac3km4/redscript", zip(t.join("rs.zip"), &[("engine/tools/scc.exe", "x")])),
        ("wopss/RED4ext", zip(t.join("r4.zip"), &[("red4ext/RED4ext.dll", "x"), ("bin/x64/winmm.dll", "x")])),
    ]
    .into_iter()
    .map(|(r, p)| (r.to_string(), std::fs::read(p).unwrap()))
    .collect();
    fx.web(move |req| {
        if let Some(repo) = req.url.strip_prefix("https://api.github.com/repos/").and_then(|u| u.strip_suffix("/releases/latest")) {
            let url = format!("https://dl.test/{repo}/x.zip");
            return Some(ok(json!({"tag_name": "v1", "assets": [{"name": "x.zip", "browser_download_url": url}]})));
        }
        let repo = req.url.strip_prefix("https://dl.test/")?.strip_suffix("/x.zip")?;
        Some(file(&req.url, zips.get(repo)?.clone()))
    });
    let c = fx.ctl(fx.store(), 2);
    c.add_links(&zip(t.join("My Tweak.zip"), &[("tweaks/a.yaml", "x"), ("s/a.reds", "x")]).to_string_lossy());
    wait_idle(&c);
    let mut names: Vec<String> = lock(&c.store).mods.iter().map(|m| m.name.clone()).collect();
    names.sort();
    assert_eq!(names, ["My Tweak", "RED4ext", "TweakXL", "redscript"]);
    assert!(fx.game.join("r6/tweaks/My Tweak/a.yaml").exists());
    assert!(fx.game.join("red4ext/plugins/TweakXL/TweakXL.dll").exists());
    assert_eq!(lock(&c.store).mods.last().unwrap().name, "My Tweak"); // frameworks go first, mods last
}

#[test]
fn files_from_the_pc_stay_untouched_even_read_only_or_in_downloads() {
    // test_files_from_the_pc_stay_untouched_even_read_only_or_in_downloads
    let fx = Fx::new();
    let ro = zip(fx.tmp.path().join("ro.zip"), &[("a.archive", "x")]);
    set_readonly(&ro, true);
    let store = fx.store();
    let downloads = store.downloads.clone();
    let inside = zip(downloads.join("b.zip"), &[("b.archive", "x")]); // picked again from the app's own folder
    let c = fx.ctl(store, 2);
    c.add_links(&format!("{}\n{}", ro.display(), inside.display()));
    wait_idle(&c);
    let ro_still = ro.exists() && std::fs::metadata(&ro).unwrap().permissions().readonly();
    set_readonly(&ro, false);
    assert_eq!(statuses(&c), ["Done", "Done"]);
    assert!(ro_still);
    assert!(inside.exists());
    let left: Vec<String> = std::fs::read_dir(&downloads).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    assert_eq!(left, ["b.zip"]); // job folders are cleaned up
}

#[test]
fn job_folders_left_by_a_cut_off_close_are_removed_at_start() {
    // test_job_folders_left_by_a_cut_off_close_are_removed_at_start
    let fx = Fx::new();
    let store = fx.store();
    let old = store.downloads.join("job-old");
    std::fs::create_dir_all(&old).unwrap();
    std::fs::write(old.join("archive.zip"), "x").unwrap();
    fx.ctl(store, 2);
    assert!(!old.exists());
}

#[test]
fn unrequested_nxm_link_asks_first() {
    // test_unrequested_nxm_link_asks_first
    let fx = Fx::new();
    fx.web(|req| {
        let (q, _) = gql_of(req)?;
        if q.contains("modFiles(modId") {
            return Some(ok(mod_gql("Shady Mod", "1", json!([]), &[], "2024-06-01")));
        }
        q.contains("collection(slug").then(|| ok(json!({"data": {"collection": {"name": "Night City Pack", "endorsements": 1}}})))
    });
    let c = fx.ctl(fx.store(), 2);
    c.handle_nxm("nxm://cyberpunk2077/mods/9/files/8?key=a&expires=1&x= https://evil.example/p.zip").unwrap();
    c.handle_nxm("nxm://cyberpunk2077/collections/abcd/revisions/1").unwrap();
    assert!(lock(&c.jobs).is_empty()); // nothing queued without the user's yes
    let asked = fx.events("confirm");
    assert!(text(&asked[0]["text"]).contains("Shady Mod"));
    assert_eq!(asked[0]["link"]["type"], "nexus"); // the hidden second line is not part of what a yes adds
    assert!(text(&asked[1]["text"]).contains("collection 'Night City Pack'"));
}

#[test]
fn advanced_crash_reporter_is_always_added_once() {
    let fx = Fx::new();
    let z = zip(fx.tmp.path().join("acr.zip"), &[("a.archive", "x")]);
    fx.web(nexus_route(mod_gql("Advanced Crash Reporter", "1", nx_files("1"), &[], "2024-06-01"), false, z));
    let mut store = fx.store();
    store.settings.api_key = "k".into();
    store.settings.auto_click = false;
    let c = fx.ctl(store, 2);
    let adding = || fx.events("log").iter().filter(|l| text(l).contains("Adding Advanced Crash Reporter")).count();
    c.ensure_acr();
    assert_eq!(adding(), 0); // already in the game folder
    std::fs::remove_file(fx.game.join(doctor::ACR).parent().unwrap().join("AdvancedCrashReporter.dll")).unwrap(); // reports stay
    c.ensure_acr();
    wait_idle(&c);
    c.ensure_acr();
    wait_idle(&c);
    let jobs = lock(&c.jobs).clone();
    assert_eq!(jobs.len(), 1, "{jobs:?}");
    assert_eq!(jobs[0]["src"]["mod_id"], doctor::ACR_NEXUS);
    lock(&c.jobs)[0]["state"] = json!("failed"); // the user skipped it: not queued again in this run
    c.ensure_acr();
    assert_eq!((lock(&c.jobs).len(), adding()), (1, 1));
}

#[test]
fn advanced_crash_reporter_stays_on_with_a_pack() {
    let fx = Fx::new();
    let mut acr = mod_rec("nexus-34403-1", &["bin/x64/plugins/AdvancedCrashReporter/a.dll"]);
    acr.source = json!({"type": "nexus", "mod_id": doctor::ACR_NEXUS, "file_id": 1});
    let mut store = fx.store();
    store.mods = vec![acr, mod_rec("a", &["archive/pc/mod/a.archive"])];
    store.packs.insert("P".into(), pairs(&[("a", true)]));
    let c = fx.ctl(store, 0);
    c.use_pack("P").unwrap();
    assert_eq!(enabled_ids(&c), ["nexus-34403-1", "a"]); // a pack that does not list it keeps it on
    lock(&c.store).packs.insert("Q".into(), pairs(&[("nexus-34403-1", false), ("a", true)])); // saved while it was off
    c.use_pack("Q").unwrap();
    assert_eq!(enabled_ids(&c), ["nexus-34403-1", "a"]);
    lock(&c.store).mods[0].enabled = false; // turned off before 2.0
    c.ensure_acr();
    assert!(lock(&c.store).mods[0].enabled);
    assert!(lock(&c.jobs).is_empty());
}

#[test]
fn free_account_waits_for_click_then_downloads() {
    // test_free_account_waits_for_click_then_downloads
    let fx = Fx::new();
    let z = zip(fx.tmp.path().join("nx.zip"), &[("a.archive", "x")]);
    fx.web(nexus_route(mod_gql("Nx", "1", nx_files("1"), &[], "2024-06-01"), false, z));
    let mut store = fx.store();
    store.settings.api_key = "k".into();
    store.settings.auto_click = false; // the manual-click way: the hidden browser is in the Wiring tests
    let c = fx.ctl(store, 2);
    c.add_links("https://www.nexusmods.com/cyberpunk2077/mods/55");
    wait_idle(&c);
    assert_eq!(job0(&c)["state"], "waiting");
    assert!(lock(&fx.r.opened).last().unwrap().contains("mods/55?tab=files&file_id=7"));
    c.handle_nxm("nxm://cyberpunk2077/mods/55/files/7?key=K&expires=1").unwrap();
    wait_idle(&c);
    assert_eq!(job0(&c)["state"], "done");
    assert!(fx.game.join("archive/pc/mod/a.archive").exists());
    assert!(lock(&c.store).mods[0].source.get("key").is_none()); // download keys are not stored
}

#[test]
fn a_failed_automatic_click_asks_the_user() {
    // test_a_failed_automatic_click_asks_the_user
    let fx = Fx::new();
    let c = fx.ctl(fx.store(), 2);
    let j = json!({"id": 99, "name": "Nx", "state": "waiting", "status": "", "progress": "", "src": {"mod_id": 55, "file_id": 7}});
    lock(&c.jobs).push(j.clone());
    c.manual_click(&j, "no login");
    let j = job0(&c);
    assert_eq!(j["manual"], true); // the window shows the manual-click banner for this job
    assert_eq!(j["status"], "Click 'Slow download' in your browser");
    assert!(lock(&fx.r.opened).last().unwrap().contains("mods/55?tab=files&file_id=7"));
}

#[test]
fn free_account_gets_the_same_version_from_github_without_a_click() {
    // test_free_account_gets_the_same_version_from_github_without_a_click
    let fx = Fx::new();
    let z = zip(fx.tmp.path().join("gh.zip"), &[("a.archive", "x")]);
    let gh = "https://github.com/a/equipment-ex/releases/download/v1.3/E.zip";
    let files = json!([{"fileId": 7, "name": "E", "version": "1.3", "category": "MAIN", "primary": 1, "date": 1}]);
    let nx = nexus_route(mod_gql("Equipment-EX", "1.3", files, &[], "2024-06-01"), false, z.clone());
    fx.web(move |req| {
        if let Some((q, _)) = gql_of(req)
            && q.contains("description")
        {
            return Some(ok(json!({"data": {"mod": {"name": "Equipment-EX", "description": "Source: https://github.com/a/equipment-ex"}}})));
        }
        if req.url == "https://api.github.com/repos/a/equipment-ex/releases/latest" {
            return Some(ok(json!({"tag_name": "v1.3", "assets": [{"name": "E.zip", "browser_download_url": gh}]})));
        }
        if req.url == gh {
            return Some(file(gh, std::fs::read(&z).unwrap()));
        }
        nx(req)
    });
    let mut store = fx.store();
    store.settings.api_key = "k".into();
    let c = fx.ctl(store, 2);
    c.add_links("https://www.nexusmods.com/cyberpunk2077/mods/6945");
    wait_idle(&c);
    let asked = lock(&fx.r.asked).clone();
    let copies: Vec<&(String, String)> = asked.iter().filter(|(_, b)| b.contains("description")).collect();
    assert_eq!(copies.len(), 1);
    assert!(copies[0].1.contains(r#""id":"6945""#));
    let got: Vec<&str> = asked.iter().map(|(u, _)| u.as_str()).filter(|u| !u.starts_with("https://api.")).collect();
    assert_eq!((job0(&c)["state"].clone(), got), (json!("done"), vec![gh]));
    assert!(lock(&fx.r.opened).is_empty());
    let st = lock(&c.store);
    assert_eq!(st.mods[0].id, "nexus-6945-7"); // still the Nexus mod: packs and updates find it
    assert!(st.mods[0].source.get("github").is_none());
}

#[test]
fn jobs_wait_for_key_then_premium_needs_no_click() {
    // test_jobs_wait_for_key_then_premium_needs_no_click
    let fx = Fx::new();
    let z = zip(fx.tmp.path().join("nx.zip"), &[("a.archive", "x")]);
    fx.web(nexus_route(mod_gql("Nx", "1", nx_files("1"), &[], "2024-06-01"), true, z));
    let c = fx.ctl(fx.store(), 2);
    c.add_links("https://www.nexusmods.com/cyberpunk2077/mods/55");
    wait_idle(&c);
    assert_eq!(job0(&c)["state"], "needkey");
    lock(&c.store).settings.api_key = "k".into();
    c.check_key();
    wait_idle(&c);
    assert_eq!(job0(&c)["state"], "done");
    assert!(lock(&fx.r.opened).is_empty());
}

#[test]
fn compatibility_patch_is_added_only_for_mods_the_user_has() {
    // test_compatibility_patch_is_added_only_for_mods_the_user_has
    let fx = Fx::new();
    let mut store = fx.store();
    store.mods = vec![Mod { source: json!({"type": "nexus", "mod_id": 20, "file_id": 1}), ..named("w", "WINGDEER HAIR COLLECTION") }];
    let mods: HashMap<i64, (&str, Vec<i64>)> = [
        (10, ("EKT", vec![])),
        (30, ("EKT - Preem hair - compatability patch", vec![10, 20])), // needs both: added
        (31, ("Female Corpo ACT 1 Patch 1.5", vec![10])),               // a game-patch save, not a compatibility patch
        (32, ("EKT - Other Hair Patch", vec![10, 99])),                 // needs a mod the user does not have
        (33, ("EKT - WINGDEER HAIR COLLECTION patch", vec![10])),       // names the user's mod: added
        (34, ("Random Compatibility Patch", vec![10])),                 // names no mod of the user's
    ]
    .into();
    fx.web(move |req| {
        let (q, v) = gql_of(req)?;
        let id: i64 = v["id"].as_str()?.parse().ok()?;
        if q.contains("modsRequiringThisMod") {
            let nodes: Vec<Value> = if id == 10 {
                [30, 31, 32, 33, 34].iter().map(|i| json!({"modId": i, "modName": mods[i].0, "gameId": nexus::GAME_ID})).collect()
            } else {
                vec![]
            };
            let n = nodes.len();
            return Some(ok(json!({"data": {"mod": {"modRequirements": {"modsRequiringThisMod": {"totalCount": n, "nodes": nodes}}}}})));
        }
        let files = json!([{"fileId": 1, "name": "x", "version": "1", "category": "MAIN", "primary": 1, "date": 1}]);
        q.contains("modFiles(modId").then(|| ok(mod_gql(mods[&id].0, "1", files, &mods[&id].1, "2024-06-01")))
    });
    let c = fx.ctl(store, 0);
    c.add(&json!({"type": "nexus", "mod_id": 10})).unwrap();
    let names: Vec<String> = lock(&c.jobs).iter().map(|j| text(&j["name"])).collect();
    assert_eq!(names, ["EKT", "EKT - Preem hair - compatability patch", "EKT - WINGDEER HAIR COLLECTION patch"]);
}

#[test]
fn job_ids_stay_unique_and_skip_retry_respect_state() {
    // test_job_ids_stay_unique_and_skip_retry_respect_state
    let fx = Fx::new();
    let c = fx.ctl(fx.store(), 0); // no workers: jobs stay queued
    let (a, b) = (job_id(&c.job("a", json!({"type": "file"}), json!({}))), job_id(&c.job("b", json!({"type": "file"}), json!({}))));
    lock(&c.jobs).retain(|j| job_id(j) == b); // what "Clear finished" leaves
    let n = job_id(&c.job("c", json!({"type": "file"}), json!({})));
    assert!(n != a && n != b);
    assert!(!c.retry(b)); // queued, not failed
    assert!(c.skip(b));
    assert_eq!(c.state_of(b).as_deref(), Some("failed"));
    c.edit(b, |j| j["state"] = json!("running"));
    assert!(!c.skip(b)); // cannot skip a running download
}

#[test]
fn skipped_job_is_not_run() {
    // test_skipped_job_is_not_run
    let fx = Fx::new();
    let c = fx.ctl(fx.store(), 0);
    let id = job_id(&c.job("x", json!({"type": "file", "path": "nope.zip"}), json!({})));
    c.skip(id);
    let w = c.clone();
    thread::spawn(move || w.worker());
    assert!(wait_for(|| c.idle()));
    assert_eq!(statuses(&c), ["Skipped"]);
}

#[test]
fn fix_and_undo() {
    // test_fix_and_undo
    let fx = Fx::new();
    let c = fx.ctl(fx.store(), 2);
    set_mods(&c, vec![named("a", "A"), named("b", "B")]);
    let p = json!({"title": "t"});
    c.apply(&p, &json!({"label": "Disable A", "action": ["disable", "a"]})).unwrap();
    c.apply(&p, &json!({"label": "Let A win", "action": ["prefer", "a", "b"]})).unwrap();
    assert_eq!(states(&c), pairs(&[("b", true), ("a", false)]));
    assert_eq!(lock(&c.store).doctor["ignored"], json!(["conflict-b-a"])); // Auto-sort keeps the user's winner
    c.undo(1).unwrap();
    c.undo(0).unwrap();
    assert_eq!(states(&c), pairs(&[("a", true), ("b", true)]));
    assert_eq!(lock(&c.store).doctor["ignored"], json!([]));
}

#[test]
fn file_stopped_by_defender_is_shown_as_a_virus() {
    // test_file_stopped_by_defender_is_shown_as_a_virus
    let fx = Fx::new();
    let deps = Deps {
        // what real-time protection gives, and what Windows gives when it refuses to open the file
        install: Arc::new(|_, _, name: &str| {
            let (code, p) = if name == "Bad" { (225, "C:/Users/me/x.zip") } else { (87, "C:/Users/me/y.zip") };
            Err(format!("{}: '{p}'", io::Error::from_raw_os_error(code)))
        }),
        ..fx.deps()
    };
    let c = fx.ctl_with(fx.store(), 2, deps);
    let links: Vec<String> =
        ["Bad", "Bad2"].iter().map(|n| zip(fx.tmp.path().join(format!("{n}.zip")), &[("archive/pc/mod/a.archive", "x")]).display().to_string()).collect();
    c.add_links(&links.join("\n"));
    wait_idle(&c);
    let status: HashMap<String, String> =
        lock(&c.jobs).iter().filter(|j| j["state"] == "failed").map(|j| (text(&j["name"]), text(&j["status"]))).collect();
    assert_eq!(status["Bad"], "Blocked: virus found (Microsoft Defender stopped the file)");
    assert!(status["Bad2"].starts_with("Blocked: Windows refused to read the file"), "{status:?}");
}

#[test]
fn mod_packs_save_use_merge_and_launch() {
    // test_mod_packs_save_use_merge_and_launch
    let fx = Fx::new();
    let c = fx.ctl(fx.store(), 2);
    set_mods(&c, vec![fw_mod("fw", "RED4ext"), m("a"), off(m("b")), m("c")]);
    c.save_pack("One").unwrap(); // fw, a, c on
    set_mods(&c, vec![fw_mod("fw", "RED4ext"), m("c"), m("b"), off(m("a")), m("d")]);
    c.save_pack("Two").unwrap();
    lock(&c.store).packs.get_mut("One").unwrap().push(("gone".into(), true)); // a mod removed since
    assert!(c.use_pack("One").unwrap().contains("1 not installed yet"));
    assert_eq!(states(&c), pairs(&[("d", false), ("fw", true), ("a", true), ("b", false), ("c", true)])); // outside the pack: first, off
    lock(&c.store).packs.insert("Coll".into(), pairs(&[("b", true), ("a", true)])); // a collection pack lists no frameworks: they stay on
    c.use_pack("Coll").unwrap();
    assert_eq!(enabled_ids(&c), ["fw", "b", "a"]);
    assert_eq!(lock(&c.store).pack, "Coll");
    assert_eq!(c.merge_packs(&["Two".into(), "Coll".into()], " Big "), Ok(5));
    assert_eq!(lock(&c.store).packs["Big"], pairs(&[("fw", true), ("c", true), ("d", true), ("b", true), ("a", true)]));
    assert!(c.merge_packs(&["Two".into()], "Small").is_err());
    c.launch(Some("Two")).unwrap();
    assert_eq!(lock(&fx.r.spawned).len(), 1);
    assert_eq!(*lock(&fx.r.blocks), [true]); // the local AI stays off while the game runs
    assert_eq!(fx.r.timers.load(SeqCst), 1);
    assert_eq!(enabled_ids(&c), ["fw", "c", "b", "d"]);
    let root = lock(&c.store).root.clone();
    assert_eq!(Store::open(root).unwrap().packs["Big"][0], ("fw".to_string(), true)); // kept in state.json
    c.delete_pack("Two").unwrap();
    let st = lock(&c.store);
    assert_eq!((st.packs.keys().cloned().collect::<Vec<_>>(), st.pack.as_str()), (vec!["One".into(), "Coll".into(), "Big".into()], ""));
}

#[test]
fn a_collection_is_also_a_mod_pack() {
    // test_a_collection_is_also_a_mod_pack
    let fx = Fx::new();
    fx.web(|req| {
        let (q, _) = gql_of(req)?;
        q.contains("collectionRevision").then(|| ok(coll_gql("My Pack ", &[(5, 50, false, "Car", "2024-06-01"), (2380, 9, false, "RED4ext", "2024-06-01")])))
    });
    let c = fx.ctl(fx.store(), 0);
    c.add(&json!({"type": "collection", "slug": "abc"})).unwrap();
    let st = lock(&c.store);
    assert_eq!(st.packs.len(), 1);
    assert_eq!(st.packs["My Pack"], pairs(&[("nexus-5-50", true)]));
    assert_eq!(st.pack_info["My Pack"]["names"]["nexus-5-50"], "Car"); // the My mod packs page shows it
}

#[test]
fn complete_pack_downloads_only_the_missing_nexus_mods() {
    // test_complete_pack_downloads_only_the_missing_nexus_mods
    let fx = Fx::new();
    fx.web(|req| {
        let (q, v) = gql_of(req)?;
        let has7 = v["ids"].as_array()?.iter().any(|i| i["modId"] == 7);
        (q.contains("legacyModsByDomain") && has7).then(|| ok(json!({"data": {"legacyModsByDomain": {"nodes": [{"modId": 7, "name": "Rain", "endorsements": 1}]}}})))
    });
    let mut store = fx.store();
    store.mods = vec![m("nexus-5-50")];
    store.packs.insert("P".into(), pairs(&[("nexus-5-50", true), ("nexus-6-60", true), ("nexus-7-70", true), ("local-x", true)]));
    store.pack_info.insert("Other".into(), json!({"url": "", "names": {"nexus-6-60": "Lights"}})); // a merged pack has no names
    let c = fx.ctl(store, 0);
    assert_eq!(c.complete_pack("P"), 2);
    assert!(wait_for(|| lock(&c.finish_pack).is_some()));
    let asked: Vec<String> = lock(&fx.r.asked).iter().filter(|(_, b)| b.contains("legacyModsByDomain")).map(|(_, b)| b.clone()).collect();
    assert_eq!(asked.len(), 1);
    let vars: Value = serde_json::from_str::<Value>(&asked[0]).unwrap()["variables"]["ids"].clone();
    assert_eq!(vars, json!([{"gameDomain": "cyberpunk2077", "modId": 7}]));
    let got: Vec<(String, Value, Value)> = lock(&c.jobs).iter().map(|j| (text(&j["name"]), j["src"]["mod_id"].clone(), j["src"]["file_id"].clone())).collect();
    assert_eq!(got, [("Lights".into(), json!(6), json!(60)), ("Rain".into(), json!(7), json!(70))]);
}

#[test]
fn complete_pack_uses_the_pack_when_its_downloads_end() {
    // test_complete_pack_uses_the_pack_when_its_downloads_end
    let fx = Fx::new();
    let mut store = fx.store();
    store.mods = vec![m("a"), m("nexus-5-50"), m("b")];
    store.packs.insert("P".into(), pairs(&[("nexus-5-50", true), ("nexus-6-60", true), ("b", true)]));
    let c = fx.ctl(store, 0);
    lock(&c.jobs).push(json!({"id": 500, "name": "Lights", "state": "running", "status": "", "progress": "",
                              "src": {"type": "nexus", "mod_id": 6, "file_id": 60}}));
    c.complete_pack("P");
    assert!(wait_for(|| lock(&c.finish_pack).as_deref() == Some("P"))); // a download still runs: the pack waits
    assert!(lock(&c.store).mods[0].enabled);
    c.edit(500, |j| j["state"] = json!("done"));
    c.after_job();
    assert!(lock(&c.finish_pack).is_none());
    assert_eq!(states(&c), pairs(&[("a", false), ("nexus-5-50", true), ("b", true)]));
    assert_eq!(lock(&c.store).pack, "P");
}

#[test]
fn update_app_swaps_in_the_newest_release_exe() {
    // test_update_app_swaps_in_the_newest_release_exe
    let fx = Fx::new();
    let own = format!("https://github.com/{}/releases/download/v9.0.0/a.exe", fetch::APP_REPO);
    let latest = format!("https://api.github.com/repos/{}/releases", fetch::APP_REPO);
    let release = |tag: &'static str| {
        let (own, latest) = (own.clone(), latest.clone());
        move |req: &Request| {
            if req.url == latest {
                return Some(ok(json!([{"tag_name": tag, "assets": [{"name": "a.exe", "browser_download_url": own}]}])));
            }
            (req.url == own).then(|| file(&own, b"MZ".to_vec()))
        }
    };
    fx.web(release("v9.0.0-manual"));
    let c = fx.ctl(fx.store(), 2);
    let updates = || fx.events("update");
    let rel = json!({"tag": "v9.0.0-manual", "url": own});
    c.update_app(None);
    assert_eq!(updates().last().unwrap(), &json!({"ask": rel})); // the window asks before anything downloads
    assert!(!lock(&fx.r.asked).iter().any(|(u, _)| *u == own));
    for (result, says) in [((Some(false), "Trojan"), "found Trojan"), ((None, "no Defender"), "could not scan")] {
        *lock(&fx.r.scan) = (result.0, result.1.to_string());
        c.update_app(Some(rel.clone()));
        let last = updates().last().unwrap().clone();
        assert!(text(&last["text"]).contains(says), "{last}");
        assert!(last.get("restart").is_none()); // the button turns back on
    }
    assert!(lock(&fx.r.spawned).is_empty()); // a file Defender flags, or could not scan, never runs
    *lock(&fx.r.scan) = (Some(true), String::new());
    c.update_app(Some(rel.clone()));
    let (_, env, _) = lock(&fx.r.spawned)[0].clone();
    let new = lock(&c.store).root.join("update").join("a.exe");
    let exe = std::env::current_exe().unwrap();
    assert_eq!((env["CP_NEW"].as_str(), env["CP_EXE"].as_str()), (new.to_str().unwrap(), exe.to_str().unwrap()));
    assert_eq!(updates().last().unwrap()["restart"], true); // the window closes so the script can swap the exe
    fx.web(release("v1.0.0"));
    c.update_app(None);
    assert!(text(&updates().last().unwrap()["text"]).contains("newest version"));
}

#[test]
fn a_pack_with_optional_or_outdated_mods_asks_first() {
    // test_a_pack_with_optional_or_outdated_mods_asks_first
    let fx = Fx::new();
    fx.web(|req| {
        let (q, _) = gql_of(req)?;
        let files = [(5, 50, false, "Car", "2024-06-01"), (6, 60, true, "Extra", "2024-06-01"), (7, 70, false, "Old", "2020-01-01")];
        q.contains("collectionRevision").then(|| ok(coll_gql("P", &files)))
    });
    let c = fx.ctl(fx.store(), 0);
    c.add(&json!({"type": "collection", "slug": "abc"})).unwrap();
    let asked = fx.events("pack_choice");
    assert_eq!(asked.len(), 1);
    let names: Vec<String> = asked[0]["files"].as_array().unwrap().iter().map(|f| text(&f["name"])).collect();
    assert_eq!(names, ["Extra", "Old"]);
    assert!(lock(&c.store).packs.is_empty() && lock(&c.jobs).is_empty()); // nothing happens before the answer
    let mut link = asked[0]["link"].clone();
    link["chosen"] = json!([60]); // Extra yes, Old skipped
    c.add(&link).unwrap();
    let names: Vec<String> = lock(&c.jobs).iter().map(|j| text(&j["name"])).collect();
    assert_eq!(names, ["Car", "Extra"]);
    assert_eq!(lock(&c.store).packs["P"], pairs(&[("nexus-5-50", true), ("nexus-6-60", true)]));
}

#[test]
fn an_outdated_mod_asks_download_or_skip() {
    // test_an_outdated_mod_asks_download_or_skip
    let fx = Fx::new();
    let files = json!([{"fileId": 1, "name": "x", "version": "1", "category": "MAIN", "primary": 1, "date": 1}]);
    fx.web(move |req| {
        let (q, _) = gql_of(req)?;
        q.contains("modFiles(modId").then(|| ok(mod_gql("Old", "1", files.clone(), &[], "2020-01-01")))
    });
    let c = fx.ctl(fx.store(), 0);
    c.add(&json!({"type": "nexus", "mod_id": 3})).unwrap();
    assert!(lock(&c.jobs).is_empty());
    let asked = fx.events("outdated");
    assert_eq!(asked.len(), 1);
    assert!(text(&asked[0]["why"]).starts_with("it was last updated on 2020-01-01"));
    let mut link = asked[0]["link"].clone();
    link["anyway"] = json!(true); // the user clicked Download
    c.add(&link).unwrap();
    assert_eq!(job0(&c)["name"], "Old");
}

#[test]
fn launch_asks_windows_when_the_game_runs_as_admin() {
    // test_launch_asks_windows_when_the_game_runs_as_admin
    let fx = Fx::new();
    let c = fx.ctl(fx.store(), 2);
    let (exe, cwd) = (fx.game.join(GAME_EXE), fx.game.join("bin/x64"));
    *lock(&fx.r.spawn_err) = Some(740);
    c.launch(None).unwrap();
    assert_eq!(*lock(&fx.r.run_as), [(exe, vec![], cwd)]);
    *lock(&fx.r.spawn_err) = Some(2);
    assert!(c.launch(None).is_err());
    assert_eq!(lock(&fx.r.run_as).len(), 1);
    let blocks = lock(&fx.r.blocks).clone();
    assert_eq!(blocks[blocks.len() - 2..], [true, false]); // no game: the AI may start again
    lock(&c.store).deployed.insert("bin/x64/cyberpunk2077.exe".into(), Some("m".into())); // a mod replaced the game exe: never elevate it
    fx.r.fake_deploy.store(true, SeqCst);
    *lock(&fx.r.spawn_err) = Some(740);
    assert!(c.launch(None).is_err());
    assert_eq!(lock(&fx.r.run_as).len(), 1);
}

#[test]
fn auto_sort_and_undo() {
    // test_auto_sort_and_undo
    let fx = Fx::new();
    let c = fx.ctl(fx.store(), 2);
    set_mods(&c, vec![m("a"), fw_mod("fw", "RED4ext")]);
    c.auto_sort().unwrap();
    assert_eq!(mod_ids(&c), ["fw", "a"]);
    assert!(!fx.events("problems").is_empty());
    c.auto_sort().unwrap(); // already sorted: no second history entry
    assert_eq!(lock(&c.store).doctor["history"].as_array().unwrap().len(), 1);
    c.undo(0).unwrap();
    assert_eq!(mod_ids(&c), ["a", "fw"]);
}

#[test]
fn local_ai_advice_apply_and_undo() {
    // test_local_ai_advice_apply_and_undo
    let fx = Fx::new();
    let c = fx.ctl(fx.store(), 2);
    set_mods(&c, vec![named("patch", "A - Patch"), named("a", "A")]);
    *lock(&fx.r.advice) = Some(Advice {
        summary: "s".into(),
        fixes: vec![(json!({"title": "t"}), json!({"label": "x", "action": ["ignore", "i"]}), String::new()); 12],
        after: vec![("a".into(), "patch".into())],
        cause: None,
        mod_fixes: vec![],
    });
    c.ask_ai("");
    let mut a = fx.events("ai").last().unwrap().clone();
    assert_eq!((a["order"].clone(), a["moved"].clone(), a["fixes"].as_array().unwrap().len()), (json!(["a", "patch"]), json!(1), 10)); // the window lists 10 fixes
    a["fixes"] = json!([[{"title": "t"}, {"label": "Turn A off", "action": ["disable", "a"]}, "why"]]);
    c.apply_ai(&a).unwrap();
    assert_eq!(states(&c), pairs(&[("a", false), ("patch", true)]));
    let fixes: Vec<String> = lock(&c.store).doctor["history"].as_array().unwrap().iter().map(|h| text(&h["fix"])).collect();
    assert_eq!(fixes, ["Sorted by the local AI (1 mod(s) changed place)", "Turn A off"]);
    c.undo(1).unwrap();
    c.undo(0).unwrap();
    assert_eq!(states(&c), pairs(&[("patch", true), ("a", true)]));
    *lock(&fx.r.start_err) = Some("The local AI did not start.".into());
    c.ask_ai("");
    assert_eq!(fx.events("ai").last().unwrap(), &json!({"error": "The local AI did not start."}));
}

#[test]
fn the_local_ai_stops_while_the_game_runs() {
    // test_the_local_ai_stops_while_the_game_runs
    let fx = Fx::new();
    let c = fx.ctl(fx.store(), 2);
    c.game_started(0.0);
    assert_eq!(*lock(&fx.r.blocks), [true]);
    c.ask_ai("my car is missing");
    c.setup_ai();
    assert_eq!(fx.r.starts.load(SeqCst), 0);
    assert_eq!(fx.events("ai").last().unwrap(), &json!({"error": GAME_ON}));
    lock(&fx.r.blocks).clear();
    c.game_exited(0.0, None, None);
    assert!(!c.game_on());
    assert_eq!(*lock(&fx.r.blocks), [false]); // the AI may start again
    c.game_exited(0.0, None, Some(Path::new("freeze.dmp")));
    assert_eq!(*lock(&fx.r.dumps), [PathBuf::from("freeze.dmp")]);
    assert_eq!(lock(&c.store).doctor["last_exit"]["crashed"], true); // a freeze the game never came back from counts as a crash
    assert!(c.crash_report(false).contains("LAST GAME EXIT"));
    let blocker = lock(&c.store).file.with_extension("tmp");
    std::fs::create_dir_all(&blocker).unwrap(); // the store cannot save
    c.game_started(1.0); // no error: else the game watch would skip the exit and the AI would stay off
    std::fs::remove_dir_all(&blocker).unwrap();
    assert!(c.game_on());
    lock(&fx.r.blocks).clear();
    *lock(&fx.r.spawn_err) = Some(2);
    assert!(c.launch(None).is_err()); // a second launch fails while the game runs
    assert_eq!(*lock(&fx.r.blocks), [true]); // the AI stays off
}

#[test]
fn bug_report_fixes_become_changes_the_user_can_apply() {
    // test_bug_report_fixes_become_changes_the_user_can_apply
    let fx = Fx::new();
    let c = fx.ctl(fx.store(), 2);
    let (car, b) = (off(named("car", "Car")), named("b", "B"));
    let w = Mod { source: json!({"type": "nexus", "mod_id": 9, "file_id": 90}), ..named("w", "W") };
    set_mods(&c, vec![car.clone(), b.clone(), w.clone()]);
    let mf = |x: &Mod, a: &str, why: &str| (x.clone(), a.to_string(), why.to_string());
    *lock(&fx.r.advice) = Some(Advice {
        summary: "s".into(),
        fixes: vec![],
        after: vec![],
        cause: Some("c".into()),
        mod_fixes: vec![
            mf(&car, "turn_on", "off"),
            mf(&b, "turn_on", "already on"),
            mf(&car, "load_last", "wins"),
            mf(&w, "update", "old"),
            mf(&w, "load_last", "already last"),
        ],
    });
    c.ask_ai("  my car is missing ");
    assert_eq!(*lock(&fx.r.reports), ["my car is missing"]);
    let a = fx.events("ai").last().unwrap().clone();
    assert!(a.get("error").is_none(), "{a}");
    assert!(a.get("mod_fixes").is_none());
    let got: Vec<(String, String)> = a["fixes"].as_array().unwrap().iter().map(|x| (text(&x[0]["title"]), text(&x[1]["label"]))).collect();
    let mine = "Your bug report".to_string();
    assert_eq!(
        got,
        [
            (mine.clone(), "Turn on Car".into()),
            (mine.clone(), "Load Car last, so it wins".into()),
            (mine.clone(), "Update W to its newest version".into()),
            (mine, "Load W last, so it wins".into())
        ]
    );
    c.apply_ai(&a).unwrap(); // the update runs in the background (and fails offline)
    assert_eq!(states(&c), pairs(&[("b", true), ("car", true), ("w", true)]));
}

#[test]
fn auto_fix_only_single_fix_problems_after_crash() {
    // test_auto_fix_only_single_fix_problems_after_crash
    let fx = Fx::new();
    let c = fx.ctl(fx.store(), 2);
    set_mods(&c, vec![named("a", "A")]);
    let one = json!({"id": "1", "title": "t", "auto": true, "severity": "high", "fixes": [{"label": "Disable A", "action": ["disable", "a"]}]});
    let x = json!({"label": "x", "action": ["ignore", "2"]});
    let two = json!({"id": "2", "title": "t2", "auto": true, "severity": "high", "fixes": [x, x]});
    c.handle_problems(vec![one, two], true).unwrap();
    assert!(!lock(&c.store).mods[0].enabled);
    let asked: Vec<Value> = fx.events("problems")[0]["list"].as_array().unwrap().iter().map(|p| p["id"].clone()).collect();
    assert_eq!(asked, [json!("2")]);
}

fn enabled_flags(c: &Controller) -> Vec<bool> {
    lock(&c.store).mods.iter().map(|m| m.enabled).collect()
}

#[test]
fn bisect_round_trip() {
    // test_bisect_round_trip
    let fx = Fx::new();
    let c = fx.ctl(fx.store(), 2);
    set_mods(&c, "abcd".chars().map(|i| m(&i.to_string())).collect());
    c.bisect_begin().unwrap();
    assert_eq!(enabled_flags(&c), [true, true, false, false]);
    c.bisect_result(false, 0.0).unwrap(); // c or d is bad
    assert_eq!(enabled_flags(&c), [true, true, true, false]);
    c.bisect_result(true, 0.0).unwrap(); // c is bad
    assert!(!lock(&c.store).doctor.contains_key("bisect"));
    assert_eq!(enabled_flags(&c), [true, true, false, true]); // c auto-disabled
}

#[test]
fn bisect_ignores_a_short_launch_without_a_crash() {
    // test_bisect_ignores_a_short_launch_without_a_crash
    let fx = Fx::new();
    let c = fx.ctl(fx.store(), 2);
    set_mods(&c, "abcd".chars().map(|i| m(&i.to_string())).collect());
    c.bisect_begin().unwrap();
    lock(&c.store).doctor.get_mut("bisect").unwrap()["crash_secs"] = json!(120);
    c.bisect_result(false, 30.0).unwrap(); // quit before the crash would have come: does not count
    assert_eq!(enabled_flags(&c), [true, true, false, false]);
    c.bisect_result(false, 200.0).unwrap();
    assert_eq!(enabled_flags(&c), [true, true, true, false]);
}

// ---------- tests/test_browser.py, class Wiring ----------

/// The Wiring setUp: a Nexus key, no game folder, and the hidden browser's click recorded.
fn wiring(fx: &Fx) -> Arc<Controller> {
    let mut store = Store::open(fx.tmp.path().join("data")).unwrap();
    store.settings.api_key = "k".into();
    store.settings.virus_scan = false;
    let r = fx.r.clone();
    let deps = Deps { click: Some(Arc::new(move |j: &Value| lock(&r.clicked).push(j.clone()))), ..fx.deps() };
    fx.ctl_with(store, 2, deps)
}

#[test]
fn waiting_job_opens_the_page_for_the_users_click() {
    // manual-click version: the hidden browser never clicks; the user clicks on the page the app opens
    let fx = Fx::new();
    let z = zip(fx.tmp.path().join("nx.zip"), &[("a.archive", "x")]);
    fx.web(nexus_route(mod_gql("Nx", "1", nx_files("1"), &[], "2024-06-01"), false, z));
    let c = wiring(&fx);
    assert!(!lock(&c.store).settings.auto_click);
    c.add_links("https://www.nexusmods.com/cyberpunk2077/mods/55");
    assert!(wait_for(|| !lock(&fx.r.opened).is_empty()));
    assert!(lock(&fx.r.opened)[0].contains("mods/55?tab=files&file_id="), "{:?}", lock(&fx.r.opened));
    c.handle_nxm("nxm://cyberpunk2077/mods/55/files/7?key=K&expires=1").unwrap();
    wait_idle(&c);
    assert_eq!(job0(&c)["state"], "done");
    assert!(lock(&fx.r.clicked).is_empty());
}

#[test]
fn automatic_click_failure_reopens_the_page() {
    // Wiring.test_automatic_click_failure_reopens_the_page
    let fx = Fx::new();
    let c = wiring(&fx);
    let j = json!({"id": 98, "name": "Nx", "state": "waiting", "status": "", "progress": "", "src": {"mod_id": 55, "file_id": 7}});
    lock(&c.jobs).push(j.clone());
    c.manual_click(&j, "no login");
    assert!(lock(&fx.r.opened).last().unwrap().contains("mods/55?tab=files&file_id=7"));
}

#[test]
fn the_setting_defaults_to_on() {
    // Wiring.test_the_setting_defaults_to_on
    assert!(shared::Settings::default().auto_click);
}

// ---------- tests/test_video.py ----------

#[test]
fn errors_reach_the_window() {
    // test_errors_reach_the_window
    let fx = Fx::new();
    fx.web(|req| (req.url == "https://www.youtube.com/watch?v=8Yexbiw0T28").then(|| file(&req.url, b"<html>consent page</html>".to_vec())));
    video_finder::set_resolver(Some(Arc::new(|_: &str| Ok(vec!["93.184.215.14".parse().unwrap()]))));
    let c = fx.ctl(fx.store(), 0);
    c.find_in_video("https://youtu.be/8Yexbiw0T28");
    let ev = lock(&fx.r.events)[0].clone();
    assert_eq!(ev.0, "video");
    assert!(text(&ev.1["error"]).contains("YouTube did not show this video"), "{}", ev.1);
}
