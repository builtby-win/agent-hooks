//! Qwen Code adapter: docs-derived payloads (Qwen Code hooks reference).
//! No live Qwen run has confirmed these shapes; the manifest carries
//! `unverified: true` until one does.

use agent_hooks_adapters::declarative::DeclarativeAdapter;
use agent_hooks_adapters::manifest::{AdapterManifest, InstallOp};
use agent_hooks_adapters::registry::AdapterRegistry;
use agent_hooks_adapters::HarnessAdapter;
use agent_hooks_protocol::{
    AgentLifecycle, EventContext, NormalizedStatus, PromptDecision, RawHookInput,
};

const BUILTIN: &str = include_str!("../builtin/qwen-code/adapter.json");

fn make_input(event_hint: &str, payload: &str) -> RawHookInput {
    RawHookInput {
        harness_hint: "qwen-code".to_string(),
        event_hint: Some(event_hint.to_string()),
        raw_payload: payload.to_string(),
        context: EventContext::default(),
        request_id: None,
        passthrough_env: Default::default(),
    }
}

fn load_adapter() -> DeclarativeAdapter {
    let manifest = AdapterManifest::parse(BUILTIN).expect("builtin qwen-code.json parses");
    assert_eq!(manifest.id, "qwen-code");
    assert!(
        manifest.unverified,
        "qwen-code is docs-only until a live run confirms it"
    );
    DeclarativeAdapter::new(manifest, "v1".to_string()).expect("v1 exists")
}

#[test]
fn qwen_manifest_pretooluse_gate_dialect() {
    let adapter = load_adapter();

    // Qwen tool ids (write_file, run_shell_command, ...) ride the same
    // hookSpecificOutput.permissionDecision contract as Claude.
    let input = make_input(
        "PreToolUse",
        r#"{"session_id": "qwen-1", "tool_name": "run_shell_command", "tool_input": {"command": "rm -rf /tmp/x"}}"#,
    );
    let event = adapter.parse_event(&input).expect("parse PreToolUse");
    assert_eq!(event.session_id, "qwen-1");
    assert_eq!(event.status, NormalizedStatus::Running);
    match event.lifecycle {
        AgentLifecycle::PreToolUse {
            tool_name, input, ..
        } => {
            assert_eq!(tool_name, "run_shell_command");
            assert_eq!(
                input.get("command").and_then(|v| v.as_str()),
                Some("rm -rf /tmp/x")
            );
        }
        ref other => panic!("expected PreToolUse, got {other:?}"),
    }

    let allow = adapter.format_response(&PromptDecision::allow());
    assert_eq!(
        allow["hookSpecificOutput"]["permissionDecision"],
        serde_json::json!("allow")
    );
    // Plain allow carries no additionalContext.
    assert!(allow["hookSpecificOutput"]
        .get("additionalContext")
        .is_none());
    let deny = adapter.format_response(&PromptDecision::deny(Some("destructive".to_string())));
    assert_eq!(
        deny["hookSpecificOutput"]["permissionDecision"],
        serde_json::json!("deny")
    );
}

#[test]
fn qwen_manifest_timeouts_stay_below_millisecond_quirk() {
    // Qwen reads command-hook timeouts >= 1000 as milliseconds. Every
    // installed timeout must stay a small seconds value.
    let manifest = AdapterManifest::parse(BUILTIN).expect("parses");
    let vs = &manifest.versions["v1"];
    for op in &vs.install {
        if let InstallOp::JsonHooks {
            entry,
            entry_overrides,
            ..
        } = op
        {
            let check = |label: &str, v: &serde_json::Value| {
                if let Some(t) = v.get("timeout").and_then(|t| t.as_u64()) {
                    assert!(
                        t < 1000,
                        "{label} timeout {t} would read as milliseconds"
                    );
                }
            };
            check("entry", entry);
            for (event, o) in entry_overrides {
                check(&format!("override {event}"), o);
            }
        }
    }
}

#[test]
fn qwen_manifest_hintless_sniffing() {
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
        sniff(r#"{"session_id": "q", "tool_use_id": "toolu_1", "duration_ms": 12}"#).as_deref(),
        Some("qwen-code")
    );
    assert_ne!(
        sniff(r#"{"thread_id": "th_123", "hook_event_name": "Stop"}"#).as_deref(),
        Some("qwen-code")
    );
}

#[test]
fn qwen_manifest_lifecycle_registry_and_batch_observe() {
    let adapter = load_adapter();

    let prompt = adapter
        .parse_event(&make_input(
            "UserPromptSubmit",
            r#"{"session_id": "qwen-1", "prompt": "refactor this"}"#,
        ))
        .expect("parse UserPromptSubmit");
    match prompt.lifecycle {
        AgentLifecycle::UserPromptSubmit { user_prompt } => {
            assert_eq!(user_prompt.as_deref(), Some("refactor this"));
        }
        ref other => panic!("expected UserPromptSubmit, got {other:?}"),
    }

    // PostToolBatch is observe-only: its decision output format is not
    // documented, so the adapter never blocks on it.
    let batch = adapter
        .parse_event(&make_input(
            "PostToolBatch",
            r#"{"session_id": "qwen-1"}"#,
        ))
        .expect("parse PostToolBatch");
    assert!(matches!(
        batch.lifecycle,
        AgentLifecycle::PostToolBatch
    ));
    let v = adapter.format_response(&PromptDecision::allow());
    assert_eq!(v, serde_json::json!({}));

    let reg = AdapterRegistry::get_versioned("qwen-code", None).expect("qwen-code registered");
    let failure = reg
        .parse_event(&make_input(
            "StopFailure",
            r#"{"session_id": "qwen-2", "error": "loop_detected"}"#,
        ))
        .expect("registry parse StopFailure");
    assert!(matches!(
        failure.lifecycle,
        AgentLifecycle::StopFailure { .. }
    ));
}
