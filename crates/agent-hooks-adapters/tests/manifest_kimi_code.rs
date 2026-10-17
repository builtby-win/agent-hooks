//! Kimi Code adapter: docs-derived payloads (Kimi Code hooks reference).
//! No live Kimi run has confirmed these shapes; the manifest carries
//! `unverified: true` until one does.

use agent_hooks_adapters::declarative::DeclarativeAdapter;
use agent_hooks_adapters::manifest::{AdapterManifest, InstallOp, ResponseMode};
use agent_hooks_adapters::registry::AdapterRegistry;
use agent_hooks_adapters::HarnessAdapter;
use agent_hooks_protocol::{
    AgentLifecycle, EventContext, NormalizedStatus, PromptDecision, RawHookInput,
};

const BUILTIN: &str = include_str!("../builtin/kimi-code/adapter.json");

fn make_input(event_hint: &str, payload: &str) -> RawHookInput {
    RawHookInput {
        harness_hint: "kimi-code".to_string(),
        event_hint: Some(event_hint.to_string()),
        raw_payload: payload.to_string(),
        context: EventContext::default(),
        request_id: None,
        passthrough_env: Default::default(),
    }
}

fn load_adapter() -> DeclarativeAdapter {
    let manifest = AdapterManifest::parse(BUILTIN).expect("builtin kimi-code.json parses");
    assert_eq!(manifest.id, "kimi-code");
    assert!(
        manifest.unverified,
        "kimi-code is docs-only until a live run confirms it"
    );
    DeclarativeAdapter::new(manifest, "v1".to_string()).expect("v1 exists")
}

#[test]
fn kimi_manifest_uses_list_op_and_exit2() {
    let manifest = AdapterManifest::parse(BUILTIN).expect("parses");
    assert_eq!(manifest.versions["v1"].response.mode, ResponseMode::Exit2);
    let mut saw_list_op = false;
    for op in &manifest.versions["v1"].install {
        if let InstallOp::TomlListHooks {
            paths,
            list_key,
            entry,
            ..
        } = op
        {
            saw_list_op = true;
            assert_eq!(paths, &vec!["~/.kimi-code/config.toml".to_string()]);
            assert_eq!(list_key, "hooks");
            // Kimi rejects [[hooks]] rows with extra fields: only the four
            // documented keys may appear.
            let obj = entry.as_object().expect("entry is an object");
            for key in obj.keys() {
                assert!(
                    ["event", "matcher", "command", "timeout"].contains(&key.as_str()),
                    "undocumented [[hooks]] key '{key}' would fail config load"
                );
            }
        }
    }
    assert!(saw_list_op, "kimi installs via toml_list_hooks");
}

#[test]
fn kimi_manifest_blockable_events_parse() {
    let adapter = load_adapter();

    // PreToolUse fires before permission checks; blocking stops the tool.
    let pre = adapter
        .parse_event(&make_input(
            "PreToolUse",
            r#"{"hook_event_name": "PreToolUse", "session_id": "kimi-1", "session_title": "fix login", "client_type": "kimi_code_cli", "tool_name": "bash", "tool_input": {"command": "rm -rf /"}}"#,
        ))
        .expect("parse PreToolUse");
    match pre.lifecycle {
        AgentLifecycle::PreToolUse {
            tool_name, input, ..
        } => {
            assert_eq!(tool_name, "bash");
            assert_eq!(
                input.get("command").and_then(|v| v.as_str()),
                Some("rm -rf /")
            );
        }
        ref other => panic!("expected PreToolUse, got {other:?}"),
    }
    assert_eq!(pre.status, NormalizedStatus::Running);
    // exit2 mode: the response body carries no decision; the process exit
    // code is the signal, so both templates stay empty.
    assert_eq!(
        adapter.format_response(&PromptDecision::allow()),
        serde_json::json!({})
    );

    let prompt = adapter
        .parse_event(&make_input(
            "UserPromptSubmit",
            r#"{"hook_event_name": "UserPromptSubmit", "session_id": "kimi-1", "prompt": "hello"}"#,
        ))
        .expect("parse UserPromptSubmit");
    assert!(matches!(
        prompt.lifecycle,
        AgentLifecycle::UserPromptSubmit { .. }
    ));

    // Permission events are observational: no gate, no verdicts.
    let perm = adapter
        .parse_event(&make_input(
            "PermissionRequest",
            r#"{"hook_event_name": "PermissionRequest", "session_id": "kimi-1", "tool_name": "bash"}"#,
        ))
        .expect("parse PermissionRequest");
    assert!(matches!(
        perm.lifecycle,
        AgentLifecycle::PermissionRequest { .. }
    ));
    assert_eq!(
        adapter.format_response(&PromptDecision::allow()),
        serde_json::json!({})
    );
}

#[test]
fn kimi_manifest_hintless_sniffing() {
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
        sniff(r#"{"hook_event_name": "PreToolUse", "session_id": "k", "session_title": "t", "client_type": "kimi_code_cli"}"#)
            .as_deref(),
        Some("kimi-code")
    );
    assert_ne!(
        sniff(r#"{"thread_id": "th_123", "hook_event_name": "Stop"}"#).as_deref(),
        Some("kimi-code")
    );
}

#[test]
fn kimi_manifest_registry_pickup_and_session_lifecycle() {
    let reg = AdapterRegistry::get_versioned("kimi-code", None).expect("kimi-code registered");
    let start = reg
        .parse_event(&make_input(
            "SessionStart",
            r#"{"hook_event_name": "SessionStart", "session_id": "kimi-9", "source": "startup"}"#,
        ))
        .expect("registry parse SessionStart");
    assert!(matches!(
        start.lifecycle,
        AgentLifecycle::SessionStart { .. }
    ));
    let end = reg
        .parse_event(&make_input(
            "SessionEnd",
            r#"{"hook_event_name": "SessionEnd", "session_id": "kimi-9"}"#,
        ))
        .expect("registry parse SessionEnd");
    assert_eq!(end.status, NormalizedStatus::Closed);
}
