use agent_hooks_client::AgentHooksClient;
use agent_hooks_core::daemon::AgentHooksDaemon;
use agent_hooks_protocol::{
    AgentLifecycle, EventContext, NormalizedStatus, RawHookInput, SubscriptionFilter,
};
use std::path::PathBuf;
use std::time::Duration;
use tempfile::tempdir;
use tokio_stream::StreamExt;

#[tokio::test]
async fn test_client_daemon_end_to_end() {
    let tmp = tempdir().unwrap();
    let socket_path = tmp.path().join("test-agent-hooks.sock");

    let daemon = AgentHooksDaemon::new();
    let socket_for_daemon = socket_path.clone();
    let daemon_handle = tokio::spawn(async move {
        let _ = daemon.run(Some(socket_for_daemon)).await;
    });

    // Allow daemon to bind socket
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Connect client
    let client = AgentHooksClient::connect_to(socket_path.to_str().unwrap())
        .await
        .expect("Client connects to daemon socket");

    // Test ping
    let ping_ok = client.ping().await.expect("ping");
    assert!(ping_ok);

    // Test subscribe
    let mut stream = client.subscribe(Some(SubscriptionFilter {
        harnesses: vec!["claude-code".to_string()],
        session_ids: vec![],
        event_types: vec![],
        decide: agent_hooks_protocol::Decide::Enabled(false),
        passthrough_env: vec![],
    }));

    // Allow subscriber background task to connect
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Simulate hook connection by sending raw hook input via a second client or direct daemon handle
    let hook_client = AgentHooksClient::connect_to(socket_path.to_str().unwrap())
        .await
        .expect("second client connects");

    let hook_input = RawHookInput {
        harness_hint: "claude".to_string(),
        event_hint: Some("UserPromptSubmit".to_string()),
        raw_payload: r#"{"sessionId": "ses_test_100", "prompt": "Build new feature"}"#.to_string(),
        context: EventContext {
            cwd: Some(PathBuf::from("/Users/user/repo")),
            git_worktree: None,
            tty: Some("/dev/ttys002".to_string()),
            pid: Some(54321),
            ppid: Some(54320),
            tmux_pane_id: None,
            tmux_session_name: None,
            tmux_window_index: None,
            tmux_pane_index: None,
            tmux_client_pid: None,
            is_ssh: false,
            ssh_connection: None,
            ssh_tty: None,
        },
        request_id: None,
        passthrough_env: Default::default(),
    };

    // Forward raw hook event to daemon
    let res = hook_client.send_raw_hook(&hook_input).await;
    assert!(res.is_ok());

    // Expect event on subscription stream
    let event = tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await
        .expect("event received within timeout")
        .expect("stream item exists");

    assert_eq!(event.session_id, "ses_test_100");
    assert_eq!(event.harness, "claude-code");
    assert_eq!(event.status, NormalizedStatus::Running);
    match event.lifecycle {
        AgentLifecycle::UserPromptSubmit { user_prompt } => {
            assert_eq!(user_prompt.as_deref(), Some("Build new feature"));
        }
        _ => panic!("Expected UserPromptSubmit lifecycle"),
    }

    // Test list_sessions
    let sessions = client.list_sessions().await.expect("list_sessions");
    assert!(sessions.iter().any(|s| s.session_id == "ses_test_100"));

    daemon_handle.abort();
}
