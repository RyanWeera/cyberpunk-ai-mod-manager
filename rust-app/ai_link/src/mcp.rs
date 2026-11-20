//! MCP server on stdin/stdout, so AI apps (Claude Desktop, Claude Code, Cursor and others) can use the mod manager.
//! The AI app starts `CP2077 AI Mod Manager.exe --mcp`; every tool call is passed to the manager window (started if
//! needed), so the window stays the only process that changes the mods. Messages are JSON-RPC, one per line.
use crate::tools::TOOLS;
use serde_json::{Value, json};
use std::io::{BufRead, Write};

/// call(tool name, arguments) -> text, or the error the AI app shows.
pub type Call<'a> = &'a dyn Fn(&str, &Value) -> Result<String, String>;

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

/// The answer to one JSON-RPC message (an object), or None for a notification.
pub fn reply(msg: &Value, call: Call, version: &str) -> Option<Value> {
    let id = msg.get("id")?.clone();
    let method = msg.get("method").cloned().unwrap_or(Value::Null);
    let empty = json!({});
    let params = msg.get("params").filter(|p| p.is_object()).unwrap_or(&empty);
    let result = match method.as_str() {
        // the tools work the same in every protocol version, so take the app's
        Some("initialize") => json!({"protocolVersion": params.get("protocolVersion").cloned().unwrap_or(json!("2025-06-18")),
                                     "capabilities": {"tools": {}},
                                     "serverInfo": {"name": "cp2077-ai-mod-manager", "version": version}}),
        Some("tools/list") => json!({"tools": TOOLS.clone()}),
        Some("tools/call") => {
            // a failed tool is a result the AI app shows, not a dead server
            let args = params.get("arguments").filter(|a| truthy(a)).cloned().unwrap_or(json!({}));
            let (text, failed) = match params.get("name").and_then(Value::as_str).ok_or_else(|| "'name'".to_string()).and_then(|n| call(n, &args)) {
                Ok(t) => (t, false),
                Err(e) if e.is_empty() => ("Error".into(), true),
                Err(e) => (e, true),
            };
            json!({"content": [{"type": "text", "text": text}], "isError": failed})
        }
        Some("ping") => json!({}),
        _ => {
            let name = match &method {
                Value::String(s) => s.clone(),
                Value::Null => "None".into(),
                v => v.to_string(),
            };
            return Some(json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": format!("Unknown method: {name}")}}));
        }
    };
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

/// Answer the JSON-RPC messages on `inp`, one per line, until the AI app closes it.
pub fn serve(call: Call, version: &str, mut inp: impl BufRead, mut out: impl Write) {
    let mut line = vec![];
    loop {
        line.clear();
        match inp.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let text = line.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&line);
        if text.trim_ascii().is_empty() {
            continue;
        }
        let answer = match serde_json::from_slice::<Value>(text) {
            Ok(msg) if msg.is_object() => reply(&msg, call, version),
            Ok(_) => None,
            Err(_) => Some(json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": "Not JSON"}})),
        };
        if let Some(a) = answer
            && (writeln!(out, "{a}").is_err() || out.flush().is_err())
        {
            return;
        }
    }
}
