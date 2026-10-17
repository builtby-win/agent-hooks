use agent_hooks_adapters::declarative::DeclarativeAdapter;
use agent_hooks_adapters::manifest::AdapterManifest;
use agent_hooks_adapters::registry::AdapterRegistry;
use agent_hooks_adapters::HarnessAdapter;
use agent_hooks_protocol::{AgentLifecycle, EventContext, RawHookInput};

const BUILTIN: &str = include_str!("../builtin/amp/adapter.json");

fn make_input(event_hint: &str, payload: &str) -> RawHookInput {
    RawHookInput {
        harness_hint: "amp".to_string(),
        event_hint: Some(event_hint.to_string()),
        raw_payload: payload.to_string(),
        context: EventContext::default(),
        request_id: None,
        passthrough_env: Default::default(),
    }
}

fn load_adapter() -> DeclarativeAdapter {
    let manifest = AdapterManifest::parse(BUILTIN).expect("builtin amp.json parses");
    assert_eq!(manifest.id, "amp");
    DeclarativeAdapter::new(manifest, "v1".to_string()).expect("v1 exists")
}

#[test]
fn amp_manifest_gate_pretooluse_dialect() {
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
fn amp_manifest_prompt_submit_and_stop_continue() {
    let adapter = load_adapter();

    // UserPromptSubmit -> UserPromptSubmit (gate)
    let input = make_input(
        "UserPromptSubmit",
        r#"{"session_id": "sess-2", "prompt": "Refactor the auth module"}"#,
    );
    let event = adapter.parse_event(&input).expect("parse UserPromptSubmit");
    assert_eq!(event.session_id, "sess-2");
    match event.lifecycle {
        AgentLifecycle::UserPromptSubmit { user_prompt } => {
            assert_eq!(user_prompt.as_deref(), Some("Refactor the auth module"));
        }
        ref other => panic!("expected UserPromptSubmit, got {:?}", other),
    }

    // agent.end -> Stop: Amp's visible turn finished (legacy decide_status
    // is_amp_agent: agent.end => idle). The plugin fire-and-forgets it, so it
    // is observe-only and the response is always empty.
    let stop_input = make_input(
        "agent.end",
        r#"{"session_id": "sess-2", "summary": "All done"}"#,
    );
    let stop_event = adapter.parse_event(&stop_input).expect("parse agent.end");
    match stop_event.lifecycle {
        AgentLifecycle::Stop { summary, success } => {
            assert_eq!(summary.as_deref(), Some("All done"));
            assert!(success);
        }
        ref other => panic!("expected Stop, got {:?}", other),
    }
    let v = adapter.format_response(&agent_hooks_protocol::PromptDecision::deny(Some(
        "keep going".to_string(),
    )));
    assert_eq!(v, serde_json::json!({}));

    // TurnEnd/Stop aliases -> TurnEnd: legacy amp `stop` => busy, never terminal.
    for alias in ["TurnEnd", "Stop", "AgentStop"] {
        let ev = adapter
            .parse_event(&make_input(alias, r#"{"session_id": "sess-2"}"#))
            .expect("parse stop alias");
        assert!(
            matches!(ev.lifecycle, AgentLifecycle::TurnEnd { .. }),
            "{alias} -> TurnEnd"
        );
    }
}

#[test]
fn amp_manifest_permission_subagents_and_registry_pickup() {
    let adapter = load_adapter();

    // PermissionRequest -> PermissionRequest (gate)
    let perm_input = make_input(
        "PermissionRequest",
        r#"{"session_id": "sess-3", "tool_name": "Bash"}"#,
    );
    let perm_event = adapter
        .parse_event(&perm_input)
        .expect("parse PermissionRequest");
    match perm_event.lifecycle {
        AgentLifecycle::PermissionRequest { prompt } => {
            assert_eq!(prompt.tool_name.as_deref(), Some("Bash"));
        }
        ref other => panic!("expected PermissionRequest, got {:?}", other),
    }
    let deny = adapter.format_response(&agent_hooks_protocol::PromptDecision::deny(Some(
        "no shell".to_string(),
    )));
    assert_eq!(
        deny,
        serde_json::json!({"decision": "deny", "reason": "no shell"})
    );

    // SubagentStart picks the subagent id out of agent_id via extraction_overrides
    let start_input = make_input(
        "SubagentStart",
        r#"{"session_id": "sess-3", "agent_id": "sub-1"}"#,
    );
    let start_event = adapter
        .parse_event(&start_input)
        .expect("parse SubagentStart");
    match start_event.lifecycle {
        AgentLifecycle::SubagentStart { subagent_id, .. } => {
            assert_eq!(subagent_id, "sub-1");
        }
        ref other => panic!("expected SubagentStart, got {:?}", other),
    }

    // SubagentStop -> SubagentStop (continue)
    let sub_stop_input = make_input(
        "SubagentStop",
        r#"{"session_id": "sess-3", "agent_id": "sub-1"}"#,
    );
    let sub_stop_event = adapter
        .parse_event(&sub_stop_input)
        .expect("parse SubagentStop");
    match sub_stop_event.lifecycle {
        AgentLifecycle::SubagentStop {
            subagent_id,
            success,
            ..
        } => {
            assert_eq!(subagent_id, "sub-1");
            assert!(success);
        }
        ref other => panic!("expected SubagentStop, got {:?}", other),
    }

    // registry must resolve the builtin manifest, not the legacy Rust adapter:
    // manifest deny carries the reason; legacy Rust deny is bare {"decision":"deny"}
    let reg = AdapterRegistry::get_versioned("amp", None).expect("amp registered");
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
