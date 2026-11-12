//! `history backfill` for the antigravity and gemini-cli transcript importers,
//! through the real binary with a fixture HOME.
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
    out["harnesses"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["harness"] == harness)
        .unwrap_or_else(|| panic!("no report for {harness}: {out}"))
        .clone()
}

fn counts(report: &Value) -> [u64; 5] {
    ["files", "new", "duplicate", "unchanged", "skipped"].map(|k| report[k].as_u64().unwrap())
}

fn messages(home: &Path, dir: &Path, session: &str) -> Vec<Value> {
    let page = history_cli(home, dir, &["messages", "--session", session]);
    page["items"].as_array().unwrap().clone()
}

fn jsonl(rows: &[Value]) -> String {
    rows.iter().map(|r| r.to_string() + "\n").collect()
}

#[test]
fn backfill_imports_antigravity_transcripts_idempotently() {
    let home = tempfile::TempDir::new().unwrap();
    let dir = home.path().join("history");
    history_cli(home.path(), &dir, &["init"]);
    let logs = home
        .path()
        .join(".gemini/antigravity-cli/brain/conv-1/.system_generated/logs");
    std::fs::create_dir_all(&logs).unwrap();

    let step = |index: u64, source: &str, kind: &str, status: &str, extra: Value| {
        let mut row = json!({"step_index": index, "source": source, "type": kind,
            "status": status, "created_at": format!("2026-01-01T00:00:0{index}Z")});
        row.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        row
    };
    let result =
        |text: &str| json!({"content": format!("Created At: x\nCompleted At: y\n\n{text}")});
    // transcript_full.jsonl: steps out of order; one planner step carries
    // text plus two tool calls answered by the next two steps.
    std::fs::write(
        logs.join("transcript_full.jsonl"),
        jsonl(&[
            step(0, "USER_EXPLICIT", "USER_INPUT", "DONE", json!({
                "content": "<USER_REQUEST>\nfix the bug\n</USER_REQUEST>\n<ADDITIONAL_METADATA>t</ADDITIONAL_METADATA>"})),
            step(2, "MODEL", "PLANNER_RESPONSE", "DONE", json!({
                "content": "looking",
                "thinking": "hmm",
                "tool_calls": [
                    {"name": "run_command", "args": {"CommandLine": "ls"}},
                    {"name": "view_file", "args": {"AbsolutePath": "/x"}}]})),
            step(4, "MODEL", "GENERIC", "ERROR", json!({"error": "boom", "content": "Created At: x\nCompleted At: y\nEncountered error: boom"})),
            step(3, "MODEL", "RUN_COMMAND", "DONE", result("file-a")),
            step(5, "SYSTEM", "CHECKPOINT", "DONE", json!({"content": "noise"})),
            step(6, "MODEL", "PLANNER_RESPONSE", "DONE", json!({"content": "all done"})),
        ]),
    )
    .unwrap();
    // transcript.jsonl: a step the full file lacks (args JSON-string encoded),
    // a step the full file also has (full wins) and a torn line.
    std::fs::write(
        logs.join("transcript.jsonl"),
        jsonl(&[
            step(
                1,
                "MODEL",
                "PLANNER_RESPONSE",
                "DONE",
                json!({"content": "earlier"}),
            ),
            step(
                6,
                "MODEL",
                "PLANNER_RESPONSE",
                "DONE",
                json!({"content": "truncated"}),
            ),
            step(
                7,
                "MODEL",
                "PLANNER_RESPONSE",
                "DONE",
                json!({
                "tool_calls": [{"name": "run_command", "args": {"CommandLine": "\"pwd\""}}]}),
            ),
        ]) + "{\"step_index\":8,\n",
    )
    .unwrap();

    let first = backfill(home.path(), &dir, "antigravity", &[]);
    assert_eq!(first["status"], "imported", "{first}");
    // user, earlier, looking, 2 tool calls + 2 results, all done, pwd call.
    assert_eq!(counts(&first), [1, 9, 0, 0, 1], "{first}");

    let items = messages(home.path(), &dir, "conv-1");
    assert_eq!(items.len(), 7, "{items:?}");
    let text = |kind: &str| -> Vec<&str> {
        items
            .iter()
            .filter(|i| i["kind"] == kind)
            .map(|i| i["text"].as_str().unwrap())
            .collect()
    };
    assert_eq!(text("user"), ["fix the bug"]);
    assert_eq!(text("assistant"), ["earlier", "looking", "all done"]);
    let tool = |id: &str| items.iter().find(|i| i["tool_call_id"] == id).unwrap();
    assert_eq!(tool("conv-1:2:0")["tool_status"], "completed");
    assert_eq!(tool("conv-1:2:0")["tool_output"], "file-a");
    assert_eq!(tool("conv-1:2:0")["tool_name"], "run_command");
    assert_eq!(tool("conv-1:2:0")["tool_input"], r#"{"CommandLine":"ls"}"#);
    assert_eq!(tool("conv-1:2:1")["tool_status"], "failed");
    assert_eq!(tool("conv-1:2:1")["tool_output"], "boom");
    assert_eq!(tool("conv-1:7:0")["tool_status"], "running");
    assert_eq!(tool("conv-1:7:0")["tool_input"], r#"{"CommandLine":"pwd"}"#);

    let again = backfill(home.path(), &dir, "antigravity", &[]);
    assert_eq!(counts(&again), [1, 0, 0, 1, 0], "{again}");
    let rescan = backfill(home.path(), &dir, "antigravity", &["--rescan"]);
    assert_eq!(counts(&rescan), [1, 0, 9, 0, 1], "{rescan}");
    assert_eq!(messages(home.path(), &dir, "conv-1").len(), 7);
}

#[test]
fn backfill_imports_gemini_cli_sessions_idempotently() {
    let home = tempfile::TempDir::new().unwrap();
    let dir = home.path().join("history");
    history_cli(home.path(), &dir, &["init"]);
    let project = home.path().join(".gemini/tmp/proj");
    let chats = project.join("chats");
    std::fs::create_dir_all(&chats).unwrap();
    std::fs::write(project.join(".project_root"), "/work/proj\n").unwrap();

    let call = |id: &str, status: &str, response: Value| {
        json!({
        "id": id, "name": "run_shell_command", "args": {"command": id}, "status": status,
        "timestamp": "2026-01-01T00:00:03.000Z",
        "result": [{"functionResponse": {"id": id, "name": "run_shell_command", "response": response}}]})
    };
    let user = |id: &str, text: &str| {
        json!({
        "id": id, "timestamp": "2026-01-01T00:00:00.000Z", "type": "user", "content": [{"text": text}]})
    };
    // .jsonl: a message is appended again once its tool calls exist; `$set`
    // metadata, notices and a garbage line are not messages.
    let first_gemini = json!({"id": "a1", "timestamp": "2026-01-01T00:00:02.000Z",
        "type": "gemini", "content": "checking"});
    let mut done_gemini = first_gemini.clone();
    done_gemini["toolCalls"] = json!([
        call("t1", "success", json!({"output": "file-a"})),
        call("t2", "error", json!({"error": "boom"})),
        call("t3", "executing", json!({}))
    ]);
    std::fs::write(
        chats.join("session-a.jsonl"),
        jsonl(&[
            json!({"sessionId": "S1", "projectHash": "h", "kind": "main"}),
            user("u1", "fix the bug"),
            json!({"$set": {"lastUpdated": "2026-01-01T00:00:01.000Z"}}),
            first_gemini,
            done_gemini,
            json!({"id": "i1", "timestamp": "2026-01-01T00:00:04.000Z", "type": "info", "content": "update"}),
        ]) + "not json\n",
    )
    .unwrap();
    // A resumed copy of the session in `.json` form: u1 repeats, u2 is new.
    std::fs::write(
        chats.join("session-b.json"),
        json!({"sessionId": "S1", "projectHash": "h", "messages": [user("u1", "fix the bug"), user("u2", "thanks")]})
            .to_string(),
    )
    .unwrap();

    let first = backfill(home.path(), &dir, "gemini-cli", &[]);
    assert_eq!(first["status"], "imported", "{first}");
    // u1, assistant, 3 running + 2 results, u2 (the repeated u1 is a duplicate).
    assert_eq!(counts(&first), [2, 8, 1, 0, 1], "{first}");

    let items = messages(home.path(), &dir, "S1");
    assert_eq!(items.len(), 6, "{items:?}");
    let text = |kind: &str| -> Vec<&str> {
        items
            .iter()
            .filter(|i| i["kind"] == kind)
            .map(|i| i["text"].as_str().unwrap())
            .collect()
    };
    assert_eq!(text("user"), ["fix the bug", "thanks"]);
    assert_eq!(text("assistant"), ["checking"]);
    let tool = |id: &str| items.iter().find(|i| i["tool_call_id"] == id).unwrap();
    assert_eq!(tool("t1")["tool_status"], "completed");
    assert_eq!(tool("t1")["tool_output"], "file-a");
    assert_eq!(tool("t1")["tool_input"], r#"{"command":"t1"}"#);
    assert_eq!(tool("t2")["tool_status"], "failed");
    assert_eq!(tool("t2")["tool_output"], "boom");
    assert_eq!(tool("t3")["tool_status"], "running");

    let again = backfill(home.path(), &dir, "gemini-cli", &[]);
    assert_eq!(counts(&again), [2, 0, 0, 2, 0], "{again}");
    let rescan = backfill(home.path(), &dir, "gemini-cli", &["--rescan"]);
    assert_eq!(counts(&rescan), [2, 0, 9, 0, 1], "{rescan}");
    assert_eq!(messages(home.path(), &dir, "S1").len(), 6);
}
