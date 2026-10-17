use agent_hooks_adapters::declarative::DeclarativeAdapter;
use agent_hooks_adapters::manifest::AdapterManifest;
use agent_hooks_adapters::registry::AdapterRegistry;
use agent_hooks_adapters::HarnessAdapter;
use agent_hooks_protocol::{AgentLifecycle, EventContext, RawHookInput};

const BUILTIN: &str = include_str!("../builtin/cursor/adapter.json");

fn make_input(event_hint: &str, payload: &str) -> RawHookInput {
    RawHookInput {
        harness_hint: "cursor".to_string(),
        event_hint: Some(event_hint.to_string()),
        raw_payload: payload.to_string(),
        context: EventContext::default(),
        request_id: None,
        passthrough_env: Default::default(),
    }
}

fn load_adapter() -> DeclarativeAdapter {
    let manifest = AdapterManifest::parse(BUILTIN).expect("builtin cursor.json parses");
    assert_eq!(manifest.id, "cursor");
    DeclarativeAdapter::new(manifest, "v1".to_string()).expect("v1 exists")
}

#[test]
fn cursor_manifest_gate_pretooluse_dialect() {
    let adapter = load_adapter();

    // preToolUse with conversation_id session key + flat tool fields
    let input = make_input(
        "preToolUse",
        r#"{"conversation_id": "conv-1", "tool_name": "Bash", "tool_input": {"command": "cargo test"}}"#,
    );
    let event = adapter.parse_event(&input).expect("parse preToolUse");
    assert_eq!(event.session_id, "conv-1");
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

    // gate dialect: permission allow/deny (NOT legacy {"decision": ...})
    let allow = adapter.format_response(&agent_hooks_protocol::PromptDecision::allow());
    assert_eq!(allow, serde_json::json!({"permission": "allow"}));

    let deny = adapter.format_response(&agent_hooks_protocol::PromptDecision::deny(Some(
        "need approval".to_string(),
    )));
    assert_eq!(
        deny,
        serde_json::json!({
            "permission": "deny",
            "user_message": "need approval",
            "agent_message": "need approval"
        })
    );
}

#[test]
fn cursor_manifest_prompt_submit_override_and_stop_continue() {
    let adapter = load_adapter();

    // beforeSubmitPrompt -> UserPromptSubmit (gate with per-event override)
    let input = make_input(
        "beforeSubmitPrompt",
        r#"{"conversation_id": "conv-2", "prompt": "Fix bug in auth.ts"}"#,
    );
    let event = adapter
        .parse_event(&input)
        .expect("parse beforeSubmitPrompt");
    assert_eq!(event.session_id, "conv-2");
    match event.lifecycle {
        AgentLifecycle::UserPromptSubmit { user_prompt } => {
            assert_eq!(user_prompt.as_deref(), Some("Fix bug in auth.ts"));
        }
        ref other => panic!("expected UserPromptSubmit, got {:?}", other),
    }

    // override dialect: {continue, user_message}, not the plain gate shape
    let allow = adapter.format_response(&agent_hooks_protocol::PromptDecision::allow());
    assert_eq!(allow, serde_json::json!({"continue": true}));
    let deny = adapter.format_response(&agent_hooks_protocol::PromptDecision::deny(Some(
        "off-topic".to_string(),
    )));
    assert_eq!(
        deny,
        serde_json::json!({"continue": false, "user_message": "off-topic"})
    );

    // stop -> Stop (continue capability: deny keeps going, allow stops)
    let stop_input = make_input(
        "stop",
        r#"{"conversation_id": "conv-2", "summary": "All tests pass"}"#,
    );
    let stop_event = adapter.parse_event(&stop_input).expect("parse stop");
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
        serde_json::json!({"followup_message": "keep going"})
    );
    let done = adapter.format_response(&agent_hooks_protocol::PromptDecision::allow());
    assert_eq!(done, serde_json::json!({}));
}

#[test]
fn cursor_manifest_observe_events_and_registry_pickup() {
    let adapter = load_adapter();

    // sessionStart -> SessionStart (observe => legacy status stdout
    // {"decision":"allow"} regardless of decision)
    let input = make_input("sessionStart", r#"{"conversation_id": "conv-3"}"#);
    let event = adapter.parse_event(&input).expect("parse sessionStart");
    assert!(matches!(
        event.lifecycle,
        AgentLifecycle::SessionStart { .. }
    ));
    let v = adapter.format_response(&agent_hooks_protocol::PromptDecision::allow());
    assert_eq!(v, serde_json::json!({"decision": "allow"}));

    // postToolUse -> PostToolUse (observe)
    let post_input = make_input(
        "postToolUse",
        r#"{"conversation_id": "conv-3", "tool_name": "Bash", "tool_output": "ok"}"#,
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

    // registry must resolve the builtin manifest, not the legacy Rust adapter:
    // gate-deny output is the permission dialect, not legacy {"decision":"deny"}
    let reg = AdapterRegistry::get_versioned("cursor", None).expect("cursor registered");
    let gate_input = make_input(
        "preToolUse",
        r#"{"conversation_id": "conv-9", "tool_name": "Bash", "tool_input": {}}"#,
    );
    reg.parse_event(&gate_input)
        .expect("registry parse preToolUse");
    let deny = reg.format_response(&agent_hooks_protocol::PromptDecision::deny(Some(
        "blocked".to_string(),
    )));
    assert_ne!(deny, serde_json::json!({"decision": "deny"}));
    assert_eq!(
        deny.get("permission").and_then(|v| v.as_str()),
        Some("deny")
    );
}

// ---------------------------------------------------------------------------
// B2V legacy parity fixtures (ah-command-harnesses)
// ---------------------------------------------------------------------------

use agent_hooks_protocol::{NormalizedStatus, PromptDecision};
use std::path::Path;

// Legacy src-b2v/src/hooks/common.rs extract_hook_cwd: workspace_roots[0] wins for Cursor.
#[test]
fn cursor_cwd_comes_from_workspace_roots() {
    let adapter = load_adapter();
    let event = adapter
        .parse_event(&make_input(
            "beforeSubmitPrompt",
            r#"{"conversation_id":"cur-1","generation_id":"gen-1","hook_event_name":"beforeSubmitPrompt","prompt":"fix it","cwd":"/Applications/Cursor.app","workspace_roots":["/Users/me/proj","/Users/me/other"],"cursor_version":"1.7.2"}"#,
        ))
        .unwrap();
    assert_eq!(event.session_id, "cur-1");
    assert_eq!(
        event.context.cwd.as_deref(),
        Some(Path::new("/Users/me/proj"))
    );
    assert_eq!(event.status, NormalizedStatus::Running);
    assert_eq!(
        adapter.format_response(&PromptDecision::allow()),
        serde_json::json!({"continue": true})
    );
    // No workspace_roots: fall back to the payload cwd.
    let event = adapter
        .parse_event(&make_input(
            "beforeSubmitPrompt",
            r#"{"conversation_id":"cur-1","hook_event_name":"beforeSubmitPrompt","prompt":"x","cwd":"/Users/me/fallback"}"#,
        ))
        .unwrap();
    assert_eq!(
        event.context.cwd.as_deref(),
        Some(Path::new("/Users/me/fallback"))
    );
}

// Legacy decide_status: beforeshellexecution / beforemcpexecution → busy, never waiting.
#[test]
fn cursor_shell_and_mcp_gates_are_busy_not_waiting() {
    let adapter = load_adapter();
    for (event_name, raw) in [
        (
            "beforeShellExecution",
            r#"{"conversation_id":"cur-2","generation_id":"g","hook_event_name":"beforeShellExecution","command":"npm run build","cwd":"/Users/me/proj","sandbox":false,"workspace_roots":["/Users/me/proj"]}"#,
        ),
        (
            "beforeMCPExecution",
            r#"{"conversation_id":"cur-2","generation_id":"g","hook_event_name":"beforeMCPExecution","tool_name":"linear.create_issue","tool_input":"{}","workspace_roots":["/Users/me/proj"]}"#,
        ),
    ] {
        let event = adapter.parse_event(&make_input(event_name, raw)).unwrap();
        assert!(matches!(event.lifecycle, AgentLifecycle::PreToolUse { .. }));
        assert_eq!(event.status, NormalizedStatus::Running, "{event_name}");
        assert_eq!(
            adapter.format_response(&PromptDecision::allow()),
            serde_json::json!({"permission": "allow"})
        );
        assert_eq!(
            adapter.format_response(&PromptDecision::deny(Some("nope".into()))),
            serde_json::json!({"permission": "deny", "user_message": "nope", "agent_message": "nope"})
        );
        assert_eq!(
            adapter.format_response(&PromptDecision::defer()),
            serde_json::json!({})
        );
    }
}

// Legacy decide_lifecycle_action: ("cursor", "sessionend") => None; decide_status
// yields None for afterAgentResponse/afterAgentThought/preCompact → TurnEnd noise.
#[test]
fn cursor_status_neutral_events_map_to_turn_end() {
    let adapter = load_adapter();
    for event_name in [
        "sessionEnd",
        "afterAgentResponse",
        "afterAgentThought",
        "preCompact",
    ] {
        let raw = format!(
            r#"{{"conversation_id":"cur-3","hook_event_name":"{event_name}","text":"done","reason":"user_close","workspace_roots":["/Users/me/proj"]}}"#
        );
        let event = adapter.parse_event(&make_input(event_name, &raw)).unwrap();
        assert!(
            matches!(event.lifecycle, AgentLifecycle::TurnEnd { .. }),
            "{event_name}: {:?}",
            event.lifecycle
        );
        assert_eq!(event.status, NormalizedStatus::Running);
    }
    let stop = adapter
        .parse_event(&make_input(
            "stop",
            r#"{"conversation_id":"cur-3","hook_event_name":"stop","status":"completed","loop_count":0}"#,
        ))
        .unwrap();
    assert_eq!(stop.status, NormalizedStatus::Idle);
}

// Live parity e2e (cursor): legacy mod.rs immediate_status_stdout printed
// {"decision":"allow"} on status hooks (agent_requires_allow_decision).
// Tool gates stay "no decision" on defer — an intentional divergence so
// Cursor's own confirmation still runs.
#[test]
fn cursor_status_hooks_print_legacy_allow_but_gates_defer_to_cursor() {
    let adapter = load_adapter();
    for (event_name, raw) in [
        (
            "beforeSubmitPrompt",
            r#"{"conversation_id":"parity-cursor-0001","hook_event_name":"beforeSubmitPrompt","prompt":"list the files","workspace_roots":["/tmp/p"]}"#,
        ),
        (
            "stop",
            r#"{"conversation_id":"parity-cursor-0001","hook_event_name":"stop","status":"completed","loop_count":0}"#,
        ),
        (
            "sessionStart",
            r#"{"conversation_id":"parity-cursor-0001","hook_event_name":"sessionStart"}"#,
        ),
        (
            "postToolUse",
            r#"{"conversation_id":"parity-cursor-0001","hook_event_name":"postToolUse","tool_name":"Shell"}"#,
        ),
    ] {
        let event = adapter.parse_event(&make_input(event_name, raw)).unwrap();
        if event_name == "beforeSubmitPrompt" {
            assert!(matches!(
                event.lifecycle,
                AgentLifecycle::UserPromptSubmit { .. }
            ));
            assert_eq!(event.status, NormalizedStatus::Running);
        }
        assert_eq!(
            adapter.format_response(&PromptDecision::defer()),
            serde_json::json!({"decision": "allow"}),
            "{event_name}"
        );
    }
    for (event_name, raw) in [
        (
            "beforeShellExecution",
            r#"{"conversation_id":"parity-cursor-0001","hook_event_name":"beforeShellExecution","command":"ls","cwd":"/tmp/p"}"#,
        ),
        (
            "beforeMCPExecution",
            r#"{"conversation_id":"parity-cursor-0001","hook_event_name":"beforeMCPExecution","tool_name":"x"}"#,
        ),
        (
            "subagentStart",
            r#"{"conversation_id":"parity-cursor-0001","hook_event_name":"subagentStart"}"#,
        ),
    ] {
        adapter.parse_event(&make_input(event_name, raw)).unwrap();
        assert_eq!(
            adapter.format_response(&PromptDecision::defer()),
            serde_json::json!({}),
            "{event_name}"
        );
    }
}
