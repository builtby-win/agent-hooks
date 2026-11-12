//! `history backfill --harness muse` end to end through the real binary.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;

const SESSION: &str = "sess-muse";

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
    let mut args = vec!["backfill", "--harness", "muse"];
    args.extend_from_slice(extra);
    let (ok, out) = history_cli(home, dir, &args);
    assert!(ok, "backfill failed: {out}");
    out["harnesses"][0].clone()
}

fn messages(home: &Path, dir: &Path) -> Vec<Value> {
    let (ok, page) = history_cli(home, dir, &["messages", "--session", SESSION]);
    assert!(ok, "messages failed: {page}");
    page["items"].as_array().unwrap().clone()
}

fn record(seq: u64, payload_type: &str, payload: Value) -> Value {
    json!({
        "schema_version": 1,
        "id": format!("rec-{seq}"),
        "stream": {"kind": "session", "id": SESSION},
        "sequence": seq,
        "recorded_at": 1_793_745_205_000_000u64 + seq * 1000,
        "record_type": "event",
        "durability": "durable",
        "payload_type": payload_type,
        "payload": payload,
    })
}

fn run_event(seq: u64, event: Value) -> Value {
    record(seq, "runtime.session", json!({"kind": "run", "run_id": "run-1", "event": event}))
}

/// Muse wraps some records in a frame whose children are JSON strings.
fn frame(children: &[Value]) -> Value {
    json!({
        "retained_frame": "session_permission_transaction",
        "children": children
            .iter()
            .enumerate()
            .map(|(i, c)| json!({"child_index": i, "record_json": c.to_string()}))
            .collect::<Vec<_>>(),
    })
}

#[test]
fn backfill_imports_muse_sessions_incrementally_and_idempotently() {
    let home = tempfile::TempDir::new().unwrap();
    let dir = home.path().join("history");
    assert!(history_cli(home.path(), &dir, &["init"]).0);
    let session_dir = home.path().join(".local/share/muse/sessions/2026/01/01/sess-muse");
    std::fs::create_dir_all(&session_dir).unwrap();
    // Derived snapshots live in dot-directories and must not be read as logs.
    let view = home.path().join(".local/share/muse/sessions/.msp-view-g6/x");
    std::fs::create_dir_all(&view).unwrap();
    std::fs::write(view.join("session.jsonl"), "{\"payload_type\":\"x\"}\n").unwrap();
    let log = session_dir.join("session.jsonl");

    let mut lines: Vec<String> = vec![
        frame(&[
            record(1, "runtime.session.metadata", json!({"kind": "metadata", "record": {"workspace_root": "/work/proj"}})),
            record(2, "runtime.session.permission_format_declared", json!({"format": "profile_v1"})),
        ]),
        record(
            3,
            "runtime.user_intent.accepted",
            json!({"intent_id": "intent-1", "surface": "main", "semantic_kind": {"kind": "chat"},
                   "model_messages": [{"content": [{"kind": "text", "text": "list the files"}]}]}),
        ),
        // Not chat: an internal intent must not become a user message.
        record(
            4,
            "runtime.user_intent.accepted",
            json!({"intent_id": "intent-x", "surface": "main", "semantic_kind": {"kind": "reminder"},
                   "model_messages": [{"content": [{"kind": "text", "text": "internal"}]}]}),
        ),
        json!({"retained_marker": "omitted_live_only", "stream": {"kind": "session", "id": SESSION}}),
        run_event(5, json!({"kind": "reasoning_committed", "text": "hmm"})),
        // One frame carrying text plus two tool calls: all three must be stored.
        frame(&[
            run_event(6, json!({"kind": "assistant_message_committed", "message_id": "msg-1", "text": "looking"})),
            run_event(
                7,
                json!({"kind": "assistant_tool_calls_committed", "message_id": "msg-2", "tool_calls": [
                    {"id": "fc_a", "call_id": "call_a", "name": "bash", "args": "{\"cmd\": \"ls\"}"},
                    {"id": "fc_b", "call_id": "call_b", "name": "read_file", "args": "{\"path\":\"a\"}"}]}),
            ),
        ]),
        run_event(
            8,
            json!({"kind": "tool_result_batch_committed", "batch_id": "b1", "results": [
                {"tool_call_index": 0, "tool_call_id": "call_a", "text": "file-a"},
                {"tool_call_index": 1, "tool_call_id": "call_b", "text": "contents"}]}),
        ),
    ]
    .iter()
    .map(Value::to_string)
    .collect();
    lines.push("not json".to_string());
    let write = |lines: &[String]| std::fs::write(&log, lines.join("\n") + "\n").unwrap();
    write(&lines);

    let first = backfill(home.path(), &dir, &[]);
    assert_eq!(first["status"], "imported");
    assert_eq!(
        (first["files"].as_u64(), first["new"].as_u64(), first["skipped"].as_u64()),
        (Some(1), Some(6), Some(1)),
        "{first}"
    );

    let items = messages(home.path(), &dir);
    assert_eq!(items.len(), 4, "{items:?}");
    let text = |kind: &str| -> Vec<&str> {
        items
            .iter()
            .filter(|i| i["kind"] == kind)
            .map(|i| i["text"].as_str().unwrap())
            .collect()
    };
    assert_eq!(text("user"), ["list the files"]);
    assert_eq!(text("assistant"), ["looking"]);
    let tool = |id: &str| items.iter().find(|i| i["tool_call_id"] == id).unwrap();
    assert_eq!(tool("call_a")["tool_name"], "bash");
    assert_eq!(tool("call_a")["tool_status"], "completed");
    assert_eq!(tool("call_a")["tool_input"], r#"{"cmd":"ls"}"#);
    assert_eq!(tool("call_a")["tool_output"], "file-a");
    assert_eq!(tool("call_b")["tool_output"], "contents");
    let (_, sessions) = history_cli(home.path(), &dir, &["sessions"]);
    assert_eq!(sessions["items"][0]["workspace"], "/work/proj");

    let again = backfill(home.path(), &dir, &[]);
    assert_eq!((again["unchanged"].as_u64(), again["new"].as_u64()), (Some(1), Some(0)));

    let rescan = backfill(home.path(), &dir, &["--rescan"]);
    assert_eq!((rescan["new"].as_u64(), rescan["duplicate"].as_u64()), (Some(0), Some(6)));
    assert_eq!(messages(home.path(), &dir).len(), 4);

    // Append-only growth imports only the new record.
    lines.push(
        run_event(9, json!({"kind": "assistant_message_committed", "message_id": "msg-3", "text": "done"}))
            .to_string(),
    );
    write(&lines);
    let grown = backfill(home.path(), &dir, &[]);
    assert_eq!((grown["new"].as_u64(), grown["duplicate"].as_u64()), (Some(1), Some(0)), "{grown}");
    assert_eq!(text_of(&messages(home.path(), &dir), "assistant"), ["looking", "done"]);
}

fn text_of<'a>(items: &'a [Value], kind: &str) -> Vec<&'a str> {
    items
        .iter()
        .filter(|i| i["kind"] == kind)
        .map(|i| i["text"].as_str().unwrap())
        .collect()
}
