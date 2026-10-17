//! Factory Droid adapter: docs-derived payloads (Factory hooks reference).
//! No live Droid run has confirmed these shapes; the manifest carries
//! `unverified: true` until one does.

use agent_hooks_adapters::declarative::DeclarativeAdapter;
use agent_hooks_adapters::manifest::AdapterManifest;
use agent_hooks_adapters::registry::AdapterRegistry;
use agent_hooks_adapters::HarnessAdapter;
use agent_hooks_protocol::{
    AgentLifecycle, EventContext, NormalizedStatus, PromptDecision, RawHookInput,
};

const BUILTIN: &str = include_str!("../builtin/droid/adapter.json");

fn make_input(event_hint: &str, payload: &str) -> RawHookInput {
    RawHookInput {
        harness_hint: "droid".to_string(),
        event_hint: Some(event_hint.to_string()),
        raw_payload: payload.to_string(),
        context: EventContext::default(),
        request_id: None,
        passthrough_env: Default::default(),
    }
}

fn load_adapter() -> DeclarativeAdapter {
    let manifest = AdapterManifest::parse(BUILTIN).expect("builtin droid.json parses");
    assert_eq!(manifest.id, "droid");
    assert!(
        manifest.unverified,
        "droid is docs-only until a live run confirms it"
    );
    DeclarativeAdapter::new(manifest, "v1".to_string()).expect("v1 exists")
}

#[test]
fn droid_manifest_pretooluse_gate_dialect() {
    let adapter = load_adapter();

    // PreToolUse with Droid tool matchers (Execute/Read/Edit/Create).
    let input = make_input(
        "PreToolUse",
        r#"{"session_id": "droid-1", "tool_name": "Execute", "tool_input": {"command": "git status"}, "cwd": "/repo"}"#,
    );
    let event = adapter.parse_event(&input).expect("parse PreToolUse");
    assert_eq!(event.session_id, "droid-1");
    assert_eq!(event.status, NormalizedStatus::Running);
    match event.lifecycle {
        AgentLifecycle::PreToolUse {
            tool_name, input, ..
        } => {
            assert_eq!(tool_name, "Execute");
            assert_eq!(
                input.get("command").and_then(|v| v.as_str()),
                Some("git status")
            );
        }
        ref other => panic!("expected PreToolUse, got {other:?}"),
    }

    // Gate dialect: hookSpecificOutput with hookEventName + permissionDecision.
    let allow = adapter.format_response(&PromptDecision::allow());
    assert_eq!(
        allow,
        serde_json::json!({"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": "allow"}})
    );
    let deny = adapter.format_response(&PromptDecision::deny(Some("risky".to_string())));
    assert_eq!(
        deny["hookSpecificOutput"]["permissionDecision"],
        serde_json::json!("deny")
    );
    assert_eq!(
        deny["hookSpecificOutput"]["hookEventName"],
        serde_json::json!("PreToolUse")
    );
}

#[test]
fn droid_manifest_lifecycle_and_stop_continue() {
    let adapter = load_adapter();

    let start = adapter
        .parse_event(&make_input(
            "SessionStart",
            r#"{"session_id": "droid-1", "source": "startup"}"#,
        ))
        .expect("parse SessionStart");
    assert!(matches!(
        start.lifecycle,
        AgentLifecycle::SessionStart { .. }
    ));
    assert_eq!(start.status, NormalizedStatus::Idle);

    let prompt = adapter
        .parse_event(&make_input(
            "UserPromptSubmit",
            r#"{"session_id": "droid-1", "prompt": "fix the build"}"#,
        ))
        .expect("parse UserPromptSubmit");
    match prompt.lifecycle {
        AgentLifecycle::UserPromptSubmit { user_prompt } => {
            assert_eq!(user_prompt.as_deref(), Some("fix the build"));
        }
        ref other => panic!("expected UserPromptSubmit, got {other:?}"),
    }

    let stop = adapter
        .parse_event(&make_input("Stop", r#"{"session_id": "droid-1"}"#))
        .expect("parse Stop");
    assert!(matches!(stop.lifecycle, AgentLifecycle::Stop { .. }));
    // continue capability: deny keeps going, allow stops.
    let keep_going = adapter.format_response(&PromptDecision::deny(Some("check lint".to_string())));
    assert_eq!(
        keep_going,
        serde_json::json!({"decision": "block", "reason": "check lint"})
    );

    let end = adapter
        .parse_event(&make_input(
            "SessionEnd",
            r#"{"session_id": "droid-1", "reason": "other"}"#,
        ))
        .expect("parse SessionEnd");
    assert_eq!(end.status, NormalizedStatus::Closed);
}

#[test]
fn droid_manifest_hintless_sniffing() {
    // Distinctive Droid keys resolve without a harness hint; a generic
    // Claude-shaped payload must not route here.
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
        sniff(r#"{"session_id": "d", "task_name": "build", "task_result": "ok"}"#).as_deref(),
        Some("droid")
    );
    assert_ne!(
        sniff(r#"{"thread_id": "th_123", "hook_event_name": "Stop"}"#).as_deref(),
        Some("droid")
    );
}

#[test]
fn droid_manifest_install_root_and_registry_pickup() {
    let manifest = AdapterManifest::parse(BUILTIN).expect("parses");
    let vs = &manifest.versions["v1"];
    // Standalone hooks.json files are keyed directly by event name.
    let op = vs.install.iter().find_map(|op| match op {
        agent_hooks_adapters::manifest::InstallOp::JsonHooks { root, .. } => Some(root.clone()),
        _ => None,
    });
    assert_eq!(op.as_deref(), Some("{event}"));

    let reg = AdapterRegistry::get_versioned("droid", None).expect("droid registered");
    let event = reg
        .parse_event(&make_input(
            "Notification",
            r#"{"session_id": "droid-2", "message": "done", "notification_type": "idle_prompt"}"#,
        ))
        .expect("registry parse Notification");
    match event.lifecycle {
        AgentLifecycle::Notification { message, .. } => {
            assert_eq!(message, "done");
        }
        ref other => panic!("expected Notification, got {other:?}"),
    }
}
