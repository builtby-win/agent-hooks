use agent_hooks_adapters::declarative::DeclarativeAdapter;
use agent_hooks_adapters::manifest::AdapterManifest;
use agent_hooks_adapters::registry::AdapterRegistry;
use agent_hooks_adapters::HarnessAdapter;
use agent_hooks_protocol::{AgentLifecycle, EventContext, NormalizedStatus, RawHookInput};

const BUILTIN: &str = include_str!("../builtin/muse/adapter.json");

fn make_input(event_hint: &str, payload: &str) -> RawHookInput {
    RawHookInput {
        harness_hint: "muse".to_string(),
        event_hint: Some(event_hint.to_string()),
        raw_payload: payload.to_string(),
        context: EventContext::default(),
        request_id: None,
        passthrough_env: Default::default(),
    }
}

fn load_adapter() -> DeclarativeAdapter {
    let manifest = AdapterManifest::parse(BUILTIN).expect("builtin muse.json parses");
    assert_eq!(manifest.id, "muse");
    DeclarativeAdapter::new(manifest, "v1".to_string()).expect("v1 exists")
}

// Live-captured payloads (muse-bin-1.4.6, `muse exec --provider echo`,
// isolated HOME): SessionStart carries source/startup without turn_id;
// UserPromptSubmit carries prompt + turn_id; Stop carries
// last_assistant_message; SessionEnd carries reason.
const SESSION_START_LIVE: &str = r#"{"hook_event_name":"SessionStart","source":"startup","session_id":"01a132aa-01ed-7de0-b13a-1500dcd15916","cwd":"/private/tmp/muse-probe/ws","transcript_path":null,"model":"unknown","permission_mode":"default"}"#;
const PROMPT_SUBMIT_LIVE: &str = r#"{"hook_event_name":"UserPromptSubmit","prompt":"run the command echo hello-world-now","session_id":"01a132aa-01ed-7de0-b13a-1500dcd15916","turn_id":"1da31d3a-a9cc-4b25-81a3-eafa9e074eaf","cwd":"/private/tmp/muse-probe/ws","transcript_path":null,"model":"unknown","permission_mode":"default"}"#;
const STOP_LIVE: &str = r#"{"hook_event_name":"Stop","stop_hook_active":false,"last_assistant_message":"echo: run the command echo hello-world-now","session_id":"01a132aa-01ed-7de0-b13a-1500dcd15916","turn_id":"1da31d3a-a9cc-4b25-81a3-eafa9e074eaf","cwd":"/private/tmp/muse-probe/ws","transcript_path":null,"model":"unknown","permission_mode":"default"}"#;
const SESSION_END_LIVE: &str = r#"{"hook_event_name":"SessionEnd","reason":"other","session_id":"01a132aa-01ed-7de0-b13a-1500dcd15916","cwd":"/private/tmp/muse-probe/ws","transcript_path":null,"model":"unknown","permission_mode":"default"}"#;

#[test]
fn muse_manifest_session_lifecycle_live_payloads() {
    let adapter = load_adapter();

    let start = adapter
        .parse_event(&make_input("SessionStart", SESSION_START_LIVE))
        .expect("parse SessionStart");
    assert_eq!(start.session_id, "01a132aa-01ed-7de0-b13a-1500dcd15916");
    assert!(matches!(
        start.lifecycle,
        AgentLifecycle::SessionStart { .. }
    ));
    // SessionStart derives Idle per protocol (session opened, nothing
    // running yet — same as every other adapter).
    assert_eq!(start.status, NormalizedStatus::Idle);

    let prompt = adapter
        .parse_event(&make_input("UserPromptSubmit", PROMPT_SUBMIT_LIVE))
        .expect("parse UserPromptSubmit");
    match prompt.lifecycle {
        AgentLifecycle::UserPromptSubmit { user_prompt } => {
            assert_eq!(
                user_prompt.as_deref(),
                Some("run the command echo hello-world-now")
            );
        }
        ref other => panic!("expected UserPromptSubmit, got {:?}", other),
    }
    assert_eq!(prompt.status, NormalizedStatus::Running);

    let stop = adapter
        .parse_event(&make_input("Stop", STOP_LIVE))
        .expect("parse Stop");
    match stop.lifecycle {
        AgentLifecycle::Stop { summary, success } => {
            assert_eq!(
                summary.as_deref(),
                Some("echo: run the command echo hello-world-now")
            );
            assert!(success);
        }
        ref other => panic!("expected Stop, got {:?}", other),
    }
    assert_eq!(stop.status, NormalizedStatus::Idle);

    let end = adapter
        .parse_event(&make_input("SessionEnd", SESSION_END_LIVE))
        .expect("parse SessionEnd");
    assert!(matches!(end.lifecycle, AgentLifecycle::SessionEnd { .. }));
    assert_eq!(end.status, NormalizedStatus::Closed);
}

#[test]
fn muse_manifest_pretooluse_native_dialect() {
    let adapter = load_adapter();

    // Native PreToolUse fields are tool_name/tool_input (binary strings);
    // no live echo payload exists (echo never calls tools), so this uses the
    // documented field names without marking them live-captured.
    let input = make_input(
        "PreToolUse",
        r#"{"hook_event_name":"PreToolUse","tool_name":"read_file","tool_input":{"path":"Cargo.toml"},"session_id":"s1","cwd":"/tmp/w","permission_mode":"default"}"#,
    );
    let event = adapter.parse_event(&input).expect("parse PreToolUse");
    assert_eq!(event.session_id, "s1");
    match event.lifecycle {
        AgentLifecycle::PreToolUse {
            tool_name, input, ..
        } => {
            assert_eq!(tool_name, "read_file");
            assert_eq!(
                input.get("path").and_then(|v| v.as_str()),
                Some("Cargo.toml")
            );
        }
        ref other => panic!("expected PreToolUse, got {:?}", other),
    }
    assert_eq!(event.status, NormalizedStatus::Running);

    // Native gate dialect (binary strings): hookSpecificOutput with
    // permissionDecision. allow echoes the ORIGINAL tool input back as
    // updatedInput (response.allow_requires_tool_input): the host rejects a
    // bare allow, and agent-hooks never rewrites tool args, so the honest
    // approval is "proceed unchanged". Structured question answers still
    // win when present (none exist for Muse — no ask_tools).
    let allow = adapter.format_response(&agent_hooks_protocol::PromptDecision::allow());
    assert_eq!(
        allow,
        serde_json::json!({"hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "allow",
            "updatedInput": {"path": "Cargo.toml"},
        }})
    );
    // No leak: an unrelated event must not reuse the cached tool input.
    // Re-parse a PostToolUse (observe) and confirm a subsequent gate for a
    // DIFFERENT tool echoes the new input, not the stale one.
    let _ = adapter
        .parse_event(&make_input(
            "PreToolUse",
            r#"{"hook_event_name":"PreToolUse","tool_name":"write_file","tool_input":{"file":"notes.txt"},"session_id":"s1","cwd":"/tmp/w"}"#,
        ))
        .expect("parse second PreToolUse");
    let allow2 = adapter.format_response(&agent_hooks_protocol::PromptDecision::allow());
    assert_eq!(
        allow2,
        serde_json::json!({"hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "allow",
            "updatedInput": {"file": "notes.txt"},
        }})
    );
    let deny = adapter.format_response(&agent_hooks_protocol::PromptDecision::deny(Some(
        "need approval".to_string(),
    )));
    assert_eq!(
        deny,
        serde_json::json!({"hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": "need approval",
        }})
    );
}

#[test]
fn muse_manifest_permission_request_decision_dialect() {
    let adapter = load_adapter();

    // PermissionRequest shares the pre-tool deny strings but answers in its
    // own dialect: hookSpecificOutput.decision.{behavior,message} (binary
    // strings) — never permissionDecision. Wrong-dialect responses are the
    // consumer bug this pins.
    let input = make_input(
        "PermissionRequest",
        r#"{"hook_event_name":"PermissionRequest","tool_name":"write_file","session_id":"s2","cwd":"/tmp/w","permission_mode":"default"}"#,
    );
    let event = adapter
        .parse_event(&input)
        .expect("parse PermissionRequest");
    match event.lifecycle {
        AgentLifecycle::PermissionRequest { prompt } => {
            assert_eq!(prompt.tool_name.as_deref(), Some("write_file"));
        }
        ref other => panic!("expected PermissionRequest, got {:?}", other),
    }
    assert_eq!(event.status, NormalizedStatus::WaitingForInput);

    let deny = adapter.format_response(&agent_hooks_protocol::PromptDecision::deny(Some(
        "no writes".to_string(),
    )));
    assert_eq!(
        deny,
        serde_json::json!({"hookSpecificOutput": {
            "hookEventName": "PermissionRequest",
            "decision": {"behavior": "deny", "message": "no writes"},
        }})
    );

    let allow = adapter.format_response(&agent_hooks_protocol::PromptDecision::allow());
    assert_eq!(
        allow,
        serde_json::json!({"hookSpecificOutput": {
            "hookEventName": "PermissionRequest",
            "decision": {"behavior": "allow"},
        }})
    );
}

#[test]
fn muse_manifest_stop_continue_and_subagent_demotion() {
    let adapter = load_adapter();

    // Parse Stop FIRST: format_response renders from adapter state (last
    // parsed capability); without a preceding Stop parse it would render
    // the default gate dialect instead of the continue dialect.
    let stop = adapter
        .parse_event(&make_input("Stop", STOP_LIVE))
        .expect("parse Stop");
    assert!(matches!(stop.lifecycle, AgentLifecycle::Stop { .. }));

    // Stop continuation is native `decision: block` + reason (live: a Stop
    // hook returning it re-drove the echo turn until the continuation cap).
    let keep_going = adapter.format_response(&agent_hooks_protocol::PromptDecision::deny(Some(
        "keep going".to_string(),
    )));
    assert_eq!(
        keep_going,
        serde_json::json!({"decision": "block", "reason": "keep going"})
    );
    let done = adapter.format_response(&agent_hooks_protocol::PromptDecision::allow());
    assert_eq!(done, serde_json::json!({}));

    // A child's Stop is not the session's turn ending: subagent-scoped Stop
    // demotes to TurnEnd (derived status stays Running).
    let child_stop = adapter
        .parse_event(&make_input(
            "Stop",
            r#"{"hook_event_name":"Stop","session_id":"child-1","parent_session_id":"root-1","cwd":"/tmp/w"}"#,
        ))
        .expect("parse subagent Stop");
    assert!(matches!(
        child_stop.lifecycle,
        AgentLifecycle::TurnEnd { .. }
    ));
    assert_eq!(child_stop.status, NormalizedStatus::Running);
}

#[test]
fn muse_manifest_llm_markers_stopfailure_and_observe() {
    let adapter = load_adapter();

    // PreLLMCall / PostLLMCall keys are live-captured; both are markers
    // (raw_event preserves the full message/tool arrays without duplicate
    // allocation). PostLLMCall summary reads output_text_preview.
    let pre = adapter
        .parse_event(&make_input(
            "PreLLMCall",
            r#"{"hook_event_name":"PreLLMCall","provider":"model.unknown.response","request_id":"r:0:1","attempt":1,"step":0,"messages":[],"message_count":1,"tools":[],"tool_count":0,"session_id":"s3","turn_id":"t3","cwd":"/tmp/w","transcript_path":null,"model":"unknown","permission_mode":"default"}"#,
        ))
        .expect("parse PreLLMCall");
    assert!(matches!(pre.lifecycle, AgentLifecycle::PreLLMCall));
    assert_eq!(pre.status, NormalizedStatus::Running);

    let post = adapter
        .parse_event(&make_input(
            "PostLLMCall",
            r#"{"hook_event_name":"PostLLMCall","provider":"model.unknown.response","request_id":"r:0:1","attempt":1,"step":0,"status":"success","response_id":"muse-tui-echo","finish_reason":null,"error":null,"output_text_preview":"echo: hi","tool_call_count":0,"session_id":"s3","turn_id":"t3","cwd":"/tmp/w","transcript_path":null,"model":"unknown","permission_mode":"default"}"#,
        ))
        .expect("parse PostLLMCall");
    assert!(matches!(post.lifecycle, AgentLifecycle::PostLLMCall));
    assert_eq!(post.status, NormalizedStatus::Running);

    // StopFailure derives Failed (per-turn, never SessionEnd/Closed):
    // error_details is the documented field; the production-dark event has
    // no live payload, so this uses the documented field names.
    let failure = adapter
        .parse_event(&make_input(
            "StopFailure",
            r#"{"hook_event_name":"StopFailure","error_details":"quota exhausted","last_assistant_message":"half answer","session_id":"s3","cwd":"/tmp/w"}"#,
        ))
        .expect("parse StopFailure");
    match failure.lifecycle {
        AgentLifecycle::StopFailure { error } => {
            assert_eq!(error, "quota exhausted");
        }
        ref other => panic!("expected StopFailure, got {:?}", other),
    }
    assert_eq!(failure.status, NormalizedStatus::Failed);

    // PostToolBatch is production-dark (binary strings); mapped per contract
    // as a gate so a future block dialect has a channel. Marker lifecycle.
    let batch = adapter
        .parse_event(&make_input(
            "PostToolBatch",
            r#"{"hook_event_name":"PostToolBatch","tool_calls":[{"tool_name":"read_file"}],"session_id":"s3","cwd":"/tmp/w"}"#,
        ))
        .expect("parse PostToolBatch");
    assert!(matches!(
        batch.lifecycle,
        AgentLifecycle::PostToolBatch
    ));

    // Observe events render {} regardless of decision (never a deny shape).
    // Re-parse an observe event first so format_response renders the observe
    // dialect (last parsed capability selects the template).
    let _ = adapter
        .parse_event(&make_input("PostToolUse", r#"{"session_id":"s3"}"#))
        .expect("parse PostToolUse");
    let observe_deny =
        adapter.format_response(&agent_hooks_protocol::PromptDecision::deny(Some(
            "ignored".to_string(),
        )));
    assert_eq!(observe_deny, serde_json::json!({}));
}

#[test]
fn muse_manifest_observe_events_session_identity_and_registry() {
    let adapter = load_adapter();

    // Notification is observational (live: never fired under echo).
    let note = adapter
        .parse_event(&make_input(
            "Notification",
            r#"{"hook_event_name":"Notification","message":"approval needed","session_id":"s4","cwd":"/tmp/w"}"#,
        ))
        .expect("parse Notification");
    assert!(matches!(
        note.lifecycle,
        AgentLifecycle::Notification { .. }
    ));

    // Session identity: payload session_id wins; cwd overrides shim cwd.
    assert_eq!(note.session_id, "s4");
    assert_eq!(
        note.context.cwd.as_deref(),
        Some(std::path::Path::new("/tmp/w"))
    );

    // Interrupt and ToolUseStart install as async fire-and-forget observe
    // hooks (live: sync versions rejected — "synchronous cancellation hooks
    // are not admitted"). ToolUseStart parses to the native ToolUseStart
    // marker (raw_event preserves tool details); Interrupt to Interrupt.
    let interrupt = adapter
        .parse_event(&make_input("Interrupt", r#"{"session_id":"s4"}"#))
        .expect("parse Interrupt");
    assert!(matches!(
        interrupt.lifecycle,
        AgentLifecycle::Interrupt
    ));
    let tool_start = adapter
        .parse_event(&make_input(
            "ToolUseStart",
            r#"{"hook_event_name":"ToolUseStart","tool_name":"read_file","tool_input":{"path":"x"},"tool_use_id":"u1","session_id":"s4","cwd":"/tmp/w"}"#,
        ))
        .expect("parse ToolUseStart");
    assert!(matches!(
        tool_start.lifecycle,
        AgentLifecycle::ToolUseStart
    ));
    assert_eq!(tool_start.status, NormalizedStatus::Running);

    // Registry resolves the builtin manifest by id and alias.
    for id in ["muse", "muse-code", "muse-cli"] {
        let reg = AdapterRegistry::get_versioned(id, None).expect("muse registered");
        let ev = reg
            .parse_event(&make_input("UserPromptSubmit", PROMPT_SUBMIT_LIVE))
            .expect("registry parse UserPromptSubmit");
        match ev.lifecycle {
            AgentLifecycle::UserPromptSubmit { user_prompt } => {
                assert_eq!(
                    user_prompt.as_deref(),
                    Some("run the command echo hello-world-now")
                );
            }
            ref other => panic!("expected UserPromptSubmit, got {:?}", other),
        }
    }

    // SessionFork is a new child session initialization: it parses to
    // SessionStart with the fork's own identity (fork_session_id — the
    // payload carries no parent id; session_fork.py reads fork_session_id
    // + cwd + resume_command only).
    let fork = adapter
        .parse_event(&make_input(
            "SessionFork",
            r#"{"hook_event_name":"SessionFork","mode":"launch","fork_session_id":"fork-9","cwd":"/tmp/w","resume_command":"muse resume fork-9"}"#,
        ))
        .expect("parse SessionFork");
    assert!(matches!(
        fork.lifecycle,
        AgentLifecycle::SessionStart { .. }
    ));
    assert_eq!(fork.session_id, "fork-9");
}


// Muse hook payloads for internal subagents carry no parent marker; the
// link lives in the parent's session.jsonl (`child_session_id`, record
// `memory_reminder_child_session_linked`). Live-shaped fixture on a temp
// HOME: parent has its own directory, the child does not.
#[test]
fn muse_subagent_parent_resolved_from_session_store() {
    let home = std::env::temp_dir().join(format!("muse-link-{}", std::process::id()));
    let day = home.join(".local/share/muse/sessions/2026/10/13");
    let parent = "01a137bb-91d1-7d23-90e6-0f949d3c455b";
    let child = "f15343ca-c7e2-4c97-95f4-a581b836770a";
    std::fs::create_dir_all(day.join(parent)).unwrap();
    std::fs::write(
        day.join(parent).join("session.jsonl"),
        format!(
            "{{\"stream\": {{\"kind\": \"session\", \"id\": \"{parent}\"}}, \"payload\": {{\"kind\": \"run\", \"event\": {{\"kind\": \"memory_reminder_child_session_linked\", \"reminder_agent_id\": \"skill-reminder\", \"child_session_id\": \"{child}\"}}}}}}\n"
        ),
    )
    .unwrap();
    std::env::set_var("HOME", &home);

    let adapter = load_adapter();
    let start = format!(
        r#"{{"hook_event_name":"SubagentStart","subagent_id":"skill-reminder","child_session_id":"{child}","session_id":"{child}","turn_id":"{child}","cwd":"/tmp/w"}}"#
    );
    let ev = adapter
        .parse_event(&make_input("SubagentStart", &start))
        .expect("parse SubagentStart");
    assert_eq!(ev.session_id, child);
    assert_eq!(ev.parent_session_id.as_deref(), Some(parent));

    // The top-level session has its own directory: no parent.
    let top = format!(
        r#"{{"hook_event_name":"UserPromptSubmit","prompt":"hi","session_id":"{parent}","cwd":"/tmp/w"}}"#
    );
    let ev = adapter
        .parse_event(&make_input("UserPromptSubmit", &top))
        .expect("parse UserPromptSubmit");
    assert_eq!(ev.parent_session_id, None);

    // Unknown child: no link record anywhere, so no parent (never invented).
    let orphan = r#"{"hook_event_name":"SubagentStart","session_id":"deadbeef-0000","cwd":"/tmp/w"}"#;
    let ev = adapter
        .parse_event(&make_input("SubagentStart", orphan))
        .expect("parse orphan");
    assert_eq!(ev.parent_session_id, None);

    let _ = std::fs::remove_dir_all(&home);
}
