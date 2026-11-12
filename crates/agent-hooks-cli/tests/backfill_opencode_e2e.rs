//! Native OpenCode backfill end to end through the real binary against a
//! synthetic OpenCode SQLite store in a temp HOME.

use rusqlite::{params, Connection};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;

const DDL: &str = "
CREATE TABLE session (id text PRIMARY KEY, project_id text NOT NULL, parent_id text,
  slug text NOT NULL, directory text NOT NULL, title text NOT NULL, version text NOT NULL,
  time_created integer NOT NULL, time_updated integer NOT NULL);
CREATE TABLE message (id text PRIMARY KEY, session_id text NOT NULL,
  time_created integer NOT NULL, time_updated integer NOT NULL, data text NOT NULL);
CREATE INDEX message_session_time_created_id_idx ON message (session_id, time_created, id);
CREATE TABLE part (id text PRIMARY KEY, message_id text NOT NULL, session_id text NOT NULL,
  time_created integer NOT NULL, time_updated integer NOT NULL, data text NOT NULL);
CREATE INDEX part_message_id_id_idx ON part (message_id, id);";

fn db_path(home: &Path) -> PathBuf {
    home.join(".local/share/opencode/opencode.db")
}

fn seed(home: &Path) -> Connection {
    std::fs::create_dir_all(db_path(home).parent().unwrap()).unwrap();
    let db = Connection::open(db_path(home)).unwrap();
    db.execute_batch(DDL).unwrap();
    db.execute(
        "INSERT INTO session VALUES ('ses_1','p',NULL,'slug','/work','Title','1',1000,5000)",
        [],
    )
    .unwrap();
    let message = |id: &str, t: i64, data: Value| {
        db.execute(
            "INSERT INTO message VALUES (?1,'ses_1',?2,?2,?3)",
            params![id, t, data.to_string()],
        )
        .unwrap();
    };
    let part = |id: &str, mid: &str, t: i64, data: Value| {
        db.execute(
            "INSERT INTO part VALUES (?1,?2,'ses_1',?3,?3,?4)",
            params![id, mid, t, data.to_string()],
        )
        .unwrap();
    };
    message("msg_1", 1000, json!({"role": "user"}));
    part("prt_1", "msg_1", 1000, json!({"type": "text", "text": "do it"}));
    part("prt_1b", "msg_1", 1001, json!({"type": "text", "text": "ignored", "synthetic": true}));
    message("msg_2", 2000, json!({"role": "assistant"}));
    part("prt_2", "msg_2", 2000, json!({"type": "text", "text": "first thought"}));
    part("prt_3", "msg_2", 2001, json!({"type": "text", "text": "second thought"}));
    part("prt_4", "msg_2", 2002, json!({"type": "tool", "callID": "c1", "tool": "bash",
        "state": {"status": "completed", "input": {"cmd": "ls"}, "output": "a b",
                  "time": {"start": 2002, "end": 2003}}}));
    part("prt_5", "msg_2", 2003, json!({"type": "tool", "callID": "c2", "tool": "read",
        "state": {"status": "error", "input": {"path": "x"}, "error": "nope",
                  "time": {"start": 2004, "end": 2005}}}));
    message("msg_3", 3000, json!({"role": "assistant", "summary": true}));
    part("prt_6", "msg_3", 3000, json!({"type": "text", "text": "compaction summary"}));
    db
}

fn agent_hooks(home: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_agent-hooks"));
    cmd.env("HOME", home).env_remove("XDG_DATA_HOME").current_dir(home);
    cmd
}

fn history_cli(home: &Path, args: &[&str]) -> Value {
    let out = agent_hooks(home)
        .arg("history")
        .args(args)
        .arg("--history-dir")
        .arg(home.join("history"))
        .output()
        .unwrap();
    serde_json::from_slice(&out.stdout).expect("history stdout is JSON")
}

fn backfill(home: &Path) -> Value {
    history_cli(home, &["backfill", "--harness", "opencode"])["harnesses"][0].clone()
}

fn counts(report: &Value) -> (u64, u64) {
    (report["new"].as_u64().unwrap(), report["duplicate"].as_u64().unwrap())
}

fn rows(home: &Path) -> Vec<(String, String)> {
    let page = history_cli(home, &["messages", "--session", "ses_1", "--harness", "opencode"]);
    page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| {
            (
                i["kind"].as_str().unwrap().to_string(),
                i["text"].as_str().or(i["tool_output"].as_str()).unwrap_or_default().to_string(),
            )
        })
        .collect()
}

#[test]
fn opencode_backfill_is_incremental_and_keeps_every_row() {
    let home = tempfile::TempDir::new().unwrap();
    let home = home.path();
    let db = seed(home);
    history_cli(home, &["init"]);

    // user text, 2 assistant texts, 2 tools x (input + output); no synthetic or summary.
    let first = backfill(home);
    assert_eq!(first["status"], "script");
    assert!(first.get("error").is_none(), "{first}");
    assert_eq!(counts(&first), (7, 0));
    let kinds: Vec<_> = rows(home).into_iter().map(|(k, _)| k).collect();
    assert_eq!(kinds, ["user", "assistant", "assistant", "tool", "tool"]);

    // The saved cursor skips the session entirely.
    assert_eq!(counts(&backfill(home)), (0, 0));
    // `--rescan` replays it: everything is a duplicate.
    let rescan = history_cli(home, &["backfill", "--harness", "opencode", "--rescan"]);
    assert_eq!(counts(&rescan["harnesses"][0]), (0, 7));

    // An edited part in a bumped session updates its row instead of duplicating it.
    db.execute(
        "UPDATE part SET data = json_set(data, '$.text', 'revised thought') WHERE id = 'prt_2'",
        [],
    )
    .unwrap();
    db.execute("UPDATE session SET time_updated = 9000", []).unwrap();
    assert_eq!(counts(&backfill(home)), (1, 6));
    let rows = rows(home);
    assert_eq!(rows.len(), 5);
    assert!(rows.iter().any(|(_, t)| t == "revised thought"));
    assert!(!rows.iter().any(|(_, t)| t == "first thought"));
}

#[test]
fn absent_database_is_a_successful_no_op_and_bad_schema_is_an_error() {
    let home = tempfile::TempDir::new().unwrap();
    let home = home.path();
    history_cli(home, &["init"]);
    let absent = backfill(home);
    assert!(absent.get("error").is_none());
    assert_eq!(counts(&absent), (0, 0));

    std::fs::create_dir_all(db_path(home).parent().unwrap()).unwrap();
    Connection::open(db_path(home))
        .unwrap()
        .execute_batch("CREATE TABLE session (id text)")
        .unwrap();
    let broken = backfill(home);
    assert!(broken["error"].as_str().unwrap().contains("SQLite error"), "{broken}");
}
