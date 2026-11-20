//! The tools that AI apps (Claude Desktop, Claude Code, Cursor and other MCP apps) can call. `TOOLS` is what the
//! MCP server lists; `run` does a call inside the mod manager window, with the same controller the buttons use.
use crate::mcp::truthy;
use downloads::nexus;
use serde_json::{Map, Value, json};
use shared::{Store, fw_by_repo};
use std::sync::{LazyLock, Mutex};

pub static TOOLS: LazyLock<Value> = LazyLock::new(|| {
    json!([
        {"name": "add_mods", "description": concat!("Download and install Cyberpunk 2077 mods in the user's CP2077 AI Mod Manager. ",
         "The manager also installs each mod's required mods and frameworks, virus-scans the files and copies them into the game."),
         "inputSchema": {"type": "object", "required": ["links"], "properties": {"links": {"type": "string", "description": concat!(
             "One link per line: Nexus Mods mod pages or collections (cyberpunk2077), or the GitHub repository of a framework ",
             "such as RED4ext. For a video link (YouTube, TikTok, Instagram...) call find_mods_in_video first. Other GitHub ",
             "repositories, direct download links and files on the PC are not taken from AI apps: the user can paste them in ",
             "the manager's Add mods tab.")}}}},
        {"name": "find_mods_in_video", "description": concat!("List the Cyberpunk 2077 mods that a video shows, from the links and text in ",
         "its description or caption (YouTube works best). Rows found by name only are guesses: confirm them with the user."),
         "inputSchema": {"type": "object", "required": ["url"], "properties": {"url": {"type": "string", "description":
             "The video link, or a caption or mod name to search Nexus for."}}}},
        {"name": "status", "description": concat!("The manager's state: game folder and Nexus account set-up, downloads in progress ",
         "(and the files that wait for a 'Slow download' click on Nexus), and the installed mods in load order."),
         "inputSchema": {"type": "object", "properties": {}}},
        {"name": "sort_load_order", "description": concat!("Put the installed mods in the best load order (frameworks first, each mod ",
         "below the mods it needs, patches below the mods they change). The user can undo it in the Crash doctor tab."),
         "inputSchema": {"type": "object", "properties": {}}},
        {"name": "check_problems", "description": concat!("Look for missing frameworks and required mods, mod errors in the game logs ",
         "and file conflicts. Returns each problem with the fixes the user can apply in the Crash doctor tab."),
         "inputSchema": {"type": "object", "properties": {}}},
    ])
});

/// The controller operations the tools use (`manager_ui/controller.py`, `Controller`). The window's controller
/// implements it; ai_link cannot depend on manager_ui.
pub trait Host {
    /// `Controller.add_links(text)`: add the links (one per line) in the background.
    fn add_links(&self, text: &str);
    /// `Controller.store`. Lock it once at a time: the lock is not re-entrant.
    fn store(&self) -> &Mutex<Store>;
    /// `bool(Controller.key)`: a Nexus API key is set. The key itself never reaches the AI app.
    fn has_key(&self) -> bool;
    /// `Controller.premium`.
    fn premium(&self) -> bool;
    /// A copy of `Controller.jobs`, taken under `Controller.lock`: [{"name", "status", "state", ...}].
    fn jobs(&self) -> Vec<Value>;
    /// `Controller.auto_sort()`: the message for the user.
    fn auto_sort(&self) -> Result<String, String>;
    /// `Controller.check_now()`: the problems, as `doctor::problem` makes them.
    fn check_now(&self) -> Result<Vec<Value>, String>;
}

/// Text of a JSON value: strings as they are, other values as JSON.
fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        v => v.to_string(),
    }
}

fn lock(ctl: &dyn Host) -> std::sync::MutexGuard<'_, Store> {
    ctl.store().lock().unwrap_or_else(|e| e.into_inner())
}

pub fn add_mods(ctl: &dyn Host, links: &str) -> Result<String, String> {
    let (mut ok, mut bad) = (vec![], vec![]);
    let lines = links.split(['\n', '\r', '\x0b', '\x0c', '\x1c', '\x1d', '\x1e', '\u{85}', '\u{2028}', '\u{2029}']);
    for line in lines.map(str::trim).filter(|l| !l.is_empty()) {
        // Nexus Mods and framework web links only: a \\host\share path makes Windows log in to that host, and text
        // the AI read (a video description, a mod page) could ask it to add an archive or a GitHub release from anyone.
        let check = || -> Result<(), String> {
            let low = line.to_lowercase();
            if !low.starts_with("https://") && !low.starts_with("http://") {
                return Err(format!("Not a web link: {line}"));
            }
            let link = nexus::parse_link(line)?.unwrap_or(Value::Null);
            let kind = link["type"].as_str().unwrap_or("");
            if !(kind == "nexus" || kind == "collection" || kind == "github" && fw_by_repo(link["repo"].as_str().unwrap_or("")).is_some()) {
                return Err(format!("Not a Nexus Mods link: {line}. Ask the user to paste it in the Add mods tab"));
            }
            Ok(())
        };
        match check() {
            Ok(()) => ok.push(line),
            Err(e) => bad.push(format!("{e} (for a video link, call find_mods_in_video)")),
        }
    }
    if !ok.is_empty() {
        ctl.add_links(&ok.join("\n"));
    }
    let mut out = vec![];
    if !ok.is_empty() {
        out.push(format!("Added {} link(s). The manager downloads and installs them now; call status to follow them.", ok.len()));
    }
    if !bad.is_empty() {
        out.push("Not added:".into());
        out.extend(bad);
    }
    let (game, auto_click) = {
        let st = lock(ctl);
        (st.game(), st.settings.auto_click)
    };
    if game.is_none() {
        out.push("The Cyberpunk 2077 folder is not set: ask the user to set it in the manager's Settings tab.".into());
    }
    if ok.iter().any(|x| x.to_lowercase().contains("nexusmods")) {
        if !ctl.has_key() {
            out.push("No Nexus API key is set: ask the user to add it in the Settings tab ('Get my key').".into());
        } else if !ctl.premium() {
            out.push(if auto_click {
                concat!(
                    "Free Nexus account: the manager clicks 'Slow download' by itself in a hidden browser ",
                    "(the user logs in to Nexus once). Tell the user no clicks are needed, and that a failed ",
                    "click makes the manager open the page for a manual click instead."
                )
                .into()
            } else {
                concat!(
                    "Free Nexus account: the manager opens each file's Nexus page in the browser, and the user must ",
                    "click 'Slow download' on each one. Tell the user."
                )
                .into()
            });
        }
    }
    Ok(out.join("\n"))
}

pub fn find_mods_in_video(_ctl: &dyn Host, url: &str) -> Result<String, String> {
    let (title, rows) = video_finder::find(url)?;
    if rows.is_empty() {
        return Ok("No mods found in the video's text. Ask the user for the mod's name.".into());
    }
    let mut out: Vec<String> = if title.is_empty() { vec![] } else { vec![format!("Video: \"{title}\"")] };
    for r in &rows {
        let e = &r["endorsements"];
        let tail = if e.is_null() || e.as_f64() == Some(0.0) { ")".to_string() } else { format!(", {} endorsements)", text(e)) };
        out.push(format!("- {}: {} ({}{tail}", text(&r["name"]), text(&r["url"]), text(&r["why"])));
    }
    out.push("Pass the links to add to add_mods. Rows found by name are guesses: ask the user.".into());
    Ok(out.join("\n"))
}

pub fn status(ctl: &dyn Host) -> Result<String, String> {
    let jobs = ctl.jobs();
    let (game, auto_click, mods) = {
        let st = lock(ctl);
        (st.game(), st.settings.auto_click, st.mods.iter().map(|m| (m.name.clone(), m.enabled)).collect::<Vec<_>>())
    };
    let nexus = if !ctl.has_key() {
        "no API key (Settings tab)"
    } else if ctl.premium() {
        "Premium"
    } else {
        "free account"
    };
    let mut out = vec![
        format!("Game folder: {}", game.map_or("not set (Settings tab)".into(), |g| g.display().to_string())),
        format!("Nexus: {nexus}"),
    ];
    if !jobs.is_empty() {
        out.push("Downloads:".into());
        out.extend(jobs.iter().map(|j| format!("- {}: {} ({})", text(&j["name"]), text(&j["status"]), text(&j["state"]))));
        if jobs.iter().any(|j| j["state"] == "waiting") {
            out.push(if !auto_click {
                "Waiting files need the user to click 'Slow download' on the Nexus page the manager opened.".into()
            } else {
                concat!(
                    "Waiting files are clicked by the manager's hidden browser; if a click fails, the manager ",
                    "opens the Nexus page for a manual click."
                )
                .into()
            });
        }
    }
    out.push(format!("Installed mods, in load order ({}; the last one wins):", mods.len()));
    out.extend(mods.iter().enumerate().map(|(n, (name, on))| format!("{}. {name}{}", n + 1, if *on { "" } else { " (off)" })));
    Ok(out.join("\n"))
}

pub fn sort_load_order(ctl: &dyn Host) -> Result<String, String> {
    ctl.auto_sort()
}

pub fn check_problems(ctl: &dyn Host) -> Result<String, String> {
    let probs = ctl.check_now()?;
    if probs.is_empty() {
        return Ok("No problems found.".into());
    }
    let mut out: Vec<String> = probs
        .iter()
        .map(|p| {
            let detail: String = text(&p["detail"]).chars().take(300).collect();
            let fixes: Vec<String> = p["fixes"].as_array().into_iter().flatten().map(|f| text(&f["label"])).collect();
            format!("- [{}] {}: {detail}\n  Fixes: {}", text(&p["severity"]), text(&p["title"]), fixes.join(" / "))
        })
        .collect();
    out.push("The user applies fixes in the manager's Crash doctor tab (or with 'Ask the local AI').".into());
    Ok(out.join("\n"))
}

/// The tool's text arguments by name, or the error Python gives for a call with wrong arguments.
fn text_args<'a>(tool: &str, args: &'a Map<String, Value>, want: &[&str]) -> Result<Vec<&'a str>, String> {
    if let Some(k) = args.keys().find(|k| !want.contains(&k.as_str())) {
        return Err(format!("{tool}() got an unexpected keyword argument '{k}'"));
    }
    want.iter()
        .map(|w| match args.get(*w) {
            None => Err(format!("{tool}() missing 1 required positional argument: '{w}'")),
            Some(v) => v.as_str().ok_or_else(|| format!("'{w}' must be text")),
        })
        .collect()
}

/// One call from the MCP server process, JSON {"tool", "args"} -> {"text"} or {"error"}.
pub fn run(ctl: &dyn Host, msg: &str) -> Value {
    let call = || -> Result<Result<String, String>, Value> {
        let req: Value = serde_json::from_str(msg).map_err(|e| json!({"error": private(&e.to_string())}))?;
        let tool = req.get("tool").ok_or_else(|| json!({"error": "'tool'"}))?;
        let name = tool.as_str().unwrap_or("");
        if !TOOLS.as_array().into_iter().flatten().any(|t| t["name"] == name) {
            return Err(json!({"error": format!("Unknown tool: {}", text(tool))}));
        }
        let empty = Map::new();
        let args = match req.get("args").filter(|a| truthy(a)) {
            None => &empty,
            Some(Value::Object(a)) => a,
            Some(_) => return Err(json!({"error": format!("{name}() argument after ** must be a mapping")})),
        };
        Ok(match name {
            "add_mods" => text_args(name, args, &["links"]).and_then(|a| add_mods(ctl, a[0])),
            "find_mods_in_video" => text_args(name, args, &["url"]).and_then(|a| find_mods_in_video(ctl, a[0])),
            "status" => text_args(name, args, &[]).and_then(|_| status(ctl)),
            "sort_load_order" => text_args(name, args, &[]).and_then(|_| sort_load_order(ctl)),
            _ => text_args(name, args, &[]).and_then(|_| check_problems(ctl)),
        })
    };
    match call() {
        Ok(Ok(t)) => json!({"text": private(&t)}),
        Ok(Err(e)) => json!({"error": private(if e.is_empty() { "Error" } else { &e })}),
        Err(e) => e,
    }
}

/// The Windows user's home folder, like Python's `Path.home()`.
pub(crate) fn home() -> String {
    let h = std::env::var("USERPROFILE").ok().filter(|h| !h.is_empty()).unwrap_or_else(|| {
        format!("{}{}", std::env::var("HOMEDRIVE").unwrap_or_default(), std::env::var("HOMEPATH").unwrap_or_default())
    });
    let h = h.replace('/', "\\");
    match h.trim_end_matches('\\') {
        t if t.len() > 2 => t.to_string(),
        _ => h,
    }
}

/// Replace `pat` in `text` with `to`, in any case.
fn replace_any_case(text: &str, pat: &str, to: &str) -> String {
    let low = |c: char| c.to_lowercase().next().unwrap_or(c);
    let p: Vec<char> = pat.chars().map(low).collect();
    let t: Vec<char> = text.chars().collect();
    let (mut out, mut i) = (String::new(), 0);
    while i < t.len() {
        if i + p.len() <= t.len() && t[i..i + p.len()].iter().zip(&p).all(|(a, b)| low(*a) == *b) {
            out.push_str(to);
            i += p.len();
        } else {
            out.push(t[i]);
            i += 1;
        }
    }
    out
}

/// The home folder holds the Windows user name: the AI app (often an online service) sees it as ~.
pub fn private(text: &str) -> String {
    let home = home();
    if home.is_empty() {
        return text.to_string();
    }
    let mut out = text.to_string();
    for h in [home.clone(), home.replace('\\', "\\\\"), home.replace('\\', "/")] {
        out = replace_any_case(&out, &h, "~");
    }
    out
}
