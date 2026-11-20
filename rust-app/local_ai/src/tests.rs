//! The local AI's answer checks, with no real model (test_ai.py LocalAi).
use super::*;
use downloads::fetch::Response;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Resets the network mock and the engine state when a test ends, also when it fails.
struct Reset;

impl Drop for Reset {
    fn drop(&mut self) {
        fetch::set_mock(None);
        let mut s = state();
        s.server = None;
        s.blocked = false;
        s.key.clear();
    }
}

fn reset() -> Reset {
    let r = Reset;
    drop(Reset);
    r
}

fn ok(v: Value) -> Response {
    Response { status: 200, body: v.to_string().into_bytes(), ..Default::default() }
}

/// The local AI answers `answers` in turn; the result holds (system, user, schema) of each question.
fn answers(answers: Vec<Value>) -> Arc<Mutex<Vec<(String, String, Value)>>> {
    let asked = Arc::new(Mutex::new(vec![]));
    let a = asked.clone();
    fetch::set_mock(Some(Arc::new(move |req: &Request| {
        let b: Value = serde_json::from_slice(req.body.as_deref().unwrap_or(b"")).unwrap();
        let mut a = a.lock().unwrap();
        let next = answers[a.len()].to_string();
        a.push((
            b["messages"][0]["content"].as_str().unwrap().to_string(),
            b["messages"][1]["content"].as_str().unwrap().to_string(),
            b["response_format"]["json_schema"]["schema"].clone(),
        ));
        Ok(ok(json!({"choices": [{"message": {"content": next}}]})))
    })));
    asked
}

fn named(id: &str, name: &str, enabled: bool) -> Mod {
    let mut m = testkit::mod_rec(id, &[]);
    m.name = name.into();
    m.enabled = enabled;
    m
}

fn problem(pid: &str, title: &str, detail: &str, fixes: Vec<Value>, severity: &str) -> Value {
    json!({"id": pid, "title": title, "detail": detail, "fixes": fixes, "severity": severity, "auto": true})
}

fn fix(label: &str, action: Value) -> Value {
    json!({"label": label, "action": action, "recommended": false})
}

/// A fake engine process that counts kills.
struct Fake(Arc<AtomicUsize>);

impl Engine for Fake {
    fn running(&mut self) -> bool {
        self.0.load(Ordering::SeqCst) == 0
    }

    fn kill(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

// test_ai.py LocalAi.test_advice_keeps_only_real_picks
#[test]
fn test_advice_keeps_only_real_picks() {
    let _r = reset();
    let mods = [named("patch", "A - Patch for B", true), named("a", "A", true), named("b", "B", true)];
    let probs = [
        problem("p1", "t", "d", vec![fix("Update A", json!(["update", "a"])), fix("Turn A off", json!(["disable", "a"]))], "high"),
        problem("p2", "t2", "d2", vec![fix("Keep", json!(["ignore", "p2"]))], "low"),
    ];
    let asked = answers(vec![
        json!({"builds_on": {"1": [2, 3, 1, 9, 0, "2", true], "2": [], "3": "1", "x": [1], "4": [1]}}),
        json!({"summary": " S ", "fixes": [{"problem": 1, "fix": 2, "why": "w"}, {"problem": 1, "fix": 1, "why": ""},
                                           {"problem": 2, "fix": 5, "why": ""}, {"problem": 3, "fix": 1, "why": ""},
                                           {"problem": true, "fix": 1, "why": ""}]}),
    ]);
    let a = advise(&mods, &probs, "").unwrap();
    assert_eq!(a.after, [("a".to_string(), "patch".to_string()), ("b".to_string(), "patch".to_string())]);
    let picks: Vec<(String, String, String)> =
        a.fixes.iter().map(|(p, f, why)| (text(&p["id"]), text(&f["label"]), why.clone())).collect();
    assert_eq!(picks, [("p1".to_string(), "Turn A off".to_string(), "w".to_string())]);
    assert_eq!(a.summary, "S");
    let asked = asked.lock().unwrap();
    assert!(asked[0].1.contains("1. A - Patch for B"));
    assert_eq!(asked[0].2["properties"]["builds_on"]["required"], json!(["1", "2", "3"]));
    assert!(asked[1].1.contains("P2 (low) t2: d2"));
}

// test_ai.py LocalAi.test_bug_report_gives_a_cause_and_only_real_mod_fixes
#[test]
fn test_bug_report_gives_a_cause_and_only_real_mod_fixes() {
    let _r = reset();
    let mods = [named("car", "Car", false), named("b", "B", true)];
    let asked = answers(vec![
        json!({"builds_on": {}}),
        json!({"summary": "s", "fixes": [], "cause": " The car is off. ", "mod_fixes": [
            {"mod": 1, "action": "turn_on", "why": "w"}, {"mod": 3, "action": "turn_on", "why": ""},
            {"mod": 2, "action": "delete", "why": ""}, {"mod": true, "action": "update", "why": ""}, "x"]}),
    ]);
    let a = advise(&mods, &[], "My car is missing").unwrap();
    assert_eq!(a.cause.as_deref(), Some("The car is off."));
    let picks: Vec<(&str, &str, &str)> = a.mod_fixes.iter().map(|(m, act, why)| (m.id.as_str(), act.as_str(), why.as_str())).collect();
    assert_eq!(picks, [("car", "turn_on", "w")]);
    let asked = asked.lock().unwrap();
    let (system, user, schema) = &asked[1];
    assert!(system.contains("describes a bug"));
    assert!(user.contains("Problems found:\nnone"));
    assert!(user.contains("The player says: My car is missing"));
    assert!(schema["required"].as_array().unwrap().contains(&json!("mod_fixes")));
}

// test_ai.py LocalAi.test_no_problems_asks_only_about_the_order
#[test]
fn test_no_problems_asks_only_about_the_order() {
    let _r = reset();
    let asked = answers(vec![json!({"builds_on": []}), json!({})]);
    let a = advise(&[testkit::mod_rec("a", &[])], &[], "").unwrap();
    assert_eq!(asked.lock().unwrap().len(), 1);
    assert_eq!(a, Advice { summary: "The Crash doctor found no problems.".into(), fixes: vec![], after: vec![], cause: None, mod_fixes: vec![] });
}

// test_ai.py LocalAi.test_a_download_with_the_wrong_hash_is_deleted
#[test]
fn test_a_download_with_the_wrong_hash_is_deleted() {
    let d = testkit::TempDir::new();
    let f = d.path().join("x.bin");
    std::fs::write(&f, b"abc").unwrap();
    let good = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    assert_eq!(checked(&f, good).unwrap(), f);
    assert!(checked(&f, &"0".repeat(64)).is_err());
    assert!(!f.exists());
}

// test_ai.py LocalAi.test_start_needs_the_files_and_stop_ends_the_engine
#[test]
fn test_start_needs_the_files_and_stop_ends_the_engine() {
    let _r = reset();
    let e = start_with(Duration::from_secs(180), &|| false, &|_, _| panic!("no engine without the files")).unwrap_err();
    assert!(e.contains("files are missing"), "{e}");
    let kills = Arc::new(AtomicUsize::new(0));
    state().server = Some(Box::new(Fake(kills.clone())));
    stop();
    assert!(state().server.is_none());
    assert_eq!(kills.load(Ordering::SeqCst), 1);
    stop(); // nothing runs: no error
}

// test_ai.py LocalAi.test_a_blocked_engine_does_not_start
#[test]
fn test_a_blocked_engine_does_not_start() {
    let _r = reset();
    block(true);
    let e = start_with(Duration::from_secs(180), &|| true, &|_, _| panic!("the engine must not start")).unwrap_err();
    assert_eq!(e, BLOCKED);
}

// test_ai.py LocalAi.test_the_game_starts_while_the_model_loads
#[test]
fn test_the_game_starts_while_the_model_loads() {
    let _r = reset();
    // the game starts while the engine loads the model
    fetch::set_mock(Some(Arc::new(|_: &Request| {
        block(true);
        Ok(ok(json!({"status": "ok"})))
    })));
    let kills = Arc::new(AtomicUsize::new(0));
    let k = kills.clone();
    let e = start_with(Duration::from_secs(180), &|| true, &move |_, _| Ok(Box::new(Fake(k.clone())) as Box<dyn Engine>)).unwrap_err();
    assert_eq!(e, BLOCKED);
    assert_eq!(kills.load(Ordering::SeqCst), 1); // the engine does not run next to the game
    assert!(state().server.is_none());
}

// test_ai.py LocalAi.test_chat_asks_for_json_without_thinking
#[test]
fn test_chat_asks_for_json_without_thinking() {
    let _r = reset();
    state().key = "k1".into();
    let seen: Arc<Mutex<Option<Request>>> = Arc::new(Mutex::new(None));
    let s = seen.clone();
    fetch::set_mock(Some(Arc::new(move |req: &Request| {
        *s.lock().unwrap() = Some(req.clone());
        Ok(ok(json!({"choices": [{"message": {"content": "{\"a\": 1}"}}]})))
    })));
    assert_eq!(chat("sys", "user", &json!({"type": "object"})).unwrap(), json!({"a": 1}));
    let req = seen.lock().unwrap().clone().unwrap();
    let body: Value = serde_json::from_slice(req.body.as_deref().unwrap()).unwrap();
    assert!(req.url.ends_with("/v1/chat/completions"));
    assert_eq!(body["response_format"]["json_schema"]["schema"], json!({"type": "object"}));
    assert_eq!(body["chat_template_kwargs"]["enable_thinking"], json!(false));
    let roles: Vec<&str> = body["messages"].as_array().unwrap().iter().map(|m| m["role"].as_str().unwrap()).collect();
    assert_eq!(roles, ["system", "user"]);
    // the engine only answers the app
    let auth: Vec<&str> = req.headers.iter().filter(|(k, _)| k == "Authorization").map(|(_, v)| v.as_str()).collect();
    assert_eq!(auth, ["Bearer k1"]);
}
