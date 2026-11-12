//! `history backfill` for the Cursor and Copilot transcript importers,
//! through the real `agent-hooks` binary with a fixture `$HOME`.
#![cfg(target_os = "macos")]

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;

fn history_cli(home: &Path, dir: &Path, args: &[&str]) -> Value {
    let out = Command::new(PathBuf::from(env!("CARGO_BIN_EXE_agent-hooks")))
        .arg("history")
        .args(args)
        .arg("--history-dir")
        .arg(dir)
        .env("HOME", home)
        .output()
        .expect("run history cli");
    assert!(out.status.success(), "{args:?} failed: {out:?}");
    serde_json::from_slice(&out.stdout).expect("history stdout is JSON")
}

fn backfill(home: &Path, dir: &Path, harness: &str, extra: &[&str]) -> Value {
    let mut args = vec!["backfill", "--harness", harness];
    args.extend_from_slice(extra);
    let out = history_cli(home, dir, &args);
    out["harnesses"][0].clone()
}

fn counts(report: &Value) -> [u64; 4] {
    ["new", "duplicate", "unchanged", "skipped"].map(|k| report[k].as_u64().unwrap())
}

fn messages(home: &Path, dir: &Path, session: &str) -> Vec<Value> {
    let page = history_cli(home, dir, &["messages", "--session", session]);
    page["items"].as_array().unwrap().clone()
}

fn setup() -> (tempfile::TempDir, PathBuf) {
    let home = tempfile::TempDir::new().unwrap();
    let dir = home.path().join("history");
    history_cli(home.path(), &dir, &["init"]);
    (home, dir)
}

#[test]
fn backfill_imports_cursor_transcripts_idempotently() {
    let (home, dir) = setup();
    let session = home
        .path()
        .join(".cursor/projects/Users-me-work/agent-transcripts/cur-1");
    std::fs::create_dir_all(&session).unwrap();
    let transcript = session.join("cur-1.jsonl");
    let mut lines = vec![
        json!({"role": "user", "message": {"content": [{"type": "text", "text":
            "<timestamp>Monday, Sep 7, 2026, 8:38 PM (UTC-7)</timestamp>\n<user_query>\nfix the bug\n</user_query>"}]}})
        .to_string(),
        json!({"role": "assistant", "message": {"content": [
            {"type": "text", "text": "looking"},
            {"type": "tool_use", "name": "Shell", "input": {"command": "ls"}},
            {"type": "tool_use", "name": "Read", "input": {"path": "x"}},
        ]}})
        .to_string(),
        json!({"type": "status", "status": "done"}).to_string(),
        "not json".to_string(),
    ];
    let write = |lines: &[String]| std::fs::write(&transcript, lines.join("\n") + "\n").unwrap();
    write(&lines);

    let first = backfill(home.path(), &dir, "cursor", &[]);
    assert_eq!(first["status"], "imported", "{first}");
    assert_eq!(counts(&first), [4, 0, 0, 1], "{first}");

    let items = messages(home.path(), &dir, "cur-1");
    assert_eq!(items.len(), 4, "{items:?}");
    let user = items.iter().find(|i| i["kind"] == "user").unwrap();
    assert_eq!(user["text"], "fix the bug");
    // 2026-09-08T03:38:00Z
    assert_eq!(user["first_seen_ms"], 1_788_838_680_000_u64);
    let tools: Vec<_> = items.iter().filter(|i| i["kind"] == "tool").collect();
    assert_eq!(tools[0]["tool_name"], "Shell");
    assert_eq!(tools[0]["tool_input"], r#"{"command":"ls"}"#);
    assert_eq!(tools[1]["tool_name"], "Read");

    let again = backfill(home.path(), &dir, "cursor", &[]);
    assert_eq!((again["unchanged"].as_u64(), again["new"].as_u64()), (Some(1), Some(0)));
    let rescan = backfill(home.path(), &dir, "cursor", &["--rescan"]);
    assert_eq!((rescan["new"].as_u64(), rescan["duplicate"].as_u64()), (Some(0), Some(4)));
    assert_eq!(messages(home.path(), &dir, "cur-1").len(), 4);

    lines.push(
        json!({"role": "assistant", "message": {"content": [{"type": "text", "text": "done"}]}})
            .to_string(),
    );
    write(&lines);
    let grown = backfill(home.path(), &dir, "cursor", &[]);
    assert_eq!((grown["new"].as_u64(), grown["duplicate"].as_u64()), (Some(1), Some(0)));
    assert_eq!(messages(home.path(), &dir, "cur-1").len(), 5);
}

fn event(kind: &str, id: &str, ts: &str, data: Value) -> String {
    json!({"type": kind, "id": id, "timestamp": ts, "parentId": null, "data": data}).to_string()
}

#[test]
fn backfill_imports_copilot_events_idempotently() {
    let (home, dir) = setup();
    let session = home.path().join(".copilot/session-state/cop-1");
    std::fs::create_dir_all(&session).unwrap();
    std::fs::write(session.join("workspace.yaml"), "id: cop-1\ncwd: /work/proj\n").unwrap();
    let events = session.join("events.jsonl");
    let mut lines = vec![
        event("session.start", "e0", "2026-01-01T00:00:00.000Z", json!({"sessionId": "cop-1"})),
        event("user.message", "e1", "2026-01-01T00:00:01.000Z", json!({"content": "fix it"})),
        event("hook.start", "e2", "2026-01-01T00:00:01.500Z", json!({"hookType": "x"})),
        event("assistant.message", "e3", "2026-01-01T00:00:02.000Z",
            json!({"messageId": "m1", "content": "on it", "toolRequests": []})),
        event("tool.execution_start", "e4", "2026-01-01T00:00:03.000Z",
            json!({"toolCallId": "tc-a", "toolName": "bash", "arguments": {"command": "ls"}})),
        event("tool.execution_start", "e5", "2026-01-01T00:00:03.100Z",
            json!({"toolCallId": "tc-b", "toolName": "view", "arguments": {"path": "x"}})),
        event("tool.execution_complete", "e6", "2026-01-01T00:00:04.000Z",
            json!({"toolCallId": "tc-a", "success": true, "result": {"content": "file-a"}})),
        event("tool.execution_complete", "e7", "2026-01-01T00:00:05.000Z",
            json!({"toolCallId": "tc-b", "success": false, "error": {"message": "boom"}})),
        "not json".to_string(),
    ];
    let write = |lines: &[String]| std::fs::write(&events, lines.join("\n") + "\n").unwrap();
    write(&lines);

    let first = backfill(home.path(), &dir, "copilot", &[]);
    assert_eq!(first["status"], "imported", "{first}");
    assert_eq!(counts(&first), [6, 0, 0, 1], "{first}");

    let items = messages(home.path(), &dir, "cop-1");
    assert_eq!(items.len(), 4, "{items:?}");
    let text = |kind: &str| -> Vec<&str> {
        items.iter().filter(|i| i["kind"] == kind).map(|i| i["text"].as_str().unwrap()).collect()
    };
    assert_eq!(text("user"), ["fix it"]);
    assert_eq!(text("assistant"), ["on it"]);
    let tool = |id: &str| items.iter().find(|i| i["tool_call_id"] == id).unwrap();
    assert_eq!(tool("tc-a")["tool_status"], "completed");
    assert_eq!(tool("tc-a")["tool_output"], "file-a");
    assert_eq!(tool("tc-a")["tool_name"], "bash");
    assert_eq!(tool("tc-b")["tool_status"], "failed");
    assert_eq!(tool("tc-b")["tool_output"], "boom");
    assert_eq!(tool("tc-b")["tool_input"], r#"{"path":"x"}"#);
    let sessions = history_cli(home.path(), &dir, &["sessions"]);
    assert_eq!(sessions["items"][0]["workspace"], "/work/proj");

    let again = backfill(home.path(), &dir, "copilot", &[]);
    assert_eq!((again["unchanged"].as_u64(), again["new"].as_u64()), (Some(1), Some(0)));
    let rescan = backfill(home.path(), &dir, "copilot", &["--rescan"]);
    assert_eq!((rescan["new"].as_u64(), rescan["duplicate"].as_u64()), (Some(0), Some(6)));
    assert_eq!(messages(home.path(), &dir, "cop-1").len(), 4);

    lines.push(event("user.message", "e8", "2026-01-01T00:00:06.000Z", json!({"content": "thanks"})));
    write(&lines);
    let grown = backfill(home.path(), &dir, "copilot", &[]);
    assert_eq!((grown["new"].as_u64(), grown["duplicate"].as_u64()), (Some(1), Some(0)));
    assert_eq!(messages(home.path(), &dir, "cop-1").len(), 5);
}
