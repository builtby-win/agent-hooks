//! Qoder adapter: docs-derived payloads (Qoder CLI hooks reference).
//! No live Qoder run has confirmed these shapes; the manifest carries
//! `unverified: true` until one does.

use agent_hooks_adapters::declarative::DeclarativeAdapter;
use agent_hooks_adapters::manifest::AdapterManifest;
use agent_hooks_adapters::registry::AdapterRegistry;
use agent_hooks_adapters::HarnessAdapter;
use agent_hooks_protocol::{
    AgentLifecycle, EventContext, NormalizedStatus, PromptDecision, RawHookInput,
};

const BUILTIN: &str = include_str!("../builtin/qoder/adapter.json");

fn make_input(event_hint: &str, payload: &str) -> RawHookInput {
    RawHookInput {
        harness_hint: "qoder".to_string(),
        event_hint: Some(event_hint.to_string()),
        raw_payload: payload.to_string(),
        context: EventContext::default(),
        request_id: None,
        passthrough_env: Default::default(),
    }
}

fn load_adapter() -> DeclarativeAdapter {
    let manifest = AdapterManifest::parse(BUILTIN).expect("builtin qoder.json parses");
    assert_eq!(manifest.id, "qoder");
    assert!(
        manifest.unverified,
        "qoder is docs-only until a live run confirms it"
    );
    DeclarativeAdapter::new(manifest, "v1".to_string()).expect("v1 exists")
}

#[test]
fn qoder_manifest_permission_request_behavior_dialect() {
    let adapter = load_adapter();

    // PermissionRequest carries tool_name/tool_input; the response uses an
    // allow/deny behavior object (never ask).
    let input = make_input(
        "PermissionRequest",
        r#"{"session_id": "qoder-1", "tool_name": "Bash", "tool_input": {"command": "rm -rf node_modules"}}"#,
    );
    let event = adapter
        .parse_event(&input)
        .expect("parse PermissionRequest");
    assert_eq!(event.session_id, "qoder-1");
    assert!(matches!(
        event.lifecycle,
        AgentLifecycle::PermissionRequest { .. }
    ));

    let allow = adapter.format_response(&PromptDecision::allow());
    assert_eq!(
        allow["hookSpecificOutput"]["decision"]["behavior"],
        serde_json::json!("allow")
    );
    // hookEventName is required on every Qoder hookSpecificOutput.
    assert_eq!(
        allow["hookSpecificOutput"]["hookEventName"],
        serde_json::json!("PermissionRequest")
    );
    let deny = adapter.format_response(&PromptDecision::deny(Some("nope".to_string())));
    assert_eq!(
        deny["hookSpecificOutput"]["decision"]["behavior"],
        serde_json::json!("deny")
    );
}

#[test]
fn qoder_manifest_elicitation_is_observe_only() {
    let adapter = load_adapter();

    // Elicitation speaks accept/decline/cancel, not allow/deny: the adapter
    // maps it to Notification observe and never answers for the user.
    let event = adapter
        .parse_event(&make_input(
            "Elicitation",
            r#"{"session_id": "qoder-1", "mcp_server_name": "my-server", "message": "Please confirm"}"#,
        ))
        .expect("parse Elicitation");
    match event.lifecycle {
        AgentLifecycle::Notification { message, .. } => {
            assert_eq!(message, "Please confirm");
        }
        ref other => panic!("expected Notification, got {other:?}"),
    }
    let v = adapter.format_response(&PromptDecision::allow());
    assert_eq!(v, serde_json::json!({}));
}

#[test]
fn qoder_manifest_hintless_sniffing() {
    let sniff = |payload: &str| {
        AdapterRegistry::detect_from_input(&RawHookInput {
            harness_hint: String::new(),
            event_hint: None,
            raw_payload: payload.to_string(),
            context: EventContext::default(),
            request_id: None,
            passthrough_env: Default::default(),
        })
        .map(|a| a.id().to_string())
    };
    assert_eq!(
        sniff(r#"{"session_id": "q", "mcp_server_name": "s", "permission_suggestions": [], "requested_schema": {}, "elicitation_id": "e"}"#).as_deref(),
        Some("qoder")
    );
    assert_ne!(
        sniff(r#"{"thread_id": "th_123", "hook_event_name": "Stop"}"#).as_deref(),
        Some("qoder")
    );
}

#[test]
fn qoder_manifest_pretooluse_and_registry_pickup() {
    let adapter = load_adapter();

    let input = make_input(
        "PreToolUse",
        r#"{"session_id": "qoder-1", "tool_name": "Edit", "tool_input": {"path": "a.ts"}}"#,
    );
    let event = adapter.parse_event(&input).expect("parse PreToolUse");
    assert_eq!(event.status, NormalizedStatus::Running);
    let deny = adapter.format_response(&PromptDecision::deny(Some("blocked".to_string())));
    assert_eq!(
        deny["hookSpecificOutput"]["permissionDecision"],
        serde_json::json!("deny")
    );

    let reg = AdapterRegistry::get_versioned("qoder", None).expect("qoder registered");
    let stop = reg
        .parse_event(&make_input("Stop", r#"{"session_id": "qoder-3"}"#))
        .expect("registry parse Stop");
    assert!(matches!(stop.lifecycle, AgentLifecycle::Stop { .. }));
}
