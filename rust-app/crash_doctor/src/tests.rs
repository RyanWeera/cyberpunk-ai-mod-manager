//! Port of tests/test_doctor.py.
use crate::doctor::{self, LogHit, OwnerIndex};
use crate::watch::{self, Os, dump_module, fault_id, parse_events, read_dump};
use serde_json::{Value, json};
use shared::{Mod, Store};
use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;
use testkit::{TempDir, fake_game, mod_rec, rdar, stage};

const EVENTS: &str = r"Event[0]
  Source: Application Error
  Description:
Faulting application name: Cyberpunk2077.exe, version: 2.3.0.0, time stamp: 0x1
Faulting module name: Bad.dll, version: 1.0.0.0
Faulting process id: 0x1
Fault offset: 0x0000000000001a2b
Faulting application start time: 0x1DD6CEF3291ECD2
Faulting module path: {game}\red4ext\plugins\Bad Plugin\Bad.dll
Event[1]
  Description:
Faulting application name: pythonw.exe, version: 3.12
Faulting module path: C:\x\torch.dll
";

fn events(game: &str) -> String {
    EVENTS.replace("{game}", game)
}

/// A small minidump: exception (none when crash_addr is None, like a freeze snapshot), module list, and thread 7
/// whose stack holds `words`, then `more_threads` [(tid, words)]. modules: [(base, size, path)].
fn minidump(modules: &[(u64, u32, &str)], crash_addr: Option<u64>, words: &[u64], more_threads: &[(u32, Vec<u64>)]) -> Vec<u8> {
    let (tid, code) = (7u32, 0xC000_0005u32);
    let mut threads = vec![(tid, words.to_vec())];
    threads.extend(more_threads.iter().cloned());
    let n: u32 = if crash_addr.is_some() { 3 } else { 2 };
    let exc = 32 + 12 * n;
    let ml = exc + if crash_addr.is_some() { 32 } else { 0 };
    let names_off = ml + 4 + 108 * modules.len() as u32;
    let (mut names, mut rvas) = (vec![], vec![]);
    for (_, _, path) in modules {
        let e: Vec<u8> = path.encode_utf16().flat_map(u16::to_le_bytes).collect();
        rvas.push(names_off + names.len() as u32);
        names.extend((e.len() as u32).to_le_bytes());
        names.extend(e);
    }
    let tl = names_off + names.len() as u32;
    let mem = tl + 4 + 48 * threads.len() as u32;
    let (mut stacks, mut entries) = (vec![], vec![]);
    for (t, words) in &threads {
        let st: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        for v in [*t, 0, 0, 0] {
            entries.extend(v.to_le_bytes());
        }
        entries.extend([0u8; 16]);
        entries.extend((st.len() as u32).to_le_bytes());
        entries.extend((mem + stacks.len() as u32).to_le_bytes());
        entries.extend([0u8; 8]);
        stacks.extend(st);
    }
    let mut out: Vec<u8> = b"MDMP".to_vec();
    for v in [0, n, 32, 0, 0] {
        out.extend(v.to_le_bytes());
    }
    out.extend(0u64.to_le_bytes());
    let mut dir = |t: u32, size: u32, at: u32| {
        for v in [t, size, at] {
            out.extend(v.to_le_bytes());
        }
    };
    if crash_addr.is_some() {
        dir(6, 32, exc);
    }
    dir(4, tl - ml, ml);
    dir(3, mem - tl, tl);
    if let Some(a) = crash_addr {
        for v in [tid, 0, code, 0] {
            out.extend(v.to_le_bytes());
        }
        out.extend(0u64.to_le_bytes());
        out.extend(a.to_le_bytes());
    }
    out.extend((modules.len() as u32).to_le_bytes());
    for ((base, size, _), r) in modules.iter().zip(&rvas) {
        out.extend(base.to_le_bytes());
        for v in [*size, 0, 0, *r] {
            out.extend(v.to_le_bytes());
        }
        out.extend([0u8; 84]);
    }
    out.extend(names);
    out.extend((threads.len() as u32).to_le_bytes());
    out.extend(entries);
    out.extend(stacks);
    out
}

fn no_update(_: &Mod) -> bool {
    false
}

/// The Python `Doctor.setUp` fixture: a fake game and five staged mods.
struct Fx {
    tmp: TempDir,
    game: PathBuf,
    store: Store,
}

impl Fx {
    fn new() -> Fx {
        let tmp = TempDir::new();
        let game = fake_game(tmp.path());
        let mut store = Store::open(tmp.path().join("data")).expect("store");
        store.settings.game_dir = game.to_string_lossy().into_owned();
        let mut add = |id: &str, files: &[&str], fw: Option<&str>, needs: &[&str]| {
            let mut m = mod_rec(id, files);
            m.framework = fw.map(String::from);
            m.needs = needs.iter().map(|n| n.to_string()).collect();
            stage(&mut store, m, &[]);
        };
        add("cet", &["bin/x64/version.dll", "bin/x64/plugins/cyber_engine_tweaks/LICENSE"], Some("Cyber Engine Tweaks"), &[]);
        add("rs", &["engine/tools/scc.exe"], Some("redscript"), &[]);
        add("fancy", &["r6/scripts/Fancy HUD/hud.reds"], None, &["redscript"]);
        add("luamod", &["bin/x64/plugins/cyber_engine_tweaks/mods/luamod/init.lua"], None, &["Cyber Engine Tweaks"]);
        add("bad", &["red4ext/plugins/Bad Plugin/Bad.dll"], None, &["RED4ext"]);
        Fx { tmp, game, store }
    }

    fn write(&self, rel: &str, text: &str) {
        let p = self.game.join(rel);
        std::fs::create_dir_all(p.parent().expect("parent")).expect("dirs");
        std::fs::write(p, text).expect("write");
    }

    fn g(&self) -> String {
        self.game.to_string_lossy().into_owned()
    }

    fn idx(&self) -> OwnerIndex {
        doctor::owner_index(&self.store)
    }

    fn diagnose(&self, crash: Value) -> Vec<Value> {
        doctor::diagnose(&self.store, 0.0, Some(&crash), &no_update)
    }

    fn by_id(&self, crash: Value) -> HashMap<String, Value> {
        by_id(self.diagnose(crash))
    }

    fn drop_mod(&mut self, id: &str) {
        self.store.mods.retain(|m| m.id != id);
    }
}

fn by_id(probs: Vec<Value>) -> HashMap<String, Value> {
    probs.into_iter().map(|p| (p["id"].as_str().expect("id").to_string(), p)).collect()
}

fn ids(probs: &[Value]) -> Vec<String> {
    probs.iter().map(|p| p["id"].as_str().expect("id").to_string()).collect()
}

fn verbs(p: &Value) -> Vec<String> {
    p["fixes"].as_array().expect("fixes").iter().map(|f| f["action"][0].as_str().expect("verb").to_string()).collect()
}

fn letters(s: &str) -> Value {
    json!(s.chars().map(String::from).collect::<Vec<_>>())
}

fn strings(s: &str) -> Vec<String> {
    s.chars().map(String::from).collect()
}

fn names_of(v: &Value) -> Vec<String> {
    v.as_array().expect("list").iter().map(|m| m.as_str().expect("str").rsplit('\\').next().expect("name").to_string()).collect()
}

fn write_temp(name: &str, bytes: &[u8]) -> (TempDir, PathBuf) {
    let t = TempDir::new();
    let p = t.path().join(name);
    std::fs::write(&p, bytes).expect("write");
    (t, p)
}

// test_doctor.py Doctor.test_log_errors_map_to_mods
#[test]
fn test_log_errors_map_to_mods() {
    let fx = Fx::new();
    fx.write("r6/logs/redscript_rCURRENT.log", &format!("[INFO - x] Compiling\n[ERROR - Mon] At {}\\r6\\scripts\\Fancy HUD\\hud.reds:12:5:\n  unresolved\n", fx.g()));
    fx.write("bin/x64/plugins/cyber_engine_tweaks/mods/luamod/luamod.log", "[error] attempt to index a nil value\n");
    fx.write("bin/x64/plugins/cyber_engine_tweaks/cyber_engine_tweaks.log", "[info] fine\n[error] Unsupported game version\n");
    fx.write("red4ext/logs/red4ext.log", "[error] something without a path\n");
    let found: HashMap<String, LogHit> = doctor::log_errors(&fx.game, 0.0, &fx.idx()).into_iter().collect();
    let keys: HashSet<&str> = found.keys().map(String::as_str).collect();
    assert_eq!(keys, HashSet::from(["fancy", "luamod", "cet"]));
    assert!(found["fancy"].strong);
    assert!(!found["luamod"].strong);
}

// test_doctor.py Doctor.test_crash_module_and_missing_framework
#[test]
fn test_crash_module_and_missing_framework() {
    let fx = Fx::new();
    let module = parse_events(&events(&fx.g()))[0].0.clone();
    let probs = fx.by_id(json!({"crashed": true, "module": module}));
    assert_eq!(probs["crash-bad"]["fixes"][0]["action"], json!(["disable", "bad"])); // no update -> one fix -> automatic
    assert_eq!(probs["crash-bad"]["fixes"].as_array().expect("fixes").len(), 1);
    assert_eq!(verbs(&probs["need-RED4ext"]), ["install_fw", "disable_many"]);
}

// test_doctor.py Doctor.test_crash_in_game_files_does_not_blame_a_mod
#[test]
fn test_crash_in_game_files_does_not_blame_a_mod() {
    let fx = Fx::new();
    let found = ids(&fx.diagnose(json!({"crashed": true, "module": format!("{}\\bin\\x64\\Cyberpunk2077.exe", fx.g())})));
    assert!(!found.iter().any(|i| i.starts_with("crash-")), "{found:?}"); // CET owns version.dll, not bin/x64
    // CET's own file: CET is to blame
    let found = ids(&fx.diagnose(json!({"crashed": true, "module": format!("{}\\bin\\x64\\version.dll", fx.g())})));
    assert!(found.contains(&"crash-cet".to_string()));
}

// test_doctor.py Doctor.test_bisect_without_any_crash_names_nobody
#[test]
fn test_bisect_without_any_crash_names_nobody() {
    let mut state = json!({"suspects": letters("abcd"), "testing": letters("ab"), "restore": letters("abcd")});
    while state.get("culprit").is_none() {
        state = doctor::bisect_step(&state, false, 0.0);
    }
    assert!(state["culprit"].is_null());
}

// test_doctor.py Doctor.test_bisect_confirms_a_mod_found_by_elimination
#[test]
fn test_bisect_confirms_a_mod_found_by_elimination() {
    let state = json!({"suspects": letters("abcd"), "testing": letters("ab"), "restore": letters("abcd")});
    let state = doctor::bisect_step(&doctor::bisect_step(&state, false, 0.0), false, 0.0); // a, b, then c clear: d is left
    assert_eq!(state["verifying"], json!(true));
    assert_eq!(doctor::bisect_step(&state, true, 0.0)["culprit"], json!("d"));
}

// test_doctor.py Doctor.test_huge_log_line_stays_cheap
#[test]
fn test_huge_log_line_stays_cheap() {
    let fx = Fx::new();
    let t = Instant::now();
    doctor::owners_in(&format!("error r6/{}", "a ".repeat(500_000)), &fx.idx(), Some(&fx.game)); // 1 MB, all spaces
    assert!(t.elapsed().as_secs_f64() < 2.0);
}

#[test]
fn advanced_crash_reporter_is_never_turned_off() {
    let mut fx = Fx::new();
    let mut acr = mod_rec("acr", &[]);
    acr.source = json!({"type": "nexus", "mod_id": "34403"});
    assert!(doctor::mod_fixes(&acr, &|_| false).is_empty()); // no "Disable" fix, so no auto-fix either
    assert_eq!(doctor::mod_fixes(&acr, &|_| true)[0]["action"][0], "update");
    fx.store.mods.push(acr);
    fx.store.doctor.insert("crashes".into(), json!([{"time": 1, "fault": null, "secs": 90}]));
    let state = doctor::bisect_start(&fx.store);
    assert!(!state["suspects"].as_array().unwrap().contains(&json!("acr"))); // stays on in every round
    let dir = fx.game.join(doctor::ACR);
    std::fs::create_dir_all(&dir).unwrap();
    assert!(!doctor::acr_installed(&fx.game)); // only the reports it wrote: the mod itself is gone
    std::fs::write(dir.parent().unwrap().join("AdvancedCrashReporter.dll"), b"MZ").unwrap();
    assert!(doctor::acr_installed(&fx.game));
}

#[test]
fn advanced_crash_reporter_is_never_blamed() {
    let mut fx = Fx::new();
    let mut acr = mod_rec("acr", &["bin/x64/plugins/AdvancedCrashReporter/AdvancedCrashReporter.dll"]);
    acr.source = json!({"type": "nexus", "mod_id": 34403});
    fx.store.mods.push(acr);
    let g = fx.g();
    let dll = format!("{g}\\bin\\x64\\plugins\\AdvancedCrashReporter\\AdvancedCrashReporter.dll");
    let probs = fx.by_id(json!({"crashed": true, "module": dll,
                                "stack": [dll, format!("{g}\\red4ext\\plugins\\Bad Plugin\\Bad.dll")]}));
    assert!(!probs.keys().any(|k| k.ends_with("-acr")), "{probs:?}"); // the crash handler, not the cause
    assert_eq!(probs["stack-bad"]["severity"], "high"); // the next module on the stack is the hint
}

// test_doctor.py Doctor.test_update_is_offered_first
#[test]
fn test_update_is_offered_first() {
    let fx = Fx::new();
    let fixes = doctor::mod_fixes(fx.store.mod_by_id("bad").expect("bad"), &|_| true);
    let v: Vec<&str> = fixes.iter().map(|f| f["action"][0].as_str().expect("verb")).collect();
    assert_eq!(v, ["update", "disable"]);
}

// test_doctor.py Doctor.test_unknown_crash_offers_choices
#[test]
fn test_unknown_crash_offers_choices() {
    let mut fx = Fx::new();
    fx.drop_mod("bad");
    fx.store.doctor.insert("last_good".into(), json!({"cet": "1", "rs": "1", "fancy": "1"}));
    let probs = fx.by_id(json!({"crashed": true, "module": "C:\\Windows\\ntdll.dll"}));
    let unknown = &probs["unknown"];
    assert_eq!(unknown["fixes"][0]["action"], json!(["disable_many", ["luamod"]]));
    assert_eq!(unknown["fixes"][1]["action"], json!(["bisect_start"]));
}

// test_doctor.py Doctor.test_archive_conflicts
#[test]
fn test_archive_conflicts() {
    let (_t, mut s) = testkit::temp_store();
    for (id, hashes) in [("x", vec![1, 2, 3]), ("y", vec![3, 4]), ("z", vec![9])] {
        let rel = format!("archive/pc/mod/{id}.archive");
        stage(&mut s, mod_rec(id, &[&rel]), &[(&rel, &rdar(&hashes))]);
    }
    assert_eq!(doctor::archive_hashes(&s.staging.join("x/archive/pc/mod/x.archive")), BTreeSet::from([1, 2, 3]));
    let found = doctor::archive_conflicts(&s);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0]["id"], json!("conflict-x-y"));
    let actions: Vec<&Value> = found[0]["fixes"].as_array().expect("fixes").iter().map(|f| &f["action"]).collect();
    assert_eq!(actions, [&json!(["ignore", "conflict-x-y"]), &json!(["prefer", "x", "y"]), &json!(["disable", "x"]), &json!(["disable", "y"])]);
    s.doctor.insert("ignored".into(), json!(["conflict-x-y"]));
    assert!(doctor::archive_conflicts(&s).is_empty());
}

// test_doctor.py Doctor.test_loose_files_two_mods_replace_are_a_conflict_too
#[test]
fn test_loose_files_two_mods_replace_are_a_conflict_too() {
    let (_t, mut s) = testkit::temp_store();
    let mut car = mod_rec("car", &["r6/scripts/car.reds", "r6/tweaks/car.yaml"]);
    car.name = "Car".into();
    let mut car2 = mod_rec("car2", &["R6/Scripts/car.reds", "r6/tweaks/car.yaml"]);
    car2.name = "Car v2".into();
    s.mods = vec![car, car2, mod_rec("x", &["r6/scripts/x.reds"])];
    let found = doctor::archive_conflicts(&s);
    assert_eq!(found.len(), 1);
    assert_eq!((&found[0]["id"], &found[0]["title"]), (&json!("conflict-car-car2"), &json!("Car and Car v2 change the same 2 game file(s)")));
    assert_eq!(found[0]["fixes"][1]["action"], json!(["prefer", "car", "car2"])); // the user picks which one wins
}

// test_doctor.py Doctor.test_best_order
#[test]
fn test_best_order() {
    let (_t, mut s) = testkit::temp_store();
    let list: [(&str, Vec<u64>); 7] = [
        ("patch", vec![1, 2]),
        ("addon", (1..21).collect()), // bigger, but needs big
        ("big", (1..11).collect()),
        ("other", vec![99]),
        ("k1", vec![50, 51]), // smaller, but the user chose to keep k2 winning
        ("k2", vec![50, 51, 52]),
        ("fw", vec![]),
    ];
    for (id, hashes) in list {
        let rel = format!("archive/pc/mod/{id}.archive");
        let mut m = mod_rec(id, &[&rel]);
        match id {
            "addon" => m.requires = vec![json!({"mod_id": 100, "name": "Big"})],
            "big" => m.source = json!({"type": "nexus", "mod_id": 100, "file_id": 1}),
            "fw" => m.framework = Some("ArchiveXL".into()),
            _ => {}
        }
        stage(&mut s, m, &[(&rel, &rdar(&hashes))]);
    }
    s.doctor.insert("ignored".into(), json!(["conflict-k1-k2"]));
    assert_eq!(doctor::best_order(&s, &[]), ["fw", "big", "addon", "patch", "other", "k1", "k2"]);
    // the local AI's rules: patch after other; addon before big loses to addon's own requirement
    let after: Vec<(String, String)> =
        [("other", "patch"), ("addon", "big"), ("gone", "patch")].iter().map(|(a, b)| (a.to_string(), b.to_string())).collect();
    assert_eq!(doctor::best_order(&s, &after), ["fw", "big", "addon", "other", "patch", "k1", "k2"]);
}

// test_doctor.py Doctor.test_best_order_skips_a_mod_added_while_it_reads
#[test]
fn test_best_order_skips_a_mod_added_while_it_reads() {
    let (a, b) = (mod_rec("a", &[]), mod_rec("b", &[]));
    let sizes = HashMap::from([("a".to_string(), 1), ("b".to_string(), 1), ("new".to_string(), 5)]);
    let pairs = vec![(("a".to_string(), "new".to_string()), 1)];
    // "new" came from a framework install meanwhile
    assert_eq!(doctor::order_with(&[&a, &b], &sizes, &pairs, &HashSet::new(), &[]), ["a", "b"]);
}

// test_doctor.py Doctor.test_moved_counts_only_mods_that_left_their_place
#[test]
fn test_moved_counts_only_mods_that_left_their_place() {
    assert_eq!(doctor::moved(&strings("abcde"), &strings("eabcd")), 1); // one mod to the top shifts the others, it moves one
    assert_eq!(doctor::moved(&strings("abc"), &strings("abc")), 0);
    assert_eq!(doctor::moved(&strings("abc"), &strings("cba")), 2);
    assert_eq!(doctor::moved(&strings("ab"), &strings("xba")), 1); // a mod added meanwhile does not count
}

// test_doctor.py Doctor.test_bisect_finds_culprit
#[test]
fn test_bisect_finds_culprit() {
    let mut state = json!({"suspects": letters("abcdefg"), "testing": letters("abcd"), "restore": letters("abcdefg")});
    let mut launches = 0;
    while state.get("culprit").is_none() {
        let crashed = state["testing"].as_array().expect("testing").contains(&json!("f"));
        state = doctor::bisect_step(&state, crashed, 0.0);
        launches += 1;
    }
    assert_eq!(state["culprit"], json!("f"));
    assert!(launches <= 3);
}

// test_doctor.py Doctor.test_minidump_names_crashing_module
#[test]
fn test_minidump_names_crashing_module() {
    let name: Vec<u8> = "C:\\G\\red4ext\\plugins\\Bad\\Bad.dll".encode_utf16().flat_map(u16::to_le_bytes).collect();
    let (dirs, exc, mods) = (32u32, 56u32, 88u32); // header, 2 directory entries, exception stream, module list, name
    let mut blob: Vec<u8> = b"MDMP".to_vec();
    for v in [0, 2, dirs, 0, 0] {
        blob.extend(v.to_le_bytes());
    }
    blob.extend(0u64.to_le_bytes());
    for v in [6, 32, exc, 4, 112, mods] {
        blob.extend(v.to_le_bytes());
    }
    for v in [1u32, 0, 0xC000_0005, 0] {
        blob.extend(v.to_le_bytes()); // 32 bytes: 56 + 32 = 88
    }
    blob.extend(0u64.to_le_bytes());
    blob.extend(0x5010u64.to_le_bytes());
    blob.extend(1u32.to_le_bytes());
    blob.extend(0x5000u64.to_le_bytes());
    for v in [0x100, 0, 0, mods + 4 + 108] {
        blob.extend(v.to_le_bytes());
    }
    blob.extend([0u8; 84]);
    blob.extend((name.len() as u32).to_le_bytes());
    blob.extend(name);
    let (_t, p) = write_temp("minidump.dmp", &blob);
    assert_eq!(dump_module(&p).as_deref(), Some("C:\\G\\red4ext\\plugins\\Bad\\Bad.dll"));
    assert_eq!(read_dump(&p).expect("dump")["fault"], json!("Bad.dll+0x10"));
    assert_eq!(fault_id("C:\\G\\Bad.dll", 0x10), "Bad.dll+0x10"); // the same ID from a Windows crash event
    std::fs::write(&p, b"junk").expect("write");
    assert_eq!(dump_module(&p), None);
}

fn three_mods(game: &str) -> Vec<(u64, u32, String)> {
    vec![
        (0x1000, 0x1000, format!("{game}\\bin\\x64\\Cyberpunk2077.exe")),
        (0x5000, 0x100, format!("{game}\\red4ext\\plugins\\Bad Plugin\\Bad.dll")),
        (0x9000, 0x100, format!("{game}\\bin\\x64\\version.dll")),
    ]
}

fn mods_ref(m: &[(u64, u32, String)]) -> Vec<(u64, u32, &str)> {
    m.iter().map(|(b, s, p)| (*b, *s, p.as_str())).collect()
}

// test_doctor.py Doctor.test_call_path_names_mods_on_the_crashing_stack
#[test]
fn test_call_path_names_mods_on_the_crashing_stack() {
    let fx = Fx::new();
    let mods = three_mods(&fx.g());
    let (_t, p) = write_temp("minidump.dmp", &minidump(&mods_ref(&mods), Some(0x1010), &[0x123, 0x9010, 0x1500, 0x5010, 0x9020], &[]));
    let d = read_dump(&p).expect("dump");
    assert_eq!(d["module"], json!(mods[0].2));
    assert_eq!(d["code"], json!(0xC000_0005u32));
    assert_eq!(names_of(&d["stack"]), ["version.dll", "Cyberpunk2077.exe", "Bad.dll"]); // newest call first
    let mut crash = d.clone();
    crash["crashed"] = json!(true);
    let probs = fx.by_id(crash);
    assert_eq!(probs["stack-cet"]["severity"], json!("high")); // ran last before the crash: main suspect
    assert_eq!(probs["stack-bad"]["severity"], json!("medium"));
    assert_eq!(probs["stack-cet"]["auto"], json!(false)); // a stack hint never disables a mod by itself
    assert!(!probs.contains_key("unknown"));
    // where in the module: an offset its author can look up
    assert!(probs["stack-cet"]["detail"].as_str().expect("detail").contains("Where: version.dll+0x10"));
}

// test_doctor.py Doctor.test_freeze_snapshot_blames_mod_code_on_the_main_thread
#[test]
fn test_freeze_snapshot_blames_mod_code_on_the_main_thread() {
    let fx = Fx::new();
    let mods = three_mods(&fx.g());
    let (_t, p) = write_temp("freeze.dmp", &minidump(&mods_ref(&mods), None, &[0x1010, 0x5020], &[(8, vec![0x9010]), (9, vec![0x5030])]));
    let hang = read_dump(&p).expect("dump");
    assert!(hang["module"].is_null()); // a snapshot has no crash
    assert_eq!(names_of(&hang["stack"]), ["Cyberpunk2077.exe", "Bad.dll"]);
    assert_eq!(names_of(&hang["others"]), ["version.dll"]); // Bad.dll is already on the main thread
    let probs = fx.by_id(json!({"crashed": true, "hang": hang}));
    let bad = &fx.store.mod_by_id("bad").expect("bad").name;
    assert_eq!(probs["freeze-bad"]["title"], json!(format!("{bad} was running when the game froze")));
    // other threads are named, not blamed
    assert!(probs["freeze-bad"]["detail"].as_str().expect("detail").contains(&fx.store.mod_by_id("cet").expect("cet").name));
    std::fs::write(&p, minidump(&mods_ref(&mods), None, &[0x1010], &[(8, vec![0x9010])])).expect("write");
    let probs = fx.by_id(json!({"crashed": true, "hang": read_dump(&p).expect("dump")}));
    assert_eq!(probs["freeze"]["fixes"][0]["action"], json!(["bisect_start"])); // no mod code on the main thread
}

/// A game process for `watch_with`: `hung` and `wait` answers come from the lists, the clock moves 10 s per call.
struct Fake {
    states: RefCell<VecDeque<bool>>,
    waits: RefCell<VecDeque<u32>>,
    clock: RefCell<f64>,
    clean_exit: bool,
    snap: PathBuf,
    dumps: RefCell<Vec<(u32, PathBuf)>>,
}

impl Os for Fake {
    fn find_pid(&self) -> Option<u32> {
        Some(42)
    }
    fn open(&self, _: u32) -> Option<usize> {
        Some(1)
    }
    fn wait(&self, _: usize, _: u32) -> u32 {
        self.waits.borrow_mut().pop_front().expect("wait")
    }
    fn exit_code(&self, _: usize) -> Option<u32> {
        self.clean_exit.then_some(0) // else Windows gave no exit code
    }
    fn close(&self, _: usize) {}
    fn hung(&self, _: u32) -> bool {
        self.states.borrow_mut().pop_front().expect("state")
    }
    fn write_dump(&self, pid: u32, folder: &Path) -> Option<PathBuf> {
        self.dumps.borrow_mut().push((pid, folder.to_path_buf()));
        Some(self.snap.clone())
    }
    fn now(&self) -> f64 {
        let mut c = self.clock.borrow_mut();
        let t = *c;
        *c += 10.0;
        t
    }
}

// test_doctor.py Doctor.test_watch_snapshots_a_freeze_and_drops_it_when_the_game_comes_back
#[test]
fn test_watch_snapshots_a_freeze_and_drops_it_when_the_game_comes_back() {
    for (recovers, clean_exit) in [(false, false), (true, false), (false, true)] {
        let mut states = vec![true, true, true, !recovers];
        states.extend([true; 5]);
        let mut waits = vec![watch::WAIT_TIMEOUT; 6];
        waits.push(0);
        let (_t, snap) = write_temp("freeze-1.dmp", b"MDMP");
        let fake = Fake {
            states: RefCell::new(states.into()),
            waits: RefCell::new(waits.into()),
            clock: RefCell::new(0.0),
            clean_exit,
            snap: snap.clone(),
            dumps: RefCell::new(vec![]),
        };
        let stop = AtomicBool::new(false);
        let mut seen = vec![];
        let dir = snap.parent().expect("dir").to_path_buf();
        watch::watch_with(
            &fake,
            &mut |_| {},
            &mut |_, _, freeze| {
                seen.push(freeze);
                stop.store(true, Ordering::SeqCst);
            },
            &stop,
            0.0,
            Some(&dir),
        );
        assert_eq!(*fake.dumps.borrow(), [(42, dir.clone())]); // one snapshot per freeze, after HANG_AFTER seconds
        let kept = !recovers && !clean_exit; // a long load or a slow clean quit is no freeze: its snapshot is deleted
        assert_eq!(seen, [if kept { Some(snap.clone()) } else { None }]);
        assert_eq!(snap.exists(), kept);
    }
}

/// Kills the child process when the test ends.
struct Kill(std::process::Child);

impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Helper for the hung-window test: run in a child process, it shows a window and then stops reading its messages.
#[test]
#[ignore]
fn hung_window_child() {
    use windows_sys::Win32::UI::WindowsAndMessaging::{CreateWindowExW, MSG, PM_REMOVE, PeekMessageW, WS_POPUP, WS_VISIBLE};
    if std::env::var_os("CP2077MM_HUNG_WINDOW").is_none() {
        return;
    }
    let class = shared::win::wide("STATIC");
    // SAFETY: a system window class and NUL-terminated strings; the window lives until the process ends.
    unsafe {
        let w = CreateWindowExW(0, class.as_ptr(), class.as_ptr(), WS_POPUP | WS_VISIBLE, -2000, -2000, 10, 10,
                                std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null());
        assert!(!w.is_null());
        let mut msg: MSG = std::mem::zeroed();
        while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {}
    }
    std::thread::sleep(std::time::Duration::from_secs(60));
}

// test_doctor.py Doctor.test_real_snapshot_of_a_hung_window
#[test]
fn test_real_snapshot_of_a_hung_window() {
    let child = std::process::Command::new(std::env::current_exe().expect("exe"))
        .args(["--ignored", "--exact", "tests::hung_window_child", "--test-threads=1"])
        .env("CP2077MM_HUNG_WINDOW", "1")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("child");
    let child = Kill(child);
    let pid = child.0.id();
    // Windows calls a window "Not responding" after 5 s without messages
    let became_hung = (0..40).any(|_| {
        let h = watch::hung(pid);
        if !h {
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        h
    });
    assert!(became_hung, "the window never became hung");
    let folder = TempDir::new();
    let snap = watch::write_dump(pid, folder.path(), 5).expect("snapshot");
    assert!(snap.exists());
    let hang = read_dump(&snap).expect("dump");
    assert!(hang["module"].is_null());
    let all: Vec<&str> = hang["stack"].as_array().into_iter().chain(hang["others"].as_array()).flatten().filter_map(Value::as_str).collect();
    assert!(all.iter().any(|m| m.to_lowercase().ends_with(".dll")), "{hang}");
    assert!(!watch::hung(std::process::id())); // this test process has no hung window
}

// test_doctor.py Doctor.test_hand_installed_mods
#[test]
fn test_hand_installed_mods() {
    let mut fx = Fx::new();
    fx.write("archive/pc/mod/handmade.archive", "x");
    fx.write("red4ext/plugins/Manual/Manual.dll", "x");
    fx.write("r6/scripts/Fancy HUD/other.reds", "x"); // a folder this app installed: not by hand
    let mut hand: Vec<String> = doctor::hand_installed(&fx.store).into_iter().map(|h| h.0).collect();
    hand.sort();
    assert_eq!(hand, ["archive/pc/mod/handmade.archive", "red4ext/plugins/Manual"]);
    let probs = fx.by_id(json!({"crashed": true}));
    let manual: Vec<&String> = probs.keys().filter(|i| i.starts_with("manual-")).collect();
    assert_eq!(manual.len(), 1);
    let pid = manual[0].clone();
    assert!(probs[&pid]["detail"].as_str().expect("detail").contains("handmade.archive"));
    fx.store.doctor.insert("ignored".into(), json!([pid]));
    assert!(!ids(&fx.diagnose(json!({"crashed": true}))).contains(&pid));
}

// test_doctor.py Doctor.test_game_update_since_last_good_launch
#[test]
fn test_game_update_since_last_good_launch() {
    let mut fx = Fx::new();
    assert!(doctor::game_updated(&fx.store, &no_update).is_empty()); // no good launch on record
    fx.store.doctor.insert("last_good_game".into(), json!("2.3"));
    assert!(doctor::game_updated_with(&fx.store, &no_update, &|_| Some("2.3".into())).is_empty());
    let found = doctor::game_updated_with(&fx.store, &|m| m.id == "cet", &|_| Some("2.31".into()));
    assert_eq!(found.len(), 1);
    assert_eq!(found[0]["fixes"][0]["action"], json!(["update", "cet"]));
    assert_eq!(found[0]["severity"], json!("high"));
    assert!(found[0]["detail"].as_str().expect("detail").contains("2.3 -> 2.31"));
}

// test_doctor.py Doctor.test_slow_dll_mod_from_red4ext_log
#[test]
fn test_slow_dll_mod_from_red4ext_log() {
    let fx = Fx::new();
    let g = fx.g();
    fx.write(
        "red4ext/logs/red4ext.log",
        &format!(
            "[2026-11-19 10:00:00.000] [RED4ext] [info] Loading plugins...\n\
             [2026-11-19 10:00:00.100] [RED4ext] [info] Loading plugin from '{g}\\red4ext\\plugins\\Bad Plugin\\Bad.dll'...\n\
             [2026-11-19 10:00:07.600] [RED4ext] [info] Bad (version: 1.0) has been loaded\n\
             [2026-11-19 10:01:00.000] [RED4ext] [info] Loading plugin from '{g}\\red4ext\\plugins\\Bad Plugin\\Bad.dll'...\n\
             [2026-11-19 11:00:00.000] [RED4ext] [info] Game closed\n"
        ), // an hour: a quiet log, not a slow load
    );
    assert_eq!(doctor::slow_mods(&fx.game, 0.0, &fx.idx(), doctor::SLOW), [("bad".to_string(), 7.5)]);
    assert!(ids(&doctor::diagnose(&fx.store, 0.0, None, &no_update)).contains(&"slow-bad".to_string()));
}

// test_doctor.py Doctor.test_report_hides_this_pc_paths
#[test]
fn test_report_hides_this_pc_paths() {
    let fx = Fx::new();
    let g = fx.g();
    let crash = json!({"crashed": true, "exit_code": 0xC000_0005u32, "code": 0xC000_0005u32,
                       "module": format!("{g}\\red4ext\\plugins\\Bad Plugin\\Bad.dll"),
                       "stack": [format!("{}\\bin\\x64\\x.dll", g.to_uppercase())]}); // Windows paths may differ in case
    let text = doctor::report(&fx.store, &fx.diagnose(crash.clone()), Some(&crash), false);
    assert!(!text.to_lowercase().contains(&g.to_lowercase()));
    let home = std::env::var("USERPROFILE").expect("USERPROFILE");
    assert!(!text.contains(&home));
    assert!(text.contains("<game>\\red4ext\\plugins\\Bad Plugin\\Bad.dll"));
    assert!(text.contains("Exit code: 0xC0000005"));
    assert!(text.contains(&fx.store.mod_by_id("bad").expect("bad").name));
}

// test_doctor.py Doctor.test_crash_code_in_plain_words
#[test]
fn test_crash_code_in_plain_words() {
    let fx = Fx::new();
    let probs = fx.by_id(json!({"crashed": true, "module": format!("{}\\red4ext\\plugins\\Bad Plugin\\Bad.dll", fx.g()), "code": 0xC000_0005u32}));
    assert!(probs["crash-bad"]["detail"].as_str().expect("detail").contains("memory it does not own"));
}

// test_doctor.py Doctor.test_advanced_crash_reporter_report
#[test]
fn test_advanced_crash_reporter_report() {
    let mut fx = Fx::new();
    fx.write(
        &format!("{}/report-1.txt", doctor::ACR),
        &format!("SUMMARY\nLikely cause: {}\\red4ext\\plugins\\Bad Plugin\\Bad.dll\nINSTALLED MODS\nr6/scripts/Fancy HUD/hud.reds\n", fx.g()),
    );
    let found = ids(&fx.diagnose(json!({"crashed": true})));
    assert!(found.contains(&"acr-bad".to_string()));
    assert!(!found.contains(&"acr-fancy".to_string())); // the installed-mod list is not a verdict
    std::fs::remove_file(fx.game.join(doctor::ACR).join("report-1.txt")).expect("unlink");
    fx.write(&format!("{}/report-2.txt", doctor::ACR), "SUMMARY\nNo mod found\nINSTALLED MODS\nr6/scripts/Fancy HUD/hud.reds\n");
    let probs = fx.by_id(json!({"crashed": true}));
    assert_eq!(probs["acr-report-2.txt"]["severity"], json!("info"));
    fx.store.doctor.insert("ignored".into(), json!(["acr-report-2.txt"]));
    assert!(!ids(&fx.diagnose(json!({"crashed": true}))).contains(&"acr-report-2.txt".to_string()));
}

// test_doctor.py Doctor.test_errors_seen_in_clean_launches_are_not_blamed
#[test]
fn test_errors_seen_in_clean_launches_are_not_blamed() {
    let mut fx = Fx::new();
    fx.write("r6/logs/redscript_rCURRENT.log", &format!("[ERROR - Mon] At {}\\r6\\scripts\\Fancy HUD\\hud.reds:12:5:\n", fx.g()));
    let crash = json!({"crashed": true, "module": null});
    fx.store.doctor.insert("clean_errors".into(), json!([["fancy"]])); // one clean launch is not enough to rule a mod out
    let p = fx.by_id(crash.clone()).remove("log-fancy").expect("log-fancy");
    assert_eq!((&p["severity"], &p["auto"]), (&json!("high"), &json!(true)));
    fx.store.doctor.insert("clean_errors".into(), json!([["fancy"], []]));
    let p = fx.by_id(crash).remove("log-fancy").expect("log-fancy");
    assert_eq!((&p["severity"], &p["auto"]), (&json!("info"), &json!(false)));
}

// test_doctor.py Doctor.test_crash_id_and_offer_of_advanced_crash_reporter
#[test]
fn test_crash_id_and_offer_of_advanced_crash_reporter() {
    let mut fx = Fx::new();
    fx.drop_mod("bad");
    let crash = json!({"crashed": true, "module": null, "fault": "Cyberpunk2077.exe+0x2A41E06", "saved": "C:\\x\\crash.zip"});
    fx.store.doctor.insert("crashes".into(), json!([{"time": 2e9, "fault": crash["fault"]}]));
    let probs = fx.by_id(crash.clone());
    let unknown = &probs["unknown"];
    let fixes = unknown["fixes"].as_array().expect("fixes");
    assert_eq!(fixes[fixes.len() - 1]["action"], json!(["queue_nexus", doctor::ACR_NEXUS])); // not installed: offer it
    let detail = unknown["detail"].as_str().expect("detail");
    assert!(detail.contains("This crash is new"));
    assert!(detail.contains("Crash logs saved in C:\\x\\crash.zip"));
    if let Some(Value::Array(c)) = fx.store.doctor.get_mut("crashes") {
        c.insert(0, json!({"time": 1e9, "fault": crash["fault"]}));
    }
    std::fs::create_dir_all(fx.game.join(doctor::ACR)).expect("mkdir");
    std::fs::write(fx.game.join("bin/x64/plugins/AdvancedCrashReporter/AdvancedCrashReporter.dll"), b"MZ").expect("dll"); // reports alone are not the mod
    let probs = fx.by_id(crash);
    assert!(probs["unknown"]["detail"].as_str().expect("detail").contains("The same crash happened 2 times"));
    assert!(!verbs(&probs["unknown"]).contains(&"queue_nexus".to_string())); // installed already
}

// test_doctor.py Doctor.test_bisect_keeps_requirements_with_the_mods_that_need_them
#[test]
fn test_bisect_keeps_requirements_with_the_mods_that_need_them() {
    let mut fx = Fx::new();
    let nx = |id: &str, nid: u64, req: &[u64]| {
        let mut m = mod_rec(id, &[]);
        m.source = json!({"type": "nexus", "mod_id": nid});
        m.requires = req.iter().map(|r| json!({"mod_id": r, "name": ""})).collect();
        m
    };
    fx.store.mods = vec![nx("user", 4, &[1]), nx("a", 2, &[]), nx("b", 3, &[]), nx("c", 5, &[]), nx("lib", 1, &[])];
    fx.store.doctor.insert("crashes".into(), json!([{"time": 1, "fault": null, "secs": 90}]));
    let state = doctor::bisect_start(&fx.store);
    assert_eq!(state["testing"], json!(["lib", "user", "a"])); // a requirement comes before the mod that needs it
    assert_eq!(state["crash_secs"], json!(90));
    let mut part = state.clone();
    part["suspects"] = json!(["lib", "a"]);
    part["testing"] = json!(["a"]);
    assert_eq!(doctor::bisect_off(&part), BTreeSet::from(["lib".to_string(), "user".to_string()]));
    assert_eq!(doctor::bisect_step(&state, true, 300.0)["crash_secs"], json!(300));
}

// test_doctor.py Doctor.test_report_for_an_ai_with_recent_crashes
#[test]
fn test_report_for_an_ai_with_recent_crashes() {
    let mut fx = Fx::new();
    fx.store.doctor.insert("crashes".into(), json!([{"time": 1e9, "fault": "Bad.dll+0x10", "secs": 300}]));
    let probs = [doctor::problem("x", "T", &format!("Saved in {}\\crash_logs\\a.zip", fx.store.root.display()), vec![], "high", true)];
    let text = doctor::report(&fx.store, &probs, None, true);
    assert!(text.starts_with(doctor::AI_ASK));
    assert!(text.contains("<data folder>\\crash_logs"));
    assert!(text.contains("Bad.dll+0x10, after 5 min"));
    assert!(!doctor::report(&fx.store, &probs, None, false).starts_with(doctor::AI_ASK));
}

// test_doctor.py Doctor.test_crash_logs_are_kept
#[test]
fn test_crash_logs_are_kept() {
    let fx = Fx::new();
    fx.write("r6/logs/redscript_rCURRENT.log", "[ERROR] x\n");
    let rep_dir = TempDir::new();
    let rep = rep_dir.path().join("report1");
    std::fs::create_dir(&rep).expect("mkdir");
    std::fs::write(rep.join("minidump.dmp"), b"MDMP").expect("write");
    let logs = fx.store.root.join("crash_logs");
    std::fs::create_dir_all(&logs).expect("mkdir");
    for old in ["crash-20000101-000000.zip", "crash-20010101-000000.zip"] {
        std::fs::write(logs.join(old), b"").expect("write");
    }
    let z = doctor::keep_logs(&fx.store, doctor::now() - 60.0, Some(&rep.to_string_lossy()), 2, doctor::LOG_LIMIT).expect("zip");
    let zf = zip::ZipArchive::new(std::fs::File::open(&z).expect("open")).expect("zip");
    let names: Vec<String> = zf.file_names().filter_map(Result::ok).map(String::from).collect();
    assert!(names.contains(&"report/report1/minidump.dmp".to_string()));
    assert!(names.contains(&"r6/logs/redscript_rCURRENT.log".to_string()));
    let mut left: Vec<String> = std::fs::read_dir(&logs).expect("dir").flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    left.sort();
    let mut want = vec!["crash-20010101-000000.zip".to_string(), z.file_name().expect("name").to_string_lossy().into_owned()];
    want.sort();
    assert_eq!(left, want);
    drop(fx.tmp);
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

// test_doctor.py Doctor.test_parse_events_only_game
#[test]
fn test_parse_events_only_game() {
    let found = parse_events(&events("C:\\G"));
    assert_eq!(found.len(), 1);
    let (module, started, offset) = &found[0];
    assert_eq!(*offset, Some(0x1A2B));
    assert_eq!(module, "C:\\G\\red4ext\\plugins\\Bad Plugin\\Bad.dll");
    let (y, m, d) = civil_from_days((started.expect("start") / 86400.0).floor() as i64); // FILETIME decoded
    assert_eq!(format!("{y:04}-{m:02}-{d:02}"), "2026-11-05");
}
