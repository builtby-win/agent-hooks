use agent_hooks_adapters::manifest::AdapterManifest;
use agent_hooks_adapters::{declarative::DeclarativeAdapter, HarnessAdapter};
use agent_hooks_protocol::{AgentLifecycle, EventContext, RawHookInput};

const BUILTIN_MANIFEST: &str = include_str!("../builtin/antigravity/adapter.json");

fn make_input(event_hint: &str, payload: &str) -> RawHookInput {
    RawHookInput {
        harness_hint: "antigravity".to_string(),
        event_hint: Some(event_hint.to_string()),
        raw_payload: payload.to_string(),
        context: EventContext::default(),
        request_id: None,
        passthrough_env: Default::default(),
    }
}

fn load_adapter() -> DeclarativeAdapter {
    let manifest = AdapterManifest::parse(BUILTIN_MANIFEST).expect("builtin manifest parses");
    assert_eq!(manifest.id, "antigravity");
    assert_eq!(manifest.default_version(), "v1");
    DeclarativeAdapter::new(manifest, "v1".to_string()).expect("create adapter")
}

#[test]
fn manifest_antigravity_session_and_invocation_lifecycle() {
    let adapter = load_adapter();
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let transcript_file = temp_dir.path().join("transcript.jsonl");
    std::fs::write(
        &transcript_file,
        concat!(
            "{\"step_index\":0,\"source\":\"USER_EXPLICIT\",\"type\":\"USER_INPUT\",\"status\":\"DONE\",\"content\":\"<USER_REQUEST>First request</USER_REQUEST>\"}\n",
            "{\"step_index\":1,\"source\":\"MODEL\",\"type\":\"PLANNER_RESPONSE\",\"status\":\"DONE\",\"content\":\"Working\"}\n",
            "{\"step_index\":2,\"source\":\"USER_EXPLICIT\",\"type\":\"USER_INPUT\",\"status\":\"DONE\",\"content\":\"<USER_REQUEST>Second request</USER_REQUEST>\"}\n"
        ),
    )
    .expect("write transcript");

    let session = adapter
        .parse_event(&make_input(
            "SessionStart",
            r#"{"conversationId":"ag-1","workspacePaths":["/tmp/project"]}"#,
        ))
        .expect("parse SessionStart");
    assert!(matches!(
        session.lifecycle,
        AgentLifecycle::SessionStart { .. }
    ));

    let pre_prompt = adapter
        .parse_event(&make_input(
            "PreInvocation",
            &format!(
                r#"{{"conversationId":"ag-1","invocationNum":0,"initialNumSteps":3,"transcriptPath":"{}"}}"#,
                transcript_file.display()
            ),
        ))
        .expect("parse first model invocation");
    assert!(matches!(
        pre_prompt.lifecycle,
        AgentLifecycle::UserPromptSubmit {
            user_prompt: Some(ref prompt)
        } if prompt == "Second request"
    ));

    let pre_tool_loop = adapter
        .parse_event(&make_input(
            "PreInvocation",
            r#"{"conversationId":"ag-1","invocationNum":1,"initialNumSteps":3}"#,
        ))
        .expect("parse model invocation after tool");
    assert!(matches!(
        pre_tool_loop.lifecycle,
        AgentLifecycle::PreLLMCall
    ));

    let post = adapter
        .parse_event(&make_input(
            "PostInvocation",
            r#"{"conversationId":"ag-1","invocationNum":0,"initialNumSteps":1}"#,
        ))
        .expect("parse PostInvocation");
    assert!(matches!(post.lifecycle, AgentLifecycle::PostLLMCall));
}

#[test]
fn manifest_antigravity_pretooluse_gate() {
    let adapter = load_adapter();

    // PreToolUse run_command (adapted from fixture 2)
    let input = make_input(
        "PreToolUse",
        r#"{
            "conversationId": "c3b14a3e-bf6a-400d-aada-4e41e43612f5",
            "toolCall": {
                "name": "run_command",
                "args": {
                    "CommandLine": "npm test",
                    "Cwd": "/workspace/project"
                }
            },
            "stepIdx": 1
        }"#,
    );
    let event = adapter.parse_event(&input).expect("parse PreToolUse");
    assert_eq!(event.session_id, "c3b14a3e-bf6a-400d-aada-4e41e43612f5");
    match event.lifecycle {
        AgentLifecycle::PreToolUse {
            tool_name, input, ..
        } => {
            assert_eq!(tool_name, "run_command");
            assert_eq!(
                input.get("CommandLine").and_then(|v| v.as_str()),
                Some("npm test")
            );
        }
        ref other => panic!("expected PreToolUse, got {:?}", other),
    }

    // Gate dialect: exact JSON
    let allow = adapter.format_response(&agent_hooks_protocol::PromptDecision::allow());
    assert_eq!(allow, serde_json::json!({"decision": "allow"}));
    let deny = adapter.format_response(&agent_hooks_protocol::PromptDecision::deny(Some(
        "Blocked by policy".to_string(),
    )));
    assert_eq!(
        deny,
        serde_json::json!({"decision": "deny", "reason": "Blocked by policy"})
    );
    // Antigravity reads a PreToolUse reply without a decision as a deny, so
    // "no opinion" (defer, fail-open) must render as an explicit allow.
    let defer = adapter.format_response(&agent_hooks_protocol::PromptDecision::defer());
    assert_eq!(defer, serde_json::json!({"decision": "allow"}));
}

#[test]
fn manifest_antigravity_deny_without_reason_omits_empty_field() {
    let adapter = load_adapter();

    // format_response is stateful on capability — parse a gate event first.
    let input = make_input(
        "PreToolUse",
        r#"{
            "conversationId": "c3b14a3e-bf6a-400d-aada-4e41e43612f5",
            "toolCall": { "name": "run_command", "args": {"CommandLine": "npm test"} }
        }"#,
    );
    adapter.parse_event(&input).expect("parse PreToolUse");

    // A consumer deny with no text must not render `"reason": ""` — agy
    // surfaces that as "denied by pre-tool hook: " with a trailing blank.
    let deny = adapter.format_response(&agent_hooks_protocol::PromptDecision::deny(None));
    assert_eq!(deny, serde_json::json!({"decision": "deny"}));
}

#[test]
fn manifest_antigravity_posttooluse_observe() {
    let adapter = load_adapter();

    let input = make_input(
        "PostToolUse",
        r#"{
            "conversationId": "c3b14a3e-bf6a-400d-aada-4e41e43612f5",
            "toolCall": { "name": "run_command" },
            "output": "test passed",
            "stepIdx": 3
        }"#,
    );
    let event = adapter.parse_event(&input).expect("parse PostToolUse");
    match event.lifecycle {
        AgentLifecycle::PostToolUse {
            tool_name,
            output_preview,
        } => {
            assert_eq!(tool_name, "run_command");
            assert_eq!(output_preview.as_deref(), Some("test passed"));
        }
        ref other => panic!("expected PostToolUse, got {:?}", other),
    }

    // Observe dialect: exactly {}
    let out = adapter.format_response(&agent_hooks_protocol::PromptDecision::allow());
    assert_eq!(out, serde_json::json!({}));
}

#[test]
fn manifest_antigravity_stop_continue() {
    let adapter = load_adapter();

    // Stop (model_stop) (adapted from fixture 5)
    let input = make_input(
        "Stop",
        r#"{
            "conversationId": "c3b14a3e-bf6a-400d-aada-4e41e43612f5",
            "executionNum": 1,
            "terminationReason": "model_stop",
            "fullyIdle": true
        }"#,
    );
    let event = adapter.parse_event(&input).expect("parse Stop");
    match event.lifecycle {
        AgentLifecycle::Stop { success, summary } => {
            assert!(success);
            assert_eq!(summary.as_deref(), Some("model_stop"));
        }
        ref other => panic!("expected Stop, got {:?}", other),
    }

    // Continue dialect: deny keeps going, allow stops
    let cont = adapter.format_response(&agent_hooks_protocol::PromptDecision::deny(Some(
        "wrapping up".to_string(),
    )));
    assert_eq!(
        cont,
        serde_json::json!({"decision": "continue", "reason": "wrapping up"})
    );
    let stop = adapter.format_response(&agent_hooks_protocol::PromptDecision::allow());
    assert_eq!(stop, serde_json::json!({}));
}

#[test]
fn manifest_antigravity_stop_reads_completed_reply_from_current_turn() {
    let adapter = load_adapter();
    let dir = tempfile::tempdir().unwrap();
    let transcript = dir.path().join("transcript.jsonl");
    let mut rows = vec![
        serde_json::json!({"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","content":"hi"}),
        serde_json::json!({"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","content":"Hi! How can I help you today?"}),
    ];
    let parse_stop = |rows: &[serde_json::Value], extra: serde_json::Value| {
        let text = rows
            .iter()
            .map(serde_json::Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&transcript, text + "\n").unwrap();
        let mut payload = serde_json::json!({
            "conversationId":"ag-history",
            "transcriptPath":transcript,
            "terminationReason":"model_stop",
            "fullyIdle":true
        });
        payload
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        adapter
            .parse_event(&make_input("Stop", &payload.to_string()))
            .unwrap()
            .lifecycle
    };
    assert!(matches!(
        parse_stop(&rows, serde_json::json!({})),
        AgentLifecycle::Stop { summary: Some(text), success: true }
            if text == "Hi! How can I help you today?"
    ));
    rows.push(serde_json::json!({"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","content":"hello"}));
    // No reply yet: do not attribute the previous turn's answer to this turn.
    assert!(matches!(
        parse_stop(&rows, serde_json::json!({})),
        AgentLifecycle::Stop { summary: Some(text), .. } if text == "model_stop"
    ));
    rows.push(serde_json::json!({"source":"MODEL","type":"PLANNER_RESPONSE","status":"RUNNING","content":"unfinished"}));
    rows.push(serde_json::json!({"source":"MODEL","type":"THINKING","status":"DONE","content":"private reasoning"}));
    assert!(matches!(
        parse_stop(&rows, serde_json::json!({})),
        AgentLifecycle::Stop { summary: Some(text), .. } if text == "model_stop"
    ));
    rows.push(serde_json::json!({"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","content":"Hello! What would you like to work on?"}));
    assert!(matches!(
        parse_stop(&rows, serde_json::json!({})),
        AgentLifecycle::Stop { summary: Some(text), success: true }
            if text == "Hello! What would you like to work on?"
    ));
    // Real errors remain authoritative; weak stops retain their lifecycle.
    assert!(matches!(
        parse_stop(&rows, serde_json::json!({"error":"connection failed"})),
        AgentLifecycle::Stop { summary: Some(text), success: false } if text == "connection failed"
    ));
    assert!(matches!(
        parse_stop(&rows, serde_json::json!({"fullyIdle":false})),
        AgentLifecycle::TurnEnd { .. }
    ));
}

#[test]
fn manifest_antigravity_stop_reply_extraction_is_bounded_to_transcript_tail() {
    let adapter = load_adapter();
    let dir = tempfile::tempdir().unwrap();
    let transcript = dir.path().join("transcript.jsonl");
    let row = |source: &str, kind: &str, content: &str| {
        serde_json::json!({"source":source,"type":kind,"status":"DONE","content":content})
            .to_string()
    };
    // Far larger than the 1MB tail window, so the window starts mid-line.
    let bulk = row("MODEL", "TOOL_RESULT", &"x".repeat(2 * 1024 * 1024));
    let stop_summary = || {
        let payload = serde_json::json!({
            "conversationId":"ag-tail",
            "transcriptPath":transcript,
            "terminationReason":"model_stop",
            "fullyIdle":true
        });
        match adapter
            .parse_event(&make_input("Stop", &payload.to_string()))
            .unwrap()
            .lifecycle
        {
            AgentLifecycle::Stop { summary, .. } => summary,
            other => panic!("expected Stop, got {other:?}"),
        }
    };
    let write_rows = |rows: &[String]| std::fs::write(&transcript, rows.join("\n") + "\n").unwrap();

    write_rows(&[
        row("USER_EXPLICIT", "USER_INPUT", "hi"),
        bulk.clone(),
        row("MODEL", "PLANNER_RESPONSE", "Final answer"),
    ]);
    assert_eq!(stop_summary().as_deref(), Some("Final answer"));

    // The previous turn's reply sits before the cut-off bulk row and the current
    // user row is outside the window: still nothing may be attributed.
    write_rows(&[
        row("MODEL", "PLANNER_RESPONSE", "Old answer"),
        row("USER_EXPLICIT", "USER_INPUT", "again"),
        bulk,
    ]);
    assert_eq!(stop_summary().as_deref(), Some("model_stop"));
}

#[test]
fn manifest_antigravity_registry_prefers_manifest() {
    let adapter = agent_hooks_adapters::AdapterRegistry::get_versioned("antigravity", None)
        .expect("antigravity adapter resolves");
    assert_eq!(adapter.id(), "antigravity");

    // Gate event first (format_response is stateful on capability)
    let input = make_input(
        "PreToolUse",
        r#"{
            "conversationId": "c3b14a3e-bf6a-400d-aada-4e41e43612f5",
            "toolCall": { "name": "run_command", "args": {"CommandLine": "npm test"} }
        }"#,
    );
    adapter.parse_event(&input).expect("parse PreToolUse");

    // Manifest dialect: an empty-reason deny omits the key instead of
    // rendering `"reason": ""` (agy prints that as a blank after the colon),
    // and defer renders an explicit allow via defer_overrides — the latter
    // is manifest-only, proving the registry returned the manifest.
    let deny = adapter.format_response(&agent_hooks_protocol::PromptDecision::deny(None));
    assert_eq!(deny, serde_json::json!({"decision": "deny"}));
    let defer = adapter.format_response(&agent_hooks_protocol::PromptDecision::defer());
    assert_eq!(defer, serde_json::json!({"decision": "allow"}));
}

fn make_input_with_cwd(event_hint: &str, payload: &str, cwd: &str) -> RawHookInput {
    let mut input = make_input(event_hint, payload);
    // Simulate the shim spawned with the harness config dir as process cwd.
    input.context.cwd = Some(std::path::PathBuf::from(cwd));
    input.context.git_worktree = Some(std::path::PathBuf::from(cwd));
    input
}

#[test]
fn manifest_antigravity_cwd_from_workspace_paths() {
    let adapter = load_adapter();
    let config_cwd = "/Users/dev/.gemini/config";
    let input = make_input_with_cwd(
        "PreToolUse",
        r#"{
            "toolCall": {"name": "run_command", "args": {"CommandLine": "npm test", "Cwd": "/tmp/other", "WaitMsBeforeAsync": 5000}},
            "stepIdx": 19,
            "conversationId": "ec33ebf9-0cba-4100-8142-c61503f6c587",
            "workspacePaths": ["/Users/dev/project"],
            "transcriptPath": "/Users/dev/.gemini/antigravity/brain/ec33ebf9/logs/transcript.jsonl",
            "modelName": "gemini-3.6-flash-medium"
        }"#,
        config_cwd,
    );
    let event = adapter.parse_event(&input).expect("parse PreToolUse");
    // workspacePaths[0] wins over both the poisoned process cwd and the
    // per-tool Cwd: session cwd is the mounted workspace.
    assert_eq!(
        event.context.cwd.as_deref(),
        Some(std::path::Path::new("/Users/dev/project"))
    );
    assert_eq!(
        event.context.git_worktree.as_deref(),
        Some(std::path::Path::new("/Users/dev/project"))
    );
}

#[test]
fn manifest_antigravity_config_fallback_cwd_discarded() {
    let adapter = load_adapter();
    // No workspacePaths / transcriptPath payload cwd: the shim fallback is
    // the poisoned process cwd (~/.gemini/config) and must be discarded.
    let input = make_input_with_cwd(
        "SessionStart",
        r#"{"conversationId":"ec33ebf9-0cba-4100-8142-c61503f6c587"}"#,
        "/Users/dev/.gemini/config",
    );
    let event = adapter.parse_event(&input).expect("parse SessionStart");
    assert_eq!(event.context.cwd, None);
    assert_eq!(event.context.git_worktree, None);
}

#[test]
fn manifest_antigravity_stop_not_idle_stays_running() {
    let adapter = load_adapter();
    // fullyIdle=false: background tasks still running, agent NOT done.
    let input = make_input(
        "Stop",
        r#"{
            "conversationId": "ec33ebf9-0cba-4100-8142-c61503f6c587",
            "executionNum": 1,
            "terminationReason": "model_stop",
            "error": "",
            "fullyIdle": false
        }"#,
    );
    let event = adapter.parse_event(&input).expect("parse Stop");
    match event.lifecycle {
        AgentLifecycle::TurnEnd { .. } => {}
        ref other => panic!("expected TurnEnd while not idle, got {:?}", other),
    }
    assert_eq!(
        event.lifecycle.derived_status(),
        agent_hooks_protocol::NormalizedStatus::Running
    );
}

#[test]
fn manifest_antigravity_stop_declares_demote_condition() {
    // The weak-Stop suppression must live in the manifest, not hardcoded on
    // a payload key in the engine: deleting `demote_when` here would let
    // fake stops (background tasks still running) mark the session idle.
    let manifest = AdapterManifest::parse(BUILTIN_MANIFEST).expect("builtin manifest parses");
    let stop = manifest
        .get_version("v1")
        .expect("v1 exists")
        .events
        .get("Stop")
        .expect("Stop event declared")
        .demote_when
        .as_ref()
        .expect("Stop declares demote_when")
        .get("fullyIdle");
    assert_eq!(stop, Some(&serde_json::json!(false)));
}

#[test]
fn manifest_antigravity_stop_error_fails() {
    let adapter = load_adapter();
    let input = make_input(
        "Stop",
        r#"{
            "conversationId": "ec33ebf9-0cba-4100-8142-c61503f6c587",
            "executionNum": 1,
            "terminationReason": "error",
            "error": "context limit exceeded",
            "fullyIdle": true
        }"#,
    );
    let event = adapter.parse_event(&input).expect("parse Stop");
    match event.lifecycle {
        AgentLifecycle::Stop { success, .. } => assert!(!success),
        ref other => panic!("expected Stop, got {:?}", other),
    }
    assert_eq!(
        event.lifecycle.derived_status(),
        agent_hooks_protocol::NormalizedStatus::Failed
    );
}

#[test]
fn manifest_antigravity_stop_weak_pair_and_absent_idle() {
    let adapter = load_adapter();
    // Present-false demotes to TurnEnd (agent still working).
    let not_idle = make_input(
        "Stop",
        r#"{
            "conversationId": "ec33ebf9-0cba-4100-8142-c61503f6c587",
            "executionNum": 1,
            "terminationReason": "model_stop",
            "error": "",
            "fullyIdle": false
        }"#,
    );
    let event = adapter.parse_event(&not_idle).expect("parse Stop");
    match event.lifecycle {
        AgentLifecycle::TurnEnd { .. } => {}
        ref other => panic!("expected TurnEnd while not idle, got {:?}", other),
    }
    assert_eq!(
        event.lifecycle.derived_status(),
        agent_hooks_protocol::NormalizedStatus::Running
    );
    // Present-true stays Stop.
    let idle = make_input(
        "Stop",
        r#"{
            "conversationId": "ec33ebf9-0cba-4100-8142-c61503f6c587",
            "executionNum": 1,
            "terminationReason": "model_stop",
            "error": "",
            "fullyIdle": true
        }"#,
    );
    let event = adapter.parse_event(&idle).expect("parse Stop");
    match event.lifecycle {
        AgentLifecycle::Stop { .. } => {}
        ref other => panic!("expected Stop when idle, got {:?}", other),
    }
    // Absent key stays Stop: only present-false demotes.
    let absent = make_input(
        "Stop",
        r#"{
            "conversationId": "ec33ebf9-0cba-4100-8142-c61503f6c587",
            "executionNum": 1,
            "terminationReason": "model_stop",
            "error": ""
        }"#,
    );
    let event = adapter.parse_event(&absent).expect("parse Stop");
    match event.lifecycle {
        AgentLifecycle::Stop { .. } => {}
        ref other => panic!("expected Stop when fullyIdle absent, got {:?}", other),
    }
}

#[test]
fn manifest_antigravity_posttooluse_observe_strict_no_leak() {
    let adapter = load_adapter();
    let input = make_input(
        "PostToolUse",
        r#"{
            "conversationId": "c3b14a3e-bf6a-400d-aada-4e41e43612f5",
            "toolCall": { "name": "run_command" },
            "output": "test passed",
            "stepIdx": 3
        }"#,
    );
    adapter.parse_event(&input).expect("parse PostToolUse");
    // Observe dialect renders exactly {} even for a deny: no reason/decision leakage.
    let out = adapter.format_response(&agent_hooks_protocol::PromptDecision::deny(Some(
        "Blocked by policy".to_string(),
    )));
    assert_eq!(out, serde_json::json!({}));
}

#[test]
fn manifest_antigravity_gate_ask_degrades_to_deny() {
    // Gate event whose verdicts exclude ask, with a distinct ask template so
    // widening verdicts to include ask would observably change rendering.
    let mut v: serde_json::Value =
        serde_json::from_str(BUILTIN_MANIFEST).expect("builtin manifest is JSON");
    v["versions"]["v1"]["events"]["PreToolUse"]["verdicts"] = serde_json::json!(["allow", "deny"]);
    v["versions"]["v1"]["response"]["gate"]["ask"] =
        serde_json::json!({"decision": "ask", "question": "{reason}"});
    let manifest = AdapterManifest::parse(&v.to_string()).expect("verdict-pinned manifest parses");
    let adapter = DeclarativeAdapter::new(manifest, "v1".to_string()).expect("create adapter");
    let input = make_input(
        "PreToolUse",
        r#"{
            "conversationId": "c3b14a3e-bf6a-400d-aada-4e41e43612f5",
            "toolCall": { "name": "run_command", "args": {"CommandLine": "npm test"} }
        }"#,
    );
    adapter.parse_event(&input).expect("parse PreToolUse");
    let reason = "need a human".to_string();
    let ask_out = adapter.format_response(&agent_hooks_protocol::PromptDecision::ask(Some(
        reason.clone(),
    )));
    let deny_out =
        adapter.format_response(&agent_hooks_protocol::PromptDecision::deny(Some(reason)));
    assert_eq!(ask_out, deny_out);
    assert_eq!(
        ask_out.get("decision").and_then(|d| d.as_str()),
        Some("deny")
    );
}

#[test]
fn manifest_antigravity_session_fallback_ephemeral_uuid() {
    let adapter = load_adapter();
    // No session keys anywhere: conversationId/sessionId/session_id all absent.
    let payload = r#"{
        "toolCall": { "name": "run_command", "args": {"CommandLine": "npm test"} },
        "stepIdx": 1
    }"#;
    let first = adapter
        .parse_event(&make_input("PreToolUse", payload))
        .expect("sessionless parse succeeds");
    let second = adapter
        .parse_event(&make_input("PreToolUse", payload))
        .expect("sessionless parse succeeds");
    assert!(!first.session_id.is_empty());
    assert!(!second.session_id.is_empty());
    assert_ne!(
        first.session_id, second.session_id,
        "ephemeral fallback ids must differ per parse"
    );
}

#[test]
fn manifest_antigravity_gate_reason_capped() {
    let adapter = load_adapter();
    let input = make_input(
        "PreToolUse",
        r#"{
            "conversationId": "c3b14a3e-bf6a-400d-aada-4e41e43612f5",
            "toolCall": { "name": "run_command", "args": {"CommandLine": "npm test"} }
        }"#,
    );
    adapter.parse_event(&input).expect("parse PreToolUse");
    let long = "x".repeat(5000);
    let out = adapter.format_response(&agent_hooks_protocol::PromptDecision::deny(Some(
        long.clone(),
    )));
    let reason = out
        .get("reason")
        .and_then(|r| r.as_str())
        .expect("deny renders a reason");
    let marker = "...[truncated]";
    assert!(
        reason.ends_with(marker),
        "capped reason must carry the tail marker"
    );
    assert!(
        reason.chars().count() <= 2000 + marker.chars().count(),
        "capped reason must fit 2000 chars plus the marker"
    );
    assert!(
        reason.len() < long.len(),
        "capped reason must be shorter than the 5000-char input"
    );
}

#[test]
fn manifest_antigravity_renders_ask_natively() {
    // Antigravity's PreToolUse takes decision "ask" (docs list allow, deny,
    // ask, force_ask, deny_unless_prior_grant). Without an ask template the
    // dialect degraded it to deny — turning "escalate this to a human" into
    // "blocked", which denies an action the decider only wanted confirmed.
    let manifest = AdapterManifest::parse(BUILTIN_MANIFEST).expect("builtin manifest parses");
    let adapter = DeclarativeAdapter::new(manifest, "v1".to_string()).expect("create adapter");
    let input = make_input(
        "PreToolUse",
        r#"{"conversationId":"c1","toolCall":{"name":"run_command","args":{"CommandLine":"ls"}}}"#,
    );
    adapter.parse_event(&input).expect("parse PreToolUse");

    let ask = adapter.format_response(&agent_hooks_protocol::PromptDecision::ask(Some(
        "need a human".to_string(),
    )));
    assert_eq!(
        ask,
        serde_json::json!({"decision": "ask", "reason": "need a human"}),
        "ask must stay ask, not collapse into deny"
    );
    let deny = adapter.format_response(&agent_hooks_protocol::PromptDecision::deny(Some(
        "nope".to_string(),
    )));
    assert_eq!(
        deny,
        serde_json::json!({"decision": "deny", "reason": "nope"})
    );
    assert_ne!(ask, deny, "ask and deny must be distinguishable");
}

#[test]
fn manifest_antigravity_declares_no_ask_tools() {
    // Antigravity's hooks gate whether a call proceeds; per its docs they
    // "do not supply user answers" and cannot inject a selected option back
    // into the tool. So `ask_question` must NOT be lifted as an answerable
    // question — holding one would stall the agent on a prompt no decider
    // can actually answer.
    let manifest = AdapterManifest::parse(BUILTIN_MANIFEST).expect("builtin manifest parses");
    let vs = manifest.get_version("v1").expect("v1");
    assert!(
        vs.ask_tools.is_empty(),
        "antigravity cannot deliver answers, so it declares no ask_tools; got {:?}",
        vs.ask_tools
    );
}

// ---------------------------------------------------------------------------
// B2V legacy parity fixtures (ah-command-harnesses)
// ---------------------------------------------------------------------------

use agent_hooks_protocol::NormalizedStatus;

// PreInvocation invocationNum 0 marks a submitted user turn; subsequent model
// calls in that same turn are PreLLMCall and must not rebind the session.
#[test]
fn antigravity_invocations_are_busy_and_only_idle_stop_completes() {
    let adapter = load_adapter();
    let pre = adapter
        .parse_event(&make_input(
            "PreInvocation",
            r#"{"conversationId":"ag-1","workspacePaths":["/Users/me/proj"],"invocationNum":0,"prompt":"go"}"#,
        ))
        .unwrap();
    assert!(matches!(
        pre.lifecycle,
        AgentLifecycle::UserPromptSubmit { .. }
    ));
    assert_eq!(pre.status, NormalizedStatus::Running);
    let post = adapter
        .parse_event(&make_input(
            "PostInvocation",
            r#"{"conversationId":"ag-1","workspacePaths":["/Users/me/proj"]}"#,
        ))
        .unwrap();
    // PostInvocation ends one model call, not the user turn; Stop is authoritative.
    assert!(matches!(post.lifecycle, AgentLifecycle::PostLLMCall));
    assert_eq!(post.status, NormalizedStatus::Running);
    let weak = adapter
        .parse_event(&make_input(
            "Stop",
            r#"{"conversationId":"ag-1","fullyIdle":false}"#,
        ))
        .unwrap();
    assert_eq!(weak.status, NormalizedStatus::Running);
    let done = adapter
        .parse_event(&make_input(
            "Stop",
            r#"{"conversationId":"ag-1","fullyIdle":true}"#,
        ))
        .unwrap();
    assert_eq!(done.status, NormalizedStatus::Idle);
    assert_eq!(
        done.context.cwd.as_deref(),
        None,
        "Stop payload carries no workspace; shim context stands"
    );
}

// Live parity e2e (Antigravity): model invocation events and tools stay busy;
// only an idle Stop completes the visible turn.
#[test]
fn antigravity_parity_scenario_statuses() {
    let adapter = load_adapter();
    let tc = |name: &str| {
        format!(
            r#""toolCall":{{"id":"c","name":"{name}","args":{{"question":"Which database?","options":["Postgres","SQLite"]}}}}"#
        )
    };
    let cases = [
        (
            "PreInvocation",
            r#""invocationNum":0,"prompt":"list the files""#.to_string(),
            NormalizedStatus::Running,
        ),
        ("PreToolUse", tc("run_command"), NormalizedStatus::Running),
        ("PostToolUse", tc("run_command"), NormalizedStatus::Running),
        (
            "PreToolUse",
            tc("ask_question"),
            NormalizedStatus::WaitingForInput,
        ),
        (
            "PostToolUse",
            format!(r#"{},"output":"SQLite""#, tc("ask_question")),
            NormalizedStatus::Running,
        ),
        (
            "PostInvocation",
            r#""x":1"#.to_string(),
            NormalizedStatus::Running,
        ),
        (
            "Stop",
            r#""terminationReason":"completed","fullyIdle":true"#.to_string(),
            NormalizedStatus::Idle,
        ),
    ];
    for (event_name, extra, status) in cases {
        let raw = format!(
            r#"{{"conversationId":"parity-antigravity-0001","workspacePaths":["/tmp/p"],"hook_event_name":"{event_name}",{extra}}}"#
        );
        let event = adapter.parse_event(&make_input(event_name, &raw)).unwrap();
        assert_eq!(event.status, status, "{event_name} {extra}");
        assert_eq!(event.session_id, "parity-antigravity-0001");
    }
}
