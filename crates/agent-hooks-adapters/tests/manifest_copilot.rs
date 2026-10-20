use agent_hooks_adapters::declarative::DeclarativeAdapter;
use agent_hooks_adapters::manifest::AdapterManifest;
use agent_hooks_adapters::registry::AdapterRegistry;
use agent_hooks_adapters::HarnessAdapter;
use agent_hooks_protocol::{AgentLifecycle, EventContext, RawHookInput};

const BUILTIN: &str = include_str!("../builtin/copilot/adapter.json");

fn make_input(event_hint: &str, payload: &str) -> RawHookInput {
    RawHookInput {
        harness_hint: "copilot".to_string(),
        event_hint: Some(event_hint.to_string()),
        raw_payload: payload.to_string(),
        context: EventContext::default(),
        request_id: None,
        passthrough_env: Default::default(),
    }
}

fn load_adapter() -> DeclarativeAdapter {
    let manifest = AdapterManifest::parse(BUILTIN).expect("builtin copilot.json parses");
    assert_eq!(manifest.id, "copilot");
    DeclarativeAdapter::new(manifest, "v1".to_string()).expect("v1 exists")
}

#[test]
fn copilot_manifest_gate_pretooluse_dialect() {
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

    // gate dialect: documented permissionDecision allow/deny/ask (the old
    // {"decision"} shape was never installed for preToolUse, so it never
    // reached a live CLI; the hooks reference mandates permissionDecision).
    let allow = adapter.format_response(&agent_hooks_protocol::PromptDecision::allow());
    assert_eq!(allow, serde_json::json!({"permissionDecision": "allow"}));

    let deny = adapter.format_response(&agent_hooks_protocol::PromptDecision::deny(Some(
        "need approval".to_string(),
    )));
    assert_eq!(
        deny,
        serde_json::json!({"permissionDecision": "deny", "permissionDecisionReason": "need approval"})
    );
}

#[test]
fn copilot_manifest_prompt_submitted_and_agent_stop_continue() {
    let adapter = load_adapter();

    // userPromptSubmitted -> UserPromptSubmit (gate)
    let input = make_input(
        "userPromptSubmitted",
        r#"{"session_id": "sess-2", "prompt": "Fix bug in auth.ts"}"#,
    );
    let event = adapter
        .parse_event(&input)
        .expect("parse userPromptSubmitted");
    assert_eq!(event.session_id, "sess-2");
    match event.lifecycle {
        AgentLifecycle::UserPromptSubmit { user_prompt } => {
            assert_eq!(user_prompt.as_deref(), Some("Fix bug in auth.ts"));
        }
        ref other => panic!("expected UserPromptSubmit, got {:?}", other),
    }

    // agentStop -> Stop (continue capability: deny keeps going, allow stops)
    let stop_input = make_input(
        "agentStop",
        r#"{"session_id": "sess-2", "summary": "All tests pass"}"#,
    );
    let stop_event = adapter.parse_event(&stop_input).expect("parse agentStop");
    match stop_event.lifecycle {
        AgentLifecycle::Stop { summary, success } => {
            assert_eq!(summary.as_deref(), Some("All tests pass"));
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
fn copilot_manifest_observe_events_and_registry_pickup() {
    let adapter = load_adapter();

    // sessionStart -> SessionStart (observe => {} regardless of decision)
    let input = make_input("sessionStart", r#"{"session_id": "sess-3"}"#);
    let event = adapter.parse_event(&input).expect("parse sessionStart");
    assert!(matches!(
        event.lifecycle,
        AgentLifecycle::SessionStart { .. }
    ));
    let v = adapter.format_response(&agent_hooks_protocol::PromptDecision::allow());
    assert_eq!(v, serde_json::json!({}));

    // sessionEnd -> SessionEnd (observe)
    let end_input = make_input("sessionEnd", r#"{"session_id": "sess-3"}"#);
    let end_event = adapter.parse_event(&end_input).expect("parse sessionEnd");
    assert!(matches!(
        end_event.lifecycle,
        AgentLifecycle::SessionEnd { .. }
    ));

    // postToolUse -> PostToolUse (observe)
    let post_input = make_input(
        "postToolUse",
        r#"{"session_id": "sess-3", "tool_name": "Bash", "tool_output": "ok"}"#,
    );
    let post_event = adapter.parse_event(&post_input).expect("parse postToolUse");
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

    // notification -> Notification (observe)
    let notif_input = make_input(
        "notification",
        r#"{"session_id": "sess-3", "message": "build finished"}"#,
    );
    let notif_event = adapter
        .parse_event(&notif_input)
        .expect("parse notification");
    match notif_event.lifecycle {
        AgentLifecycle::Notification { message, .. } => {
            assert_eq!(message, "build finished");
        }
        ref other => panic!("expected Notification, got {:?}", other),
    }

    // registry must resolve the builtin manifest, not the legacy Rust adapter:
    // manifest deny carries the reason; legacy Rust deny is bare {"decision":"deny"}
    let reg = AdapterRegistry::get_versioned("copilot", None).expect("copilot registered");
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
        serde_json::json!({"permissionDecision": "deny", "permissionDecisionReason": "blocked"})
    );
}

// ---------------------------------------------------------------------------
// B2V legacy parity fixtures (ah-command-harnesses)
// ---------------------------------------------------------------------------

use agent_hooks_protocol::{NormalizedStatus, PromptDecision};

// Legacy decide_status: user_prompt_submitted → busy, agent_stop → idle; sessionEnd unregisters.
#[test]
fn copilot_lifecycle_statuses_match_legacy() {
    let adapter = load_adapter();
    let prompt = adapter
        .parse_event(&make_input(
            "userPromptSubmitted",
            r#"{"sessionId":"cop-1","timestamp":1760000000000,"cwd":"/Users/me/repo","prompt":"add tests"}"#,
        ))
        .unwrap();
    assert_eq!(prompt.session_id, "cop-1");
    assert_eq!(prompt.status, NormalizedStatus::Running);
    assert_eq!(
        prompt.context.cwd.as_deref(),
        Some(std::path::Path::new("/Users/me/repo"))
    );
    let stop = adapter
        .parse_event(&make_input(
            "agentStop",
            r#"{"sessionId":"cop-1","timestamp":1760000000001,"cwd":"/Users/me/repo"}"#,
        ))
        .unwrap();
    assert_eq!(stop.status, NormalizedStatus::Idle);
    let end = adapter
        .parse_event(&make_input(
            "sessionEnd",
            r#"{"sessionId":"cop-1","timestamp":1760000000002,"cwd":"/Users/me/repo","reason":"complete"}"#,
        ))
        .unwrap();
    assert_eq!(end.status, NormalizedStatus::Closed);
    let tool = adapter
        .parse_event(&make_input(
            "preToolUse",
            r#"{"sessionId":"cop-1","cwd":"/Users/me/repo","toolName":"bash","toolArgs":"{\"command\":\"ls\"}"}"#,
        ))
        .unwrap();
    assert_eq!(tool.status, NormalizedStatus::Running);
    assert_eq!(
        adapter.format_response(&PromptDecision::deny(Some("no".into()))),
        serde_json::json!({"permissionDecision": "deny", "permissionDecisionReason": "no"})
    );
    assert_eq!(
        adapter.format_response(&PromptDecision::defer()),
        serde_json::json!({})
    );
}

// ---------------------------------------------------------------------------
// permissionRequest + ask_user (docs-only: GitHub hooks reference,
// not yet exercised against a live Copilot CLI)
// ---------------------------------------------------------------------------

#[test]
fn copilot_manifest_permission_request_behavior_dialect() {
    let adapter = load_adapter();

    let input = make_input(
        "permissionRequest",
        r#"{"session_id": "cop-10", "toolName": "bash", "toolInput": {"command": "rm -rf /tmp/x"}}"#,
    );
    let event = adapter
        .parse_event(&input)
        .expect("parse permissionRequest");
    assert_eq!(event.session_id, "cop-10");
    assert!(matches!(
        event.lifecycle,
        AgentLifecycle::PermissionRequest { .. }
    ));

    // behavior allow/deny object (never ask): empty output falls through
    // to normal permission handling, which the engine models as allow.
    let allow = adapter.format_response(&agent_hooks_protocol::PromptDecision::allow());
    assert_eq!(allow, serde_json::json!({"behavior": "allow"}));
    let deny = adapter.format_response(&agent_hooks_protocol::PromptDecision::deny(Some(
        "policy".to_string(),
    )));
    assert_eq!(
        deny,
        serde_json::json!({"behavior": "deny", "message": "policy"})
    );

    // PascalCase Claude-format alias normalizes to the same lifecycle.
    let pascal = adapter
        .parse_event(&make_input(
            "PermissionRequest",
            r#"{"session_id": "cop-10", "tool_name": "bash", "tool_input": {}}"#,
        ))
        .expect("parse PermissionRequest");
    assert!(matches!(
        pascal.lifecycle,
        AgentLifecycle::PermissionRequest { .. }
    ));
}

#[test]
fn copilot_manifest_pretooluse_installs_and_ask_tools_listed() {
    let manifest = AdapterManifest::parse(BUILTIN).expect("parses");
    let vs = &manifest.versions["v1"];
    // preToolUse must install now: without it the adapter can never gate.
    assert!(
        vs.events["preToolUse"].install,
        "preToolUse installs its hook"
    );
    assert!(
        vs.events["permissionRequest"].install,
        "permissionRequest installs its hook"
    );
    assert!(
        !vs.unavailable.iter().any(|u| u == "PermissionRequest"),
        "PermissionRequest is available"
    );
    let ask_tools: Vec<String> = vs.ask_tools.iter().map(|t| t.name().to_string()).collect();
    assert!(ask_tools.contains(&"ask_user".to_string()));
    assert!(ask_tools.contains(&"AskUserQuestion".to_string()));
}
