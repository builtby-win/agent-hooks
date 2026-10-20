use agent_hooks_adapters::manifest::AdapterManifest;
use agent_hooks_adapters::registry::AdapterRegistry;
use agent_hooks_adapters::{declarative::DeclarativeAdapter, HarnessAdapter};
use agent_hooks_protocol::{AgentLifecycle, EventContext, RawHookInput};

const BUILTIN: &str = include_str!("../builtin/pi/adapter.json");

fn make_input(event_hint: &str, payload: &str) -> RawHookInput {
    RawHookInput {
        harness_hint: "pi".to_string(),
        event_hint: Some(event_hint.to_string()),
        raw_payload: payload.to_string(),
        context: EventContext::default(),
        request_id: Some("req-pi-1".to_string()),
        passthrough_env: Default::default(),
    }
}

fn manifest_adapter() -> DeclarativeAdapter {
    let manifest = AdapterManifest::parse(BUILTIN).expect("builtin pi manifest parses");
    assert_eq!(manifest.id, "pi");
    DeclarativeAdapter::new(manifest, "v1".to_string()).expect("create pi adapter")
}

#[test]
fn builtin_pi_manifest_parses_with_expected_shape() {
    let manifest = AdapterManifest::parse(BUILTIN).expect("builtin pi manifest parses");
    assert_eq!(manifest.default_version(), "v1");
    assert_eq!(manifest.aliases, vec!["pi-agent", "pi_agent"]);
    let vs = manifest.get_version("v1").expect("v1 exists");
    // every host event the Rust adapter handles (canonical + aliases)
    for ev in [
        "SessionStart",
        "SessionEnd",
        "sessionshutdown",
        "UserPromptSubmit",
        "promptsubmit",
        "beforesubmitprompt",
        "agentstart",
        "userbash",
        "userpython",
        "PreToolUse",
        "beforetool",
        "toolstart",
        "toolcall",
        "toolexecutionstart",
        "PostToolUse",
        "aftertool",
        "toolend",
        "toolresult",
        "toolexecutionend",
        "PermissionRequest",
        "toolapprovalrequested",
        "approvalrequested",
        "Notification",
        "Stop",
        "afteragent",
        "agentsettled",
        "TurnStart",
        "TurnEnd",
        "SubagentStart",
        "SubagentStop",
    ] {
        assert!(vs.events.contains_key(ev), "missing event {ev}");
    }
    // pi-native install: stage the drop-in extension file, no registration command
    assert_eq!(vs.install.len(), 1);
}

#[test]
fn pi_parse_event_gate_observe_continue() {
    let adapter = manifest_adapter();

    // gate: omp-native tool_call -> PreToolUse
    let input = make_input(
        "tool_call",
        r#"{"session_id": "ses_pi_1", "tool_name": "Bash", "tool_input": {"command": "ls"}}"#,
    );
    let event = adapter.parse_event(&input).expect("parse tool_call");
    assert_eq!(event.session_id, "ses_pi_1");
    match event.lifecycle {
        AgentLifecycle::PreToolUse {
            tool_name, input, ..
        } => {
            assert_eq!(tool_name, "Bash");
            assert_eq!(input.get("command").and_then(|v| v.as_str()), Some("ls"));
        }
        ref other => panic!("expected PreToolUse, got {other:?}"),
    }

    // gate: omp-native agent_start -> UserPromptSubmit
    let input = make_input(
        "agent_start",
        r#"{"session_id": "ses_pi_1", "message": "hello pi"}"#,
    );
    let event = adapter.parse_event(&input).expect("parse agent_start");
    match event.lifecycle {
        AgentLifecycle::UserPromptSubmit { user_prompt } => {
            assert_eq!(user_prompt.as_deref(), Some("hello pi"));
        }
        ref other => panic!("expected UserPromptSubmit, got {other:?}"),
    }

    // gate: PermissionRequest via tool_approval_requested
    let input = make_input(
        "tool_approval_requested",
        r#"{"session_id": "ses_pi_1", "tool_name": "Bash", "description": "run ls"}"#,
    );
    let event = adapter
        .parse_event(&input)
        .expect("parse tool_approval_requested");
    match event.lifecycle {
        AgentLifecycle::PermissionRequest { prompt } => {
            assert_eq!(prompt.tool_name.as_deref(), Some("Bash"));
            assert_eq!(prompt.description.as_deref(), Some("run ls"));
        }
        ref other => panic!("expected PermissionRequest, got {other:?}"),
    }

    // observe: omp-native tool_result -> PostToolUse
    let input = make_input(
        "tool_result",
        r#"{"session_id": "ses_pi_1", "tool_name": "Bash", "output": "ok"}"#,
    );
    let event = adapter.parse_event(&input).expect("parse tool_result");
    match event.lifecycle {
        AgentLifecycle::PostToolUse {
            tool_name,
            output_preview,
        } => {
            assert_eq!(tool_name, "Bash");
            assert_eq!(output_preview.as_deref(), Some("ok"));
        }
        ref other => panic!("expected PostToolUse, got {other:?}"),
    }

    // observe: omp-native session_shutdown -> SessionEnd
    let input = make_input("session_shutdown", r#"{"session_id": "ses_pi_1"}"#);
    let event = adapter.parse_event(&input).expect("parse session_shutdown");
    match event.lifecycle {
        AgentLifecycle::SessionEnd { .. } => {}
        ref other => panic!("expected SessionEnd, got {other:?}"),
    }

    // observe: Notification
    let input = make_input(
        "Notification",
        r#"{"session_id": "ses_pi_1", "message": "deploy done"}"#,
    );
    let event = adapter.parse_event(&input).expect("parse Notification");
    match event.lifecycle {
        AgentLifecycle::Notification { message, .. } => {
            assert_eq!(message, "deploy done");
        }
        ref other => panic!("expected Notification, got {other:?}"),
    }

    // turns are busy signals, not boundaries
    let input = make_input("turn_start", r#"{"session_id": "ses_pi_1"}"#);
    let event = adapter.parse_event(&input).expect("parse turn_start");
    match event.lifecycle {
        AgentLifecycle::TurnStart { .. } => {}
        ref other => panic!("expected TurnStart, got {other:?}"),
    }
    let input = make_input("turn_end", r#"{"session_id": "ses_pi_1"}"#);
    let event = adapter.parse_event(&input).expect("parse turn_end");
    match event.lifecycle {
        AgentLifecycle::TurnEnd { .. } => {}
        ref other => panic!("expected TurnEnd, got {other:?}"),
    }

    // gate/continue: subagent pair
    let input = make_input(
        "SubagentStart",
        r#"{"session_id": "ses_pi_1", "agent_id": "sub-1", "description": "helper"}"#,
    );
    let event = adapter.parse_event(&input).expect("parse SubagentStart");
    match event.lifecycle {
        AgentLifecycle::SubagentStart {
            subagent_id,
            description,
            ..
        } => {
            assert_eq!(subagent_id, "sub-1");
            assert_eq!(description.as_deref(), Some("helper"));
        }
        ref other => panic!("expected SubagentStart, got {other:?}"),
    }
    let input = make_input(
        "SubagentStop",
        r#"{"session_id": "ses_pi_1", "agent_id": "sub-1", "success": true, "summary": "done"}"#,
    );
    let event = adapter.parse_event(&input).expect("parse SubagentStop");
    match event.lifecycle {
        AgentLifecycle::SubagentStop {
            subagent_id,
            success,
            summary,
        } => {
            assert_eq!(subagent_id, "sub-1");
            assert!(success);
            assert_eq!(summary.as_deref(), Some("done"));
        }
        ref other => panic!("expected SubagentStop, got {other:?}"),
    }

    // continue: omp-native agent_settled -> Stop
    let input = make_input(
        "agent_settled",
        r#"{"session_id": "ses_pi_1", "summary": "all done"}"#,
    );
    let event = adapter.parse_event(&input).expect("parse agent_settled");
    match event.lifecycle {
        AgentLifecycle::Stop { summary, success } => {
            assert_eq!(summary.as_deref(), Some("all done"));
            assert!(success);
        }
        ref other => panic!("expected Stop, got {other:?}"),
    }
}

#[test]
fn pi_stop_carries_last_assistant_message_as_summary() {
    let adapter = manifest_adapter();
    for ev in ["agent_settled", "Stop"] {
        let input = make_input(
            ev,
            r#"{"session_id": "ses_pi_1", "last_assistant_message": "All done."}"#,
        );
        let event = adapter.parse_event(&input).expect("parse stop signal");
        match event.lifecycle {
            AgentLifecycle::Stop { summary, .. } => {
                assert_eq!(summary.as_deref(), Some("All done."), "summary for {ev}")
            }
            other => panic!("expected Stop for {ev}, got {other:?}"),
        }
    }
}

#[test]
fn pi_format_response_dialect() {
    let adapter = manifest_adapter();

    // gate allow
    let input = make_input(
        "tool_call",
        r#"{"session_id": "ses_pi_1", "tool_name": "Bash", "tool_input": {}}"#,
    );
    adapter.parse_event(&input).expect("parse tool_call");
    let v = adapter.format_response(&agent_hooks_protocol::PromptDecision::allow());
    // `reply` lets the asset remember allow_always (b2v-pi.ts gatePiToolCall).
    assert_eq!(
        v,
        serde_json::json!({"decision": "allow", "reply": "allow"})
    );
    let v = adapter.format_response(&agent_hooks_protocol::PromptDecision::allow_always());
    assert_eq!(
        v,
        serde_json::json!({"decision": "allow", "reply": "allow_always"})
    );

    // gate deny with reason (legacy JSON dialect)
    let input = make_input(
        "tool_call",
        r#"{"session_id": "ses_pi_1", "tool_name": "Bash", "tool_input": {}}"#,
    );
    adapter.parse_event(&input).expect("parse tool_call");
    let v = adapter.format_response(&agent_hooks_protocol::PromptDecision::deny(Some(
        "need approval".to_string(),
    )));
    assert_eq!(
        v,
        serde_json::json!({"decision": "deny", "reason": "need approval"})
    );

    // observe: response is always empty
    let input = make_input(
        "tool_result",
        r#"{"session_id": "ses_pi_1", "tool_name": "Bash", "output": "ok"}"#,
    );
    adapter.parse_event(&input).expect("parse tool_result");
    let v = adapter.format_response(&agent_hooks_protocol::PromptDecision::deny(Some(
        "ignored".to_string(),
    )));
    assert_eq!(v, serde_json::json!({}));

    // continue: deny keeps the agent going with the block dialect
    let input = make_input(
        "agent_settled",
        r#"{"session_id": "ses_pi_1", "summary": "done"}"#,
    );
    adapter.parse_event(&input).expect("parse agent_settled");
    let v = adapter.format_response(&agent_hooks_protocol::PromptDecision::deny(Some(
        "keep going".to_string(),
    )));
    assert_eq!(
        v,
        serde_json::json!({"decision": "block", "reason": "keep going"})
    );

    // continue: allow stops
    let input = make_input(
        "agent_settled",
        r#"{"session_id": "ses_pi_1", "summary": "done"}"#,
    );
    adapter.parse_event(&input).expect("parse agent_settled");
    let v = adapter.format_response(&agent_hooks_protocol::PromptDecision::allow());
    assert_eq!(v, serde_json::json!({}));
}

#[test]
fn pi_registry_resolves_manifest_not_legacy() {
    let adapter = AdapterRegistry::get_versioned("pi", None).expect("pi resolves from registry");
    let input = make_input(
        "tool_call",
        r#"{"session_id": "ses_pi_1", "tool_name": "Bash", "tool_input": {}}"#,
    );
    adapter.parse_event(&input).expect("parse tool_call");
    // legacy PiAdapter::format_response drops the reason; the manifest keeps it.
    let v = adapter.format_response(&agent_hooks_protocol::PromptDecision::deny(Some(
        "need approval".to_string(),
    )));
    assert_eq!(
        v,
        serde_json::json!({"decision": "deny", "reason": "need approval"})
    );
}

#[test]
fn pi_ask_tool_lifts_normalized_questions() {
    let adapter = manifest_adapter();
    let vs = AdapterManifest::parse(BUILTIN)
        .expect("parse")
        .get_version("v1")
        .expect("v1")
        .ask_tools
        .clone();
    assert!(vs.iter().any(|t| t == "ask"));
    // omp ask tool payload (favorite-color shape): questions lift to prompt.
    let input = make_input(
        "tool_call",
        r#"{"session_id": "ses_pi_1", "tool_name": "ask", "tool_input": {"questions": [{"id": "favorite_color", "header": "Preference", "question": "What is your favorite color?", "multi": false, "options": [{"label": "Red", "description": "Bold and energetic"}, {"label": "Blue", "description": "Calm and trustworthy"}, {"label": "Green", "description": "Fresh and natural"}, {"label": "Yellow", "description": "Sunny and cheerful"}]}]}}"#,
    );
    let event = adapter.parse_event(&input).expect("parse ask tool_call");
    match event.lifecycle {
        AgentLifecycle::PreToolUse {
            tool_name, prompt, ..
        } => {
            assert_eq!(tool_name, "ask");
            let pr = prompt.expect("ask lifts a Question prompt");
            assert_eq!(pr.kind, agent_hooks_protocol::PromptKind::Question);
            assert_eq!(pr.questions.len(), 1);
            assert_eq!(pr.questions[0].id, "favorite_color");
            assert_eq!(pr.questions[0].question, "What is your favorite color?");
            assert_eq!(pr.questions[0].options.len(), 4);
            assert_eq!(pr.questions[0].options[1].label, "Blue");
        }
        ref other => panic!("expected PreToolUse, got {other:?}"),
    }
}
