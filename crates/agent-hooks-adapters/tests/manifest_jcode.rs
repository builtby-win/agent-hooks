use agent_hooks_adapters::declarative::DeclarativeAdapter;
use agent_hooks_adapters::manifest::AdapterManifest;
use agent_hooks_adapters::registry::AdapterRegistry;
use agent_hooks_adapters::HarnessAdapter;
use agent_hooks_protocol::{AgentLifecycle, EventContext, RawHookInput};

const BUILTIN: &str = include_str!("../builtin/jcode/adapter.json");

fn make_input(event_hint: &str, payload: &str) -> RawHookInput {
    RawHookInput {
        harness_hint: "jcode".to_string(),
        event_hint: Some(event_hint.to_string()),
        raw_payload: payload.to_string(),
        context: EventContext::default(),
        request_id: None,
        passthrough_env: Default::default(),
    }
}

fn load_adapter() -> DeclarativeAdapter {
    let manifest = AdapterManifest::parse(BUILTIN).expect("builtin jcode.json parses");
    assert_eq!(manifest.id, "jcode");
    DeclarativeAdapter::new(manifest, "v1".to_string()).expect("v1 exists")
}

#[test]
fn jcode_manifest_gate_pretooluse_dialect() {
    let adapter = load_adapter();

    // preToolUse with flat tool fields
    let input = make_input(
        "preToolUse",
        r#"{"session_id": "sess-1", "tool_name": "Bash", "tool_input": {"command": "cargo test"}}"#,
    );
    let event = adapter.parse_event(&input).expect("parse preToolUse");
    assert_eq!(event.session_id, "sess-1");
    match event.lifecycle {
        AgentLifecycle::PreToolUse {
            tool_name, input, ..
        } => {
            assert_eq!(tool_name, "Bash");
            assert_eq!(
                input.get("command").and_then(|v| v.as_str()),
                Some("cargo test")
            );
        }
        ref other => panic!("expected PreToolUse, got {:?}", other),
    }

    // gate dialect: legacy {"decision": allow/deny}
    let allow = adapter.format_response(&agent_hooks_protocol::PromptDecision::allow());
    assert_eq!(allow, serde_json::json!({"decision": "allow"}));

    let deny = adapter.format_response(&agent_hooks_protocol::PromptDecision::deny(Some(
        "need approval".to_string(),
    )));
    assert_eq!(
        deny,
        serde_json::json!({"decision": "deny", "reason": "need approval"})
    );
}

#[test]
fn jcode_manifest_turn_events_and_stop_continue() {
    let adapter = load_adapter();

    // turn_start -> UserPromptSubmit (gate), mirroring the Rust turnstart arm
    let input = make_input(
        "turn_start",
        r#"{"session_id": "sess-2", "prompt": "Fix bug in auth.ts"}"#,
    );
    let event = adapter.parse_event(&input).expect("parse turn_start");
    assert_eq!(event.session_id, "sess-2");
    match event.lifecycle {
        AgentLifecycle::UserPromptSubmit { user_prompt } => {
            assert_eq!(user_prompt.as_deref(), Some("Fix bug in auth.ts"));
        }
        ref other => panic!("expected UserPromptSubmit, got {:?}", other),
    }

    // turn_end is not a completion (B2V legacy lifecycle.rs decide_status /
    // decide_explicit_completion ignore jcode turn_end, 51acc82da): TurnEnd,
    // derived Running, observe no-op output.
    let end_input = make_input(
        "turn_end",
        r#"{"event": "turn_end", "session_id": "sess-2", "summary": "Turn complete"}"#,
    );
    let end_event = adapter.parse_event(&end_input).expect("parse turn_end");
    assert!(
        matches!(end_event.lifecycle, AgentLifecycle::TurnEnd { .. }),
        "got {:?}",
        end_event.lifecycle
    );
    assert_eq!(
        end_event.status,
        agent_hooks_protocol::NormalizedStatus::Running
    );
    let out = adapter.format_response(&agent_hooks_protocol::PromptDecision::deny(Some(
        "keep going".to_string(),
    )));
    assert_eq!(out, serde_json::json!({}));
}

#[test]
fn jcode_manifest_observe_events_and_registry_pickup() {
    let adapter = load_adapter();

    // session_start -> SessionStart (observe => {} regardless of decision)
    let input = make_input("session_start", r#"{"session_id": "sess-3"}"#);
    let event = adapter.parse_event(&input).expect("parse session_start");
    assert!(matches!(
        event.lifecycle,
        AgentLifecycle::SessionStart { .. }
    ));
    let v = adapter.format_response(&agent_hooks_protocol::PromptDecision::allow());
    assert_eq!(v, serde_json::json!({}));

    // post_tool -> PostToolUse (observe)
    let post_input = make_input(
        "post_tool",
        r#"{"session_id": "sess-3", "tool_name": "Bash", "tool_output": "ok"}"#,
    );
    let post_event = adapter.parse_event(&post_input).expect("parse post_tool");
    match post_event.lifecycle {
        AgentLifecycle::PostToolUse {
            tool_name,
            output_preview,
        } => {
            assert_eq!(tool_name, "Bash");
            assert_eq!(output_preview.as_deref(), Some("ok"));
        }
        ref other => panic!("expected PostToolUse, got {:?}", other),
    }

    // registry must resolve the builtin manifest, not the legacy Rust adapter:
    // manifest deny carries the reason; legacy Rust deny is bare {"decision":"deny"}
    let reg = AdapterRegistry::get_versioned("jcode", None).expect("jcode registered");
    let gate_input = make_input(
        "preToolUse",
        r#"{"session_id": "sess-9", "tool_name": "Bash", "tool_input": {}}"#,
    );
    reg.parse_event(&gate_input)
        .expect("registry parse preToolUse");
    let deny = reg.format_response(&agent_hooks_protocol::PromptDecision::deny(Some(
        "blocked".to_string(),
    )));
    assert_eq!(
        deny,
        serde_json::json!({"decision": "deny", "reason": "blocked"})
    );
}

// ---------------------------------------------------------------------------
// B2V legacy parity fixtures (ah-command-harnesses)
// ---------------------------------------------------------------------------

use agent_hooks_protocol::NormalizedStatus;

/// The shared jcode server spawns hooks from its own tmux pane.
fn stale_server_input(event_hint: &str, payload: &str) -> RawHookInput {
    let mut input = make_input(event_hint, payload);
    input.context.tmux_pane_id = Some("%149".to_string());
    input.context.tmux_session_name = Some("server-session".to_string());
    input.context.tmux_window_index = Some(96);
    input
}

// Legacy agent_event/mod.rs enrich_jcode_client_attachment_context.
#[test]
fn jcode_client_attach_rebinds_tmux_to_the_client_pane() {
    let adapter = load_adapter();
    let event = adapter
        .parse_event(&stale_server_input(
            "session_start",
            r#"{"event":"session_start","source":"client_attach","session_id":"ses_root","cwd":"/Users/me/proj","client_tmux_pane":"%new","client_tmux_session":"client-session","client_tmux_window_index":"2"}"#,
        ))
        .unwrap();
    assert!(matches!(
        event.lifecycle,
        AgentLifecycle::SessionStart { .. }
    ));
    assert_eq!(event.session_id, "ses_root");
    assert_eq!(event.context.tmux_pane_id.as_deref(), Some("%new"));
    assert_eq!(
        event.context.tmux_session_name.as_deref(),
        Some("client-session")
    );
    assert_eq!(event.context.tmux_window_index, Some(2));
    assert_eq!(
        event.context.cwd.as_deref(),
        Some(std::path::Path::new("/Users/me/proj"))
    );
}

// Legacy: every session_start clears the stale server tmux, even without a client pane.
#[test]
fn jcode_session_start_without_client_pane_clears_stale_tmux() {
    let adapter = load_adapter();
    let event = adapter
        .parse_event(&stale_server_input(
            "session_start",
            r#"{"event":"session_start","source":"resume","session_id":"ses_root"}"#,
        ))
        .unwrap();
    assert_eq!(event.context.tmux_pane_id, None);
    assert_eq!(event.context.tmux_session_name, None);
    assert_eq!(event.context.tmux_window_index, None);
}

// Negative: status events keep the shim context (legacy only rebinds on session_start).
#[test]
fn jcode_status_events_do_not_touch_tmux_context() {
    let adapter = load_adapter();
    let event = adapter
        .parse_event(&stale_server_input(
            "post_tool",
            r#"{"event":"post_tool","session_id":"ses_root","tool_name":"bash","client_tmux_pane":"%ignored"}"#,
        ))
        .unwrap();
    assert_eq!(event.context.tmux_pane_id.as_deref(), Some("%149"));
    assert_eq!(event.status, NormalizedStatus::Running);
}

// Legacy tests/lifecycle_classification.rs jcode_lifecycle_events_map_to_session_state.
#[test]
fn jcode_lifecycle_statuses_match_legacy() {
    let adapter = load_adapter();
    for (event_name, status) in [
        ("turn_start", NormalizedStatus::Running),
        ("post_tool", NormalizedStatus::Running),
        ("turn_end", NormalizedStatus::Running),
        ("session_end", NormalizedStatus::Closed),
    ] {
        let raw = format!(r#"{{"event":"{event_name}","session_id":"ses_root"}}"#);
        let event = adapter.parse_event(&make_input(event_name, &raw)).unwrap();
        assert_eq!(event.status, status, "{event_name}");
        assert_eq!(event.session_id, "ses_root");
    }
}
