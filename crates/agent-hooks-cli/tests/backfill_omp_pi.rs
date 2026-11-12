//! `history backfill` for the omp and pi session transcripts (shared format).

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

fn backfill(home: &Path, dir: &Path, harness: &str, extra: &[&str]) -> Value {
    let mut args = vec!["backfill", "--harness", harness];
    args.extend_from_slice(extra);
    let (ok, out) = history_cli(home, dir, &args);
    assert!(ok, "backfill failed: {out}");
    out["harnesses"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["harness"] == harness)
        .unwrap_or_else(|| panic!("no report for {harness}: {out}"))
        .clone()
}

fn messages(home: &Path, dir: &Path, session: &str) -> Vec<Value> {
    let (ok, page) = history_cli(home, dir, &["messages", "--session", session]);
    assert!(ok, "messages failed: {page}");
    page["items"].as_array().unwrap().clone()
}

fn message(id: &str, ts: &str, message: Value) -> String {
    json!({"type": "message", "id": id, "parentId": null, "timestamp": ts, "message": message})
        .to_string()
}

fn counts(report: &Value) -> [Option<u64>; 4] {
    ["files", "new", "duplicate", "skipped"].map(|k| report[k].as_u64())
}

fn check(harness: &str, root: &str) {
    let home = tempfile::TempDir::new().unwrap();
    let dir = home.path().join("history");
    assert!(history_cli(home.path(), &dir, &["init"]).0);
    let project = home.path().join(root).join("--work--");
    std::fs::create_dir_all(&project).unwrap();
    let transcript = project.join("2026-01-01T00-00-00-000Z_sess-1.jsonl");
    let sid = format!("{harness}-sess-1");
    // One assistant row carries thinking, two text parts and two tool calls;
    // bookkeeping rows are ignored and the garbage row is counted as skipped.
    let mut lines = vec![
        json!({"type": "title", "v": 1, "title": "t", "pad": "   "}).to_string(),
        json!({"type": "session", "version": 3, "id": sid, "timestamp": "2026-01-01T00:00:00.000Z",
               "cwd": "/work"})
        .to_string(),
        json!({"type": "model_change", "id": "m0", "timestamp": "2026-01-01T00:00:00.100Z"})
            .to_string(),
        message(
            "u1",
            "2026-01-01T00:00:01.000Z",
            json!({"role": "user", "content": [{"type": "text", "text": "fix the bug"}]}),
        ),
        message(
            "a1",
            "2026-01-01T00:00:02.000Z",
            json!({"role": "assistant", "content": [
                {"type": "thinking", "thinking": "hmm"},
                {"type": "text", "text": "looking"},
                {"type": "text", "text": "reading files"},
                {"type": "toolCall", "id": "call_a", "name": "bash", "arguments": {"command": "ls"}},
                {"type": "toolCall", "id": "call_b", "name": "read", "arguments": {"path": "x"}},
            ]}),
        ),
        message(
            "r1",
            "2026-01-01T00:00:03.000Z",
            json!({"role": "toolResult", "toolCallId": "call_a", "toolName": "bash",
                   "content": [{"type": "text", "text": "file-a"}], "isError": false}),
        ),
        message(
            "r2",
            "2026-01-01T00:00:04.000Z",
            json!({"role": "toolResult", "toolCallId": "call_b", "toolName": "read",
                   "content": [{"type": "text", "text": "boom"}], "isError": true}),
        ),
        message(
            "d1",
            "2026-01-01T00:00:05.000Z",
            json!({"role": "developer", "content": "ignored"}),
        ),
        "not json".to_string(),
    ];
    let write = |lines: &[String]| std::fs::write(&transcript, lines.join("\n") + "\n").unwrap();
    write(&lines);

    let first = backfill(home.path(), &dir, harness, &[]);
    assert_eq!(first["status"], "imported", "{first}");
    assert_eq!(counts(&first), [Some(1), Some(7), Some(0), Some(1)], "{first}");

    let items = messages(home.path(), &dir, &sid);
    let text = |kind: &str| -> Vec<&str> {
        items
            .iter()
            .filter(|i| i["kind"] == kind)
            .map(|i| i["text"].as_str().unwrap())
            .collect()
    };
    assert_eq!(items.len(), 5, "{items:?}");
    assert_eq!(text("user"), ["fix the bug"]);
    assert_eq!(text("assistant"), ["looking", "reading files"]);
    let tool = |id: &str| items.iter().find(|i| i["tool_call_id"] == id).unwrap();
    assert_eq!(tool("call_a")["tool_status"], "completed");
    assert_eq!(tool("call_a")["tool_output"], "file-a");
    assert_eq!(tool("call_a")["tool_name"], "bash");
    assert_eq!(tool("call_b")["tool_status"], "failed");
    assert_eq!(tool("call_b")["tool_input"], r#"{"path":"x"}"#);

    // Unchanged file: not re-read.
    let again = backfill(home.path(), &dir, harness, &[]);
    assert_eq!((again["unchanged"].as_u64(), again["new"].as_u64()), (Some(1), Some(0)));

    // Forced re-read: every observation is a duplicate.
    let rescan = backfill(home.path(), &dir, harness, &["--rescan"]);
    assert_eq!((rescan["new"].as_u64(), rescan["duplicate"].as_u64()), (Some(0), Some(7)));
    assert_eq!(messages(home.path(), &dir, &sid).len(), 5);

    // Append-only growth imports only the new row.
    lines.push(message(
        "u2",
        "2026-01-01T00:00:06.000Z",
        json!({"role": "user", "content": "thanks"}),
    ));
    write(&lines);
    let grown = backfill(home.path(), &dir, harness, &[]);
    assert_eq!((grown["new"].as_u64(), grown["duplicate"].as_u64()), (Some(1), Some(0)), "{grown}");
    assert_eq!(messages(home.path(), &dir, &sid).len(), 6);

    // A nested subagent transcript is its own session.
    let nested = project.join("2026-01-01T00-00-00-000Z_sess-1");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(
        nested.join("Sub.jsonl"),
        [
            json!({"type": "session", "version": 3, "id": "sub-1", "cwd": "/work"}).to_string(),
            message(
                "s1",
                "2026-01-01T00:01:00.000Z",
                json!({"role": "user", "content": "sub task"}),
            ),
        ]
        .join("\n")
            + "\n",
    )
    .unwrap();
    let sub = backfill(home.path(), &dir, harness, &[]);
    assert_eq!((sub["new"].as_u64(), sub["unchanged"].as_u64()), (Some(1), Some(1)), "{sub}");
    assert_eq!(messages(home.path(), &dir, "sub-1").len(), 1);
}

#[test]
fn backfill_imports_omp_sessions() {
    check("omp", ".omp/agent/sessions");
}

#[test]
fn backfill_imports_pi_sessions() {
    check("pi", ".pi/agent/sessions");
}
