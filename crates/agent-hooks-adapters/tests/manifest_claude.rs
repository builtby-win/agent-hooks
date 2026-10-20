use agent_hooks_adapters::manifest::{AdapterManifest, Capability, InstallOp, Verdict};
use agent_hooks_adapters::registry::AdapterRegistry;
use agent_hooks_adapters::{declarative::DeclarativeAdapter, HarnessAdapter};
use agent_hooks_protocol::{AgentLifecycle, EventContext, RawHookInput};

const BUILTIN: &str = include_str!("../builtin/claude-code/adapter.json");

fn make_input(event_hint: &str, payload: &str) -> RawHookInput {
    RawHookInput {
        harness_hint: "claude".to_string(),
        event_hint: Some(event_hint.to_string()),
        raw_payload: payload.to_string(),
        context: EventContext::default(),
        request_id: Some("req-123".to_string()),
        passthrough_env: Default::default(),
    }
}

fn manifest_adapter() -> DeclarativeAdapter {
    let manifest = AdapterManifest::parse(BUILTIN).expect("builtin claude manifest parses");
    assert_eq!(manifest.id, "claude-code");
    DeclarativeAdapter::new(manifest, "v1".to_string()).expect("create claude adapter")
}

#[test]
fn builtin_claude_manifest_parses_with_expected_shape() {
    let manifest = AdapterManifest::parse(BUILTIN).expect("builtin claude manifest parses");
    assert_eq!(manifest.default_version(), "v1");
    let vs = manifest.get_version("v1").expect("v1 exists");
    // all host events the Rust adapter handles
    for ev in [
        "SessionStart",
        "SessionEnd",
        "UserPromptSubmit",
        "PreToolUse",
        "PostToolUse",
        "PostToolUseFailure",
        "PermissionRequest",
        "Notification",
        "SubagentStart",
        "SubagentStop",
        "Stop",
    ] {
        assert!(vs.events.contains_key(ev), "missing event {ev}");
    }
}

#[test]
fn claude_parse_event_gate_and_observe() {
    let adapter = manifest_adapter();

    // gate: PreToolUse
    let input = make_input(
        "PreToolUse",
        r#"{"session_id": "ses_claude_1", "hook_event_name": "PreToolUse", "tool_name": "Bash", "tool_input": {"command": "cargo test"}}"#,
    );
    let event = adapter.parse_event(&input).expect("parse PreToolUse");
    assert_eq!(event.session_id, "ses_claude_1");
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
        ref other => panic!("expected PreToolUse, got {other:?}"),
    }

    // gate: UserPromptSubmit
    let input = make_input(
        "UserPromptSubmit",
        r#"{"session_id": "ses_claude_1", "prompt": "Fix bug in auth.ts"}"#,
    );
    let event = adapter.parse_event(&input).expect("parse UserPromptSubmit");
    match event.lifecycle {
        AgentLifecycle::UserPromptSubmit { user_prompt } => {
            assert_eq!(user_prompt.as_deref(), Some("Fix bug in auth.ts"));
        }
        ref other => panic!("expected UserPromptSubmit, got {other:?}"),
    }

    // observe: PostToolUse
    let input = make_input(
        "PostToolUse",
        r#"{"session_id": "ses_claude_1", "tool_name": "Bash", "tool_output": "ok"}"#,
    );
    let event = adapter.parse_event(&input).expect("parse PostToolUse");
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

    // continue: Stop
    let input = make_input(
        "Stop",
        r#"{"session_id": "ses_claude_1", "summary": "All tests pass"}"#,
    );
    let event = adapter.parse_event(&input).expect("parse Stop");
    match event.lifecycle {
        AgentLifecycle::Stop { summary, success } => {
            assert_eq!(summary.as_deref(), Some("All tests pass"));
            assert!(success);
        }
        ref other => panic!("expected Stop, got {other:?}"),
    }
}

#[test]
fn claude_format_response_dialect() {
    let adapter = manifest_adapter();

    // gate allow
    let input = make_input(
        "PreToolUse",
        r#"{"session_id": "ses_claude_1", "tool_name": "Bash", "tool_input": {}}"#,
    );
    adapter.parse_event(&input).expect("parse PreToolUse");
    let v = adapter.format_response(&agent_hooks_protocol::PromptDecision::allow());
    assert_eq!(
        v,
        serde_json::json!({"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": "allow"}})
    );

    // gate deny with reason
    let v = adapter.format_response(&agent_hooks_protocol::PromptDecision::deny(Some(
        "need approval".to_string(),
    )));
    assert_eq!(
        v,
        serde_json::json!({"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": "deny", "permissionDecisionReason": "need approval"}})
    );

    // continue: deny keeps the agent going with the block dialect
    let stop_input = make_input(
        "Stop",
        r#"{"session_id": "ses_claude_1", "summary": "done"}"#,
    );
    adapter.parse_event(&stop_input).expect("parse Stop");
    let v = adapter.format_response(&agent_hooks_protocol::PromptDecision::deny(Some(
        "keep going".to_string(),
    )));
    assert_eq!(
        v,
        serde_json::json!({"decision": "block", "reason": "keep going"})
    );
}

#[test]
fn claude_registry_resolves_manifest_not_legacy() {
    let adapter = AdapterRegistry::get_versioned("claude-code", None)
        .expect("claude-code resolves from registry");
    let input = make_input(
        "PreToolUse",
        r#"{"session_id": "ses_claude_1", "tool_name": "Bash", "tool_input": {}}"#,
    );
    adapter.parse_event(&input).expect("parse PreToolUse");
    let v = adapter.format_response(&agent_hooks_protocol::PromptDecision::deny(None));
    assert_ne!(v, serde_json::json!({"decision": "deny"}));
    assert_eq!(
        v,
        // Empty reason is omitted, not sent as "" (live parity e2e vs legacy).
        serde_json::json!({"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": "deny"}})
    );
}

#[test]
fn claude_ask_user_question_lifts_normalized_questions() {
    // Claude's questions surface on PermissionRequest (legacy kind Permission,
    // questions lifted); PreToolUse(AskUserQuestion) holds nothing.
    let adapter = manifest_adapter();
    let input = make_input(
        "PermissionRequest",
        r#"{"session_id": "ses_claude_1", "tool_name": "AskUserQuestion", "tool_input": {"questions": [{"question": "Deploy to prod?", "header": "Deploy", "multiSelect": false, "options": [{"label": "Yes", "description": "ship it"}, {"label": "No", "description": "hold off"}]}]}}"#,
    );
    let event = adapter.parse_event(&input).expect("parse AskUserQuestion");
    match event.lifecycle {
        AgentLifecycle::PermissionRequest { prompt: pr } => {
            assert_eq!(pr.tool_name.as_deref(), Some("AskUserQuestion"));
            assert_eq!(pr.kind, agent_hooks_protocol::PromptKind::Permission);
            assert_eq!(pr.questions.len(), 1);
            assert_eq!(pr.questions[0].question, "Deploy to prod?");
            assert_eq!(pr.questions[0].options.len(), 2);
            assert_eq!(pr.questions[0].options[0].label, "Yes");
        }
        ref other => panic!("expected PermissionRequest, got {other:?}"),
    }
}

#[test]
fn claude_user_prompt_submit_uses_top_level_block_dialect() {
    let adapter = manifest_adapter();
    // Per Claude docs UserPromptSubmit takes top-level decision:block, NOT
    // hookSpecificOutput.permissionDecision (which fails schema validation
    // there and fails open). The override must render the block dialect.
    let input = make_input(
        "UserPromptSubmit",
        r#"{"session_id": "ses_claude_1", "prompt": "Fix bug in auth.ts"}"#,
    );
    adapter.parse_event(&input).expect("parse UserPromptSubmit");
    let v = adapter.format_response(&agent_hooks_protocol::PromptDecision::deny(Some(
        "not now".to_string(),
    )));
    assert_eq!(
        v,
        serde_json::json!({"decision": "block", "reason": "not now"})
    );
    // Allow omits decision entirely (omit = proceed).
    let v = adapter.format_response(&agent_hooks_protocol::PromptDecision::allow());
    assert_eq!(v, serde_json::json!({}));
}

#[test]
fn claude_subagent_start_is_observe_not_gate() {
    // Docs: SubagentStart is context-only, no blocking or decision control.
    // Holding it for a verdict would be pure latency with zero enforcement.
    let manifest = AdapterManifest::parse(BUILTIN).expect("parse");
    let vs = manifest.get_version("v1").expect("v1");
    let spec = vs
        .events
        .get("SubagentStart")
        .expect("SubagentStart mapped");
    assert_eq!(
        spec.capability,
        agent_hooks_adapters::manifest::Capability::Observe
    );
}

#[test]
fn claude_permission_request_ask_falls_back_to_deny() {
    // PermissionRequest honors allow/deny only: an ask decision renders the
    // deny template (fail-safe), even though a base-gate ask dialect exists.
    let adapter = manifest_adapter();
    let input = make_input(
        "PermissionRequest",
        r#"{"session_id": "ses_claude_1", "tool_name": "Bash", "tool_input": {}}"#,
    );
    adapter
        .parse_event(&input)
        .expect("parse PermissionRequest");
    let v = adapter.format_response(&agent_hooks_protocol::PromptDecision::ask(Some(
        "need input".to_string(),
    )));
    assert_eq!(
        v,
        serde_json::json!({"hookSpecificOutput": {"hookEventName": "PermissionRequest", "decision": {"behavior": "deny", "message": "need input"}}})
    );
}

#[test]
fn claude_verdicts_declared_per_event() {
    let manifest = AdapterManifest::parse(BUILTIN).expect("builtin claude manifest parses");
    let vs = manifest.get_version("v1").expect("v1 exists");
    assert_eq!(
        vs.events.get("PreToolUse").expect("PreToolUse").verdicts,
        Some(vec![Verdict::Allow, Verdict::Deny, Verdict::Ask])
    );
    assert_eq!(
        vs.events
            .get("PermissionRequest")
            .expect("PermissionRequest")
            .verdicts,
        Some(vec![Verdict::Allow, Verdict::Deny])
    );
    assert_eq!(
        vs.events
            .get("UserPromptSubmit")
            .expect("UserPromptSubmit")
            .verdicts,
        Some(vec![Verdict::Allow, Verdict::Deny])
    );
    // No other events declare verdicts (absent = all supported).
    for (name, spec) in &vs.events {
        if name == "PreToolUse" || name == "PermissionRequest" || name == "UserPromptSubmit" {
            continue;
        }
        assert!(
            spec.verdicts.is_none(),
            "unexpected verdicts on {name}: {:?}",
            spec.verdicts
        );
    }
}

#[test]
fn claude_entry_timeouts_match_harness_defaults() {
    // We install Claude's own documented per-event timeouts rather than a
    // hold-sized value of our own. Claude lowers the default to 30s on
    // UserPromptSubmit specifically (a stalled prompt is the most visible
    // hang there is) and keeps 600s on PreToolUse/PermissionRequest, where a
    // human verdict is expected to take time.
    //
    // This is only safe because gating is scoped: the daemon holds a gate
    // event ONLY when a subscriber declared it would answer that event
    // (`PromptMediator::has_decider_for`). An unclaimed UserPromptSubmit
    // returns immediately, so it never approaches the 30s ceiling. Raising
    // this to outlast our own hold budget would override a deliberate
    // upstream guardrail and turn a wedged daemon into a 10-minute stall.
    let manifest = AdapterManifest::parse(BUILTIN).expect("builtin claude manifest parses");
    let vs = manifest.get_version("v1").expect("v1 exists");
    let overrides = vs
        .install
        .iter()
        .find_map(|op| match op {
            InstallOp::JsonHooks {
                entry_overrides, ..
            } => Some(entry_overrides),
            _ => None,
        })
        .expect("claude install has a json_hooks step");

    let timeout_for = |name: &str| -> u64 {
        overrides
            .get(name)
            .and_then(|o| o.get("timeout"))
            .and_then(|t| t.as_u64())
            .unwrap_or_else(|| panic!("no entry_overrides timeout for {name}"))
    };
    assert_eq!(timeout_for("UserPromptSubmit"), 30);
    assert_eq!(timeout_for("PreToolUse"), 600);
    assert_eq!(timeout_for("PermissionRequest"), 600);

    // Every gate event states its timeout explicitly — none may silently
    // inherit the generic entry default.
    for (name, spec) in &vs.events {
        if spec.capability != Capability::Gate {
            continue;
        }
        assert!(
            overrides.get(name).and_then(|o| o.get("timeout")).is_some(),
            "gate event {name} has no explicit entry_overrides timeout"
        );
    }
}

#[test]
fn claude_post_tool_use_tool_response_extraction() {
    // Docs: PostToolUse output arrives as `tool_response`; `duration_ms` is a
    // harmless metadata key that must not break parsing.
    let adapter = manifest_adapter();
    let input = make_input(
        "PostToolUse",
        r#"{"session_id": "ses_claude_1", "tool_name": "Bash", "tool_response": "done in 12ms", "duration_ms": 12}"#,
    );
    let event = adapter.parse_event(&input).expect("parse PostToolUse");
    match event.lifecycle {
        AgentLifecycle::PostToolUse {
            tool_name,
            output_preview,
        } => {
            assert_eq!(tool_name, "Bash");
            assert_eq!(output_preview.as_deref(), Some("done in 12ms"));
        }
        ref other => panic!("expected PostToolUse, got {other:?}"),
    }
}

#[test]
fn claude_notification_message_extraction() {
    // Notification-only override maps prompt <- ["message", "title"] so
    // notification text normalizes into the message.
    let adapter = manifest_adapter();
    let input = make_input(
        "Notification",
        r#"{"session_id": "ses_claude_1", "message": "Task completed"}"#,
    );
    let event = adapter.parse_event(&input).expect("parse Notification");
    match event.lifecycle {
        AgentLifecycle::Notification { message, .. } => {
            assert_eq!(message, "Task completed");
        }
        ref other => panic!("expected Notification, got {other:?}"),
    }
    let input = make_input(
        "Notification",
        r#"{"session_id": "ses_claude_1", "title": "Build done"}"#,
    );
    let event = adapter
        .parse_event(&input)
        .expect("parse Notification title");
    match event.lifecycle {
        AgentLifecycle::Notification { message, .. } => {
            assert_eq!(message, "Build done");
        }
        ref other => panic!("expected Notification, got {other:?}"),
    }
}

#[test]
fn claude_session_end_reason_key_present() {
    // SessionEnd carries a `reason` payload key; extraction.summary already
    // lists "reason" so no manifest change is needed — verify the key is there.
    let manifest = AdapterManifest::parse(BUILTIN).expect("builtin claude manifest parses");
    let vs = manifest.get_version("v1").expect("v1 exists");
    assert!(
        vs.extraction
            .summary
            .as_deref()
            .unwrap_or_default()
            .contains(&agent_hooks_adapters::manifest::ExtractionSource::Path(
                "reason".to_string()
            )),
        "summary extraction must list `reason` for SessionEnd"
    );
}

#[test]
fn claude_normalize_response_is_event_aware() {
    use agent_hooks_protocol::NormalizedResponse;
    let adapter = manifest_adapter();

    // PreToolUse declares ask: escalation survives as Ask.
    let input = make_input(
        "PreToolUse",
        r#"{"session_id": "ses_claude_1", "tool_name": "Bash", "tool_input": {}}"#,
    );
    adapter.parse_event(&input).expect("parse PreToolUse");
    assert_eq!(
        adapter.normalize_response(&agent_hooks_protocol::PromptDecision::ask(Some(
            "need input".to_string()
        ))),
        NormalizedResponse::Ask {
            question: "need input".to_string()
        }
    );

    // PermissionRequest is allow/deny-only: the same ask degrades to Deny.
    let input = make_input(
        "PermissionRequest",
        r#"{"session_id": "ses_claude_1", "tool_name": "Bash", "tool_input": {}}"#,
    );
    adapter
        .parse_event(&input)
        .expect("parse PermissionRequest");
    assert_eq!(
        adapter.normalize_response(&agent_hooks_protocol::PromptDecision::ask(Some(
            "need input".to_string()
        ))),
        NormalizedResponse::Deny {
            reason: "need input".to_string()
        }
    );
}

#[test]
fn pretooluse_never_carries_answers_and_permission_request_keeps_free_text_exact() {
    // Claude's PreToolUse(AskUserQuestion) is not a question prompt, so it has
    // no questions to resolve answers against: answers sent to it fail closed
    // instead of being guessed into additionalContext.
    let adapter = manifest_adapter();
    let input = make_input(
        "PreToolUse",
        r#"{"session_id": "s", "tool_name": "AskUserQuestion", "tool_input": {"questions": [{"question": "Which files?", "multiSelect": true, "options": [{"label": "a.ts"}, {"label": "b.ts"}]}]}}"#,
    );
    adapter.parse_event(&input).expect("parse PreToolUse");
    let with_picks = agent_hooks_protocol::PromptDecision {
        answers: vec![agent_hooks_protocol::PromptAnswer {
            question_id: "q0".to_string(),
            selected_options: vec!["a.ts".to_string()],
            custom_input: None,
        }],
        ..agent_hooks_protocol::PromptDecision::allow()
    };
    let out = adapter.format_response(&with_picks);
    assert_eq!(
        out["hookSpecificOutput"]["permissionDecision"],
        serde_json::json!("deny"),
        "got {out}"
    );
    assert!(
        out["hookSpecificOutput"].get("additionalContext").is_none(),
        "answers must not leak as context, got {out}"
    );
    let plain = adapter.format_response(&agent_hooks_protocol::PromptDecision::allow());
    assert_eq!(
        plain["hookSpecificOutput"]["permissionDecision"],
        serde_json::json!("allow")
    );
    assert!(plain["hookSpecificOutput"]
        .get("additionalContext")
        .is_none());

    // The real answer channel is PermissionRequest `updatedInput`; free text
    // there is carried verbatim, padding and length included.
    let event = adapter
        .parse_event(&make_input("PermissionRequest", ASK_DB))
        .unwrap();
    let qid = permission_prompt(&event).questions[0].id.clone();
    let long = format!("  {}\n", "t".repeat(5_000));
    let with_custom = agent_hooks_protocol::PromptDecision {
        answers: vec![agent_hooks_protocol::PromptAnswer {
            question_id: qid,
            selected_options: vec![],
            custom_input: Some(long.clone()),
        }],
        ..agent_hooks_protocol::PromptDecision::allow()
    };
    let out = adapter.format_response(&with_custom);
    assert_eq!(
        out["hookSpecificOutput"]["decision"]["updatedInput"]["answers"]["Which database?"],
        serde_json::json!(long)
    );
}

// ---------------------------------------------------------------------------
// B2V legacy parity fixtures (ah-command-harnesses)
// ---------------------------------------------------------------------------

use agent_hooks_protocol::{NormalizedStatus, PromptDecision};

fn claude_notification(notification_type: &str) -> String {
    format!(
        r#"{{"session_id":"c-sess","transcript_path":"/tmp/c.jsonl","cwd":"/Users/me/proj","hook_event_name":"Notification","message":"Claude needs your attention","title":"Claude Code","notification_type":"{notification_type}"}}"#
    )
}

// Legacy lifecycle.rs notification_status_for_agent: permission/elicitation → waiting.
#[test]
fn claude_notification_permission_and_elicitation_wait() {
    let adapter = manifest_adapter();
    for (raw, canonical) in [
        ("permission_prompt", "permission"),
        ("elicitation_dialog", "question"),
    ] {
        let event = adapter
            .parse_event(&make_input("Notification", &claude_notification(raw)))
            .expect("parse Notification");
        match &event.lifecycle {
            AgentLifecycle::Notification {
                message,
                title,
                notification_type,
            } => {
                assert_eq!(message, "Claude needs your attention");
                assert_eq!(title.as_deref(), Some("Claude Code"));
                assert_eq!(notification_type.as_deref(), Some(canonical));
            }
            other => panic!("expected Notification, got {other:?}"),
        }
        assert_eq!(event.status, NormalizedStatus::WaitingForInput, "{raw}");
        assert_eq!(event.session_id, "c-sess");
        assert_eq!(event.parent_session_id, None);
        assert_eq!(
            event.context.cwd.as_deref(),
            Some(std::path::Path::new("/Users/me/proj"))
        );
        // Observe: the harness gets the no-op whatever the decision.
        assert_eq!(
            adapter.format_response(&PromptDecision::allow()),
            serde_json::json!({})
        );
    }
}

// Contract: idle → Idle; unknown types pass through raw and stay Running.
#[test]
fn claude_notification_idle_and_unknown_types() {
    let adapter = manifest_adapter();
    let idle = adapter
        .parse_event(&make_input(
            "Notification",
            &claude_notification("idle_prompt"),
        ))
        .unwrap();
    assert_eq!(idle.status, NormalizedStatus::Idle);
    let auth = adapter
        .parse_event(&make_input(
            "Notification",
            &claude_notification("auth_success"),
        ))
        .unwrap();
    assert!(matches!(
        &auth.lifecycle,
        AgentLifecycle::Notification { notification_type: Some(t), .. } if t == "auth_success"
    ));
    assert_eq!(auth.status, NormalizedStatus::Running);
}

// Legacy decide_status saw only "posttooluse" (cheap CLI summary) → busy.
#[test]
fn claude_ask_user_question_post_tool_use_is_busy() {
    let adapter = manifest_adapter();
    let ask = adapter
        .parse_event(&make_input(
            "PostToolUse",
            r#"{"session_id":"c-sess","hook_event_name":"PostToolUse","tool_name":"AskUserQuestion","tool_input":{"questions":[{"question":"Which?","options":[{"label":"A"}]}]},"tool_response":{}}"#,
        ))
        .unwrap();
    // Live parity e2e: legacy's CLI stripped PostToolUse to its name → busy.
    assert_eq!(ask.status, NormalizedStatus::Running);
    let bash = adapter
        .parse_event(&make_input(
            "PostToolUse",
            r#"{"session_id":"c-sess","hook_event_name":"PostToolUse","tool_name":"Bash","tool_input":{"command":"ls"},"tool_response":{"stdout":""}}"#,
        ))
        .unwrap();
    assert_eq!(bash.status, NormalizedStatus::Running);
}

// Legacy status_classification.rs documented_parent_and_subagent_markers_prevent_global_completion.
#[test]
fn claude_stop_with_parent_marker_is_not_a_completion() {
    let adapter = manifest_adapter();
    let child = adapter
        .parse_event(&make_input(
            "Stop",
            r#"{"session_id":"c-sess","hook_event_name":"Stop","stop_hook_active":false,"message":{"parentID":"msg-parent"}}"#,
        ))
        .unwrap();
    assert!(matches!(child.lifecycle, AgentLifecycle::TurnEnd { .. }));
    assert_eq!(child.status, NormalizedStatus::Running);

    let root = adapter
        .parse_event(&make_input(
            "Stop",
            r#"{"session_id":"c-sess","hook_event_name":"Stop","stop_hook_active":false}"#,
        ))
        .unwrap();
    assert!(matches!(root.lifecycle, AgentLifecycle::Stop { .. }));
    assert_eq!(root.status, NormalizedStatus::Idle);
    // Stop is `continue`: allow lets it stop, deny keeps it going.
    assert_eq!(
        adapter.format_response(&PromptDecision::allow()),
        serde_json::json!({})
    );
    assert_eq!(
        adapter.format_response(&PromptDecision::deny(Some("keep going".into()))),
        serde_json::json!({"decision": "block", "reason": "keep going"})
    );
}

#[test]
fn claude_pre_tool_use_renders_allow_deny_defer() {
    let adapter = manifest_adapter();
    let event = adapter
        .parse_event(&make_input(
            "PreToolUse",
            r#"{"session_id":"c-sess","cwd":"/Users/me/proj","hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"rm -rf build"}}"#,
        ))
        .unwrap();
    assert_eq!(event.status, NormalizedStatus::Running);
    assert_eq!(
        adapter.format_response(&PromptDecision::allow()),
        serde_json::json!({"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": "allow"}})
    );
    assert_eq!(
        adapter.format_response(&PromptDecision::deny(Some("no".into()))),
        serde_json::json!({"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": "deny", "permissionDecisionReason": "no"}})
    );
    assert_eq!(
        adapter.format_response(&PromptDecision::defer()),
        serde_json::json!({})
    );
}

// ---------------------------------------------------------------------------
// Live parity e2e divergences (legacy b2v vs agent-hooks, Claude payloads)
// ---------------------------------------------------------------------------

use agent_hooks_protocol::{PromptAnswer, PromptKind};

fn permission_prompt(
    event: &agent_hooks_protocol::AgentEvent,
) -> &agent_hooks_protocol::PromptRequest {
    match &event.lifecycle {
        AgentLifecycle::PermissionRequest { prompt } => prompt,
        other => panic!("expected PermissionRequest, got {other:?}"),
    }
}

const ASK_DB: &str = r#"{"session_id":"c-sess","transcript_path":"/tmp/c.jsonl","cwd":"/Users/me/proj","permission_mode":"default","hook_event_name":"PermissionRequest","tool_name":"AskUserQuestion","tool_input":{"questions":[{"question":"Which database?","header":"DB","multiSelect":false,"options":[{"label":"Postgres","description":"Relational"},{"label":"SQLite","description":"Embedded"}]}]}}"#;

// 1. Legacy payload.rs extract_prompt_fields: command / file_path / description.
#[test]
fn claude_permission_prompt_carries_command_and_file_path() {
    let adapter = manifest_adapter();
    let bash = adapter
        .parse_event(&make_input(
            "PermissionRequest",
            r#"{"session_id":"c-sess","hook_event_name":"PermissionRequest","tool_name":"Bash","tool_input":{"command":"rm -rf build","description":"Remove build output"}}"#,
        ))
        .unwrap();
    let p = permission_prompt(&bash);
    assert_eq!(p.kind, PromptKind::Permission);
    assert_eq!(p.tool_name.as_deref(), Some("Bash"));
    assert_eq!(p.command.as_deref(), Some("rm -rf build"));
    assert_eq!(p.file_path, None);
    assert_eq!(p.description.as_deref(), Some("Remove build output"));

    let write = adapter
        .parse_event(&make_input(
            "PermissionRequest",
            r#"{"session_id":"c-sess","hook_event_name":"PermissionRequest","tool_name":"Write","tool_input":{"file_path":"/etc/hosts","content":"x"}}"#,
        ))
        .unwrap();
    let p = permission_prompt(&write);
    assert_eq!(p.file_path.as_deref(), Some("/etc/hosts"));
    assert_eq!(p.command, None);
}

// 2. Legacy prompt.rs build_claude_permission_response: deny has no empty message.
#[test]
fn claude_deny_omits_empty_reason() {
    let adapter = manifest_adapter();
    adapter
        .parse_event(&make_input(
            "PermissionRequest",
            r#"{"session_id":"c-sess","hook_event_name":"PermissionRequest","tool_name":"Bash","tool_input":{"command":"ls"}}"#,
        ))
        .unwrap();
    assert_eq!(
        adapter.format_response(&PromptDecision::deny(None)),
        serde_json::json!({"hookSpecificOutput": {"decision": {"behavior": "deny"}, "hookEventName": "PermissionRequest"}})
    );
    assert_eq!(
        adapter.format_response(&PromptDecision::deny(Some(String::new()))),
        serde_json::json!({"hookSpecificOutput": {"decision": {"behavior": "deny"}, "hookEventName": "PermissionRequest"}})
    );
    // A real reason still reaches Claude.
    assert_eq!(
        adapter.format_response(&PromptDecision::deny(Some("nope".into()))),
        serde_json::json!({"hookSpecificOutput": {"decision": {"behavior": "deny", "message": "nope"}, "hookEventName": "PermissionRequest"}})
    );
    // Plain allow: no updatedInput key.
    assert_eq!(
        adapter.format_response(&PromptDecision::allow()),
        serde_json::json!({"hookSpecificOutput": {"decision": {"behavior": "allow"}, "hookEventName": "PermissionRequest"}})
    );

    adapter
        .parse_event(&make_input(
            "PreToolUse",
            r#"{"session_id":"c-sess","hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"ls"}}"#,
        ))
        .unwrap();
    assert_eq!(
        adapter.format_response(&PromptDecision::deny(None)),
        serde_json::json!({"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": "deny"}})
    );
}

// 3. Legacy: Claude PreToolUse(AskUserQuestion) raises no prompt; Claude asks
// through PermissionRequest. Status still waits (tool_blocks_on_user).
#[test]
fn claude_pre_tool_use_ask_user_question_is_not_a_held_prompt() {
    let adapter = manifest_adapter();
    let event = adapter
        .parse_event(&make_input(
            "PreToolUse",
            &ASK_DB.replace("PermissionRequest", "PreToolUse"),
        ))
        .unwrap();
    match &event.lifecycle {
        AgentLifecycle::PreToolUse {
            prompt, tool_name, ..
        } => {
            assert_eq!(tool_name, "AskUserQuestion");
            assert!(
                prompt.is_none(),
                "must not hold a question gate: {prompt:?}"
            );
        }
        other => panic!("expected PreToolUse, got {other:?}"),
    }
    assert_eq!(event.status, NormalizedStatus::WaitingForInput);
    // Unclaimed gate → daemon defers → the observe no-op.
    assert_eq!(
        adapter.format_response(&PromptDecision::defer()),
        serde_json::json!({})
    );
}

// 4 + 5. Legacy prompt.rs build_claude_permission_response +
// build_claude_question_updated_input: kind Permission with questions, and the
// answer rides back in updatedInput (questions echoed without multiSelect:false).
#[test]
fn claude_permission_request_ask_user_question_answer_round_trip() {
    let adapter = manifest_adapter();
    let event = adapter
        .parse_event(&make_input("PermissionRequest", ASK_DB))
        .unwrap();
    let p = permission_prompt(&event);
    assert_eq!(p.kind, PromptKind::Permission);
    assert_eq!(p.questions.len(), 1);
    assert_eq!(p.questions[0].question, "Which database?");
    assert_eq!(p.description.as_deref(), Some("Which database?"));
    assert_eq!(event.status, NormalizedStatus::WaitingForInput);

    let qid = p.questions[0].id.clone();
    let decision = PromptDecision {
        allow: true,
        custom_input: None,
        answers: vec![PromptAnswer {
            question_id: qid,
            selected_options: vec!["SQLite".into()],
            custom_input: None,
        }],
        ask: false,
        allow_always: false,
        suggestion: None,
        defer: false,
        additional_context: None,
    };
    let legacy: serde_json::Value = serde_json::from_str(
        r#"{"hookSpecificOutput":{"decision":{"behavior":"allow","updatedInput":{"answers":{"Which database?":"SQLite"},"questions":[{"header":"DB","options":[{"description":"Relational","label":"Postgres"},{"description":"Embedded","label":"SQLite"}],"question":"Which database?"}]}},"hookEventName":"PermissionRequest"}}"#,
    )
    .unwrap();
    let out = adapter.format_response(&decision);
    assert_eq!(out, legacy);
    // Byte-identical to legacy (serde_json maps sort keys).
    assert_eq!(
        serde_json::to_string(&out).unwrap(),
        serde_json::to_string(&legacy).unwrap()
    );
}

// Legacy serialize_question_answers: multi-select picks and custom input join with ", ";
// multiSelect is echoed only when true.
#[test]
fn claude_permission_request_multi_select_and_custom_answers() {
    let adapter = manifest_adapter();
    let event = adapter
        .parse_event(&make_input(
            "PermissionRequest",
            r#"{"session_id":"c-sess","hook_event_name":"PermissionRequest","tool_name":"AskUserQuestion","tool_input":{"questions":[{"question":"Which features?","header":"Feat","multiSelect":true,"options":[{"label":"Auth"},{"label":"Billing"}]},{"question":"Name?","header":"Name","multiSelect":false,"options":[{"label":"foo"}]}]}}"#,
        ))
        .unwrap();
    let qs = permission_prompt(&event).questions.clone();
    let decision = PromptDecision {
        allow: true,
        custom_input: None,
        answers: vec![
            PromptAnswer {
                question_id: qs[0].id.clone(),
                selected_options: vec!["Auth".into(), "Billing".into()],
                custom_input: None,
            },
            PromptAnswer {
                question_id: qs[1].id.clone(),
                selected_options: vec![],
                custom_input: Some("my-app".into()),
            },
        ],
        ask: false,
        allow_always: false,
        suggestion: None,
        defer: false,
        additional_context: None,
    };
    let out = adapter.format_response(&decision);
    let updated = &out["hookSpecificOutput"]["decision"]["updatedInput"];
    assert_eq!(updated["answers"]["Which features?"], "Auth, Billing");
    assert_eq!(updated["answers"]["Name?"], "my-app");
    assert_eq!(updated["questions"][0]["multiSelect"], true);
    assert!(updated["questions"][1].get("multiSelect").is_none());
    assert_eq!(out["hookSpecificOutput"]["decision"]["behavior"], "allow");
}

// Legacy prompt.rs test_build_claude_permission_response_allow_always_uses_suggestions:
// AllowAlways echoes permission_suggestions as updatedPermissions.
#[test]
fn claude_permission_request_allow_always_echoes_suggestions() {
    let adapter = manifest_adapter();
    adapter
        .parse_event(&make_input(
            "PermissionRequest",
            r#"{"session_id":"c-sess","hook_event_name":"PermissionRequest","tool_name":"Bash","tool_input":{"command":"git status"},"permission_suggestions":[{"type":"tool","tool_name":"Bash","patterns":["git status"]}]}"#,
        ))
        .unwrap();
    let legacy: serde_json::Value = serde_json::from_str(
        r#"{"hookSpecificOutput":{"decision":{"behavior":"allow","updatedPermissions":[{"patterns":["git status"],"tool_name":"Bash","type":"tool"}]},"hookEventName":"PermissionRequest"}}"#,
    )
    .unwrap();
    let out = adapter.format_response(&PromptDecision::allow_always());
    assert_eq!(
        serde_json::to_string(&out).unwrap(),
        serde_json::to_string(&legacy).unwrap()
    );
    // Plain allow never carries the suggestions.
    assert_eq!(
        adapter.format_response(&PromptDecision::allow()),
        serde_json::json!({"hookSpecificOutput": {"decision": {"behavior": "allow"}, "hookEventName": "PermissionRequest"}})
    );
}

// Legacy test_build_claude_response: AllowAlways without suggestions == allow.
#[test]
fn claude_allow_always_without_suggestions_is_plain_allow() {
    let adapter = manifest_adapter();
    adapter
        .parse_event(&make_input(
            "PermissionRequest",
            r#"{"session_id":"c-sess","hook_event_name":"PermissionRequest","tool_name":"Bash","tool_input":{"command":"ls"},"permission_suggestions":[]}"#,
        ))
        .unwrap();
    assert_eq!(
        adapter.format_response(&PromptDecision::allow_always()),
        serde_json::json!({"hookSpecificOutput": {"decision": {"behavior": "allow"}, "hookEventName": "PermissionRequest"}})
    );
}

// Suggestions are surfaced as display-ready choices and a decision can echo
// back just one of them (indexes match the host's array order).
#[test]
fn claude_allow_with_suggestion_echoes_only_the_chosen_entry() {
    let adapter = manifest_adapter();
    let lifecycle = adapter
        .parse_event(&make_input(
            "PermissionRequest",
            r#"{"session_id":"c-sess","hook_event_name":"PermissionRequest","tool_name":"Bash","tool_input":{"command":"npm test"},"permission_suggestions":[{"type":"addRules","rules":[{"toolName":"Bash","ruleContent":"npm test"}],"behavior":"allow","destination":"localSettings"},{"type":"setMode","mode":"auto","destination":"session"}]}"#,
        ))
        .unwrap();
    let agent_hooks_protocol::AgentLifecycle::PermissionRequest { prompt } = lifecycle.lifecycle
    else {
        panic!("expected a permission request");
    };
    let labels: Vec<&str> = prompt
        .suggestions
        .iter()
        .map(|s| s.label.as_str())
        .collect();
    assert_eq!(
        labels,
        ["Always allow Bash(npm test)", "Auto mode for this session"]
    );

    let out = adapter.format_response(&PromptDecision::allow_with_suggestion(1));
    let permissions = &out["hookSpecificOutput"]["decision"]["updatedPermissions"];
    assert_eq!(permissions.as_array().map(Vec::len), Some(1));
    assert_eq!(permissions[0]["type"], "setMode");

    let all = adapter.format_response(&PromptDecision::allow_always());
    assert_eq!(
        all["hookSpecificOutput"]["decision"]["updatedPermissions"]
            .as_array()
            .map(Vec::len),
        Some(2)
    );
}
