//! `--mcp` on the built exe: it answers MCP on stdio and opens no window. A throwaway data folder keeps
//! the real one out of reach. Only `initialize` and `tools/list`: a tool call would reach a running window.

use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use testkit::TempDir;

#[test]
fn mcp_answers_on_stdio() {
    let data = TempDir::new();
    let mut child = Command::new(env!("CARGO_BIN_EXE_cp2077-ai-mod-manager"))
        .arg("--mcp")
        .env("CP2077MM_DATA", data.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    {
        let mut inp = child.stdin.take().unwrap();
        writeln!(inp, r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"2025-06-18"}}}}"#).unwrap();
        writeln!(inp, r#"{{"jsonrpc":"2.0","method":"notifications/initialized"}}"#).unwrap();
        writeln!(inp, r#"{{"jsonrpc":"2.0","id":2,"method":"tools/list"}}"#).unwrap();
    } // stdin closes: the server ends
    let lines: Vec<Value> =
        BufReader::new(child.stdout.take().unwrap()).lines().map(|l| serde_json::from_str(&l.unwrap()).unwrap()).collect();
    assert!(child.wait().unwrap().success());
    assert_eq!(lines.len(), 2); // the notification gets no answer
    assert_eq!(lines[0]["id"], 1);
    assert_eq!(lines[0]["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(lines[1]["id"], 2);
    assert!(lines[1]["result"]["tools"].as_array().is_some_and(|t| t.iter().any(|t| t["name"] == "status")));
}
