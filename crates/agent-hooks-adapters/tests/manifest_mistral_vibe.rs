//! Mistral Vibe adapter: docs-derived payloads (Mistral Vibe hooks reference).
//! No live Vibe run has confirmed these shapes; the manifest carries
//! `unverified: true` until one does.

use agent_hooks_adapters::declarative::DeclarativeAdapter;
use agent_hooks_adapters::manifest::{AdapterManifest, InstallOp};
use agent_hooks_adapters::registry::AdapterRegistry;
use agent_hooks_adapters::HarnessAdapter;
use agent_hooks_protocol::{
    AgentLifecycle, EventContext, NormalizedStatus, PromptDecision, RawHookInput,
};

const BUILTIN: &str = include_str!("../builtin/mistral-vibe/adapter.json");

fn make_input(event_hint: &str, payload: &str) -> RawHookInput {
    RawHookInput {
        harness_hint: "mistral-vibe".to_string(),
        event_hint: Some(event_hint.to_string()),
        raw_payload: payload.to_string(),
        context: EventContext::default(),
        request_id: None,
        passthrough_env: Default::default(),
    }
}

fn load_adapter() -> DeclarativeAdapter {
    let manifest = AdapterManifest::parse(BUILTIN).expect("builtin mistral-vibe.json parses");
    assert_eq!(manifest.id, "mistral-vibe");
    assert!(
        manifest.unverified,
        "mistral-vibe is docs-only until a live run confirms it"
    );
    DeclarativeAdapter::new(manifest, "v1".to_string()).expect("v1 exists")
}

#[test]
fn mistral_manifest_list_rows_carry_hook_types() {
    let manifest = AdapterManifest::parse(BUILTIN).expect("parses");
    let vs = &manifest.versions["v1"];
    let Some((entry, overrides)) = vs.install.iter().find_map(|op| match op {
        InstallOp::TomlListHooks {
            entry,
            entry_overrides,
            ..
        } => Some((entry, entry_overrides)),
        _ => None,
    }) else {
        panic!("mistral installs via toml_list_hooks");
    };
    // One named row per installed event; the hook type comes from the
    // per-event override, defaulting to pre_tool.
    assert_eq!(
        entry["type"].as_str(),
        Some("pre_tool"),
        "default row type is pre_tool"
    );
    assert_eq!(overrides["PostToolUse"]["type"].as_str(), Some("post_tool"));
    assert_eq!(overrides["Stop"]["type"].as_str(), Some("post_agent"));
    for (event, o) in overrides {
        assert!(
            o.get("name")
                .and_then(|n| n.as_str())
                .is_some_and(|n| n.starts_with("agent-hooks-")),
            "row {event} needs a unique name for project/user dedupe"
        );
    }
}

#[test]
fn mistral_manifest_pre_tool_gate_dialect() {
    let adapter = load_adapter();

    // pre_tool fires per tool call before the permission prompt, with the
    // model's raw arguments available for rewrite.
    let input = make_input(
        "pre_tool",
        r#"{"session_id": "vibe-1", "tool_name": "bash", "tool_call_id": "call_1", "tool_input": {"command": "rm -rf node_modules"}}"#,
    );
    let event = adapter.parse_event(&input).expect("parse pre_tool");
    assert_eq!(event.session_id, "vibe-1");
    assert_eq!(event.status, NormalizedStatus::Running);
    match event.lifecycle {
        AgentLifecycle::PreToolUse {
            tool_name, input, ..
        } => {
            assert_eq!(tool_name, "bash");
            assert_eq!(
                input.get("command").and_then(|v| v.as_str()),
                Some("rm -rf node_modules")
            );
        }
        ref other => panic!("expected PreToolUse, got {other:?}"),
    }

    // decision allow/deny with snake_case fields. Allow echoes the
    // original tool_input back as hook_specific_output.tool_input: a
    // full-replacement rewrite with identical arguments (a no-op
    // rewrite, mirroring the documented rewrite path).
    let allow = adapter.format_response(&PromptDecision::allow());
    assert_eq!(
        allow["hook_specific_output"]["tool_input"],
        serde_json::json!({"command": "rm -rf node_modules"})
    );
    let deny = adapter.format_response(&PromptDecision::deny(Some("dangerous".to_string())));
    assert_eq!(deny["decision"], serde_json::json!("deny"));
    assert_eq!(deny["reason"], serde_json::json!("dangerous"));
}

#[test]
fn mistral_manifest_hintless_sniffing() {
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
        sniff(r#"{"session_id": "v", "tool_name": "bash", "tool_call_id": "c", "tool_status": "success", "tool_output_text": "ok"}"#).as_deref(),
        Some("mistral-vibe")
    );
    assert_ne!(
        sniff(r#"{"thread_id": "th_123", "hook_event_name": "Stop"}"#).as_deref(),
        Some("mistral-vibe")
    );
}

#[test]
fn mistral_manifest_post_tool_and_agent_registry() {
    let reg =
        AdapterRegistry::get_versioned("mistral-vibe", None).expect("mistral-vibe registered");

    let post = reg
        .parse_event(&make_input(
            "post_tool",
            r#"{"session_id": "vibe-1", "tool_name": "bash", "tool_call_id": "call_1", "tool_status": "success", "tool_output_text": "done"}"#,
        ))
        .expect("registry parse post_tool");
    match post.lifecycle {
        AgentLifecycle::PostToolUse {
            tool_name,
            output_preview,
        } => {
            assert_eq!(tool_name, "bash");
            assert_eq!(output_preview.as_deref(), Some("done"));
        }
        ref other => panic!("expected PostToolUse, got {other:?}"),
    }

    let agent = reg
        .parse_event(&make_input("post_agent", r#"{"session_id": "vibe-1"}"#))
        .expect("registry parse post_agent");
    assert!(matches!(agent.lifecycle, AgentLifecycle::Stop { .. }));
}
