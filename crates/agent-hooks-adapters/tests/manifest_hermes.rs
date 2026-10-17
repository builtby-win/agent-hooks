use agent_hooks_adapters::declarative::DeclarativeAdapter;
use agent_hooks_adapters::manifest::AdapterManifest;
use agent_hooks_adapters::registry::AdapterRegistry;
use agent_hooks_adapters::HarnessAdapter;
use agent_hooks_protocol::{AgentLifecycle, EventContext, RawHookInput};

const BUILTIN: &str = include_str!("../builtin/hermes/adapter.json");

fn make_input(event_hint: &str, payload: &str) -> RawHookInput {
    RawHookInput {
        harness_hint: "hermes".to_string(),
        event_hint: Some(event_hint.to_string()),
        raw_payload: payload.to_string(),
        context: EventContext::default(),
        request_id: None,
        passthrough_env: Default::default(),
    }
}

fn load_adapter() -> DeclarativeAdapter {
    let manifest = AdapterManifest::parse(BUILTIN).expect("builtin hermes.json parses");
    assert_eq!(manifest.id, "hermes");
    DeclarativeAdapter::new(manifest, "v1".to_string()).expect("v1 exists")
}

#[test]
fn hermes_manifest_gate_pretooluse_dialect() {
    let adapter = load_adapter();

    // PreToolUse with flat tool fields
    let input = make_input(
        "PreToolUse",
        r#"{"session_id": "sess-1", "tool_name": "Bash", "tool_input": {"command": "cargo test"}}"#,
    );
    let event = adapter.parse_event(&input).expect("parse PreToolUse");
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
fn hermes_manifest_prompt_submit_and_stop_continue() {
    let adapter = load_adapter();

    // beforeSubmitPrompt alias -> UserPromptSubmit (gate)
    let input = make_input(
        "beforeSubmitPrompt",
        r#"{"session_id": "sess-2", "prompt": "Refactor the auth module"}"#,
    );
    let event = adapter
        .parse_event(&input)
        .expect("parse beforeSubmitPrompt");
    assert_eq!(event.session_id, "sess-2");
    match event.lifecycle {
        AgentLifecycle::UserPromptSubmit { user_prompt } => {
            assert_eq!(user_prompt.as_deref(), Some("Refactor the auth module"));
        }
        ref other => panic!("expected UserPromptSubmit, got {:?}", other),
    }

    // stop -> Stop (continue capability: deny keeps going, allow stops)
    let stop_input = make_input("Stop", r#"{"session_id": "sess-2", "summary": "All done"}"#);
    let stop_event = adapter.parse_event(&stop_input).expect("parse Stop");
    match stop_event.lifecycle {
        AgentLifecycle::Stop { summary, success } => {
            assert_eq!(summary.as_deref(), Some("All done"));
            assert!(success);
        }
        ref other => panic!("expected Stop, got {:?}", other),
    }
    let keep_going = adapter.format_response(&agent_hooks_protocol::PromptDecision::deny(Some(
        "keep going".to_string(),
    )));
    assert_eq!(
        keep_going,
        serde_json::json!({"decision": "block", "reason": "keep going"})
    );
    let done = adapter.format_response(&agent_hooks_protocol::PromptDecision::allow());
    assert_eq!(done, serde_json::json!({}));
}

#[test]
fn hermes_manifest_observe_events_and_registry_pickup() {
    let adapter = load_adapter();

    // SessionStart -> SessionStart (observe => {} regardless of decision)
    let input = make_input("SessionStart", r#"{"session_id": "sess-3"}"#);
    let event = adapter.parse_event(&input).expect("parse SessionStart");
    assert!(matches!(
        event.lifecycle,
        AgentLifecycle::SessionStart { .. }
    ));
    let v = adapter.format_response(&agent_hooks_protocol::PromptDecision::allow());
    assert_eq!(v, serde_json::json!({}));

    // PostToolUse -> PostToolUse (observe)
    let post_input = make_input(
        "PostToolUse",
        r#"{"session_id": "sess-3", "tool_name": "Bash", "tool_output": "ok"}"#,
    );
    let post_event = adapter.parse_event(&post_input).expect("parse PostToolUse");
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

    // Notification -> Notification (observe)
    let notif_input = make_input(
        "Notification",
        r#"{"session_id": "sess-3", "message": "build finished"}"#,
    );
    let notif_event = adapter
        .parse_event(&notif_input)
        .expect("parse Notification");
    match notif_event.lifecycle {
        AgentLifecycle::Notification { message, .. } => {
            assert_eq!(message, "build finished");
        }
        ref other => panic!("expected Notification, got {:?}", other),
    }

    // registry must resolve the builtin manifest, not the legacy Rust adapter:
    // manifest deny carries the reason; legacy Rust deny is bare {"decision":"deny"}
    let reg = AdapterRegistry::get_versioned("hermes", None).expect("hermes registered");
    let gate_input = make_input(
        "PreToolUse",
        r#"{"session_id": "sess-9", "tool_name": "Bash", "tool_input": {}}"#,
    );
    reg.parse_event(&gate_input)
        .expect("registry parse PreToolUse");
    let deny = reg.format_response(&agent_hooks_protocol::PromptDecision::deny(Some(
        "blocked".to_string(),
    )));
    assert_eq!(
        deny,
        serde_json::json!({"decision": "deny", "reason": "blocked"})
    );
}
