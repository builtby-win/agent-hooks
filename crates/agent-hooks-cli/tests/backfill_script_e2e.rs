//! Manifest `history_backfill` and `history import` end to end through the
//! real binary: a user-tier manifest runs a tiny shell script.

use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Echoes the cursor it was given into a delivery id; `$HOME/fail` makes it
/// advertise a new cursor and then exit 3.
const SCRIPT: &str = r#"#!/bin/sh
read req
cursor=$(printf '%s' "$req" | sed 's/.*"cursor":\([^}]*\)}.*/\1/' | tr -d '"')
echo '{"record":{"session_id":"s1","delivery_id":"backfill:d1","timestamp_ms":1000,"workspace":"/w","title":"T","entry":{"kind":"user","text":"hello"}}}'
echo '{"record":{"session_id":"s1","delivery_id":"backfill:d2","timestamp_ms":2000,"entry":{"kind":"assistant","text":"hi"}}}'
echo "{\"record\":{\"session_id\":\"s1\",\"delivery_id\":\"backfill:after-$cursor\",\"timestamp_ms\":3000,\"entry\":{\"kind\":\"tool\",\"tool_name\":\"bash\",\"tool_call_id\":\"c1\",\"tool_status\":\"running\",\"text\":\"{}\"}}}"
echo 'not json'
echo '{"record":{"session_id":"","delivery_id":"backfill:x","timestamp_ms":1}}'
if [ -e "$HOME/fail" ]; then
  echo '{"cursor":"bad"}'
  echo boom >&2
  exit 3
fi
echo '{"cursor":"c1"}'
"#;

fn agent_hooks(home: &Path) -> Command {
    let mut cmd = Command::new(PathBuf::from(env!("CARGO_BIN_EXE_agent-hooks")));
    cmd.env("HOME", home).current_dir(home);
    cmd
}

fn history_cli(home: &Path, args: &[&str]) -> Value {
    let out = agent_hooks(home)
        .arg("history")
        .args(args)
        .arg("--history-dir")
        .arg(home.join("history"))
        .output()
        .expect("run history cli");
    serde_json::from_slice(&out.stdout).expect("history stdout is JSON")
}

fn write_manifest(home: &Path, id: &str, os: &str) {
    let dir = home.join(".config/agent-hooks/adapters").join(id);
    std::fs::create_dir_all(&dir).unwrap();
    let manifest = json!({
        "id": id,
        "display_name": id,
        "default_version": "v1",
        "history_backfill": {os: {"command": ["./hist.sh"], "timeout_secs": 30}},
        "versions": {"v1": {"install": [], "events": {}, "extraction": {}}},
    });
    std::fs::write(dir.join("adapter.json"), manifest.to_string()).unwrap();
    let script = dir.join("hist.sh");
    std::fs::write(&script, SCRIPT).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

fn backfill(home: &Path, harness: &str, extra: &[&str]) -> Value {
    let mut args = vec!["backfill", "--harness", harness];
    args.extend_from_slice(extra);
    history_cli(home, &args)["harnesses"][0].clone()
}

fn counts(report: &Value) -> (u64, u64, u64) {
    let n = |k: &str| report[k].as_u64().unwrap();
    (n("new"), n("duplicate"), n("skipped"))
}

fn texts(home: &Path, harness: &str) -> Vec<(String, Value)> {
    let page = history_cli(home, &["messages", "--session", "s1", "--harness", harness]);
    page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| (i["kind"].as_str().unwrap().to_string(), i["text"].clone()))
        .collect()
}

#[test]
fn script_backfill_stores_records_and_honors_the_cursor() {
    let home = tempfile::TempDir::new().unwrap();
    let home = home.path();
    write_manifest(home, "fakeh", std::env::consts::OS);
    history_cli(home, &["init"]);

    let first = backfill(home, "fakeh", &[]);
    assert_eq!(first["status"], "script");
    assert_eq!(counts(&first), (3, 0, 2));
    assert!(first.get("error").is_none());
    let kinds: Vec<_> = texts(home, "fakeh").into_iter().map(|(k, _)| k).collect();
    assert_eq!(kinds, ["user", "assistant", "tool"]);

    // The saved cursor `c1` reaches the script: `after-c1` is the only new record.
    assert_eq!(counts(&backfill(home, "fakeh", &[])), (1, 2, 2));
    // `--rescan` sends a null cursor again: everything is a duplicate.
    assert_eq!(counts(&backfill(home, "fakeh", &["--rescan"])), (0, 3, 2));

    // A failing run keeps its records but not its cursor `bad`.
    std::fs::write(home.join("fail"), "").unwrap();
    let failed = backfill(home, "fakeh", &[]);
    assert!(failed["error"].as_str().unwrap().contains("exit status: 3"));
    assert_eq!(counts(&failed), (0, 3, 2));
    std::fs::remove_file(home.join("fail")).unwrap();
    assert_eq!(counts(&backfill(home, "fakeh", &[])), (0, 3, 2));
}

#[test]
fn manifest_without_this_os_is_unsupported() {
    let home = tempfile::TempDir::new().unwrap();
    let home = home.path();
    write_manifest(home, "otheros", "plan9");
    history_cli(home, &["init"]);
    assert_eq!(backfill(home, "otheros", &[])["status"], "unsupported os");
}

#[test]
fn import_reads_record_lines_from_stdin() {
    let home = tempfile::TempDir::new().unwrap();
    let home = home.path();
    write_manifest(home, "fakeh", std::env::consts::OS);
    history_cli(home, &["init"]);

    let mut child = agent_hooks(home)
        .args(["history", "import", "--harness", "fakeh", "--history-dir"])
        .arg(home.join("history"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(SCRIPT.lines().filter(|l| l.starts_with("echo '{\"record")).map(|l| &l[6..l.len() - 1]).collect::<Vec<_>>().join("\n").as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!((report["new"].clone(), report["skipped"].clone()), (json!(2), json!(1)));
    assert_eq!(texts(home, "fakeh").len(), 2);
}
