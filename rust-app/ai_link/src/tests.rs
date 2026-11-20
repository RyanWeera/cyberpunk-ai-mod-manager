//! The AI-app link (MCP server and its tools), with no real AI app (test_ai.py Mcp, Tools).
use crate::{mcp, tools};
use serde_json::{Value, json};
use shared::Store;
use std::sync::Mutex;

fn rpc(i: i64, method: &str, params: Option<Value>) -> Value {
    let mut m = json!({"jsonrpc": "2.0", "id": i, "method": method});
    if let Some(p) = params {
        m["params"] = p;
    }
    m
}

// test_ai.py Mcp.test_handshake_list_and_calls
#[test]
fn test_handshake_list_and_calls() {
    let call = |name: &str, args: &Value| -> Result<String, String> {
        if name == "bad" {
            return Err("window gone".into());
        }
        Ok(format!("{name} {args}"))
    };
    let msgs = [
        rpc(1, "initialize", Some(json!({"protocolVersion": "2025-03-26"}))).to_string(),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}).to_string(),
        "".into(),
        "not json".into(),
        rpc(2, "tools/list", None).to_string(),
        rpc(3, "tools/call", Some(json!({"name": "status"}))).to_string(),
        rpc(4, "tools/call", Some(json!({"name": "bad", "arguments": {}}))).to_string(),
        rpc(5, "nope", None).to_string(),
        json!({"jsonrpc": "2.0", "id": 6, "method": "tools/call", "params": [1]}).to_string(),
        rpc(7, "ping", None).to_string(),
    ];
    let input: String = msgs.iter().map(|m| format!("{m}\n")).collect();
    let mut out = vec![];
    mcp::serve(&call, "9.9", input.as_bytes(), &mut out);
    let r: Vec<Value> = String::from_utf8(out).unwrap().lines().map(|x| serde_json::from_str(x).unwrap()).collect();
    // no answer to the notification or the empty line
    assert_eq!(r.iter().map(|x| x["id"].clone()).collect::<Vec<_>>(), [json!(1), Value::Null, json!(2), json!(3), json!(4), json!(5), json!(6), json!(7)]);
    assert_eq!(r[6]["result"]["isError"], true); // params that are not an object: an error, and the server goes on
    assert_eq!(r[0]["result"]["protocolVersion"], "2025-03-26");
    assert_eq!(r[0]["result"]["serverInfo"]["version"], "9.9");
    assert_eq!(r[1]["error"]["code"], -32700);
    let names = |v: &Value| v.as_array().unwrap().iter().map(|t| t["name"].clone()).collect::<Vec<_>>();
    assert_eq!(names(&r[2]["result"]["tools"]), names(&tools::TOOLS));
    assert_eq!(r[3]["result"], json!({"content": [{"type": "text", "text": "status {}"}], "isError": false}));
    assert_eq!((&r[4]["result"]["content"][0]["text"], &r[4]["result"]["isError"]), (&json!("window gone"), &json!(true)));
    assert_eq!(r[5]["error"]["code"], -32601);
}

/// The controller as the tools see it: the Tools setUp.
struct Fake {
    store: Mutex<Store>,
    _dir: testkit::TempDir,
    jobs: Vec<Value>,
    added: Mutex<Vec<String>>,
}

impl tools::Host for Fake {
    fn add_links(&self, text: &str) {
        self.added.lock().unwrap().push(text.into());
    }
    fn store(&self) -> &Mutex<Store> {
        &self.store
    }
    fn has_key(&self) -> bool {
        true // key "k"
    }
    fn premium(&self) -> bool {
        false
    }
    fn jobs(&self) -> Vec<Value> {
        self.jobs.clone()
    }
    fn auto_sort(&self) -> Result<String, String> {
        Ok("sorted".into())
    }
    fn check_now(&self) -> Result<Vec<Value>, String> {
        Ok(vec![])
    }
}

fn fake() -> Fake {
    let (dir, store) = testkit::temp_store();
    Fake { store: Mutex::new(store), _dir: dir, jobs: vec![], added: Mutex::new(vec![]) }
}

fn run_tool(ctl: &Fake, tool: &str, args: Value) -> Value {
    tools::run(ctl, &json!({"tool": tool, "args": args}).to_string())
}

// test_ai.py Tools.test_add_mods_takes_only_mod_links_and_says_what_the_user_must_do
#[test]
fn test_add_mods_takes_only_mod_links_and_says_what_the_user_must_do() {
    let ctl = fake();
    let links = [
        "https://www.nexusmods.com/cyberpunk2077/mods/107",
        "",
        "https://www.youtube.com/watch?v=x",
        "https://example.com/x.zip",
        r"\\host\share\x.zip",
        "C:/Downloads/x.zip",
        "https://github.com/wopss/RED4ext",
        "https://github.com/someone/cp2077-thing",
    ];
    let text = run_tool(&ctl, "add_mods", json!({"links": links.join("\n")}))["text"].as_str().unwrap().to_string();
    assert_eq!(*ctl.added.lock().unwrap(), ["https://www.nexusmods.com/cyberpunk2077/mods/107\nhttps://github.com/wopss/RED4ext"]);
    // "Not a web link" comes before parse_link, which would look at the path (that logs in to the host)
    for part in [
        "Added 2 link(s)",
        "Not added:",
        "find_mods_in_video",
        "folder is not set",
        "Slow download",
        "Not a Nexus Mods link: https://example.com/x.zip",
        "Not a Nexus Mods link: https://github.com/someone",
        r"Not a web link: \\host",
        "Not a web link: C:/",
    ] {
        assert!(text.contains(part), "{part}: {text}");
    }
}

// test_ai.py Tools.test_status_sort_and_bad_calls
#[test]
fn test_status_sort_and_bad_calls() {
    let mut ctl = fake();
    {
        let mut st = ctl.store.lock().unwrap();
        let mut a = testkit::mod_rec("a", &[]);
        a.name = "Alpha".into();
        let mut b = testkit::mod_rec("b", &[]);
        b.name = "Beta".into();
        b.enabled = false;
        st.mods = vec![a, b];
    }
    ctl.jobs = vec![json!({"name": "Gamma", "status": "Click 'Slow download'", "state": "waiting"})];
    let text = run_tool(&ctl, "status", json!({}))["text"].as_str().unwrap().to_string();
    for part in ["free account", "- Gamma:", "Waiting files", "1. Alpha", "2. Beta (off)"] {
        assert!(text.contains(part), "{part}: {text}");
    }
    assert_eq!(run_tool(&ctl, "sort_load_order", json!({})), json!({"text": "sorted"}));
    assert_eq!(run_tool(&ctl, "format_disk", json!({})), json!({"error": "Unknown tool: format_disk"}));
    assert!(run_tool(&ctl, "status", json!({"extra": 1})).get("error").is_some());
    assert!(tools::run(&ctl, "{not json").get("error").is_some());
}

// test_ai.py Tools.test_the_home_folder_never_goes_to_the_ai_app
#[test]
fn test_the_home_folder_never_goes_to_the_ai_app() {
    let mut ctl = fake();
    let home = tools::home();
    let bad = format!("{home}\\x\\a.zip");
    let repr = format!("'{}'", bad.replace('\\', "\\\\"));
    ctl.jobs = vec![json!({"name": "M", "status": format!("Failed: {bad} {repr} {}", bad.replace('\\', "/")), "state": "failed"})];
    let text = run_tool(&ctl, "status", json!({}))["text"].as_str().unwrap().to_lowercase();
    for form in [home.clone(), home.replace('\\', "\\\\"), home.replace('\\', "/")] {
        assert!(!text.contains(&form.to_lowercase()), "{form}");
    }
    assert!(text.contains("~\\x\\a.zip"));
}
