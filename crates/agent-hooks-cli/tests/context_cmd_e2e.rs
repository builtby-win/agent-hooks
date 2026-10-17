//! Black-box proof for `agent-hooks hook --context-cmd`: the REAL shim runs a
//! consumer context script, and its JSON-object stdout reaches a daemon
//! subscriber as `AgentEvent.extensions` (shim -> hook line `extensions` ->
//! daemon merge -> enricher -> subscriber). Every failure mode still delivers
//! the event, just without the consumer keys.
#![cfg(unix)]

use agent_hooks_client::AgentHooksClient;
use agent_hooks_core::daemon::AgentHooksDaemon;
use agent_hooks_protocol::{AgentEvent, RawHookInput};
use futures::StreamExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The exact payload B2V's `hook-context` prints (contract keys).
const B2V_CONTEXT: &str = r#"{"b2v.runtime_context":{"tty":"/dev/ttys001"},"b2v.disabled":false}"#;

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_agent-hooks"))
}

fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

struct Shim {
    stderr: String,
    elapsed: Duration,
}

/// `agent-hooks hook claude --event Stop --socket S --context-cmd C` with a
/// realistic Claude Stop payload on stdin.
fn run_shim(
    home: &Path,
    cwd: &Path,
    socket: &Path,
    context_cmd: &Path,
    session: &str,
    env: &[(&str, &str)],
) -> Shim {
    use std::io::Write;
    let start = Instant::now();
    let mut cmd = Command::new(bin());
    cmd.args(["hook", "claude", "--event", "Stop", "--socket"])
        .arg(socket)
        .arg("--context-cmd")
        .arg(context_cmd)
        .env("HOME", home)
        .env("AGENT_HOOKS_NO_LOCAL", "1")
        .env_remove("AGENT_HOOKS_CONTEXT_TIMEOUT_MS")
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().expect("spawn shim");
    let payload = format!(r#"{{"session_id": "{session}", "summary": "All tests pass"}}"#);
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.as_bytes())
        .unwrap();
    let out = child.wait_with_output().expect("shim exits");
    assert!(out.status.success(), "shim always exits 0");
    Shim {
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        elapsed: start.elapsed(),
    }
}

struct Harness {
    _home: tempfile::TempDir,
    home: PathBuf,
    cwd: tempfile::TempDir,
    socket: PathBuf,
    client: AgentHooksClient,
    daemon: tokio::task::JoinHandle<()>,
}

async fn start(daemon: AgentHooksDaemon) -> Harness {
    let home = tempfile::TempDir::new().unwrap();
    let cwd = tempfile::TempDir::new().unwrap();
    let socket = home.path().join("ctx.sock");
    let sock = socket.clone();
    let handle = tokio::spawn(async move {
        let _ = daemon.run(Some(sock)).await;
    });
    tokio::time::sleep(Duration::from_millis(150)).await;
    let client = AgentHooksClient::connect_to(socket.to_str().unwrap())
        .await
        .expect("daemon up");
    Harness {
        home: home.path().to_path_buf(),
        _home: home,
        cwd,
        socket,
        client,
        daemon: handle,
    }
}

/// Fire the shim and return the first subscriber event for `session`.
async fn deliver(
    h: &Harness,
    context_cmd: PathBuf,
    session: &str,
    env: Vec<(&'static str, &'static str)>,
) -> (AgentEvent, Shim) {
    let mut stream = Box::pin(h.client.subscribe(None));
    tokio::time::sleep(Duration::from_millis(250)).await;
    let shim = tokio::task::spawn_blocking({
        let (home, cwd, socket) = (h.home.clone(), h.cwd.path().to_path_buf(), h.socket.clone());
        let session = session.to_string();
        move || run_shim(&home, &cwd, &socket, &context_cmd, &session, &env)
    })
    .await
    .unwrap();
    let event = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let ev = stream.next().await.expect("stream open");
            if ev.session_id == session {
                return ev;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("event delivered; shim stderr: {}", shim.stderr));
    (event, shim)
}

#[tokio::test]
async fn context_cmd_extensions_reach_subscribers() {
    let h = start(AgentHooksDaemon::new()).await;
    let scripts = tempfile::TempDir::new().unwrap();
    // Also report argv + cwd so the contract's invocation shape is proven
    // end to end (cwd = hook cwd).
    let cmd = script(
        scripts.path(),
        "ctx.sh",
        &format!(
            r#"printf '%s\n' "$*" > "{argv}"; pwd -P > "{cwd}"; printf '%s' '{B2V_CONTEXT}'"#,
            argv = scripts.path().join("argv").display(),
            cwd = scripts.path().join("cwd").display(),
        ),
    );
    let (ev, _) = deliver(&h, cmd, "ctx-happy", vec![]).await;
    assert_eq!(
        ev.extensions.get("b2v.runtime_context"),
        Some(&serde_json::json!({"tty": "/dev/ttys001"}))
    );
    assert_eq!(
        ev.extensions.get("b2v.disabled"),
        Some(&serde_json::json!(false))
    );
    // Subscribers never see raw payloads, but extensions survive the strip.
    assert!(ev.raw_event.is_none());
    let argv = std::fs::read_to_string(scripts.path().join("argv")).unwrap();
    // The payload's real session id rides along for per-session consumer state.
    assert_eq!(
        argv.trim(),
        "hook-context --harness claude-code --event Stop --session ctx-happy"
    );
    let cwd = std::fs::read_to_string(scripts.path().join("cwd")).unwrap();
    assert_eq!(
        PathBuf::from(cwd.trim()),
        h.cwd.path().canonicalize().unwrap()
    );
    h.daemon.abort();
}

#[tokio::test]
async fn enricher_keys_merge_with_context_keys() {
    // The enricher sees the shim's keys and adds its own; neither side is
    // clobbered wholesale.
    let seen: Arc<Mutex<Option<bool>>> = Arc::new(Mutex::new(None));
    let seen_in = seen.clone();
    let daemon = AgentHooksDaemon::with_enricher(Arc::new(
        move |ev: &mut AgentEvent, _raw: &RawHookInput| {
            *seen_in.lock().unwrap() = Some(ev.extensions.contains_key("b2v.disabled"));
            ev.extensions.insert(
                "b2v.terminal_target".into(),
                serde_json::json!({"bundle_id": "x"}),
            );
        },
    ));
    let h = start(daemon).await;
    let scripts = tempfile::TempDir::new().unwrap();
    let cmd = script(
        scripts.path(),
        "ctx.sh",
        &format!("printf '%s' '{B2V_CONTEXT}'"),
    );
    let (ev, _) = deliver(&h, cmd, "ctx-merge", vec![]).await;
    assert_eq!(
        *seen.lock().unwrap(),
        Some(true),
        "enricher runs after the merge"
    );
    assert!(ev.extensions.contains_key("b2v.runtime_context"));
    assert!(ev.extensions.contains_key("b2v.disabled"));
    assert_eq!(
        ev.extensions.get("b2v.terminal_target"),
        Some(&serde_json::json!({"bundle_id": "x"}))
    );
    h.daemon.abort();
}

#[tokio::test]
async fn context_cmd_timeout_still_delivers_quickly() {
    let h = start(AgentHooksDaemon::new()).await;
    let scripts = tempfile::TempDir::new().unwrap();
    let cmd = script(
        scripts.path(),
        "slow.sh",
        &format!("sleep 5; printf '%s' '{B2V_CONTEXT}'"),
    );
    let (ev, shim) = deliver(
        &h,
        cmd,
        "ctx-timeout",
        vec![("AGENT_HOOKS_CONTEXT_TIMEOUT_MS", "200")],
    )
    .await;
    assert!(
        shim.elapsed < Duration::from_secs(2),
        "timeout must not stall the agent: {:?}",
        shim.elapsed
    );
    assert!(ev.extensions.is_empty(), "{:?}", ev.extensions);
    assert!(shim.stderr.contains("timed out"), "{}", shim.stderr);
    h.daemon.abort();
}

#[tokio::test]
async fn context_cmd_non_json_stdout_is_ignored() {
    let h = start(AgentHooksDaemon::new()).await;
    let scripts = tempfile::TempDir::new().unwrap();
    let cmd = script(scripts.path(), "junk.sh", "echo 'tty=/dev/ttys001'");
    let (ev, shim) = deliver(&h, cmd, "ctx-junk", vec![]).await;
    assert!(ev.extensions.is_empty());
    assert!(shim.stderr.contains("not JSON"), "{}", shim.stderr);
    h.daemon.abort();
}

#[tokio::test]
async fn context_cmd_non_object_json_is_ignored() {
    let h = start(AgentHooksDaemon::new()).await;
    let scripts = tempfile::TempDir::new().unwrap();
    let cmd = script(scripts.path(), "arr.sh", r#"echo '["b2v.disabled"]'"#);
    let (ev, shim) = deliver(&h, cmd, "ctx-array", vec![]).await;
    assert!(ev.extensions.is_empty());
    assert!(shim.stderr.contains("not an object"), "{}", shim.stderr);
    h.daemon.abort();
}

#[tokio::test]
async fn context_cmd_nonzero_exit_is_ignored() {
    let h = start(AgentHooksDaemon::new()).await;
    let scripts = tempfile::TempDir::new().unwrap();
    // Valid JSON is still discarded: a failing command's output is untrusted.
    let cmd = script(
        scripts.path(),
        "fail.sh",
        &format!("printf '%s' '{B2V_CONTEXT}'; exit 1"),
    );
    let (ev, shim) = deliver(&h, cmd, "ctx-exit", vec![]).await;
    assert!(ev.extensions.is_empty());
    assert!(shim.stderr.contains("exited"), "{}", shim.stderr);
    h.daemon.abort();
}

#[tokio::test]
async fn context_cmd_missing_binary_is_ignored() {
    let h = start(AgentHooksDaemon::new()).await;
    let missing = h.home.join("no-such-b2v");
    let (ev, shim) = deliver(&h, missing, "ctx-missing", vec![]).await;
    assert!(ev.extensions.is_empty());
    assert!(shim.stderr.contains("spawn failed"), "{}", shim.stderr);
    h.daemon.abort();
}

#[test]
fn context_cmd_skipped_when_daemon_is_down() {
    let home = tempfile::TempDir::new().unwrap();
    let cwd = tempfile::TempDir::new().unwrap();
    let marker = home.path().join("ran");
    let cmd = script(
        home.path(),
        "ctx.sh",
        &format!("touch '{}'; echo '{{}}'", marker.display()),
    );
    let shim = run_shim(
        home.path(),
        cwd.path(),
        &home.path().join("absent.sock"),
        &cmd,
        "ctx-down",
        &[],
    );
    assert!(!marker.exists(), "no daemon send -> no context command");
    assert!(shim.elapsed < Duration::from_secs(2), "{:?}", shim.elapsed);
}
