use agent_hooks_adapters::declarative::DeclarativeAdapter;
use agent_hooks_adapters::manifest::AdapterManifest;
use agent_hooks_adapters::registry::AdapterRegistry;
use agent_hooks_adapters::HarnessAdapter;
use agent_hooks_protocol::{AgentLifecycle, EventContext, RawHookInput};

const BUILTIN: &str = include_str!("../builtin/kiro/adapter.json");

fn make_input(event_hint: &str, payload: &str) -> RawHookInput {
    RawHookInput {
        harness_hint: "kiro".to_string(),
        event_hint: Some(event_hint.to_string()),
        raw_payload: payload.to_string(),
        context: EventContext::default(),
        request_id: None,
        passthrough_env: Default::default(),
    }
}

fn load_adapter() -> DeclarativeAdapter {
    let manifest = AdapterManifest::parse(BUILTIN).expect("builtin kiro.json parses");
    assert_eq!(manifest.id, "kiro");
    DeclarativeAdapter::new(manifest, "v1".to_string()).expect("v1 exists")
}

#[test]
fn kiro_manifest_gate_pretooluse_dialect() {
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
fn kiro_manifest_prompt_submit_and_stop_continue() {
    let adapter = load_adapter();

    // userPromptSubmit -> UserPromptSubmit (gate)
    let input = make_input(
        "userPromptSubmit",
        r#"{"session_id": "sess-2", "prompt": "Fix bug in auth.ts"}"#,
    );
    let event = adapter.parse_event(&input).expect("parse userPromptSubmit");
    assert_eq!(event.session_id, "sess-2");
    match event.lifecycle {
        AgentLifecycle::UserPromptSubmit { user_prompt } => {
            assert_eq!(user_prompt.as_deref(), Some("Fix bug in auth.ts"));
        }
        ref other => panic!("expected UserPromptSubmit, got {:?}", other),
    }

    // stop -> Stop (continue capability: deny keeps going, allow stops)
    let stop_input = make_input(
        "stop",
        r#"{"session_id": "sess-2", "summary": "All tests pass"}"#,
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
        serde_json::json!({"decision": "block", "reason": "keep going"})
    );
    let done = adapter.format_response(&agent_hooks_protocol::PromptDecision::allow());
    assert_eq!(done, serde_json::json!({}));
}

#[test]
fn kiro_manifest_observe_events_and_registry_pickup() {
    let adapter = load_adapter();

    // agentSpawn -> SessionStart (observe => {} regardless of decision).
    // agentSpawn is the event Kiro fires (B2V legacy agent_config/agents/kiro.rs, bbc7bab46).
    let input = make_input(
        "agentSpawn",
        r#"{"hook_event_name": "agentSpawn", "session_id": "sess-3"}"#,
    );
    let event = adapter.parse_event(&input).expect("parse agentSpawn");
    assert!(matches!(
        event.lifecycle,
        AgentLifecycle::SessionStart { .. }
    ));
    let v = adapter.format_response(&agent_hooks_protocol::PromptDecision::allow());
    assert_eq!(v, serde_json::json!({}));

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

    // Kiro has no notification/sessionEnd hooks; the manifest no longer
    // installs keys Kiro never fires.
    assert!(adapter
        .parse_event(&make_input("notification", r#"{"session_id": "sess-3"}"#))
        .is_err());
    assert!(adapter
        .parse_event(&make_input("SessionStart", r#"{"session_id": "sess-3"}"#))
        .is_err());

    // registry must resolve the builtin manifest, not the legacy Rust adapter:
    // manifest deny carries the reason; legacy Rust deny is bare {"decision":"deny"}
    let reg = AdapterRegistry::get_versioned("kiro", None).expect("kiro registered");
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
        serde_json::json!({"decision": "deny", "reason": "blocked"})
    );
}

// ---------------------------------------------------------------------------
// B2V legacy parity fixtures (ah-command-harnesses)
// ---------------------------------------------------------------------------

use agent_hooks_protocol::{NormalizedStatus, PromptDecision};

// Legacy decide_lifecycle_action: agentspawn → Register; stop → idle.
#[test]
fn kiro_agent_spawn_registers_and_stop_idles() {
    let adapter = load_adapter();
    let spawn = adapter
        .parse_event(&make_input(
            "agentSpawn",
            r#"{"hook_event_name":"agentSpawn","cwd":"/Users/me/proj","session_id":"kiro-1"}"#,
        ))
        .unwrap();
    assert!(matches!(
        spawn.lifecycle,
        AgentLifecycle::SessionStart { .. }
    ));
    assert_eq!(spawn.session_id, "kiro-1");
    assert_eq!(
        spawn.context.cwd.as_deref(),
        Some(std::path::Path::new("/Users/me/proj"))
    );
    let prompt = adapter
        .parse_event(&make_input(
            "userPromptSubmit",
            r#"{"hook_event_name":"userPromptSubmit","cwd":"/Users/me/proj","session_id":"kiro-1","prompt":"hi"}"#,
        ))
        .unwrap();
    assert_eq!(prompt.status, NormalizedStatus::Running);
    let stop = adapter
        .parse_event(&make_input(
            "stop",
            r#"{"hook_event_name":"stop","cwd":"/Users/me/proj","session_id":"kiro-1"}"#,
        ))
        .unwrap();
    assert_eq!(stop.status, NormalizedStatus::Idle);
    let pre = adapter
        .parse_event(&make_input(
            "preToolUse",
            r#"{"hook_event_name":"preToolUse","cwd":"/Users/me/proj","session_id":"kiro-1","tool_name":"execute_bash","tool_input":{"command":"ls"}}"#,
        ))
        .unwrap();
    assert_eq!(pre.status, NormalizedStatus::Running);
    assert_eq!(
        adapter.format_response(&PromptDecision::allow()),
        serde_json::json!({"decision": "allow"})
    );
    assert_eq!(
        adapter.format_response(&PromptDecision::defer()),
        serde_json::json!({})
    );
}

#[test]
fn kiro_installs_the_events_kiro_fires() {
    let manifest = AdapterManifest::parse(BUILTIN).unwrap();
    let events: Vec<&str> = manifest.versions["v1"]
        .events
        .keys()
        .map(String::as_str)
        .collect();
    assert!(events.contains(&"agentSpawn"));
    for bogus in ["SessionStart", "sessionEnd", "notification"] {
        assert!(!events.contains(&bogus), "{bogus} is not a Kiro hook");
    }
}
