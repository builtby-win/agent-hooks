//! Held gates versus the hook connections waiting on them, over a real daemon
//! socket: a hold belongs to every connection that asked, and ends for one of
//! them only when that client is really gone.
#![cfg(unix)]

use agent_hooks_client::AgentHooksClient;
use agent_hooks_core::daemon::AgentHooksDaemon;
use agent_hooks_protocol::PromptDecision;
use std::path::Path;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio_stream::StreamExt;

/// A Claude permission request; `request_id` is the hold's prompt id, so a
/// shim retry (same id) joins the hold instead of opening a second one.
fn permission_line(request_id: &str) -> String {
    let payload = serde_json::json!({
        "session_id": "hold-1",
        "hook_event_name": "PermissionRequest",
        "tool_name": "Bash",
        "tool_input": {"command": "cargo test"},
    });
    let mut line = serde_json::json!({
        "harness_hint": "claude",
        "event_hint": "PermissionRequest",
        "raw_payload": payload.to_string(),
        "request_id": request_id,
    })
    .to_string();
    line.push('\n');
    line
}

async fn send_gate(socket: &Path, request_id: &str) -> UnixStream {
    let mut stream = UnixStream::connect(socket).await.expect("connect");
    stream
        .write_all(permission_line(request_id).as_bytes())
        .await
        .expect("send gate");
    stream
}

async fn read_decision(stream: UnixStream) -> String {
    let mut line = String::new();
    tokio::time::timeout(
        Duration::from_secs(10),
        BufReader::new(stream).read_line(&mut line),
    )
    .await
    .expect("verdict arrives")
    .expect("read verdict");
    let reply: serde_json::Value = serde_json::from_str(&line).expect("verdict json");
    reply["decision"]
        .as_str()
        .expect("decision verb")
        .to_string()
}

/// Daemon plus a connected decider; resolves once the decider is registered.
async fn daemon_with_decider(
    socket: &Path,
) -> (
    AgentHooksClient,
    impl tokio_stream::Stream<Item = agent_hooks_protocol::AgentEvent> + Unpin,
) {
    let daemon_socket = socket.to_path_buf();
    tokio::spawn(async move {
        let _ = AgentHooksDaemon::new().run(Some(daemon_socket)).await;
    });
    let client = AgentHooksClient::connect_to(socket.to_str().unwrap())
        .await
        .expect("daemon up");
    let events = Box::pin(client.subscribe_decider());
    tokio::time::sleep(Duration::from_millis(500)).await;
    (client, events)
}

async fn held_prompt_id(
    events: &mut (impl tokio_stream::Stream<Item = agent_hooks_protocol::AgentEvent> + Unpin),
) -> String {
    let event = tokio::time::timeout(Duration::from_secs(10), events.next())
        .await
        .expect("held event arrives")
        .expect("stream item");
    event.hold.expect("event carries its hold").prompt_id
}

#[tokio::test]
async fn retry_keeps_its_hold_when_the_first_attempt_disconnects() {
    let tmp = tempfile::TempDir::new().unwrap();
    let socket = tmp.path().join("d.sock");
    let (decider, mut events) = daemon_with_decider(&socket).await;

    let first = send_gate(&socket, "retry-1").await;
    let prompt_id = held_prompt_id(&mut events).await;
    let retry = send_gate(&socket, "retry-1").await;
    held_prompt_id(&mut events).await;

    // The shim gave up on its first attempt; the retry is still waiting.
    drop(first);
    tokio::time::sleep(Duration::from_millis(300)).await;

    decider
        .respond_prompt(&prompt_id, PromptDecision::allow())
        .await
        .expect("the hold is still answerable");
    assert_eq!(read_decision(retry).await, "allow");
}

#[tokio::test]
async fn half_closed_client_still_receives_the_verdict() {
    let tmp = tempfile::TempDir::new().unwrap();
    let socket = tmp.path().join("d.sock");
    let (decider, mut events) = daemon_with_decider(&socket).await;

    // `nc -U` / `socket.end(line)`: done sending, still reading.
    let mut hook = send_gate(&socket, "half-1").await;
    hook.shutdown().await.expect("half-close");
    let prompt_id = held_prompt_id(&mut events).await;
    tokio::time::sleep(Duration::from_millis(600)).await;

    decider
        .respond_prompt(&prompt_id, PromptDecision::allow())
        .await
        .expect("the hold is still answerable");
    assert_eq!(read_decision(hook).await, "allow");
}

#[tokio::test]
async fn client_that_left_mid_hold_cancels_it() {
    let tmp = tempfile::TempDir::new().unwrap();
    let socket = tmp.path().join("d.sock");
    let (decider, mut events) = daemon_with_decider(&socket).await;

    let mut hook = send_gate(&socket, "gone-1").await;
    hook.shutdown().await.expect("half-close");
    let prompt_id = held_prompt_id(&mut events).await;
    drop(hook);
    tokio::time::sleep(Duration::from_secs(1)).await;

    assert!(
        decider
            .respond_prompt(&prompt_id, PromptDecision::allow())
            .await
            .is_err(),
        "nobody is left to answer"
    );
}

#[tokio::test]
async fn rejected_subscription_ends_the_stream_without_retrying() {
    let tmp = tempfile::TempDir::new().unwrap();
    let socket = tmp.path().join("d.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let (accepted_tx, mut accepted_rx) = tokio::sync::mpsc::unbounded_channel();
    // A daemon that refuses every subscribe, as one does for a filter it
    // cannot parse.
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let _ = accepted_tx.send(());
            let mut reader = BufReader::new(stream);
            let mut request = String::new();
            let _ = reader.read_line(&mut request).await;
            let _ = reader
                .get_mut()
                .write_all(
                    b"{\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"code\":-32602,\"message\":\"invalid subscribe params\"}}\n",
                )
                .await;
        }
    });

    let client = AgentHooksClient::new(socket.to_str().unwrap().to_string());
    let mut events = Box::pin(client.subscribe(None));
    let ended = tokio::time::timeout(Duration::from_secs(5), events.next())
        .await
        .expect("the stream ends instead of going silent");
    assert!(ended.is_none());

    tokio::time::sleep(Duration::from_secs(1)).await;
    accepted_rx.recv().await.expect("one subscribe attempt");
    assert!(
        accepted_rx.try_recv().is_err(),
        "no reconnect after a rejection"
    );
}
