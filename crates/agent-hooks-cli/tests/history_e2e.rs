//! Opt-in history store, end to end through the real `agent-hooks` binary.
//! Capture happens in the `hook` shim with no daemon running; the store
//! existing is the opt-in.

use agent_hooks_history::SCHEMA_VERSION;
use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_agent-hooks"))
}

/// Run a `history` subcommand; returns (exit success, stdout JSON).
fn history_cli(home: &Path, dir: &Path, args: &[&str]) -> (bool, Value) {
    let out = Command::new(bin())
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

/// Run one hook the way a harness does. The daemon socket does not exist, so
/// anything stored was written by the shim itself.
fn shim(home: &Path, dir: &Path, harness: &str, event: &str, payload: Value) -> Output {
    shim_via(
        home,
        dir,
        &home.join("no-daemon.sock"),
        &[],
        harness,
        event,
        payload,
    )
}

fn shim_via(
    home: &Path,
    dir: &Path,
    socket: &Path,
    extra: &[&str],
    harness: &str,
    event: &str,
    payload: Value,
) -> Output {
    let mut child = Command::new(bin())
        .args(["hook", harness, "--event", event, "--socket"])
        .arg(socket)
        .args(extra)
        .env("HOME", home)
        .env("AGENT_HOOKS_HISTORY_DIR", dir)
        .current_dir(home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn messages(home: &Path, dir: &Path, session: &str) -> Vec<Value> {
    let (ok, page) = history_cli(home, dir, &["messages", "--session", session]);
    assert!(ok, "messages failed: {page}");
    page["items"].as_array().unwrap().clone()
}

#[test]
fn shim_captures_without_a_daemon_and_stores_tool_bodies() {
    let home = tempfile::TempDir::new().unwrap();
    let dir = home.path().join("history");
    let (ok, init) = history_cli(home.path(), &dir, &["init"]);
    assert!(ok);
    assert_eq!(init["created"], true);
    let (_, again) = history_cli(home.path(), &dir, &["init"]);
    assert_eq!(again["created"], false);

    let prompt = json!({"session_id": "s-1", "prompt": "  keep my whitespace  "});
    // Identical text twice is two messages: each hook occurrence is distinct.
    for _ in 0..2 {
        let out = shim(
            home.path(),
            &dir,
            "claude-code",
            "UserPromptSubmit",
            prompt.clone(),
        );
        assert!(out.status.success());
    }
    // One tool call observed as a start and an end marker with a shared native id.
    shim(
        home.path(),
        &dir,
        "claude-code",
        "PreToolUse",
        json!({"session_id": "s-1", "tool_name": "Bash", "tool_use_id": "toolu_1",
               "tool_input": {"command": "cat /etc/shadow"}}),
    );
    shim(
        home.path(),
        &dir,
        "claude-code",
        "PostToolUse",
        json!({"session_id": "s-1", "tool_name": "Bash", "tool_use_id": "toolu_1",
               "tool_input": {"command": "cat /etc/shadow"},
               "tool_response": {"stdout": "TOP-SECRET-OUTPUT"}}),
    );
    let reply = "é".repeat(5000);
    shim(
        home.path(),
        &dir,
        "claude-code",
        "Stop",
        json!({"session_id": "s-1", "last_assistant_message": reply}),
    );
    // No native session id: never stored under a generated one.
    shim(
        home.path(),
        &dir,
        "claude-code",
        "UserPromptSubmit",
        json!({"prompt": "orphan"}),
    );

    let items = messages(home.path(), &dir, "s-1");
    let kinds: Vec<&str> = items.iter().map(|e| e["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["user", "user", "tool", "assistant"]);
    assert_eq!(items[0]["text"], "  keep my whitespace  ");
    assert_eq!(items[2]["tool_call_id"], "toolu_1");
    assert_eq!(items[2]["tool_status"], "completed");
    assert_eq!(
        items[3]["text"].as_str().unwrap(),
        reply,
        "summary is stored whole"
    );

    let (_, status) = history_cli(home.path(), &dir, &["status"]);
    assert_eq!(status["sessions"], 1);
    assert_eq!(status["entries"], 4);

    // Tool bodies are stored whole and kept apart as input and output.
    let tool_input: Value =
        serde_json::from_str(items[2]["tool_input"].as_str().expect("tool_input")).unwrap();
    assert_eq!(tool_input["command"], "cat /etc/shadow");
    // Object-valued responses are stored whole as compact JSON.
    assert_eq!(
        items[2]["tool_output"].as_str().unwrap(),
        json!({"stdout": "TOP-SECRET-OUTPUT"}).to_string()
    );
    assert!(items[2].get("text").is_none_or(Value::is_null));
    // Prompts without a native session id are never stored.
    let raw = std::fs::read(dir.join("history.sqlite3")).unwrap();
    let wal = std::fs::read(dir.join("history.sqlite3-wal")).unwrap_or_default();
    for needle in [&b"orphan"[..]] {
        assert!(!raw.windows(needle.len()).any(|w| w == needle));
        assert!(!wal.windows(needle.len()).any(|w| w == needle));
    }
}

/// The messages of one tool call, by native call id.
fn tool_rows(items: &[Value]) -> Vec<&Value> {
    items.iter().filter(|e| e["kind"] == "tool").collect()
}

#[test]
fn repeated_tool_phases_with_one_native_call_id_stay_one_row() {
    let home = tempfile::TempDir::new().unwrap();
    let dir = home.path().join("history");
    history_cli(home.path(), &dir, &["init"]);

    let pre = |id: &str, cmd: &str| {
        shim(
            home.path(),
            &dir,
            "claude-code",
            "PreToolUse",
            json!({"session_id": "rep", "tool_name": "Bash", "tool_use_id": id,
                   "tool_input": {"command": cmd}}),
        )
    };
    let post = |id: &str, out: &str| {
        shim(
            home.path(),
            &dir,
            "claude-code",
            "PostToolUse",
            json!({"session_id": "rep", "tool_name": "Bash", "tool_use_id": id,
                   "tool_response": out}),
        )
    };
    for out in [
        pre("call-1", "ls"),
        pre("call-1", "ls"),
        post("call-1", "first-output"),
        post("call-1", "first-output"),
        // A late start marker never regresses a finished call or erases its output.
        pre("call-1", "ls"),
        pre("call-2", "pwd"),
    ] {
        assert!(out.status.success());
    }

    let items = messages(home.path(), &dir, "rep");
    let tools = tool_rows(&items);
    assert_eq!(tools.len(), 2, "one row per native call id: {items:?}");
    let first = tools
        .iter()
        .find(|t| t["tool_call_id"] == "call-1")
        .unwrap();
    assert_eq!(first["tool_status"], "completed");
    assert_eq!(first["tool_output"], "first-output");
    let first_input: Value = serde_json::from_str(first["tool_input"].as_str().unwrap()).unwrap();
    assert_eq!(first_input["command"], "ls");
    let second = tools
        .iter()
        .find(|t| t["tool_call_id"] == "call-2")
        .unwrap();
    assert_eq!(second["tool_status"], "running");
    assert!(second["tool_output"].is_null());
    let second_input: Value = serde_json::from_str(second["tool_input"].as_str().unwrap()).unwrap();
    assert_eq!(second_input["command"], "pwd");

    let (_, status) = history_cli(home.path(), &dir, &["status"]);
    assert_eq!(status["entries"], 2);
}

#[test]
fn opencode_before_and_after_pair_on_call_id() {
    let home = tempfile::TempDir::new().unwrap();
    let dir = home.path().join("history");
    history_cli(home.path(), &dir, &["init"]);

    for (event, payload) in [
        (
            "tool.execute.before",
            json!({"sessionID": "ses_oc", "tool": "bash", "callID": "oc-1",
                   "args": {"command": "echo hi"}}),
        ),
        (
            "tool.execute.after",
            json!({"sessionID": "ses_oc", "tool": "bash", "callID": "oc-1",
                   "output": "hi", "title": "echo hi"}),
        ),
        (
            "tool.execute.before",
            json!({"sessionID": "ses_oc", "tool": "bash", "callID": "oc-2",
                   "args": {"command": "pwd"}}),
        ),
    ] {
        let out = shim(home.path(), &dir, "opencode", event, payload);
        assert!(out.status.success());
    }

    let items = messages(home.path(), &dir, "ses_oc");
    let tools = tool_rows(&items);
    assert_eq!(
        tools.len(),
        2,
        "different call ids stay separate: {items:?}"
    );
    let paired = tools.iter().find(|t| t["tool_call_id"] == "oc-1").unwrap();
    assert_eq!(paired["tool_status"], "completed");
    assert_eq!(paired["tool_output"], "hi");
    let input: Value = serde_json::from_str(paired["tool_input"].as_str().unwrap()).unwrap();
    assert_eq!(input["command"], "echo hi");
    let open = tools.iter().find(|t| t["tool_call_id"] == "oc-2").unwrap();
    assert_eq!(open["tool_status"], "running");
    assert!(open["tool_output"].is_null());
}

#[test]
fn opencode_chat_parts_and_assistant_summary_are_persisted() {
    let home = tempfile::TempDir::new().unwrap();
    let dir = home.path().join("history");
    history_cli(home.path(), &dir, &["init"]);

    let prompt = shim(
        home.path(),
        &dir,
        "opencode",
        "chat.message",
        json!({
            "sessionID": "ses_oc_history",
            "parts": [
                {"type": "text", "text": "Keep "},
                {"type": "tool_use", "text": "not a prompt"},
                {"type": "text", "text": "the user words"}
            ]
        }),
    );
    assert!(prompt.status.success());
    let stop = shim(
        home.path(),
        &dir,
        "opencode",
        "session.idle",
        json!({"sessionID": "ses_oc_history", "summary": "The real assistant answer"}),
    );
    assert!(stop.status.success());

    let items = messages(home.path(), &dir, "ses_oc_history");
    assert_eq!(items.len(), 2, "{items:?}");
    assert_eq!(items[0]["kind"], "user");
    assert_eq!(items[0]["text"], "Keep\nthe user words");
    assert_eq!(items[1]["kind"], "assistant");
    assert_eq!(items[1]["text"], "The real assistant answer");
}

#[test]
fn large_unicode_tool_bodies_roundtrip_whole_and_separate() {
    let home = tempfile::TempDir::new().unwrap();
    let dir = home.path().join("history");
    history_cli(home.path(), &dir, &["init"]);

    let command = "日本語🦀é-in ".repeat(20_000);
    let output = "Ωmega→out\n".repeat(30_000);
    let pre = shim(
        home.path(),
        &dir,
        "claude-code",
        "PreToolUse",
        json!({"session_id": "big", "tool_name": "Bash", "tool_use_id": "big-1",
               "tool_input": {"command": command}}),
    );
    assert!(pre.status.success());
    let post = shim(
        home.path(),
        &dir,
        "claude-code",
        "PostToolUse",
        json!({"session_id": "big", "tool_name": "Bash", "tool_use_id": "big-1",
               "tool_response": output}),
    );
    assert!(post.status.success());
    for out in [&pre, &post] {
        assert!(!String::from_utf8_lossy(&out.stderr).contains("history"));
    }

    let items = messages(home.path(), &dir, "big");
    let tools = tool_rows(&items);
    assert_eq!(tools.len(), 1);
    let input: Value = serde_json::from_str(tools[0]["tool_input"].as_str().unwrap()).unwrap();
    assert_eq!(input["command"].as_str().unwrap(), command);
    assert_eq!(tools[0]["tool_output"].as_str().unwrap(), output);
    assert_eq!(tools[0]["tool_status"], "completed");
}

#[test]
fn concurrent_shims_all_land() {
    let home = tempfile::TempDir::new().unwrap();
    let dir = home.path().join("history");
    history_cli(home.path(), &dir, &["init"]);

    let handles: Vec<_> = (0..12)
        .map(|i| {
            let (home, dir) = (home.path().to_path_buf(), dir.clone());
            std::thread::spawn(move || {
                let out = shim(
                    &home,
                    &dir,
                    "claude-code",
                    "UserPromptSubmit",
                    json!({"session_id": "par", "prompt": format!("p{i}")}),
                );
                assert!(out.status.success());
                String::from_utf8_lossy(&out.stderr).into_owned()
            })
        })
        .collect();
    for h in handles {
        let stderr = h.join().unwrap();
        assert!(!stderr.contains("history"), "history warning: {stderr}");
    }
    assert_eq!(messages(home.path(), &dir, "par").len(), 12);
}

#[test]
fn same_session_id_on_two_harnesses_requires_disambiguation() {
    let home = tempfile::TempDir::new().unwrap();
    let dir = home.path().join("history");
    history_cli(home.path(), &dir, &["init"]);

    shim(
        home.path(),
        &dir,
        "codex",
        "UserPromptSubmit",
        json!({"session_id": "dup", "prompt": "from codex"}),
    );
    shim(
        home.path(),
        &dir,
        "claude-code",
        "UserPromptSubmit",
        json!({"session_id": "dup", "prompt": "from claude"}),
    );

    let (ok, err) = history_cli(home.path(), &dir, &["messages", "--session", "dup"]);
    assert!(!ok);
    assert_eq!(err["error"]["code"], "ambiguous_session");
    let (ok, page) = history_cli(
        home.path(),
        &dir,
        &["messages", "--session", "dup", "--harness", "codex"],
    );
    assert!(ok);
    assert_eq!(page["items"][0]["text"], "from codex");
}

#[test]
fn history_failure_never_changes_the_hook_outcome() {
    let home = tempfile::TempDir::new().unwrap();
    let dir = home.path().join("history");
    history_cli(home.path(), &dir, &["init"]);
    let payload = json!({"session_id": "s-held", "prompt": "x"});

    let healthy = shim(
        home.path(),
        &dir,
        "claude-code",
        "UserPromptSubmit",
        payload.clone(),
    );

    // A newer binary migrated the store: every write now fails.
    let db = rusqlite::Connection::open(dir.join("history.sqlite3")).unwrap();
    db.pragma_update(None, "user_version", 99).unwrap();
    let broken = shim(
        home.path(),
        &dir,
        "claude-code",
        "UserPromptSubmit",
        json!({"session_id": "s-held", "prompt": "lost"}),
    );
    assert_eq!(broken.status.code(), healthy.status.code());
    assert_eq!(broken.stdout, healthy.stdout);
    let stderr = String::from_utf8_lossy(&broken.stderr);
    assert!(stderr.contains("history"), "warns: {stderr}");
    assert!(
        !stderr.contains("lost"),
        "never echoes captured text: {stderr}"
    );

    // Store recovers; later observations persist again.
    db.pragma_update(None, "user_version", SCHEMA_VERSION)
        .unwrap();
    shim(
        home.path(),
        &dir,
        "claude-code",
        "UserPromptSubmit",
        json!({"session_id": "s-held", "prompt": "after recovery"}),
    );
    let texts: Vec<String> = messages(home.path(), &dir, "s-held")
        .iter()
        .map(|e| e["text"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(texts, ["x", "after recovery"]);
}

#[test]
fn history_is_never_created_implicitly() {
    let home = tempfile::TempDir::new().unwrap();
    let dir = home.path().join("history");

    // Readers do not create a store.
    let (ok, err) = history_cli(home.path(), &dir, &["status"]);
    assert!(!ok);
    assert!(err["error"]["code"].is_string());
    assert!(!dir.exists());

    // A hook with no store is the off state: silent, exit 0, nothing created.
    let out = shim(
        home.path(),
        &dir,
        "claude-code",
        "UserPromptSubmit",
        json!({"session_id": "s", "prompt": "hi"}),
    );
    assert!(out.status.success());
    assert!(!String::from_utf8_lossy(&out.stderr).contains("history"));
    assert!(!dir.exists());
    assert!(!home.path().join(".agent-hooks").join("history").exists());

    // A daemon never touches history paths.
    let mut daemon = Command::new(bin())
        .arg("daemon")
        .arg("--socket")
        .arg(home.path().join("p.sock"))
        .env("HOME", home.path())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(800));
    daemon.kill().unwrap();
    daemon.wait().unwrap();
    assert!(!dir.exists());
    assert!(!home.path().join(".agent-hooks").join("history").exists());
}

#[test]
fn daemon_records_forwarded_events_by_origin_and_only_those() {
    let home = tempfile::TempDir::new().unwrap();
    let dir = home.path().join("history");
    history_cli(home.path(), &dir, &["init"]);
    let socket = home.path().join("d.sock");
    let mut daemon = Command::new(bin())
        .args(["daemon", "--history", "--history-dir"])
        .arg(&dir)
        .arg("--socket")
        .arg(&socket)
        .env("HOME", home.path())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    for _ in 0..50 {
        if socket.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    // The "remote" machine: its own HOME and no store of its own, so the only
    // place its history can land is the daemon it forwards to.
    let remote = tempfile::TempDir::new().unwrap();
    let remote_store = remote.path().join("none");
    let unlabeled = shim_via(
        remote.path(),
        &remote_store,
        &socket,
        &[],
        "claude-code",
        "UserPromptSubmit",
        json!({"session_id": "r-1", "prompt": "no origin"}),
    );
    assert!(unlabeled.status.success());
    let labeled = shim_via(
        remote.path(),
        &remote_store,
        &socket,
        &["--origin", "box-a"],
        "claude-code",
        "UserPromptSubmit",
        json!({"session_id": "r-1", "prompt": "from box-a"}),
    );
    assert!(labeled.status.success());

    // The daemon commits before replying, so the labeled event is already
    // visible; poll defensively for scheduler jitter.
    let mut page = Value::Null;
    for _ in 0..50 {
        let (ok, p) = history_cli(home.path(), &dir, &["sessions"]);
        if ok && p["items"].as_array().is_some_and(|i| !i.is_empty()) {
            page = p;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    daemon.kill().unwrap();
    daemon.wait().unwrap();

    let items = page["items"].as_array().expect("a session landed");
    assert_eq!(
        items.len(),
        1,
        "unlabeled event must not be recorded: {page}"
    );
    assert_eq!(items[0]["origin"], "box-a");
    let (ok, msgs) = history_cli(
        home.path(),
        &dir,
        &["messages", "--session", "r-1", "--origin", "box-a"],
    );
    assert!(ok);
    assert_eq!(msgs["items"][0]["text"], "from box-a");
    assert!(
        !remote_store.exists(),
        "forwarding must not create a remote store"
    );
}

#[test]
fn daemon_history_refuses_without_a_store() {
    let home = tempfile::TempDir::new().unwrap();
    let dir = home.path().join("history");
    let out = Command::new(bin())
        .args(["daemon", "--history", "--history-dir"])
        .arg(&dir)
        .arg("--socket")
        .arg(home.path().join("d.sock"))
        .env("HOME", home.path())
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(!dir.exists());
    assert!(
        !home.path().join("d.sock").exists(),
        "a refused daemon must not start serving"
    );
}

#[test]
fn status_json_reports_daemon_history_capability() {
    let home = tempfile::TempDir::new().unwrap();
    let dir = home.path().join("history");
    history_cli(home.path(), &dir, &["init"]);
    // A reused daemon started without `--history` records nothing even at
    // the same version; `status --json` tells reuse apart from capture.
    for (with_history, expect) in [(false, false), (true, true)] {
        let socket = home
            .path()
            .join(if with_history { "h.sock" } else { "p.sock" });
        let mut cmd = Command::new(bin());
        cmd.arg("daemon")
            .arg("--socket")
            .arg(&socket)
            .env("HOME", home.path())
            .stderr(Stdio::null());
        if with_history {
            cmd.args(["--history", "--history-dir"]).arg(&dir);
        }
        let mut daemon = cmd.spawn().unwrap();
        for _ in 0..50 {
            if socket.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let out = Command::new(bin())
            .arg("status")
            .arg("--socket")
            .arg(&socket)
            .arg("--json")
            .env("HOME", home.path())
            .output()
            .unwrap();
        assert!(out.status.success());
        let report: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(report["daemon"]["running"], true);
        assert_eq!(
            report["daemon"]["history_enabled"], expect,
            "daemon history capability must be visible: {report}"
        );
        daemon.kill().unwrap();
        daemon.wait().unwrap();
    }
}

#[test]
fn sessions_session_flag_filters_exactly_and_unknown_id_is_empty() {
    let home = tempfile::TempDir::new().unwrap();
    let dir = home.path().join("history");
    let (ok, _) = history_cli(home.path(), &dir, &["init"]);
    assert!(ok);
    for (harness, session) in [("claude-code", "s-1"), ("codex", "s-2")] {
        let out = shim(
            home.path(),
            &dir,
            harness,
            "UserPromptSubmit",
            json!({"session_id": session, "prompt": "hello"}),
        );
        assert!(out.status.success());
    }

    // Exact match plus harness/origin narrowing returns only that session.
    let (ok, page) = history_cli(
        home.path(),
        &dir,
        &[
            "sessions",
            "--session",
            "s-1",
            "--harness",
            "claude-code",
            "--origin",
            "local",
        ],
    );
    assert!(ok, "sessions --session failed: {page}");
    let items = page["items"].as_array().expect("items");
    assert_eq!(items.len(), 1, "expected only s-1: {page}");
    assert_eq!(items[0]["session_id"], "s-1");

    // A mismatched harness narrows the same session filter to nothing.
    let (ok, page) = history_cli(
        home.path(),
        &dir,
        &["sessions", "--session", "s-1", "--harness", "codex"],
    );
    assert!(ok, "sessions --session --harness failed: {page}");
    assert!(page["items"].as_array().unwrap().is_empty());

    // Unknown id is a normal empty page with exit 0, never an error envelope.
    let (ok, page) = history_cli(
        home.path(),
        &dir,
        &["sessions", "--session", "does-not-exist"],
    );
    assert!(ok, "unknown session must not error: {page}");
    assert!(page["items"].as_array().unwrap().is_empty());
    assert_eq!(page["api_version"], 2);
    assert!(page.get("error").is_none());
}

#[test]
fn sessions_search_uses_pushed_titles_and_preserves_them_when_absent() {
    let home = tempfile::TempDir::new().unwrap();
    let dir = home.path().join("history");
    assert!(history_cli(home.path(), &dir, &["init"]).0);

    for payload in [
        json!({"session_id": "named", "session_title": "Fix login", "prompt": "start"}),
        json!({"session_id": "named", "prompt": "continue"}),
        json!({"session_id": "other", "session_title": "Refactor storage", "prompt": "start"}),
    ] {
        assert!(shim(home.path(), &dir, "omp", "UserPromptSubmit", payload)
            .status
            .success());
    }

    let (ok, page) = history_cli(home.path(), &dir, &["sessions", "--search", "LOGIN"]);
    assert!(ok, "{page}");
    let items = page["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "{page}");
    assert_eq!(items[0]["session_id"], "named");
    assert_eq!(items[0]["title"], "Fix login");

    assert!(shim(
        home.path(),
        &dir,
        "omp",
        "UserPromptSubmit",
        json!({"session_id": "named", "session_title": "Ship authentication", "prompt": "rename"}),
    )
    .status
    .success());
    let (ok, old) = history_cli(home.path(), &dir, &["sessions", "--search", "login"]);
    assert!(ok, "{old}");
    assert!(old["items"].as_array().unwrap().is_empty());
    let (ok, renamed) = history_cli(
        home.path(),
        &dir,
        &["sessions", "--search", "authentication", "--harness", "omp"],
    );
    assert!(ok, "{renamed}");
    assert_eq!(renamed["items"][0]["title"], "Ship authentication");
}

/// Run `history search`, feeding the session scope on stdin.
fn search_cli(home: &Path, dir: &Path, query: &str, scope: &Value) -> (bool, Value) {
    let mut child = Command::new(bin())
        .args(["history", "search", "--query", query, "--history-dir"])
        .arg(dir)
        .env("HOME", home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(scope.to_string().as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let json = serde_json::from_slice(&out.stdout).expect("search stdout is JSON");
    (out.status.success(), json)
}

#[test]
fn search_returns_newest_hit_only_for_the_sessions_on_stdin() {
    let home = tempfile::TempDir::new().unwrap();
    let dir = home.path().join("history");
    assert!(history_cli(home.path(), &dir, &["init"]).0);
    for (session, prompt) in [
        ("s-1", "fix the Albuquerque bug"),
        ("s-2", "Albuquerque too"),
    ] {
        let out = shim(
            home.path(),
            &dir,
            "claude-code",
            "UserPromptSubmit",
            json!({"session_id": session, "prompt": prompt}),
        );
        assert!(out.status.success());
    }
    let key =
        |session: &str| json!({"origin": "local", "harness": "claude-code", "session_id": session});

    let (ok, page) = search_cli(home.path(), &dir, " ALBUQUERQUE ", &json!([key("s-1")]));
    assert!(ok, "search failed: {page}");
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
    assert_eq!(page["items"][0]["session"]["session_id"], "s-1");
    assert_eq!(page["items"][0]["kind"], "user");
    assert_eq!(page["items"][0]["snippet"], "fix the Albuquerque bug");

    // A scope that is not a key array is a failure envelope, not a crash.
    let (ok, err) = search_cli(home.path(), &dir, "albuquerque", &json!({"nope": 1}));
    assert!(!ok);
    assert_eq!(err["error"]["code"], "invalid_argument");
}

fn claude_line(kind: &str, uuid: &str, ts: &str, content: Value) -> String {
    json!({
        "type": kind, "uuid": uuid, "timestamp": ts, "sessionId": "sess-1", "cwd": "/work",
        "isSidechain": false, "message": {"role": kind, "content": content},
    })
    .to_string()
}

fn backfill(home: &Path, dir: &Path, extra: &[&str]) -> Value {
    let mut args = vec!["backfill"];
    args.extend_from_slice(extra);
    let (ok, out) = history_cli(home, dir, &args);
    assert!(ok, "backfill failed: {out}");
    out
}

fn harness_report<'a>(out: &'a Value, harness: &str) -> &'a Value {
    out["harnesses"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["harness"] == harness)
        .unwrap_or_else(|| panic!("no report for {harness}: {out}"))
}

#[test]
fn backfill_imports_claude_transcripts_incrementally_and_idempotently() {
    let home = tempfile::TempDir::new().unwrap();
    let dir = home.path().join("history");
    assert!(history_cli(home.path(), &dir, &["init"]).0);
    let project = home.path().join(".claude/projects/-work");
    std::fs::create_dir_all(&project).unwrap();
    let transcript = project.join("sess-1.jsonl");
    // One assistant line carries two text parts and two tool calls; every
    // part must be stored. Noise lines are ignored; the garbage line is
    // counted as skipped.
    let mut lines = vec![
        claude_line("user", "u1", "2026-01-01T00:00:00Z", json!("fix the bug")),
        claude_line(
            "assistant",
            "a1",
            "2026-01-01T00:00:01Z",
            json!([
                {"type": "thinking", "thinking": "", "signature": "x"},
                {"type": "text", "text": "looking"},
                {"type": "text", "text": "reading files"},
                {"type": "tool_use", "id": "toolu_a", "name": "Bash", "input": {"command": "ls"}},
                {"type": "tool_use", "id": "toolu_b", "name": "Read", "input": {"path": "x"}},
            ]),
        ),
        claude_line(
            "user",
            "r1",
            "2026-01-01T00:00:02Z",
            json!([{"type": "tool_result", "tool_use_id": "toolu_a", "content": "file-a"}]),
        ),
        claude_line(
            "user",
            "r2",
            "2026-01-01T00:00:03Z",
            json!([{"type": "tool_result", "tool_use_id": "toolu_b", "is_error": true,
                    "content": [{"type": "text", "text": "boom"}]}]),
        ),
        json!({"type": "attachment", "sessionId": "sess-1"}).to_string(),
        "not json".to_string(),
    ];
    let write = |lines: &[String]| std::fs::write(&transcript, lines.join("\n") + "\n").unwrap();
    write(&lines);

    let first = backfill(home.path(), &dir, &[]);
    let claude = harness_report(&first, "claude-code");
    assert_eq!(claude["status"], "imported");
    assert_eq!(
        (claude["files"].as_u64(), claude["new"].as_u64(), claude["skipped"].as_u64()),
        (Some(1), Some(7), Some(1)),
        "{first}"
    );
    assert_eq!(harness_report(&first, "droid")["status"], "no importer");

    let items = messages(home.path(), &dir, "sess-1");
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
    assert_eq!(tool("toolu_a")["tool_status"], "completed");
    assert_eq!(tool("toolu_a")["tool_output"], "file-a");
    assert_eq!(tool("toolu_a")["tool_name"], "Bash");
    assert_eq!(tool("toolu_b")["tool_status"], "failed");
    assert_eq!(tool("toolu_b")["tool_output"], "boom");
    assert_eq!(tool("toolu_b")["tool_input"], r#"{"path":"x"}"#);
    // Backfill created exactly one session.
    let (_, sessions) = history_cli(home.path(), &dir, &["sessions"]);
    assert_eq!(sessions["items"].as_array().unwrap().len(), 1);

    // Unchanged file: not re-read at all.
    let again = backfill(home.path(), &dir, &[]);
    let claude = harness_report(&again, "claude-code");
    assert_eq!((claude["unchanged"].as_u64(), claude["new"].as_u64()), (Some(1), Some(0)));

    // Forced full re-read: every observation is a duplicate, nothing changes.
    let rescan = backfill(home.path(), &dir, &["--rescan"]);
    let claude = harness_report(&rescan, "claude-code");
    assert_eq!((claude["new"].as_u64(), claude["duplicate"].as_u64()), (Some(0), Some(7)));
    assert_eq!(messages(home.path(), &dir, "sess-1").len(), 5);

    // Append-only growth imports only the new line.
    lines.push(claude_line("user", "u2", "2026-01-01T00:00:04Z", json!("thanks")));
    write(&lines);
    let grown = backfill(home.path(), &dir, &[]);
    let claude = harness_report(&grown, "claude-code");
    assert_eq!(
        (claude["new"].as_u64(), claude["duplicate"].as_u64(), claude["unchanged"].as_u64()),
        (Some(1), Some(0), Some(0)),
        "{grown}"
    );
    assert_eq!(messages(home.path(), &dir, "sess-1").len(), 6);

    // A rewritten file (new head) is re-read from 0, not resumed.
    lines[0] = claude_line("user", "u0", "2026-01-01T00:00:00Z", json!("rewritten"));
    write(&lines);
    let rewritten = backfill(home.path(), &dir, &[]);
    let claude = harness_report(&rewritten, "claude-code");
    let seen = claude["new"].as_u64().unwrap() + claude["duplicate"].as_u64().unwrap();
    assert_eq!(seen, 8, "{rewritten}");
}

/// Backfill only fills sessions missing from the store (or ones it backfilled
/// itself): a session captured by live hooks is left alone entirely.
#[test]
fn backfill_skips_live_sessions_and_fills_missing_ones() {
    let home = tempfile::TempDir::new().unwrap();
    let dir = home.path().join("history");
    assert!(history_cli(home.path(), &dir, &["init"]).0);
    let project = home.path().join(".claude/projects/-work");
    std::fs::create_dir_all(&project).unwrap();
    let line = |session: &str, kind: &str, uuid: &str, ts: &str, text: &str| {
        json!({
            "type": kind, "uuid": uuid, "timestamp": ts, "sessionId": session, "cwd": "/work",
            "isSidechain": false, "message": {"role": kind, "content": text},
        })
        .to_string()
    };

    // (a) A live-captured session: same id as a transcript on disk.
    assert!(shim(
        home.path(),
        &dir,
        "claude-code",
        "UserPromptSubmit",
        json!({"session_id": "live-1", "prompt": "hello live"}),
    )
    .status
    .success());
    let live_before = messages(home.path(), &dir, "live-1");
    assert_eq!(live_before.len(), 1);
    std::fs::write(
        project.join("live-1.jsonl"),
        [
            line("live-1", "user", "u1", "2026-01-01T00:00:00Z", "hello live"),
            line("live-1", "assistant", "a1", "2026-01-01T00:00:01Z", "hi"),
        ]
        .join("\n")
            + "\n",
    )
    .unwrap();
    // (c) A session missing from the store imports normally.
    let new_path = project.join("new-1.jsonl");
    std::fs::write(
        &new_path,
        line("new-1", "user", "n1", "2026-01-01T00:00:00Z", "fresh") + "\n",
    )
    .unwrap();

    let first = backfill(home.path(), &dir, &[]);
    let claude = harness_report(&first, "claude-code");
    assert_eq!(
        (
            claude["new"].as_u64(),
            claude["duplicate"].as_u64(),
            claude["skipped"].as_u64(),
            claude["live_sessions_skipped"].as_u64()
        ),
        (Some(1), Some(0), Some(0), Some(1)),
        "{first}"
    );
    assert_eq!(messages(home.path(), &dir, "live-1"), live_before);
    assert_eq!(messages(home.path(), &dir, "new-1").len(), 1);

    // --rescan still honors the rule.
    let rescan = backfill(home.path(), &dir, &["--rescan"]);
    let claude = harness_report(&rescan, "claude-code");
    assert_eq!(
        (claude["new"].as_u64(), claude["duplicate"].as_u64(), claude["live_sessions_skipped"].as_u64()),
        (Some(0), Some(1), Some(1)),
        "{rescan}"
    );
    assert_eq!(messages(home.path(), &dir, "live-1"), live_before);

    // (b) A session that was only backfilled keeps resuming when it grows.
    let mut grown = std::fs::read_to_string(&new_path).unwrap();
    grown += &(line("new-1", "user", "n2", "2026-01-01T00:00:05Z", "more") + "\n");
    std::fs::write(&new_path, grown).unwrap();
    let again = backfill(home.path(), &dir, &[]);
    let claude = harness_report(&again, "claude-code");
    // The live session's transcript is unchanged, so it is not revisited or recounted.
    assert_eq!((claude["new"].as_u64(), claude["live_sessions_skipped"].as_u64()), (Some(1), Some(0)), "{again}");
    assert_eq!(messages(home.path(), &dir, "new-1").len(), 2);
}

/// Backfill tables are additive: they appear lazily, never change
/// `user_version`, and a store without them backfills and reads fine.
#[test]
fn backfill_tables_never_bump_the_schema_version() {
    let home = tempfile::TempDir::new().unwrap();
    let dir = home.path().join("history");
    assert!(history_cli(home.path(), &dir, &["init"]).0);
    let project = home.path().join(".claude/projects/-work");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
        project.join("sess-1.jsonl"),
        claude_line("user", "u1", "2026-01-01T00:00:00Z", json!("albuquerque")) + "\n",
    )
    .unwrap();

    let db_path = dir.join("history.sqlite3");
    let raw = || rusqlite::Connection::open(&db_path).unwrap();
    let version = || -> i64 {
        raw()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap()
    };
    let backfill_tables = || -> i64 {
        raw()
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name LIKE 'backfill_%'",
                [],
                |r| r.get(0),
            )
            .unwrap()
    };
    assert_eq!((version(), backfill_tables()), (2, 0));
    assert_eq!(i64::from(SCHEMA_VERSION), 2);

    // First backfill on a v2 store without the tables creates them lazily.
    let first = backfill(home.path(), &dir, &[]);
    assert_eq!(harness_report(&first, "claude-code")["new"], 1, "{first}");
    assert_eq!((version(), backfill_tables()), (2, 1));

    // Old-style reads work on a store that has the extra tables.
    let (ok, sessions) = history_cli(home.path(), &dir, &["sessions"]);
    assert!(ok, "{sessions}");
    assert_eq!(sessions["items"].as_array().unwrap().len(), 1);
    assert_eq!(messages(home.path(), &dir, "sess-1").len(), 1);
    let key = json!([{"origin": "local", "harness": "claude-code", "session_id": "sess-1"}]);
    let (ok, hits) = search_cli(home.path(), &dir, "albuquerque", &key);
    assert!(ok, "{hits}");
    assert_eq!(hits["items"].as_array().unwrap().len(), 1);
    let (ok, status) = history_cli(home.path(), &dir, &["status"]);
    assert!(ok, "{status}");
    assert_eq!(status["schema_version"], 2);

    // Dropping the tables (an older binary never had them) is harmless.
    raw()
        .execute_batch("DROP TABLE backfill_files; DROP TABLE IF EXISTS backfill_cursors;")
        .unwrap();
    let again = backfill(home.path(), &dir, &["--rescan"]);
    let claude = harness_report(&again, "claude-code");
    assert_eq!((claude["new"].as_u64(), claude["duplicate"].as_u64()), (Some(0), Some(1)), "{again}");
    assert_eq!(version(), 2);
}
