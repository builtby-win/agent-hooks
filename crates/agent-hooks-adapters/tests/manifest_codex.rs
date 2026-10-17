use agent_hooks_adapters::manifest::{AdapterManifest, Verdict};
use agent_hooks_adapters::registry::AdapterRegistry;
use agent_hooks_adapters::{declarative::DeclarativeAdapter, HarnessAdapter};
use agent_hooks_protocol::{AgentLifecycle, EventContext, RawHookInput};

const BUILTIN: &str = std::include_str!("../builtin/codex/adapter.json");

fn make_input(event_hint: &str, payload: &str) -> RawHookInput {
    RawHookInput {
        harness_hint: "codex".to_string(),
        event_hint: Some(event_hint.to_string()),
        raw_payload: payload.to_string(),
        context: EventContext::default(),
        request_id: Some("req-123".to_string()),
        passthrough_env: Default::default(),
    }
}

fn load_adapter() -> DeclarativeAdapter {
    let manifest = AdapterManifest::parse(BUILTIN).expect("codex builtin manifest parses");
    assert_eq!(manifest.id, "codex");
    DeclarativeAdapter::new(manifest, "v1".to_string()).expect("create codex adapter")
}

#[test]
fn manifest_codex_parse_events() {
    let adapter = load_adapter();

    // 1. SessionStart keyed off thread_id (Codex session key).
    let input = make_input(
        "SessionStart",
        r#"{"thread_id": "th_codex_99", "cwd": "/Users/user/project"}"#,
    );
    let event = adapter.parse_event(&input).expect("parse SessionStart");
    assert_eq!(event.session_id, "th_codex_99");
    assert_eq!(event.harness, "codex");
    assert!(matches!(
        event.lifecycle,
        AgentLifecycle::SessionStart { .. }
    ));

    // 2. PreToolUse gate with tool name + input.
    let input = make_input(
        "PreToolUse",
        r#"{"thread_id": "th_codex_99", "tool_name": "Bash", "tool_input": {"command": "cargo test"}}"#,
    );
    let event = adapter.parse_event(&input).expect("parse PreToolUse");
    assert_eq!(event.session_id, "th_codex_99");
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

    // 3. PermissionRequest carries the human-readable description.
    let input = make_input(
        "PermissionRequest",
        r#"{"thread_id": "th_codex_99", "tool_name": "Bash", "command": "rm -rf target", "description": "clean build"}"#,
    );
    let event = adapter
        .parse_event(&input)
        .expect("parse PermissionRequest");
    match event.lifecycle {
        AgentLifecycle::PermissionRequest { prompt } => {
            assert_eq!(prompt.tool_name.as_deref(), Some("Bash"));
            assert_eq!(prompt.description.as_deref(), Some("clean build"));
        }
        ref other => panic!("expected PermissionRequest, got {:?}", other),
    }

    // 4. Stop with summary.
    let input = make_input(
        "Stop",
        r#"{"thread_id": "th_codex_99", "summary": "All tests pass"}"#,
    );
    let event = adapter.parse_event(&input).expect("parse Stop");
    match event.lifecycle {
        AgentLifecycle::Stop { summary, success } => {
            assert_eq!(summary.as_deref(), Some("All tests pass"));
            assert!(success);
        }
        ref other => panic!("expected Stop, got {:?}", other),
    }
}

#[test]
fn manifest_codex_gate_dialect() {
    let adapter = load_adapter();

    // Gate allow: bare `{}` — Codex rejects bare permissionDecision:allow as
    // unsupported; anything but a deny lets the call proceed.
    let input = make_input(
        "PreToolUse",
        r#"{"thread_id": "th_codex_99", "tool_name": "Bash", "tool_input": {}}"#,
    );
    adapter.parse_event(&input).expect("parse PreToolUse");
    let v = adapter.format_response(&agent_hooks_protocol::PromptDecision::allow());
    assert_eq!(v, serde_json::json!({}));

    // Gate deny renders the reason into permissionDecisionReason.
    let deny = agent_hooks_protocol::PromptDecision::deny(Some("needs approval".to_string()));
    let v2 = adapter.format_response(&deny);
    assert_eq!(
        v2,
        serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": "needs approval"
            }
        })
    );
}

#[test]
fn manifest_codex_permission_request_override_and_stop_continue() {
    let adapter = load_adapter();

    // PermissionRequest speaks decision.behavior, not permissionDecision.
    let input = make_input(
        "PermissionRequest",
        r#"{"thread_id": "th_codex_99", "tool_name": "Bash", "description": "clean build"}"#,
    );
    adapter
        .parse_event(&input)
        .expect("parse PermissionRequest");
    let deny = agent_hooks_protocol::PromptDecision::deny(Some("blocked by policy".to_string()));
    let v = adapter.format_response(&deny);
    assert_eq!(
        v,
        serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": "PermissionRequest",
                "decision": { "behavior": "deny", "message": "blocked by policy" }
            }
        })
    );
    let v_allow = adapter.format_response(&agent_hooks_protocol::PromptDecision::allow());
    assert_eq!(
        v_allow,
        serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": "PermissionRequest",
                "decision": { "behavior": "allow" }
            }
        })
    );

    // Stop is a continue-capability event: deny keeps the agent going.
    let stop_input = make_input("Stop", r#"{"thread_id": "th_codex_99", "summary": "done"}"#);
    adapter.parse_event(&stop_input).expect("parse Stop");
    let cont = agent_hooks_protocol::PromptDecision::deny(Some("keep going".to_string()));
    let v_cont = adapter.format_response(&cont);
    assert_eq!(
        v_cont,
        serde_json::json!({ "decision": "block", "reason": "keep going" })
    );
    let v_stop = adapter.format_response(&agent_hooks_protocol::PromptDecision::allow());
    assert_eq!(v_stop, serde_json::json!({}));
}

#[test]
fn manifest_codex_registry_prefers_manifest() {
    let adapter = AdapterRegistry::get_versioned("codex", None).expect("codex adapter resolves");

    // parse_event must precede format_response: the adapter is stateful.
    let input = make_input(
        "PreToolUse",
        r#"{"thread_id": "th_codex_99", "tool_name": "Bash", "tool_input": {}}"#,
    );
    let event = adapter.parse_event(&input).expect("parse PreToolUse");
    assert_eq!(event.session_id, "th_codex_99");

    // The legacy Rust adapter emits {"decision":"deny"}; the manifest-backed
    // adapter must emit the real harness dialect instead.
    let deny = agent_hooks_protocol::PromptDecision::deny(Some("needs approval".to_string()));
    let v = adapter.format_response(&deny);
    assert_ne!(v, serde_json::json!({"decision": "deny"}));
    assert_eq!(
        v,
        serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": "needs approval"
            }
        })
    );
}

#[test]
fn manifest_codex_user_prompt_submit_override() {
    let adapter = load_adapter();

    // UserPromptSubmit requires the top-level block shape: the generic
    // gate.deny permissionDecision shape fails open on this event.
    let input = make_input(
        "UserPromptSubmit",
        r#"{"thread_id": "th_codex_99", "prompt": "hello"}"#,
    );
    adapter.parse_event(&input).expect("parse UserPromptSubmit");
    let deny = agent_hooks_protocol::PromptDecision::deny(Some("blocked by policy".to_string()));
    let v = adapter.format_response(&deny);
    assert_eq!(
        v,
        serde_json::json!({ "decision": "block", "reason": "blocked by policy" })
    );
    let v_allow = adapter.format_response(&agent_hooks_protocol::PromptDecision::allow());
    assert_eq!(v_allow, serde_json::json!({}));
}

#[test]
fn manifest_codex_verdicts_allow_deny_no_ask() {
    let manifest = AdapterManifest::parse(BUILTIN).expect("codex builtin manifest parses");
    let v1 = manifest.get_version("v1").expect("v1 exists");
    // Codex fails ask/approve/continue:false on these events: allow+deny only.
    for event in [
        "UserPromptSubmit",
        "PreToolUse",
        "PermissionRequest",
        "PostToolUse",
    ] {
        let spec = v1
            .events
            .get(event)
            .unwrap_or_else(|| panic!("{event} exists"));
        assert_eq!(
            spec.verdicts,
            Some(vec![Verdict::Allow, Verdict::Deny]),
            "{event} verdicts"
        );
    }
}

#[test]
fn manifest_codex_precompact_trigger_and_continue_dialect() {
    let adapter = load_adapter();

    // PreCompact parses the matcher trigger out of the payload.
    let input = make_input(
        "PreCompact",
        r#"{"thread_id": "th_codex_99", "trigger": "manual"}"#,
    );
    let event = adapter.parse_event(&input).expect("parse PreCompact");
    match event.lifecycle {
        AgentLifecycle::PreCompact { trigger } => {
            assert_eq!(trigger.as_deref(), Some("manual"));
        }
        ref other => panic!("expected PreCompact, got {other:?}"),
    }

    // Continue capability: deny renders the continue-block dialect.
    let deny = agent_hooks_protocol::PromptDecision::deny(Some("skip compact".to_string()));
    let v = adapter.format_response(&deny);
    assert_eq!(
        v,
        serde_json::json!({ "decision": "block", "reason": "skip compact" })
    );
    let v_allow = adapter.format_response(&agent_hooks_protocol::PromptDecision::allow());
    assert_eq!(v_allow, serde_json::json!({}));

    // PostCompact carries the trigger the same way.
    let input = make_input(
        "PostCompact",
        r#"{"thread_id": "th_codex_99", "trigger": "auto"}"#,
    );
    let event = adapter.parse_event(&input).expect("parse PostCompact");
    match event.lifecycle {
        AgentLifecycle::PostCompact { trigger } => {
            assert_eq!(trigger.as_deref(), Some("auto"));
        }
        ref other => panic!("expected PostCompact, got {other:?}"),
    }
}

#[test]
fn manifest_codex_interrupt_observe() {
    let adapter = load_adapter();

    let input = make_input("Interrupt", r#"{"thread_id": "th_codex_99"}"#);
    let event = adapter.parse_event(&input).expect("parse Interrupt");
    assert!(matches!(event.lifecycle, AgentLifecycle::Interrupt));

    // Observe capability renders an empty object for any verdict.
    let v = adapter.format_response(&agent_hooks_protocol::PromptDecision::allow());
    assert_eq!(v, serde_json::json!({}));
}

#[test]
fn manifest_codex_normalize_response_is_agent_aware() {
    use agent_hooks_protocol::NormalizedResponse;
    let adapter = load_adapter();

    // PreToolUse is allow/deny-only: ask degrades to deny (Codex marks ask
    // unsupported), allow resolves to Proceed (renders `{}`).
    let input = make_input(
        "PreToolUse",
        r#"{"thread_id": "th_codex_99", "tool_name": "Bash", "tool_input": {}}"#,
    );
    adapter.parse_event(&input).expect("parse PreToolUse");
    assert_eq!(
        adapter.normalize_response(&agent_hooks_protocol::PromptDecision::allow()),
        NormalizedResponse::Proceed
    );
    assert_eq!(
        adapter.normalize_response(&agent_hooks_protocol::PromptDecision::deny(Some(
            "needs approval".to_string()
        ))),
        NormalizedResponse::Deny {
            reason: "needs approval".to_string()
        }
    );
    assert_eq!(
        adapter.normalize_response(&agent_hooks_protocol::PromptDecision::ask(Some(
            "proceed?".to_string()
        ))),
        NormalizedResponse::Deny {
            reason: "proceed?".to_string()
        }
    );

    // Stop is continue-capability: deny keeps the agent going.
    let stop = make_input("Stop", r#"{"thread_id": "th_codex_99", "summary": "done"}"#);
    adapter.parse_event(&stop).expect("parse Stop");
    assert_eq!(
        adapter.normalize_response(&agent_hooks_protocol::PromptDecision::deny(Some(
            "keep going".to_string()
        ))),
        NormalizedResponse::Continue {
            reason: "keep going".to_string()
        }
    );
}

#[test]
fn manifest_codex_pre_tool_use_question_handling() {
    use agent_hooks_protocol::{NormalizedStatus, PromptAnswer, PromptDecision, PromptKind};
    let adapter = load_adapter();

    let input = make_input(
        "PreToolUse",
        r#"{
            "thread_id": "th_codex_100",
            "tool_call_id": "call_xyz789",
            "tool_name": "request_user_input",
            "tool_input": {
                "questions": [
                    {
                        "id": "favorite_color",
                        "header": "What is your favorite color?",
                        "question": "What is your favorite color?",
                        "options": [
                            {"label": "Red"},
                            {"label": "Blue"}
                        ]
                    }
                ]
            }
        }"#,
    );

    let event = adapter
        .parse_event(&input)
        .expect("parse PreToolUse question");
    assert_eq!(event.session_id, "th_codex_100");
    assert_eq!(event.status, NormalizedStatus::WaitingForInput);

    match &event.lifecycle {
        AgentLifecycle::PreToolUse {
            tool_name, prompt, ..
        } => {
            assert_eq!(tool_name, "request_user_input");
            let p = prompt.as_ref().expect("question prompt is present");
            assert_eq!(p.prompt_id, "call_xyz789");
            assert_eq!(p.kind, PromptKind::Question);
            assert_eq!(p.questions.len(), 1);
            assert_eq!(p.questions[0].id, "favorite_color");
            assert_eq!(p.questions[0].question, "What is your favorite color?");
            // Header equal to question text should be deduped/omitted
            assert_eq!(p.questions[0].header, None);
        }
        ref other => panic!("expected PreToolUse, got {:?}", other),
    }

    // Format response: allow with answers
    let decision = PromptDecision {
        allow: true,
        custom_input: None,
        answers: vec![PromptAnswer {
            question_id: "favorite_color".to_string(),
            selected_options: vec!["Blue".to_string()],
            custom_input: None,
        }],
        ask: false,
        allow_always: false,
        suggestion: None,
        defer: false,
        additional_context: None,
    };

    let response = adapter.format_response(&decision);
    let hook_output = &response["hookSpecificOutput"];
    assert_eq!(hook_output["hookEventName"], "PreToolUse");
    // Codex has no in-band answer channel (`updatedInput` only re-parses the
    // tool's own args), so the answer rides the block reason the model reads.
    assert_eq!(hook_output["permissionDecision"], "deny");
    assert!(hook_output.get("updatedInput").is_none());
    let reason = hook_output["permissionDecisionReason"].as_str().unwrap();
    assert!(
        reason.contains("> What is your favorite color?\n\nBlue"),
        "{reason}"
    );

    // Format response: deny
    let deny_decision = PromptDecision {
        allow: false,
        custom_input: Some("I decline to answer".to_string()),
        answers: vec![],
        ask: false,
        allow_always: false,
        suggestion: None,
        defer: false,
        additional_context: None,
    };
    let deny_response = adapter.format_response(&deny_decision);
    let deny_output = &deny_response["hookSpecificOutput"];
    assert_eq!(deny_output["hookEventName"], "PreToolUse");
    assert_eq!(deny_output["permissionDecision"], "deny");
    assert_eq!(
        deny_output["permissionDecisionReason"],
        "I decline to answer"
    );
}

// ---------------------------------------------------------------------------
// B2V legacy parity fixtures (ah-command-harnesses)
// ---------------------------------------------------------------------------

use agent_hooks_adapters::AdapterError;
use agent_hooks_protocol::NormalizedStatus;

// Legacy src-b2v/src/hooks/common.rs is_codex_realtime_voice_payload (89c1cae09).
#[test]
fn codex_realtime_voice_payloads_are_ignored() {
    let adapter = load_adapter();
    for raw in [
        r#"{"session_id":"t-voice","hook_event_name":"UserPromptSubmit","prompt":"hi","thread_source":"realtime_voice"}"#,
        r#"{"session_id":"t-voice","hook_event_name":"SessionStart","thread":{"source":"x","thread_source":"REALTIME_VOICE"}}"#,
        r#"{"session_id":"t-voice","hook_event_name":"UserPromptSubmit","items":[{"type":"user_message","message":"<realtime_delegation><input>hello</input></realtime_delegation>"}]}"#,
    ] {
        let err = adapter
            .parse_event(&make_input("UserPromptSubmit", raw))
            .expect_err("realtime voice payload must be dropped");
        assert!(matches!(err, AdapterError::Ignored(_)), "{err:?}");
    }
    // Harness no-op: the shim renders the observe shape for the daemon's defer.
    assert_eq!(
        adapter.format_response(&agent_hooks_protocol::PromptDecision::defer()),
        serde_json::json!({})
    );
    // Registry path (what the daemon/shim use) drops it too.
    let reg = AdapterRegistry::get_versioned("codex", None).unwrap();
    assert!(matches!(
        reg.parse_event(&make_input(
            "UserPromptSubmit",
            r#"{"session_id":"t","thread_source":"realtime_voice"}"#
        )),
        Err(AdapterError::Ignored(_))
    ));
}

#[test]
fn codex_ordinary_user_messages_are_not_ignored() {
    let adapter = load_adapter();
    for raw in [
        r#"{"session_id":"t-1","hook_event_name":"UserPromptSubmit","prompt":"hello","items":[{"type":"user_message","message":"hello"}]}"#,
        // Delegation marker outside a user_message object is not a voice thread.
        r#"{"session_id":"t-1","hook_event_name":"UserPromptSubmit","prompt":"explain <realtime_delegation> tags"}"#,
        r#"{"session_id":"t-1","hook_event_name":"UserPromptSubmit","prompt":"hi","thread_source":"cli"}"#,
    ] {
        let event = adapter
            .parse_event(&make_input("UserPromptSubmit", raw))
            .expect("ordinary prompt parses");
        assert_eq!(event.status, NormalizedStatus::Running);
    }
}

// Legacy identity.rs codex_subagent_identity + tests/codex_normalization.rs (ceade31b6).
#[test]
fn codex_subagent_events_carry_child_and_parent_identity() {
    let adapter = load_adapter();
    for event_name in ["SubagentStart", "SubagentStop"] {
        let raw = format!(
            r#"{{"session_id":"codex-parent","transcript_path":"/tmp/r.jsonl","cwd":"/Users/me/proj","hook_event_name":"{event_name}","agent_id":"codex-child","agent_type":"worker","turn_id":"turn-1"}}"#
        );
        let event = adapter.parse_event(&make_input(event_name, &raw)).unwrap();
        assert_eq!(event.session_id, "codex-child", "{event_name}");
        assert_eq!(event.parent_session_id.as_deref(), Some("codex-parent"));
        assert_eq!(
            event.context.cwd.as_deref(),
            Some(std::path::Path::new("/Users/me/proj"))
        );
        match &event.lifecycle {
            AgentLifecycle::SubagentStart { subagent_id, .. }
            | AgentLifecycle::SubagentStop { subagent_id, .. } => {
                assert_eq!(subagent_id, "codex-child")
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(event.status, NormalizedStatus::Running);
    }
}

#[test]
fn codex_subagent_without_agent_id_is_never_its_own_parent() {
    let adapter = load_adapter();
    let event = adapter
        .parse_event(&make_input(
            "SubagentStart",
            r#"{"session_id":"codex-parent","hook_event_name":"SubagentStart"}"#,
        ))
        .unwrap();
    assert_eq!(event.session_id, "codex-parent");
    assert_eq!(event.parent_session_id, None);
}

// Legacy status_classification.rs codex_child_registration_does_not_claim_parent_chatgpt_task.
#[test]
fn codex_child_thread_carries_parent_thread_and_hyphenated_thread_id() {
    let adapter = load_adapter();
    let event = adapter
        .parse_event(&make_input(
            "SessionStart",
            r#"{"thread-id":"019f4acc-7896-70c2-9d87-3e304a7647ad","parent_session_id":"019f4a34-6c8c-7a81-b4e3-7315f7432abd","hook_event_name":"SessionStart"}"#,
        ))
        .unwrap();
    assert_eq!(event.session_id, "019f4acc-7896-70c2-9d87-3e304a7647ad");
    assert_eq!(
        event.parent_session_id.as_deref(),
        Some("019f4a34-6c8c-7a81-b4e3-7315f7432abd")
    );
}

// Legacy status_classification.rs codex_request_user_input_{pretooluse_maps_to_waiting,posttooluse_stays_waiting}.
#[test]
fn codex_request_user_input_waits_on_pre_tool_use_only() {
    let adapter = load_adapter();
    let pre = adapter
        .parse_event(&make_input(
            "PreToolUse",
            r#"{"session_id":"codex-session","hook_event_name":"PreToolUse","tool_name":"request_user_input","tool_use_id":"call_123","tool_input":{"questions":[{"id":"scope","header":"Scope","question":"Which scope should the plan cover?","options":[{"label":"Current workspace"}]}]}}"#,
        ))
        .unwrap();
    assert_eq!(pre.status, NormalizedStatus::WaitingForInput);
    let post = adapter
        .parse_event(&make_input(
            "PostToolUse",
            r#"{"session_id":"codex-session","hook_event_name":"PostToolUse","tool_name":"request_user_input","tool_use_id":"call_123"}"#,
        ))
        .unwrap();
    // Live parity e2e: legacy's CLI forwarded PostToolUse without tool
    // fields, so it was busy — the question is answered by now.
    assert_eq!(post.status, NormalizedStatus::Running);
    let bash = adapter
        .parse_event(&make_input(
            "PostToolUse",
            r#"{"session_id":"codex-session","hook_event_name":"PostToolUse","tool_name":"Bash","tool_use_id":"call_9"}"#,
        ))
        .unwrap();
    assert_eq!(bash.status, NormalizedStatus::Running);
}

// Legacy decide_status: stop + subagent_scoped (mode: subagent, d88b67e09) → busy.
#[test]
fn codex_subagent_mode_stop_is_not_a_completion() {
    let adapter = load_adapter();
    let child = adapter
        .parse_event(&make_input(
            "Stop",
            r#"{"session_id":"codex-child","hook_event_name":"Stop","mode":"subagent","stop_hook_active":false}"#,
        ))
        .unwrap();
    assert!(matches!(child.lifecycle, AgentLifecycle::TurnEnd { .. }));
    assert_eq!(child.status, NormalizedStatus::Running);
    let root = adapter
        .parse_event(&make_input(
            "Stop",
            r#"{"session_id":"codex-root","hook_event_name":"Stop","stop_hook_active":false,"last_assistant_message":"done"}"#,
        ))
        .unwrap();
    assert_eq!(root.status, NormalizedStatus::Idle);
}

#[test]
fn codex_pre_tool_use_renders_allow_deny_defer() {
    let adapter = load_adapter();
    adapter
        .parse_event(&make_input(
            "PreToolUse",
            r#"{"session_id":"codex-session","hook_event_name":"PreToolUse","tool_name":"Bash","tool_use_id":"call_1","tool_input":{"command":"cargo test"}}"#,
        ))
        .unwrap();
    use agent_hooks_protocol::PromptDecision;
    assert_eq!(
        adapter.format_response(&PromptDecision::allow()),
        serde_json::json!({})
    );
    assert_eq!(
        adapter.format_response(&PromptDecision::deny(Some("blocked".into()))),
        serde_json::json!({"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": "deny", "permissionDecisionReason": "blocked"}})
    );
    assert_eq!(
        adapter.format_response(&PromptDecision::defer()),
        serde_json::json!({})
    );
}

// Legacy mod.rs falls through to build_claude_response_for_event for Codex
// PermissionRequest: AllowAlways echoes permission_suggestions when present.
#[test]
fn codex_permission_request_allow_always_echoes_suggestions() {
    use agent_hooks_protocol::PromptDecision;
    let adapter = load_adapter();
    adapter
        .parse_event(&make_input(
            "PermissionRequest",
            r#"{"session_id":"codex-session","hook_event_name":"PermissionRequest","tool_name":"Bash","tool_input":{"command":"git status"},"permission_suggestions":[{"type":"tool","tool_name":"Bash"}]}"#,
        ))
        .unwrap();
    assert_eq!(
        adapter.format_response(&PromptDecision::allow_always()),
        serde_json::json!({"hookSpecificOutput": {"hookEventName": "PermissionRequest", "decision": {"behavior": "allow", "updatedPermissions": [{"type": "tool", "tool_name": "Bash"}]}}})
    );
    assert_eq!(
        adapter.format_response(&PromptDecision::allow()),
        serde_json::json!({"hookSpecificOutput": {"hookEventName": "PermissionRequest", "decision": {"behavior": "allow"}}})
    );
}

// Live parity e2e (codex step 4): the answer is delivered as the block reason
// (Codex hands it to the model as the tool result).
#[test]
fn codex_request_user_input_answer_rides_block_reason() {
    use agent_hooks_protocol::{PromptAnswer, PromptDecision};
    let adapter = load_adapter();
    let event = adapter
        .parse_event(&make_input(
            "PreToolUse",
            r#"{"session_id":"019f4a34-0000-7000-8000-00000000c0de","cwd":"/tmp/p","hook_event_name":"PreToolUse","model":"gpt-5-codex","permission_mode":"default","turn_id":"turn-parity-1","tool_name":"request_user_input","tool_use_id":"call_parity_q","tool_input":{"questions":[{"id":"db","header":"DB","question":"Which database?","options":[{"label":"Postgres","description":"Relational"},{"label":"SQLite","description":"Embedded"}]}]}}"#,
        ))
        .unwrap();
    assert_eq!(event.status, NormalizedStatus::WaitingForInput);
    let out = adapter.format_response(&PromptDecision {
        allow: true,
        custom_input: None,
        answers: vec![PromptAnswer {
            question_id: "db".into(),
            selected_options: vec!["SQLite".into()],
            custom_input: None,
        }],
        ask: false,
        allow_always: false,
        suggestion: None,
        defer: false,
        additional_context: None,
    });
    assert_eq!(
        serde_json::to_string(&out).unwrap(),
        r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"The user already answered this question outside Codex. Use these answers and continue; do not ask again.\n\n> Which database?\n\nSQLite"}}"#
    );
    // Answer in Codex (no answers): no output, Codex's own question UI runs.
    assert_eq!(
        adapter.format_response(&PromptDecision::allow()),
        serde_json::json!({})
    );
    // Deny carries no reason when none was given.
    assert_eq!(
        adapter.format_response(&PromptDecision::deny(None)),
        serde_json::json!({"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": "deny"}})
    );
}

// Real `request_user_input_async` call (codex-cli 0.156.1): options are bare
// strings and the text is `title`. Both must lift, or the panel shows a
// question with no options.
#[test]
fn codex_request_user_input_async_lifts_string_options() {
    use agent_hooks_protocol::{PromptAnswer, PromptDecision};
    let adapter = load_adapter();
    let event = adapter
        .parse_event(&make_input(
            "PreToolUse",
            r#"{"session_id":"01a0d16d-604d-7e41-b8f7-0991cba27ddb","hook_event_name":"PreToolUse","tool_name":"request_user_input_async","tool_use_id":"call_async_q","tool_input":{"questions":[{"title":"Favorite color","options":["Red","Blue","Green"],"question":"What is your favorite color?"}]}}"#,
        ))
        .unwrap();
    let request = event.prompt_request().expect("async ask is a prompt");
    let labels: Vec<&str> = request.questions[0]
        .options
        .iter()
        .map(|o| o.label.as_str())
        .collect();
    assert_eq!(labels, ["Red", "Blue", "Green"]);
    let out = adapter.format_response(&PromptDecision {
        answers: vec![PromptAnswer {
            question_id: request.questions[0].id.clone(),
            selected_options: vec!["Blue".into()],
            custom_input: None,
        }],
        ..PromptDecision::allow()
    });
    assert_eq!(out["hookSpecificOutput"]["permissionDecision"], "deny");
    assert!(out["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .unwrap()
        .ends_with("> What is your favorite color?\n\nBlue"));
}

// Live parity e2e (codex steps 5-7): after the answered question, PostToolUse
// and the subagent events are busy/no-op, never waiting.
#[test]
fn codex_after_answered_question_statuses_are_not_waiting() {
    let adapter = load_adapter();
    let q = r#""tool_input":{"questions":[{"id":"db","question":"Which database?","options":[{"label":"SQLite"}]}]}"#;
    let post = adapter
        .parse_event(&make_input(
            "PostToolUse",
            &format!(r#"{{"session_id":"t","hook_event_name":"PostToolUse","tool_name":"request_user_input","tool_use_id":"call_parity_q",{q},"tool_response":"SQLite"}}"#),
        ))
        .unwrap();
    assert_eq!(post.status, NormalizedStatus::Running);
    for ev in ["SubagentStart", "SubagentStop"] {
        let e = adapter
            .parse_event(&make_input(
                ev,
                &format!(r#"{{"session_id":"t","hook_event_name":"{ev}","agent_id":"child","agent_type":"worker"}}"#),
            ))
            .unwrap();
        assert_eq!(e.status, NormalizedStatus::Running, "{ev}");
        assert_eq!(e.session_id, "child");
        assert_eq!(e.parent_session_id.as_deref(), Some("t"));
    }
}

// Live parity e2e codex step 4 (src-tauri/tests/live_e2e/hook_parity/codex.rs):
// the exact scenario payload must raise a held question prompt, via both the
// builtin manifest and the registry path the daemon uses.
const PARITY_STEP4: &str = r#"{"session_id":"019f4a34-0000-7000-8000-00000000c0de","transcript_path":"/tmp/b2v-parity-Y2E2yP/.parity-codex-rollout.jsonl","cwd":"/tmp/b2v-parity-Y2E2yP","hook_event_name":"PreToolUse","model":"gpt-5-codex","permission_mode":"default","turn_id":"turn-parity-1","tool_name":"request_user_input","tool_use_id":"call_parity_q","tool_input":{"questions":[{"id":"db","header":"DB","question":"Which database?","options":[{"label":"Postgres","description":"Relational"},{"label":"SQLite","description":"Embedded"}]}]}}"#;

#[test]
fn codex_parity_step4_raises_a_held_question_prompt() {
    use agent_hooks_protocol::PromptKind;
    let registry = AdapterRegistry::get_versioned("codex", None).unwrap();
    let local = load_adapter();
    for adapter in [&*registry as &dyn HarnessAdapter, &local] {
        let event = adapter
            .parse_event(&make_input("PreToolUse", PARITY_STEP4))
            .unwrap();
        let p = event.prompt_request().expect("question prompt");
        assert_eq!(p.kind, PromptKind::Question);
        assert_eq!(p.tool_name.as_deref(), Some("request_user_input"));
        assert_eq!(p.prompt_id, "call_parity_q");
        assert_eq!(p.questions.len(), 1);
        assert_eq!(p.questions[0].id, "db");
        assert_eq!(p.questions[0].question, "Which database?");
        assert!(
            event.is_interactive(),
            "a decider claiming interactive events must hold it"
        );
        assert_eq!(event.status, NormalizedStatus::WaitingForInput);
    }
}
