//! `history backfill` for the kiro and amp transcript importers, through the
//! real `agent-hooks` binary. Fixtures mirror the real on-disk formats.

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
    out["harnesses"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["harness"] == harness)
        .unwrap_or_else(|| panic!("no report for {harness}: {out}"))
        .clone()
}

fn counts(report: &Value) -> [u64; 4] {
    ["files", "new", "duplicate", "skipped"].map(|k| report[k].as_u64().unwrap())
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

fn jsonl(rows: &[Value]) -> String {
    rows.iter().map(|r| r.to_string() + "\n").collect()
}

#[test]
fn backfill_imports_kiro_sessions_idempotently() {
    let (home, dir) = setup();
    let sessions = home.path().join(".kiro/sessions/cli");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::write(
        sessions.join("k-1.json"),
        json!({"session_id": "k-1", "cwd": "/work/app", "title": "fix it",
               "created_at": "2026-01-01T00:00:00Z", "updated_at": "2026-01-01T00:05:00Z"})
        .to_string(),
    )
    .unwrap();
    let rows = [
        json!({"version": "v1", "kind": "Prompt", "data": {"message_id": "m1",
            "content": [{"kind": "text", "data": "fix the bug"}],
            "meta": {"timestamp": 1_767_225_660}}}),
        json!({"version": "v1", "kind": "AssistantMessage", "data": {"message_id": "m2",
            "content": [
                {"kind": "text", "data": "looking"},
                {"kind": "toolUse", "data": {"toolUseId": "tu_a", "name": "read", "input": {"path": "x"}}},
                {"kind": "toolUse", "data": {"toolUseId": "tu_b", "name": "shell", "input": {"cmd": "ls"}}},
            ]}}),
        json!({"version": "v1", "kind": "ToolResults", "data": {"message_id": "m3",
            "content": [
                {"kind": "toolResult", "data": {"toolUseId": "tu_a", "status": "success",
                    "content": [{"kind": "json", "data": {"lines": 3}}]}},
                {"kind": "toolResult", "data": {"toolUseId": "tu_b", "status": "error",
                    "content": [{"kind": "text", "data": "boom"}]}},
            ],
            "results": {}}}),
        json!({"version": "v1", "kind": "Clear"}),
    ];
    let transcript = sessions.join("k-1.jsonl");
    std::fs::write(&transcript, jsonl(&rows) + "not json\n").unwrap();

    let first = backfill(home.path(), &dir, "kiro", &[]);
    assert_eq!(first["status"], "imported");
    assert_eq!(counts(&first), [1, 6, 0, 1], "{first}");

    let items = messages(home.path(), &dir, "k-1");
    assert_eq!(items.len(), 4, "{items:?}");
    let by_kind = |kind: &str| -> Vec<&str> {
        items.iter().filter(|i| i["kind"] == kind).map(|i| i["text"].as_str().unwrap()).collect()
    };
    assert_eq!(by_kind("user"), ["fix the bug"]);
    assert_eq!(by_kind("assistant"), ["looking"]);
    let tool = |id: &str| items.iter().find(|i| i["tool_call_id"] == id).unwrap();
    assert_eq!(tool("tu_a")["tool_status"], "completed");
    assert_eq!(tool("tu_a")["tool_output"], r#"{"lines":3}"#);
    assert_eq!(tool("tu_a")["tool_name"], "read");
    assert_eq!(tool("tu_a")["tool_input"], r#"{"path":"x"}"#);
    assert_eq!(tool("tu_b")["tool_status"], "failed");
    assert_eq!(tool("tu_b")["tool_output"], "boom");
    let sessions_page = history_cli(home.path(), &dir, &["sessions"]);
    assert_eq!(sessions_page["items"][0]["workspace"], "/work/app");

    let again = backfill(home.path(), &dir, "kiro", &[]);
    assert_eq!((again["unchanged"].as_u64(), again["new"].as_u64()), (Some(1), Some(0)));
    let rescan = backfill(home.path(), &dir, "kiro", &["--rescan"]);
    assert_eq!((rescan["new"].as_u64(), rescan["duplicate"].as_u64()), (Some(0), Some(6)));
    assert_eq!(messages(home.path(), &dir, "k-1").len(), 4);

    let mut grown = std::fs::read_to_string(&transcript).unwrap();
    grown += &jsonl(&[json!({"version": "v1", "kind": "Prompt", "data": {"message_id": "m4",
        "content": [{"kind": "text", "data": "thanks"}], "meta": {"timestamp": 1_767_225_700}}})]);
    std::fs::write(&transcript, grown).unwrap();
    let grown = backfill(home.path(), &dir, "kiro", &[]);
    assert_eq!((grown["new"].as_u64(), grown["duplicate"].as_u64()), (Some(1), Some(0)), "{grown}");
    assert_eq!(messages(home.path(), &dir, "k-1").len(), 5);
}

#[test]
fn backfill_imports_amp_threads_idempotently() {
    let (home, dir) = setup();
    let threads = home.path().join(".local/share/amp/threads");
    std::fs::create_dir_all(&threads).unwrap();
    let thread = |extra: Vec<Value>| {
        let mut messages = vec![
            json!({"role": "user", "messageId": 0, "meta": {"sentAt": 1_767_225_600_000_u64},
                "content": [{"type": "text", "text": "fix the bug"}]}),
            json!({"role": "assistant", "messageId": 1, "content": [
                {"type": "thinking", "thinking": "hmm", "signature": "x"},
                {"type": "text", "text": "looking"},
                {"type": "tool_use", "complete": true, "id": "toolu_a", "name": "Bash", "input": {"cmd": "ls"}},
                {"type": "tool_use", "complete": true, "id": "toolu_b", "name": "Read", "input": {"path": "x"}},
            ]}),
            json!({"role": "user", "messageId": 2, "content": [
                {"type": "tool_result", "toolUseID": "toolu_a",
                    "run": {"status": "done", "result": {"output": "file-a", "exitCode": 0}}},
                {"type": "tool_result", "toolUseID": "toolu_b",
                    "run": {"status": "error", "error": {"message": "boom"}}},
            ]}),
        ];
        messages.extend(extra);
        std::fs::write(
            threads.join("T-1.json"),
            json!({"v": 1, "id": "T-1", "created": 1_767_225_600_000_u64, "title": "t",
                   "env": {"initial": {"trees": [{"uri": "file:///work/app"}]}},
                   "messages": messages})
            .to_string(),
        )
        .unwrap();
    };
    thread(vec![]);

    let first = backfill(home.path(), &dir, "amp", &[]);
    assert_eq!(first["status"], "imported");
    assert_eq!(counts(&first), [1, 6, 0, 0], "{first}");

    let items = messages(home.path(), &dir, "T-1");
    assert_eq!(items.len(), 4, "{items:?}");
    let tool = |id: &str| items.iter().find(|i| i["tool_call_id"] == id).unwrap();
    assert_eq!(tool("toolu_a")["tool_status"], "completed");
    assert_eq!(tool("toolu_a")["tool_output"], r#"{"exitCode":0,"output":"file-a"}"#);
    assert_eq!(tool("toolu_a")["tool_name"], "Bash");
    assert_eq!(tool("toolu_b")["tool_status"], "failed");
    assert_eq!(tool("toolu_b")["tool_output"], "boom");
    let sessions_page = history_cli(home.path(), &dir, &["sessions"]);
    assert_eq!(sessions_page["items"][0]["workspace"], "/work/app");

    // The thread is rewritten in place, so a changed file is re-read in full
    // and only the new message is new.
    thread(vec![json!({"role": "user", "messageId": 3,
        "meta": {"sentAt": 1_767_225_700_000_u64},
        "content": [{"type": "text", "text": "thanks"}]})]);
    let grown = backfill(home.path(), &dir, "amp", &[]);
    assert_eq!((grown["new"].as_u64(), grown["duplicate"].as_u64()), (Some(1), Some(6)), "{grown}");
    assert_eq!(messages(home.path(), &dir, "T-1").len(), 5);

    let rescan = backfill(home.path(), &dir, "amp", &["--rescan"]);
    assert_eq!((rescan["new"].as_u64(), rescan["duplicate"].as_u64()), (Some(0), Some(7)));
}
