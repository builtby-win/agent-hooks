//! Black-box proof that ~/.agent-hooks/hooks.json answers as a decider: runs the
//! REAL `agent-hooks hook` binary with an isolated HOME (user tier), an
//! isolated cwd (project tier), and an optional in-process daemon, asserting
//! the rendered harness-dialect stdout.
//!
//! Matrix:
//!   1. daemonless Blue auto-answer -> pi allow (static decider, no daemon)
//!   2. daemon decider deny beats local allow (hold + merge)
use agent_hooks_client::AgentHooksClient;
use agent_hooks_core::daemon::AgentHooksDaemon;
use agent_hooks_protocol::PromptDecision;
use futures::StreamExt;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

const ASK_PAYLOAD: &str = r#"{"session_id": "e2e-1", "tool_name": "ask", "tool_input": {"questions": [{"id": "favorite_color", "question": "What is your favorite color?", "options": [{"label": "Red"}, {"label": "Blue"}]}]}}"#;
const BASH_PAYLOAD: &str =
    r#"{"session_id": "e2e-1", "tool_name": "Bash", "tool_input": {"command": "ls"}}"#;

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_agent-hooks"))
}

fn write(path: &Path, content: &str) {
    std::fs::create_dir_all(path.parent().expect("fixture path has a parent"))
        .expect("create fixture dir");
    std::fs::write(path, content).expect("write fixture file");
}

/// Run the real shim: `agent-hooks hook omp --event tool_call` with stdin.
/// HOME/cwd/socket fully isolated; returns parsed stdout JSON.
/// Blocking child wait: callers in async tests must wrap with spawn_blocking
/// so the runtime hosting the in-process daemon keeps moving.
fn run_hook(home: &Path, cwd: &Path, socket: &Path, payload: &str) -> serde_json::Value {
    let mut child = Command::new(bin())
        .args(["hook", "omp", "--event", "tool_call", "--socket"])
        .arg(socket)
        .env("HOME", home)
        .env("AGENT_HOOKS_NO_LOCAL", "")
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn shim");
    use std::io::Write;
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(payload.as_bytes())
        .expect("write payload");
    let out = child.wait_with_output().expect("shim exits");
    assert!(out.status.success(), "shim always exits 0");
    serde_json::from_slice(&out.stdout).expect("stdout is JSON")
}

fn blue_config() -> String {
    r#"{"version": 1, "hooks": {"PreToolUse": [
      {"when": {"tool": "ask"}, "respond": {"allow": true,
        "answers": [{"question_id": "favorite_color", "selected_options": ["Blue"]}]}},
      {"when": {"tool": "Bash"}, "respond": {"allow": true}}
    ]}}"#
        .to_string()
}

#[test]
fn daemonless_blue_auto_answer_renders_allow() {
    let home = tempfile::TempDir::new().unwrap();
    let cwd = tempfile::TempDir::new().unwrap();
    write(&home.path().join(".agent-hooks/hooks.json"), &blue_config());
    // Socket that nothing listens on: local decision must stand alone.
    let dead = home.path().join("no-daemon.sock");

    let v = run_hook(home.path(), cwd.path(), &dead, ASK_PAYLOAD);
    // The pick rides the verdict so the omp ask bridge resolves the `ask` call
    // in-band (asset verdictAnswers reads one string[] per question) instead of
    // re-asking natively — the legacy b2v-pi.ts pi-ask-v1 reply carried the
    // answers the same way (parsePiAskAnswers). `reply` feeds allow_always
    // memory (b2v-pi.ts gatePiToolCall).
    assert_eq!(
        v,
        serde_json::json!({
            "decision": "allow",
            "reply": "allow",
            "answers": [{"question_id": "favorite_color", "selected_options": ["Blue"]}]
        })
    );
}

#[test]
fn project_deny_beats_user_allow() {
    let home = tempfile::TempDir::new().unwrap();
    let cwd = tempfile::TempDir::new().unwrap();
    write(&home.path().join(".agent-hooks/hooks.json"), &blue_config());
    std::fs::create_dir(cwd.path().join(".agent-hooks")).unwrap();
    write(
        &cwd.path().join(".agent-hooks/hooks.json"),
        r#"{"version": 1, "hooks": {"PreToolUse": [
          {"when": {"tool": "Bash"}, "respond": {"allow": false, "custom_input": "project policy"}}
        ]}}"#,
    );
    let dead = home.path().join("no-daemon.sock");

    let v = run_hook(home.path(), cwd.path(), &dead, BASH_PAYLOAD);
    assert_eq!(
        v,
        serde_json::json!({"decision": "deny", "reason": "project policy"})
    );
}

/// The project tier follows the session's project (the payload's cwd), not the
/// shim's own cwd: OpenCode's shared service spawns the shim from wherever it
/// was started.
#[test]
fn project_tier_follows_payload_cwd_not_process_cwd() {
    let home = tempfile::TempDir::new().unwrap();
    let elsewhere = tempfile::TempDir::new().unwrap();
    let project = tempfile::TempDir::new().unwrap();
    std::fs::create_dir(project.path().join(".agent-hooks")).unwrap();
    write(
        &project.path().join(".agent-hooks/hooks.json"),
        r#"{"version": 1, "hooks": {"PreToolUse": [
          {"when": {"tool": "Bash"}, "respond": {"allow": false, "custom_input": "project policy"}}
        ]}}"#,
    );
    let dead = home.path().join("no-daemon.sock");
    let hook = |payload_cwd: &Path| {
        let payload = serde_json::json!({
            "sessionID": "ses-cwd", "toolName": "Bash", "args": {"command": "ls"},
            "cwd": payload_cwd,
        });
        run_hook_full(
            home.path(),
            elsewhere.path(),
            "opencode",
            "tool.execute.before",
            &dead,
            payload.to_string().as_bytes(),
        )
    };

    let in_project = hook(project.path());
    assert!(
        String::from_utf8_lossy(&in_project.stdout).contains("project policy"),
        "project policy must apply: {:?}",
        String::from_utf8_lossy(&in_project.stdout),
    );
    let outside = hook(elsewhere.path());
    assert!(
        !String::from_utf8_lossy(&outside.stdout).contains("project policy"),
        "another project must not inherit it"
    );
}

/// Monorepo: user tier, repo root and nested package hooks for the same event
/// all run (they accumulate); none replaces another.
#[test]
fn user_root_and_nested_hooks_for_one_event_all_run() {
    let home = tempfile::TempDir::new().unwrap();
    let repo = tempfile::TempDir::new().unwrap();
    std::fs::create_dir(repo.path().join(".git")).unwrap();
    let pkg = repo.path().join("packages/web");
    let cwd = pkg.join("src");
    std::fs::create_dir_all(&cwd).unwrap();
    let marks = tempfile::TempDir::new().unwrap();
    let hook_cfg = |name: &str| {
        format!(
            r#"{{"version": 1, "hooks": {{"PreToolUse": [{{"command": "touch {}"}}]}}}}"#,
            marks.path().join(name).display()
        )
    };
    write(&home.path().join(".agent-hooks/hooks.json"), &hook_cfg("user"));
    write(&repo.path().join(".agent-hooks/hooks.json"), &hook_cfg("root"));
    write(&pkg.join(".agent-hooks/hooks.json"), &hook_cfg("pkg"));
    let dead = home.path().join("no-daemon.sock");

    run_hook(home.path(), &cwd, &dead, BASH_PAYLOAD);
    for name in ["user", "root", "pkg"] {
        assert!(marks.path().join(name).exists(), "{name} hook must run");
    }
}

/// An agent working in `repo/src/deep` still gets `repo/.agent-hooks/hooks.json`.
#[test]
fn project_tier_found_from_subdirectory() {
    let home = tempfile::TempDir::new().unwrap();
    let repo = tempfile::TempDir::new().unwrap();
    std::fs::create_dir(repo.path().join(".git")).unwrap();
    write(
        &repo.path().join(".agent-hooks/hooks.json"),
        r#"{"version": 1, "hooks": {"PreToolUse": [
          {"when": {"tool": "Bash"}, "respond": {"allow": false, "custom_input": "repo policy"}}
        ]}}"#,
    );
    let deep = repo.path().join("src/deep");
    std::fs::create_dir_all(&deep).unwrap();
    let dead = home.path().join("no-daemon.sock");

    let v = run_hook(home.path(), &deep, &dead, BASH_PAYLOAD);
    assert_eq!(
        v,
        serde_json::json!({"decision": "deny", "reason": "repo policy"})
    );
}

/// The pre-rename `~/.agent-hooks.json` is no longer read.
#[test]
fn legacy_home_file_is_ignored() {
    let home = tempfile::TempDir::new().unwrap();
    let cwd = tempfile::TempDir::new().unwrap();
    write(&home.path().join(".agent-hooks.json"), &blue_config());
    let dead = home.path().join("no-daemon.sock");

    let v = run_hook(home.path(), cwd.path(), &dead, BASH_PAYLOAD);
    assert_eq!(v, serde_json::json!({}));
}

#[test]
fn amp_tool_call_obeys_pre_tool_command_policy() {
    let home = tempfile::TempDir::new().unwrap();
    let cwd = tempfile::TempDir::new().unwrap();
    write(
        &home.path().join(".agent-hooks/hooks.json"),
        r#"{"version":1,"hooks":{"PreToolUse":[
            {"when":{"prompt_matches":"policy-marker"},"respond":{"allow":false,"custom_input":"blocked command"}}
        ]}}"#,
    );
    let mut child = Command::new(bin())
        .args(["hook", "amp", "--event", "tool.call", "--socket"])
        .arg(home.path().join("no-daemon.sock"))
        .env("HOME", home.path())
        .env("AGENT_HOOKS_NO_LOCAL", "")
        .current_dir(cwd.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    use std::io::Write;
    child.stdin.take().unwrap().write_all(
        br#"{"thread":{"id":"amp-policy"},"toolUseID":"call-1","tool":"shell_command","input":{"command":"printf policy-marker"}}"#,
    ).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    let verdict: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(verdict, serde_json::json!({"decision":"deny","reason":"blocked command"}));
}

/// Config whose one PreToolUse entry runs `command` (unix shell).
#[cfg(unix)]
fn command_config(tool: &str, command: &str) -> String {
    serde_json::json!({"version": 1, "hooks": {"PreToolUse": [
        {"when": {"tool": tool}, "command": command}
    ]}})
    .to_string()
}

#[cfg(unix)]
#[test]
fn command_stdout_canonical_decision_answers_like_respond() {
    let home = tempfile::TempDir::new().unwrap();
    let cwd = tempfile::TempDir::new().unwrap();
    let verdict = PromptDecision {
        answers: vec![agent_hooks_protocol::PromptAnswer {
            question_id: "favorite_color".to_string(),
            selected_options: vec!["Blue".to_string()],
            custom_input: None,
        }],
        ..PromptDecision::allow()
    };
    write(
        &home.path().join(".agent-hooks/hooks.json"),
        &command_config(
            "ask",
            &format!("printf '%s' '{}'", serde_json::to_string(&verdict).unwrap()),
        ),
    );
    let dead = home.path().join("no-daemon.sock");

    let v = run_hook(home.path(), cwd.path(), &dead, ASK_PAYLOAD);
    assert_eq!(
        v,
        serde_json::json!({
            "decision": "allow",
            "reply": "allow",
            "answers": [{"question_id": "favorite_color", "selected_options": ["Blue"]}]
        })
    );
}

#[cfg(unix)]
#[test]
fn command_stdout_invalid_answer_fails_closed() {
    let home = tempfile::TempDir::new().unwrap();
    let cwd = tempfile::TempDir::new().unwrap();
    write(
        &home.path().join(".agent-hooks/hooks.json"),
        &command_config(
            "ask",
            r#"printf '%s' '{"allow":true,"answers":[{"question_id":"favorite_color","selected_options":["Green"]}]}'"#,
        ),
    );
    let dead = home.path().join("no-daemon.sock");

    let v = run_hook(home.path(), cwd.path(), &dead, ASK_PAYLOAD);
    assert_eq!(v["decision"], "deny", "{v}");
    assert!(
        v["reason"]
            .as_str()
            .is_some_and(|r| r.contains("invalid local hook response")),
        "{v}"
    );
}

#[cfg(unix)]
#[test]
fn command_exit_code_beats_stdout_allow() {
    let home = tempfile::TempDir::new().unwrap();
    let cwd = tempfile::TempDir::new().unwrap();
    write(
        &home.path().join(".agent-hooks/hooks.json"),
        &command_config(
            "Bash",
            r#"printf '%s' '{"allow":true}'; echo vetoed >&2; exit 2"#,
        ),
    );
    let dead = home.path().join("no-daemon.sock");

    let v = run_hook(home.path(), cwd.path(), &dead, BASH_PAYLOAD);
    assert_eq!(
        v,
        serde_json::json!({"decision": "deny", "reason": "vetoed"})
    );
}

#[tokio::test]
async fn daemon_deny_beats_local_allow() {
    let home = tempfile::TempDir::new().unwrap();
    let cwd = tempfile::TempDir::new().unwrap();
    write(&home.path().join(".agent-hooks/hooks.json"), &blue_config());
    let socket = home.path().join("e2e.sock");

    let daemon = AgentHooksDaemon::new();
    let sock = socket.clone();
    let daemon_handle = tokio::spawn(async move {
        let _ = daemon.run(Some(sock)).await;
    });
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Decider denies the Bash call; local JSON allows it.
    let socket_str = socket.to_str().unwrap().to_string();
    let mut verdicts = HashMap::new();
    verdicts.insert(
        "e2e-1".to_string(),
        PromptDecision::deny(Some("decider veto".to_string())),
    );
    let decider = tokio::spawn(async move {
        let c = AgentHooksClient::connect_to(socket_str).await.unwrap();
        // Mid-flight PreToolUse: nothing was waiting until we claimed it, so
        // it must be named explicitly. `decide: true` would not hold it.
        let mut stream = c.subscribe_decider_for(agent_hooks_protocol::DecideScope {
            event_types: vec!["PreToolUse".to_string()],
            ..Default::default()
        });
        let event = tokio::time::timeout(Duration::from_secs(10), stream.next())
            .await
            .expect("held event")
            .expect("item");
        // The daemon says it is holding and which id answers it. No
        // capability table, no id convention, no lifecycle match.
        let hold = event.hold.as_ref().expect("held event carries its hold");
        assert!(event.answerable, "held event is marked answerable");
        assert!(!hold.interactive, "PreToolUse is a claimed mid-flight hold");
        assert!(hold.verdicts.iter().any(|v| v == "deny"));
        let pid = hold.prompt_id.clone();
        c.respond_prompt(&pid, verdicts.remove("e2e-1").unwrap())
            .await
            .unwrap();
    });
    tokio::time::sleep(Duration::from_millis(500)).await;

    let v = tokio::task::spawn_blocking({
        let home = home.path().to_path_buf();
        let cwd = cwd.path().to_path_buf();
        let socket = socket.clone();
        move || run_hook(&home, &cwd, &socket, BASH_PAYLOAD)
    })
    .await
    .expect("shim finishes");
    assert_eq!(
        v,
        serde_json::json!({"decision": "deny", "reason": "decider veto"})
    );

    decider.await.unwrap();
    daemon_handle.abort();
}

#[tokio::test]
async fn local_ask_survives_daemon_allow() {
    let home = tempfile::TempDir::new().unwrap();
    let cwd = tempfile::TempDir::new().unwrap();
    write(
        &home.path().join(".agent-hooks/hooks.json"),
        r#"{"version": 1, "hooks": {"PreToolUse": [
          {"when": {"tool": "Bash"}, "respond": {"allow": false, "ask": true, "custom_input": "sure about this?"}}
        ]}}"#,
    );
    let socket = home.path().join("e2e.sock");

    let daemon = AgentHooksDaemon::new();
    let sock = socket.clone();
    let daemon_handle = tokio::spawn(async move {
        let _ = daemon.run(Some(sock)).await;
    });
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Daemon decider allows; local ask must survive the merge (deny > ask >
    // allow) and pi renders it via the deny fallback (no native ask dialect).
    let socket_str = socket.to_str().unwrap().to_string();
    let decider = tokio::spawn(async move {
        let c = AgentHooksClient::connect_to(socket_str).await.unwrap();
        // Mid-flight PreToolUse: nothing was waiting until we claimed it, so
        // it must be named explicitly. `decide: true` would not hold it.
        let mut stream = c.subscribe_decider_for(agent_hooks_protocol::DecideScope {
            event_types: vec!["PreToolUse".to_string()],
            ..Default::default()
        });
        let event = tokio::time::timeout(Duration::from_secs(10), stream.next())
            .await
            .expect("held event")
            .expect("item");
        c.respond_prompt(&event.event_id, PromptDecision::allow())
            .await
            .unwrap();
    });
    tokio::time::sleep(Duration::from_millis(500)).await;

    let v = tokio::task::spawn_blocking({
        let home = home.path().to_path_buf();
        let cwd = cwd.path().to_path_buf();
        let socket = socket.clone();
        move || run_hook(&home, &cwd, &socket, BASH_PAYLOAD)
    })
    .await
    .expect("shim finishes");
    assert_eq!(
        v,
        serde_json::json!({"decision": "deny", "reason": "sure about this?"})
    );

    decider.await.unwrap();
    daemon_handle.abort();
}

// ---------------------------------------------------------------------------
// Retry/failure-case regressions: malformed payloads, unknown events,
// dead-socket timing, and exit2 discipline. Each drives the real `hook`
// binary (no daemon, no network) and fails on the bug it guards (panic on
// garbage, nonzero exit on unknown events, hangs on dead sockets, exit 0 on
// exit2-deny).
// ---------------------------------------------------------------------------

/// Full-fidelity shim spawn: arbitrary harness/event, raw stdin bytes, and
/// captured exit code + stdout + stderr. (The `run_hook` helper above drops
/// stderr and asserts success, which cannot express failure-case contracts.)
struct HookOutcome {
    code: i32,
    stdout: Vec<u8>,
    stderr: String,
}

fn run_hook_full(
    home: &Path,
    cwd: &Path,
    harness: &str,
    event: &str,
    socket: &Path,
    stdin_bytes: &[u8],
) -> HookOutcome {
    use std::io::Write;
    let mut child = Command::new(bin())
        .args(["hook", harness, "--event", event, "--socket"])
        .arg(socket)
        .env("HOME", home)
        .env("AGENT_HOOKS_NO_LOCAL", "")
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn shim");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(stdin_bytes)
        .expect("write payload");
    let out = child.wait_with_output().expect("shim exits");
    HookOutcome {
        code: out.status.code().unwrap_or(-1),
        stdout: out.stdout,
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

fn stdout_json(outcome: &HookOutcome) -> serde_json::Value {
    serde_json::from_slice(&outcome.stdout)
        .unwrap_or_else(|e| panic!("stdout is JSON ({e}): {:?}", outcome.stdout))
}

#[test]
fn malformed_empty_stdin_fails_open() {
    let home = tempfile::TempDir::new().unwrap();
    let cwd = tempfile::TempDir::new().unwrap();
    let dead = home.path().join("no-daemon.sock");

    let o = run_hook_full(home.path(), cwd.path(), "omp", "tool_call", &dead, b"");
    assert_eq!(
        o.code, 0,
        "empty payload must fail open, not crash: {}",
        o.stderr
    );
    // Empty input falls back to an ephemeral session, so it still parses to
    // a real PreToolUse gate event; nobody decided it, so omp gets the
    // no-verdict `{}` and proceeds under its own handling.
    assert_eq!(stdout_json(&o), serde_json::json!({}));
}

#[test]
fn malformed_bom_prefix_fails_open() {
    let home = tempfile::TempDir::new().unwrap();
    let cwd = tempfile::TempDir::new().unwrap();
    let dead = home.path().join("no-daemon.sock");

    let payload = format!("\u{FEFF}{BASH_PAYLOAD}");
    let o = run_hook_full(
        home.path(),
        cwd.path(),
        "omp",
        "tool_call",
        &dead,
        payload.as_bytes(),
    );
    assert_eq!(
        o.code, 0,
        "BOM prefix must not crash the shim: {}",
        o.stderr
    );
    assert_eq!(stdout_json(&o), serde_json::json!({}));
}

#[test]
fn malformed_double_encoded_payload_fails_open() {
    let home = tempfile::TempDir::new().unwrap();
    let cwd = tempfile::TempDir::new().unwrap();
    let dead = home.path().join("no-daemon.sock");

    // A JSON string containing JSON (one layer unwrapped by the shim).
    let payload = serde_json::to_string(BASH_PAYLOAD).expect("double-encode");
    let o = run_hook_full(
        home.path(),
        cwd.path(),
        "omp",
        "tool_call",
        &dead,
        payload.as_bytes(),
    );
    assert_eq!(
        o.code, 0,
        "double-encoded payload must not crash: {}",
        o.stderr
    );
    assert_eq!(stdout_json(&o), serde_json::json!({}));
}

#[test]
fn malformed_invalid_utf8_fails_open() {
    let home = tempfile::TempDir::new().unwrap();
    let cwd = tempfile::TempDir::new().unwrap();
    let dead = home.path().join("no-daemon.sock");

    // Stdin is decoded lossily (never a panic); the replacement chars make
    // the payload unparseable, so omp renders the observe/empty allow dialect.
    let raw = b"\xff\xfe\x00{\"session_id\": \"e2e-1\"";
    let o = run_hook_full(home.path(), cwd.path(), "omp", "tool_call", &dead, raw);
    assert_eq!(
        o.code, 0,
        "invalid UTF-8 must fail open, not panic: {}",
        o.stderr
    );
    assert_eq!(stdout_json(&o), serde_json::json!({}));
}

#[test]
fn unknown_event_matches_no_local_hooks() {
    let home = tempfile::TempDir::new().unwrap();
    let cwd = tempfile::TempDir::new().unwrap();
    let sentinel = cwd.path().join("hook-ran.sentinel");
    // Command-only entry (no `respond`, so any match spawns the shell and
    // leaves the sentinel). Unknown events must never evaluate local hooks.
    // `echo a > "path"` runs under both `sh -c` and `cmd /C`.
    let cfg = serde_json::json!({"version": 1, "hooks": {"PreToolUse": [
        {"command": format!("echo sentinel > \"{}\"", sentinel.display()), "timeout": 5}
    ]}})
    .to_string();
    write(&home.path().join(".agent-hooks/hooks.json"), &cfg);
    let dead = home.path().join("no-daemon.sock");

    let o = run_hook_full(
        home.path(),
        cwd.path(),
        "omp",
        "NoSuchEvent",
        &dead,
        BASH_PAYLOAD.as_bytes(),
    );
    assert_eq!(o.code, 0, "unknown events must exit 0: {}", o.stderr);
    assert_eq!(
        stdout_json(&o),
        serde_json::json!({}),
        "unknown events render the observe/empty dialect"
    );
    assert!(
        !sentinel.exists(),
        "unknown event must not run local hook scripts"
    );
}

#[test]
fn dead_socket_gate_event_fails_open_fast() {
    let home = tempfile::TempDir::new().unwrap();
    let cwd = tempfile::TempDir::new().unwrap();
    // Only the `ask` tool is configured: Bash matches nothing, so with the
    // daemon dead there is no verdict from any source.
    write(
        &home.path().join(".agent-hooks/hooks.json"),
        r#"{"version": 1, "hooks": {"PreToolUse": [{"when": {"tool": "ask"}, "respond": {"allow": true}}]}}"#,
    );
    // Nothing listens here: connect-refused exercises the gate retry budget
    // (bounded attempts with backoff) with no daemon running.
    let dead = home.path().join("no-daemon.sock");

    let start = std::time::Instant::now();
    let o = run_hook_full(
        home.path(),
        cwd.path(),
        "omp",
        "tool_call",
        &dead,
        BASH_PAYLOAD.as_bytes(),
    );
    let elapsed = start.elapsed();
    // Fail open = no verdict (`{}`), never an explicit allow.
    assert_eq!(o.code, 0, "dead daemon must fail open: {}", o.stderr);
    assert_eq!(stdout_json(&o), serde_json::json!({}));
    // Generous seconds-scale bound (retries take ~100ms): far below the 330s
    // gate hold budget, so a regression to unbounded waiting fails here.
    assert!(
        elapsed < Duration::from_secs(5),
        "dead-socket gate took {elapsed:?}"
    );
}

#[test]
fn dead_socket_observe_event_fails_open_fast() {
    let home = tempfile::TempDir::new().unwrap();
    let cwd = tempfile::TempDir::new().unwrap();
    write(&home.path().join(".agent-hooks/hooks.json"), &blue_config());
    // Observe events get exactly one attempt (fail fast); no daemon running.
    let dead = home.path().join("no-daemon.sock");

    let start = std::time::Instant::now();
    let o = run_hook_full(
        home.path(),
        cwd.path(),
        "omp",
        "tool_result",
        &dead,
        BASH_PAYLOAD.as_bytes(),
    );
    let elapsed = start.elapsed();
    assert_eq!(o.code, 0, "dead daemon must fail open: {}", o.stderr);
    assert_eq!(stdout_json(&o), serde_json::json!({}));
    assert!(
        elapsed < Duration::from_secs(5),
        "dead-socket observe took {elapsed:?}"
    );
}

/// No builtin adapter ships `response.mode: exit2` (all builtins are `json`
/// mode), so these tests stage a project-tier fixture manifest `exit2t` that
/// inherits everything from `pi` and flips only the response mode. The real
/// `hook` binary resolves it from `<cwd>/.agent-hooks/adapters` — this still
/// exercises the full shim output contract, not a `format_response` unit.
fn write_exit2t_manifest(cwd: &Path) {
    let dir = cwd.join(".agent-hooks/adapters/exit2t");
    std::fs::create_dir_all(&dir).expect("fixture adapter dir");
    write(
        &dir.join("adapter.json"),
        r#"{"id": "exit2t", "display_name": "Exit2 E2E fixture", "extends": "pi",
            "default_version": "v1",
            "versions": {"v1": {"response": {"mode": "exit2"}}}}"#,
    );
}

#[test]
fn exit2_deny_exits_2_with_reason_on_stderr() {
    let home = tempfile::TempDir::new().unwrap();
    let cwd = tempfile::TempDir::new().unwrap();
    write_exit2t_manifest(cwd.path());
    write(
        &home.path().join(".agent-hooks/hooks.json"),
        r#"{"version": 1, "hooks": {"PreToolUse": [
          {"when": {"tool": "Bash"}, "respond": {"allow": false, "custom_input": "exit2 deny reason"}}
        ]}}"#,
    );
    let dead = home.path().join("no-daemon.sock");

    let o = run_hook_full(
        home.path(),
        cwd.path(),
        "exit2t",
        "tool_call",
        &dead,
        BASH_PAYLOAD.as_bytes(),
    );
    assert_eq!(
        o.code, 2,
        "exit2 deny must exit 2, got {}: {}",
        o.code, o.stderr
    );
    assert!(
        o.stderr.contains("exit2 deny reason"),
        "deny reason on stderr: {:?}",
        o.stderr
    );
    assert!(
        String::from_utf8_lossy(&o.stdout).trim().is_empty(),
        "exit2 deny writes no stdout: {:?}",
        o.stdout
    );
}

#[test]
fn exit2_ask_exits_2_with_question_on_stderr() {
    let home = tempfile::TempDir::new().unwrap();
    let cwd = tempfile::TempDir::new().unwrap();
    write_exit2t_manifest(cwd.path());
    write(
        &home.path().join(".agent-hooks/hooks.json"),
        r#"{"version": 1, "hooks": {"PreToolUse": [
          {"when": {"tool": "Bash"}, "respond": {"allow": false, "ask": true, "custom_input": "are you quite sure?"}}
        ]}}"#,
    );
    let dead = home.path().join("no-daemon.sock");

    let o = run_hook_full(
        home.path(),
        cwd.path(),
        "exit2t",
        "tool_call",
        &dead,
        BASH_PAYLOAD.as_bytes(),
    );
    assert_eq!(
        o.code, 2,
        "exit2 ask must exit 2, got {}: {}",
        o.code, o.stderr
    );
    assert!(
        o.stderr.contains("are you quite sure?"),
        "ask question on stderr: {:?}",
        o.stderr
    );
}

#[test]
fn exit2_allow_exits_0() {
    let home = tempfile::TempDir::new().unwrap();
    let cwd = tempfile::TempDir::new().unwrap();
    write_exit2t_manifest(cwd.path());
    write(&home.path().join(".agent-hooks/hooks.json"), &blue_config());
    let dead = home.path().join("no-daemon.sock");

    let o = run_hook_full(
        home.path(),
        cwd.path(),
        "exit2t",
        "tool_call",
        &dead,
        BASH_PAYLOAD.as_bytes(),
    );
    // The plain canned allow is a real decision: exit 0, rendered as an allow
    // in the pi dialect (never exit 2).
    assert_eq!(o.code, 0, "exit2 allow must exit 0: {}", o.stderr);
    assert_eq!(stdout_json(&o)["decision"], "allow");
}

// ---------------------------------------------------------------------------
// Live parity regressions: shim → daemon → decider → shim, real binary.
// ---------------------------------------------------------------------------

/// Start a daemon, claim interactive events with `decide: true` (what B2V
/// does), answer the first held event with `verdict`, and return the shim's
/// raw stdout for `hook <harness> --event <event>` fed `payload`.
async fn held_roundtrip(
    harness: &'static str,
    event: &'static str,
    payload: &'static str,
    verdict: PromptDecision,
) -> (String, agent_hooks_protocol::AgentEvent) {
    let home = tempfile::TempDir::new().unwrap();
    let cwd = tempfile::TempDir::new().unwrap();
    let socket = home.path().join("e2e.sock");
    let daemon = AgentHooksDaemon::new();
    let sock = socket.clone();
    let daemon_handle = tokio::spawn(async move {
        let _ = daemon.run(Some(sock)).await;
    });
    tokio::time::sleep(Duration::from_millis(150)).await;

    let socket_str = socket.to_str().unwrap().to_string();
    let decider = tokio::spawn(async move {
        let c = AgentHooksClient::connect_to(socket_str).await.unwrap();
        let mut stream = c.subscribe_decider();
        let held = tokio::time::timeout(Duration::from_secs(10), stream.next())
            .await
            .expect("interactive event is held for the decider")
            .expect("item");
        let hold = held.hold.as_ref().expect("held event carries its hold");
        assert!(hold.interactive, "{event} is a natural (interactive) hold");
        c.respond_prompt(&hold.prompt_id.clone(), verdict)
            .await
            .unwrap();
        held
    });
    tokio::time::sleep(Duration::from_millis(500)).await;

    let out = tokio::task::spawn_blocking({
        let home = home.path().to_path_buf();
        let cwd = cwd.path().to_path_buf();
        let socket = socket.clone();
        move || run_hook_full(&home, &cwd, harness, event, &socket, payload.as_bytes())
    })
    .await
    .expect("shim finishes");
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
    let held = decider.await.unwrap();
    daemon_handle.abort();
    (
        String::from_utf8(out.stdout).unwrap().trim().to_string(),
        held,
    )
}

/// Live parity codex step 4: request_user_input is a held question prompt and
/// the answer reaches the model as the block reason (Codex has no in-band
/// answer channel; `updatedInput` only re-parses the tool's own args).
#[tokio::test]
async fn codex_request_user_input_answer_roundtrip_rides_block_reason() {
    const PAYLOAD: &str = r#"{"session_id":"019f4a34-0000-7000-8000-00000000c0de","transcript_path":"/tmp/p/.parity-codex-rollout.jsonl","cwd":"/tmp/p","hook_event_name":"PreToolUse","model":"gpt-5-codex","permission_mode":"default","turn_id":"turn-parity-1","tool_name":"request_user_input","tool_use_id":"call_parity_q","tool_input":{"questions":[{"id":"db","header":"DB","question":"Which database?","options":[{"label":"Postgres","description":"Relational"},{"label":"SQLite","description":"Embedded"}]}]}}"#;
    let verdict = PromptDecision {
        allow: true,
        custom_input: None,
        answers: vec![agent_hooks_protocol::PromptAnswer {
            question_id: "db".into(),
            selected_options: vec!["SQLite".into()],
            custom_input: None,
        }],
        ask: false,
        allow_always: false,
        suggestion: None,
        defer: false,
        additional_context: None,
    };
    let (stdout, held) = held_roundtrip("codex", "PreToolUse", PAYLOAD, verdict).await;
    let prompt = held
        .prompt_request()
        .expect("question prompt reaches the decider");
    assert_eq!(prompt.kind, agent_hooks_protocol::PromptKind::Question);
    assert_eq!(prompt.tool_name.as_deref(), Some("request_user_input"));
    assert_eq!(prompt.questions[0].question, "Which database?");
    assert_eq!(
        stdout,
        r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"The user already answered this question outside Codex. Use these answers and continue; do not ask again.\n\n> Which database?\n\nSQLite"}}"#
    );
}

/// Legacy prompt.rs test_build_claude_permission_response_allow_always_uses_suggestions,
/// through the daemon: AllowAlways must survive mediator → verdict line → shim.
#[tokio::test]
async fn claude_allow_always_roundtrip_echoes_permission_suggestions() {
    const PAYLOAD: &str = r#"{"session_id":"c-sess","hook_event_name":"PermissionRequest","tool_name":"Bash","tool_input":{"command":"git status"},"permission_suggestions":[{"type":"tool","tool_name":"Bash","patterns":["git status"]}]}"#;
    let (stdout, _) = held_roundtrip(
        "claude",
        "PermissionRequest",
        PAYLOAD,
        PromptDecision::allow_always(),
    )
    .await;
    assert_eq!(
        stdout,
        r#"{"hookSpecificOutput":{"decision":{"behavior":"allow","updatedPermissions":[{"patterns":["git status"],"tool_name":"Bash","type":"tool"}]},"hookEventName":"PermissionRequest"}}"#
    );
}
