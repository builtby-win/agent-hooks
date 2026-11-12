//! `history backfill --harness codex` end to end through the real binary.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;

fn history_cli(home: &Path, dir: &Path, args: &[&str]) -> (bool, Value) {
    let out = Command::new(PathBuf::from(env!("CARGO_BIN_EXE_agent-hooks")))
        .arg("history")
        .args(args)
        .arg("--history-dir")
        .arg(dir)
        .env("HOME", home)
        .output()
        .expect("run history cli");
    let json = serde_json::from_slice(&out.stdout).expect("history stdout is JSON");
    (out.status.success(), json)
}

fn backfill(home: &Path, dir: &Path, extra: &[&str]) -> Value {
    let mut args = vec!["backfill", "--harness", "codex"];
    args.extend_from_slice(extra);
    let (ok, out) = history_cli(home, dir, &args);
    assert!(ok, "backfill failed: {out}");
    out["harnesses"][0].clone()
}

fn messages(home: &Path, dir: &Path, session: &str) -> Vec<Value> {
    let (ok, page) = history_cli(home, dir, &["messages", "--session", session]);
    assert!(ok, "messages failed: {page}");
    page["items"].as_array().unwrap().clone()
}

fn row(ts: &str, kind: &str, payload: Value) -> String {
    json!({"timestamp": ts, "type": kind, "payload": payload}).to_string()
}

fn message(ts: &str, id: &str, role: &str, text: &str) -> String {
    let part = if role == "user" { "input_text" } else { "output_text" };
    row(
        ts,
        "response_item",
        json!({"type": "message", "id": id, "role": role,
               "content": [{"type": part, "text": text}]}),
    )
}

#[test]
fn backfill_imports_codex_rollouts_incrementally_and_idempotently() {
    let home = tempfile::TempDir::new().unwrap();
    let dir = home.path().join("history");
    assert!(history_cli(home.path(), &dir, &["init"]).0);
    let day = home.path().join(".codex/sessions/2026/01/01");
    std::fs::create_dir_all(&day).unwrap();
    let rollout = day.join("rollout-2026-01-01T00-00-00-sess-1.jsonl");

    let mut lines = vec![
        row(
            "2026-01-01T00:00:00.000Z",
            "session_meta",
            json!({"id": "sess-1", "cwd": "/work/proj"}),
        ),
        message("2026-01-01T00:00:01.000Z", "m0", "user", "# AGENTS.md instructions\nrules"),
        message("2026-01-01T00:00:01.100Z", "m1", "user", "fix the bug"),
        row("2026-01-01T00:00:02.000Z", "turn_context", json!({"turn_id": "t1", "cwd": "/work/proj"})),
        row(
            "2026-01-01T00:00:02.500Z",
            "response_item",
            json!({"type": "reasoning", "summary": [], "encrypted_content": "x"}),
        ),
        message("2026-01-01T00:00:03.000Z", "m2", "assistant", "looking"),
        row(
            "2026-01-01T00:00:04.000Z",
            "response_item",
            json!({"type": "function_call", "name": "shell", "call_id": "call_a",
                   "arguments": "{\"command\":[\"ls\"]}"}),
        ),
        row(
            "2026-01-01T00:00:04.100Z",
            "response_item",
            json!({"type": "custom_tool_call", "name": "exec", "call_id": "call_b",
                   "input": "text(1)"}),
        ),
        row(
            "2026-01-01T00:00:05.000Z",
            "response_item",
            json!({"type": "function_call_output", "call_id": "call_a", "output": "file-a"}),
        ),
        row(
            "2026-01-01T00:00:05.100Z",
            "response_item",
            json!({"type": "custom_tool_call_output", "call_id": "call_b",
                   "output": [{"type": "input_text", "text": "ok"}, {"type": "input_text", "text": "done"}]}),
        ),
        row("2026-01-01T00:00:05.200Z", "event_msg", json!({"type": "token_count"})),
        "not json".to_string(),
    ];
    let write = |lines: &[String]| std::fs::write(&rollout, lines.join("\n") + "\n").unwrap();
    write(&lines);

    let first = backfill(home.path(), &dir, &[]);
    assert_eq!(first["status"], "imported");
    assert_eq!(
        (first["files"].as_u64(), first["new"].as_u64(), first["skipped"].as_u64()),
        (Some(1), Some(6), Some(1)),
        "{first}"
    );

    let items = messages(home.path(), &dir, "sess-1");
    assert_eq!(items.len(), 4, "{items:?}");
    let text = |kind: &str| -> Vec<&str> {
        items
            .iter()
            .filter(|i| i["kind"] == kind)
            .map(|i| i["text"].as_str().unwrap())
            .collect()
    };
    assert_eq!(text("user"), ["fix the bug"]);
    assert_eq!(text("assistant"), ["looking"]);
    let tool = |id: &str| items.iter().find(|i| i["tool_call_id"] == id).unwrap();
    assert_eq!(tool("call_a")["tool_name"], "shell");
    assert_eq!(tool("call_a")["tool_status"], "completed");
    assert_eq!(tool("call_a")["tool_output"], "file-a");
    assert_eq!(tool("call_a")["tool_input"], r#"{"command":["ls"]}"#);
    assert_eq!(tool("call_b")["tool_output"], "ok\ndone");
    let (_, sessions) = history_cli(home.path(), &dir, &["sessions"]);
    assert_eq!(sessions["items"].as_array().unwrap().len(), 1);

    let again = backfill(home.path(), &dir, &[]);
    assert_eq!((again["unchanged"].as_u64(), again["new"].as_u64()), (Some(1), Some(0)));

    let rescan = backfill(home.path(), &dir, &["--rescan"]);
    assert_eq!((rescan["new"].as_u64(), rescan["duplicate"].as_u64()), (Some(0), Some(6)));
    assert_eq!(messages(home.path(), &dir, "sess-1").len(), 4);

    // Append-only growth resumes at the offset; the session id still comes
    // from the first row.
    lines.push(message("2026-01-01T00:00:06.000Z", "m3", "assistant", "done"));
    write(&lines);
    let grown = backfill(home.path(), &dir, &[]);
    assert_eq!(
        (grown["new"].as_u64(), grown["duplicate"].as_u64()),
        (Some(1), Some(0)),
        "{grown}"
    );
    assert_eq!(messages(home.path(), &dir, "sess-1").len(), 5);
}
