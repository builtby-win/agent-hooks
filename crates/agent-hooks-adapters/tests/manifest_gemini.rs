use agent_hooks_adapters::declarative::DeclarativeAdapter;
use agent_hooks_adapters::manifest::AdapterManifest;
use agent_hooks_adapters::registry::AdapterRegistry;
use agent_hooks_adapters::HarnessAdapter;
use agent_hooks_protocol::{AgentLifecycle, EventContext, RawHookInput};

const BUILTIN: &str = include_str!("../builtin/gemini-cli/adapter.json");

fn make_input(event_hint: &str, payload: &str) -> RawHookInput {
    RawHookInput {
        harness_hint: "gemini-cli".to_string(),
        event_hint: Some(event_hint.to_string()),
        raw_payload: payload.to_string(),
        context: EventContext::default(),
        request_id: None,
        passthrough_env: Default::default(),
    }
}

fn load_adapter() -> DeclarativeAdapter {
    let manifest = AdapterManifest::parse(BUILTIN).expect("builtin gemini-cli.json parses");
    assert_eq!(manifest.id, "gemini-cli");
    DeclarativeAdapter::new(manifest, "v1".to_string()).expect("v1 exists")
}

#[test]
fn gemini_manifest_gate_pretooluse_dialect() {
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
fn gemini_manifest_preinvocation_and_stop_continue() {
    let adapter = load_adapter();

    // PreInvocation -> UserPromptSubmit (gate), per the Rust adapter's mapping
    let input = make_input(
        "PreInvocation",
        r#"{"session_id": "sess-2", "prompt": "Explain this diff"}"#,
    );
    let event = adapter.parse_event(&input).expect("parse PreInvocation");
    assert_eq!(event.session_id, "sess-2");
    match event.lifecycle {
        AgentLifecycle::UserPromptSubmit { user_prompt } => {
            assert_eq!(user_prompt.as_deref(), Some("Explain this diff"));
        }
        ref other => panic!("expected UserPromptSubmit, got {:?}", other),
    }

    // AfterAgent alias -> Stop (continue capability: deny keeps going, allow stops)
    let stop_input = make_input(
        "AfterAgent",
        r#"{"session_id": "sess-2", "summary": "All done"}"#,
    );
    let stop_event = adapter.parse_event(&stop_input).expect("parse AfterAgent");
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
fn gemini_manifest_observe_events_and_registry_pickup() {
    let adapter = load_adapter();

    // SessionStart -> SessionStart (observe => legacy status stdout
    // {"decision":"allow"} regardless of decision)
    let input = make_input("SessionStart", r#"{"session_id": "sess-3"}"#);
    let event = adapter.parse_event(&input).expect("parse SessionStart");
    assert!(matches!(
        event.lifecycle,
        AgentLifecycle::SessionStart { .. }
    ));
    let v = adapter.format_response(&agent_hooks_protocol::PromptDecision::allow());
    assert_eq!(v, serde_json::json!({"decision": "allow"}));

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
    // on a Stop event the manifest denies with the continue dialect
    // ({"decision":"block",...}), which the legacy Rust adapter never emits
    // (it always denies with {"decision":"deny"}).
    let reg = AdapterRegistry::get_versioned("gemini-cli", None).expect("gemini-cli registered");
    let stop_input = make_input("Stop", r#"{"session_id": "sess-9", "summary": "done"}"#);
    reg.parse_event(&stop_input).expect("registry parse Stop");
    let deny = reg.format_response(&agent_hooks_protocol::PromptDecision::deny(Some(
        "blocked".to_string(),
    )));
    assert_eq!(
        deny,
        serde_json::json!({"decision": "block", "reason": "blocked"})
    );
}

// ---------------------------------------------------------------------------
// B2V legacy parity fixtures (ah-command-harnesses)
// ---------------------------------------------------------------------------

use agent_hooks_protocol::{NormalizedStatus, PromptDecision};

// Legacy decide_status/decide_busy_gated: beforemodel/aftermodel are busy per-cycle steps.
#[test]
fn gemini_model_events_are_turn_steps_not_prompts() {
    let adapter = load_adapter();
    let before = adapter
        .parse_event(&make_input(
            "BeforeModel",
            r#"{"session_id":"gem-1","transcript_path":"/tmp/g.json","cwd":"/Users/me/proj","hook_event_name":"BeforeModel","timestamp":"2026-09-01T00:00:00Z","llm_request":{"model":"gemini-2.5-pro","messages":[]}}"#,
        ))
        .unwrap();
    assert!(matches!(before.lifecycle, AgentLifecycle::TurnStart { .. }));
    assert_eq!(before.status, NormalizedStatus::Running);
    assert_eq!(
        before.context.cwd.as_deref(),
        Some(std::path::Path::new("/Users/me/proj"))
    );
    assert_eq!(
        adapter.format_response(&PromptDecision::deny(Some("stop".into()))),
        serde_json::json!({"decision": "deny", "reason": "stop"})
    );
    let after = adapter
        .parse_event(&make_input(
            "AfterModel",
            r#"{"session_id":"gem-1","hook_event_name":"AfterModel","llm_response":{"text":"hi"}}"#,
        ))
        .unwrap();
    // Legacy aftermodel: busy, busy-gated turn step (not a no-op TurnEnd).
    assert!(matches!(after.lifecycle, AgentLifecycle::TurnStart { .. }));
    // BeforeAgent is still the prompt boundary.
    let agent = adapter
        .parse_event(&make_input(
            "BeforeAgent",
            r#"{"session_id":"gem-1","hook_event_name":"BeforeAgent","prompt":"refactor"}"#,
        ))
        .unwrap();
    assert!(matches!(
        agent.lifecycle,
        AgentLifecycle::UserPromptSubmit { .. }
    ));
}

// Legacy decide_lifecycle_action: sessionend → Unregister (was mapped to Stop).
#[test]
fn gemini_session_end_closes_the_session() {
    let adapter = load_adapter();
    let end = adapter
        .parse_event(&make_input(
            "SessionEnd",
            r#"{"session_id":"gem-1","hook_event_name":"SessionEnd","reason":"exit"}"#,
        ))
        .unwrap();
    assert!(matches!(end.lifecycle, AgentLifecycle::SessionEnd { .. }));
    assert_eq!(end.status, NormalizedStatus::Closed);
    assert_eq!(
        adapter.format_response(&PromptDecision::allow()),
        serde_json::json!({"decision": "allow"})
    );
    let after_agent = adapter
        .parse_event(&make_input(
            "AfterAgent",
            r#"{"session_id":"gem-1","hook_event_name":"AfterAgent","prompt_response":"done"}"#,
        ))
        .unwrap();
    assert_eq!(after_agent.status, NormalizedStatus::Idle);
}

// Legacy status_classification.rs status_for_event_maps_busy_and_idle +
// non_codex_completion_notifications_map_to_idle_when_done.
#[test]
fn gemini_notifications_follow_legacy_status() {
    let adapter = load_adapter();
    for (raw, canonical, status) in [
        (
            r#"{"session_id":"gem-1","hook_event_name":"Notification","notification_type":"ToolPermission","message":"Allow shell?","details":{}}"#,
            Some("permission"),
            NormalizedStatus::WaitingForInput,
        ),
        (
            r#"{"session_id":"gem-1","hook_event_name":"Notification","notification_type":"permission_request"}"#,
            Some("permission"),
            NormalizedStatus::WaitingForInput,
        ),
        (
            r#"{"session_id":"gem-1","hook_event_name":"Notification","notification_type":"complete"}"#,
            Some("idle"),
            NormalizedStatus::Idle,
        ),
        (
            r#"{"session_id":"gem-1","hook_event_name":"Notification","notification_type":"finished"}"#,
            Some("idle"),
            NormalizedStatus::Idle,
        ),
        (
            r#"{"session_id":"gem-1","hook_event_name":"Notification","promptResponse":true,"notification_type":"info"}"#,
            Some("idle"),
            NormalizedStatus::Idle,
        ),
        (
            r#"{"session_id":"gem-1","hook_event_name":"Notification","notification_type":"info","message":"fyi"}"#,
            Some("info"),
            NormalizedStatus::Running,
        ),
    ] {
        let event = adapter
            .parse_event(&make_input("Notification", raw))
            .unwrap();
        match &event.lifecycle {
            AgentLifecycle::Notification {
                notification_type, ..
            } => {
                assert_eq!(notification_type.as_deref(), canonical, "{raw}")
            }
            other => panic!("expected Notification, got {other:?}"),
        }
        assert_eq!(event.status, status, "{raw}");
    }
}

// Live parity e2e (gemini): legacy printed {"decision":"allow"} on every
// status hook; BeforeTool stays "no decision" on defer (intentional divergence).
#[test]
fn gemini_status_hooks_print_legacy_allow_but_before_tool_defers() {
    let adapter = load_adapter();
    for event_name in [
        "SessionStart",
        "BeforeAgent",
        "BeforeModel",
        "AfterModel",
        "AfterAgent",
        "SessionEnd",
        "AfterTool",
        "Notification",
    ] {
        let raw = format!(
            r#"{{"session_id":"parity-gemini","hook_event_name":"{event_name}","cwd":"/tmp/p"}}"#
        );
        adapter.parse_event(&make_input(event_name, &raw)).unwrap();
        assert_eq!(
            adapter.format_response(&PromptDecision::defer()),
            serde_json::json!({"decision": "allow"}),
            "{event_name}"
        );
    }
    adapter
        .parse_event(&make_input(
            "BeforeTool",
            r#"{"session_id":"parity-gemini","hook_event_name":"BeforeTool","tool_name":"run_shell_command","tool_input":{"command":"ls"}}"#,
        ))
        .unwrap();
    assert_eq!(
        adapter.format_response(&PromptDecision::defer()),
        serde_json::json!({})
    );
}
