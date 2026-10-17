//! Decider round-trip across ALL builtin harnesses: spin a real daemon on a
//! temp socket, subscribe as a `decide:true` decider, fire one synthetic gate
//! payload per harness as hook input, and assert the full loop per case:
//!   input payload -> normalized event (lifecycle + lifted questions) ->
//!   decider verdict via `prompt.respond` -> daemon decision JSON ->
//!   harness-dialect rendering via the manifest adapter.
//!
//! Payload shapes mirror each harness's manifest test (same keys/events).
use agent_hooks_adapters::AdapterRegistry;
use agent_hooks_client::AgentHooksClient;
use agent_hooks_core::daemon::AgentHooksDaemon;
use agent_hooks_protocol::{
    AgentEvent, AgentLifecycle, DecideScope, EventContext, PromptAnswer, PromptDecision,
    PromptKind, RawHookInput,
};
use std::collections::HashMap;
use std::time::Duration;
use tokio_stream::StreamExt;

const FAVORITE_COLOR: &str = r#"{"session_id": "qa-1", "tool_name": "ask", "tool_input": {"questions": [{"id": "favorite_color", "header": "Preference", "question": "What is your favorite color?", "multi": false, "options": [{"label": "Red", "description": "Bold and energetic"}, {"label": "Blue", "description": "Calm and trustworthy"}, {"label": "Green", "description": "Fresh and natural"}, {"label": "Yellow", "description": "Sunny and cheerful"}]}]}}"#;

// Claude asks through PermissionRequest(AskUserQuestion), never a held
// PreToolUse (B2V legacy payload.rs parse_claude_value).
const CLAUDE_ASK: &str = r#"{"session_id": "qa-2", "hook_event_name": "PermissionRequest", "tool_name": "AskUserQuestion", "tool_input": {"questions": [{"question": "Deploy to prod?", "header": "Deploy", "multiSelect": false, "options": [{"label": "Yes", "description": "ship it"}, {"label": "No", "description": "hold off"}]}]}}"#;

const OPENCODE_PERM: &str = r#"{"sessionID": "qa-3", "toolName": "Bash"}"#;

const CURSOR_TOOL: &str =
    r#"{"session_id": "qa-4", "toolCall": {"name": "Bash", "args": {"command": "cargo test"}}}"#;

const AMP_TOOL: &str =
    r#"{"session_id": "qa-5", "tool_name": "Bash", "tool_input": {"command": "cargo test"}}"#;

const ANTIGRAVITY_TOOL: &str = r#"{"conversationId": "qa-6", "toolCall": {"name": "run_command", "args": {"CommandLine": "npm test"}}}"#;

const CODEX_TOOL: &str =
    r#"{"thread_id": "qa-7", "tool_name": "Bash", "tool_input": {"command": "cargo test"}}"#;

const COPILOT_TOOL: &str =
    r#"{"session_id": "qa-8", "tool_name": "Bash", "tool_input": {"command": "cargo test"}}"#;

const GEMINI_TOOL: &str =
    r#"{"session_id": "qa-9", "tool_name": "Bash", "tool_input": {"command": "rm -rf /tmp/x"}}"#;

const HERMES_TOOL: &str =
    r#"{"session_id": "qa-10", "tool_name": "Bash", "tool_input": {"command": "cargo test"}}"#;

const JCODE_TOOL: &str =
    r#"{"session_id": "qa-11", "tool_name": "Bash", "tool_input": {"command": "cargo test"}}"#;

const KIRO_TOOL: &str =
    r#"{"session_id": "qa-12", "tool_name": "Bash", "tool_input": {"command": "cargo test"}}"#;

fn hook_input(harness: &str, event: &str, payload: &str, request_id: Option<&str>) -> RawHookInput {
    RawHookInput {
        harness_hint: harness.to_string(),
        event_hint: Some(event.to_string()),
        raw_payload: payload.to_string(),
        context: EventContext::default(),
        request_id: request_id.map(str::to_string),
        passthrough_env: Default::default(),
    }
}

/// Route a verdict back using the id the daemon stamped on the held event.
/// Deciders never reconstruct this — the `hold` says it outright, which is
/// why the `PermissionRequest` special case does not appear here.
fn hold_prompt_id(event: &AgentEvent) -> String {
    if let Some(hold) = &event.hold {
        return hold.prompt_id.clone();
    }
    legacy_hold_prompt_id(event)
}

/// Pre-`hold` derivation, kept only to prove the stamped id agrees with it.
fn legacy_hold_prompt_id(event: &AgentEvent) -> String {
    match &event.lifecycle {
        AgentLifecycle::PermissionRequest { prompt } => prompt.prompt_id.clone(),
        _ => event.event_id.clone(),
    }
}

/// Generic opaque-tool assertion shared by the eight flat harnesses.
fn check_tool(event: &AgentEvent, session: &str, tool: &str) {
    assert_eq!(event.session_id, session);
    match &event.lifecycle {
        AgentLifecycle::PreToolUse {
            tool_name, prompt, ..
        } => {
            assert_eq!(tool_name, tool);
            assert!(prompt.is_none(), "unmapped tool stays opaque");
        }
        other => panic!("expected PreToolUse, got {other:?}"),
    }
}

fn check_pi_ask(event: &AgentEvent) {
    assert_eq!(event.session_id, "qa-1");
    match &event.lifecycle {
        AgentLifecycle::PreToolUse {
            tool_name, prompt, ..
        } => {
            assert_eq!(tool_name, "ask");
            let pr = prompt.as_ref().expect("pi ask lifts a Question prompt");
            assert_eq!(pr.kind, PromptKind::Question);
            assert_eq!(pr.questions.len(), 1);
            assert_eq!(pr.questions[0].id, "favorite_color");
            assert_eq!(pr.questions[0].options.len(), 4);
            assert_eq!(pr.questions[0].options[1].label, "Blue");
        }
        other => panic!("expected PreToolUse, got {other:?}"),
    }
}

fn check_claude_ask(event: &AgentEvent) {
    assert_eq!(event.session_id, "qa-2");
    match &event.lifecycle {
        AgentLifecycle::PermissionRequest { prompt: pr } => {
            assert_eq!(pr.tool_name.as_deref(), Some("AskUserQuestion"));
            // Legacy kind: a permission prompt that carries the questions.
            assert_eq!(pr.kind, PromptKind::Permission);
            assert_eq!(pr.questions.len(), 1);
            assert_eq!(pr.questions[0].question, "Deploy to prod?");
            assert_eq!(pr.questions[0].options.len(), 2);
        }
        other => panic!("expected PermissionRequest, got {other:?}"),
    }
}

fn check_opencode_perm(event: &AgentEvent) {
    assert_eq!(event.session_id, "qa-3");
    match &event.lifecycle {
        AgentLifecycle::PermissionRequest { prompt } => {
            assert_eq!(prompt.tool_name.as_deref(), Some("Bash"));
            assert!(!prompt.prompt_id.is_empty());
            assert_ne!(
                prompt.prompt_id, event.event_id,
                "PermissionRequest keeps its own prompt_id"
            );
        }
        other => panic!("expected PermissionRequest, got {other:?}"),
    }
}

/// Per-case expectation: how to check the normalized event.
enum Check {
    /// Custom assertion function.
    Custom(fn(&AgentEvent)),
    /// Opaque PreToolUse with expected session + tool name.
    Tool(&'static str, &'static str),
}

struct Case {
    harness: &'static str,
    event_hint: &'static str,
    payload: &'static str,
    request_id: Option<&'static str>,
    session: &'static str,
    check: Check,
    verdict: PromptDecision,
    expect_decision: &'static str,
}

fn deny(reason: &str) -> PromptDecision {
    PromptDecision::deny(Some(reason.to_string()))
}

#[tokio::test]
async fn decider_roundtrip_all_harnesses() {
    let tmp = tempfile::TempDir::new().unwrap();
    let socket_path = tmp.path().join("qa-roundtrip.sock");

    let daemon = AgentHooksDaemon::new();
    let socket_for_daemon = socket_path.clone();
    let daemon_handle = tokio::spawn(async move {
        let _ = daemon.run(Some(socket_for_daemon)).await;
    });
    let socket_str = socket_path.to_str().unwrap().to_string();
    let hook_client = AgentHooksClient::connect_to(socket_str.clone())
        .await
        .unwrap();

    let cases = vec![
        Case {
            harness: "omp",
            event_hint: "tool_call",
            payload: FAVORITE_COLOR,
            request_id: None,
            session: "qa-1",
            check: Check::Custom(check_pi_ask),
            verdict: PromptDecision {
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
            },
            expect_decision: "allow",
        },
        Case {
            harness: "claude",
            event_hint: "PermissionRequest",
            payload: CLAUDE_ASK,
            request_id: None,
            session: "qa-2",
            check: Check::Custom(check_claude_ask),
            verdict: deny("blocked by test"),
            expect_decision: "deny",
        },
        Case {
            harness: "opencode",
            event_hint: "permission.ask",
            payload: OPENCODE_PERM,
            request_id: Some("p-1"),
            session: "qa-3",
            check: Check::Custom(check_opencode_perm),
            verdict: PromptDecision::ask(Some("Proceed?".to_string())),
            expect_decision: "ask",
        },
        Case {
            harness: "cursor",
            event_hint: "preToolUse",
            payload: CURSOR_TOOL,
            request_id: None,
            session: "qa-4",
            check: Check::Tool("qa-4", "Bash"),
            verdict: PromptDecision::allow(),
            expect_decision: "allow",
        },
        Case {
            harness: "amp",
            event_hint: "PreToolUse",
            payload: AMP_TOOL,
            request_id: None,
            session: "qa-5",
            check: Check::Tool("qa-5", "Bash"),
            verdict: PromptDecision::allow(),
            expect_decision: "allow",
        },
        Case {
            harness: "antigravity",
            event_hint: "PreToolUse",
            payload: ANTIGRAVITY_TOOL,
            request_id: None,
            session: "qa-6",
            check: Check::Tool("qa-6", "run_command"),
            verdict: deny("blocked by test"),
            expect_decision: "deny",
        },
        Case {
            harness: "codex",
            event_hint: "PreToolUse",
            payload: CODEX_TOOL,
            request_id: None,
            session: "qa-7",
            check: Check::Tool("qa-7", "Bash"),
            verdict: PromptDecision::allow(),
            expect_decision: "allow",
        },
        Case {
            harness: "copilot",
            event_hint: "preToolUse",
            payload: COPILOT_TOOL,
            request_id: None,
            session: "qa-8",
            check: Check::Tool("qa-8", "Bash"),
            verdict: PromptDecision::allow(),
            expect_decision: "allow",
        },
        Case {
            harness: "gemini",
            event_hint: "PreToolUse",
            payload: GEMINI_TOOL,
            request_id: None,
            session: "qa-9",
            check: Check::Tool("qa-9", "Bash"),
            verdict: deny("blocked by test"),
            expect_decision: "deny",
        },
        Case {
            harness: "hermes",
            event_hint: "PreToolUse",
            payload: HERMES_TOOL,
            request_id: None,
            session: "qa-10",
            check: Check::Tool("qa-10", "Bash"),
            verdict: PromptDecision::allow(),
            expect_decision: "allow",
        },
        Case {
            harness: "jcode",
            event_hint: "preToolUse",
            payload: JCODE_TOOL,
            request_id: None,
            session: "qa-11",
            check: Check::Tool("qa-11", "Bash"),
            verdict: PromptDecision::allow(),
            expect_decision: "allow",
        },
        Case {
            harness: "kiro",
            event_hint: "preToolUse",
            payload: KIRO_TOOL,
            request_id: None,
            session: "qa-12",
            check: Check::Tool("qa-12", "Bash"),
            verdict: deny("blocked by test"),
            expect_decision: "deny",
        },
    ];
    let n_cases = cases.len();

    // Verdicts by session for the decider task.
    let mut verdicts: HashMap<String, PromptDecision> = HashMap::new();
    for c in &cases {
        verdicts.insert(c.session.to_string(), c.verdict.clone());
    }
    // Decider task: answer every held gate as it arrives.
    let decider_handle = tokio::spawn(async move {
        let decider_client = AgentHooksClient::connect_to(socket_str).await.unwrap();
        // This case sweeps every harness's gate events, mid-flight ones
        // included, so it claims them all explicitly. An unscoped
        // `subscribe_decider()` would (correctly) hold only the events the
        // harness had already stopped on.
        let mut stream = decider_client.subscribe_decider_for(DecideScope::default());
        for _ in 0..n_cases {
            let event = tokio::time::timeout(Duration::from_secs(10), stream.next())
                .await
                .expect("held event arrives")
                .expect("stream item");
            let verdict = verdicts
                .remove(&event.session_id)
                .expect("known qa session");
            // Every held event announces its own hold, and the stamped id
            // matches what a decider would have had to derive by hand.
            let hold = event.hold.as_ref().expect("held event carries its hold");
            assert_eq!(hold.prompt_id, legacy_hold_prompt_id(&event));
            assert!(!hold.verdicts.is_empty(), "hold states legal verdicts");
            assert!(hold.deadline_ms > 0);
            decider_client
                .respond_prompt(&hold_prompt_id(&event), verdict)
                .await
                .expect("respond ok");
        }
    });
    // Let the decider's subscribe register before firing gates; otherwise the
    // first gate fast-allows down the no-decider path and the test flakes.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Fire each case through the hook path (holds until the decider answers).
    for case in &cases {
        let input = hook_input(case.harness, case.event_hint, case.payload, case.request_id);
        // Parse locally (what the shim sees) to assert normalization.
        let adapter = AdapterRegistry::get_versioned(case.harness, None)
            .unwrap_or_else(|| panic!("{} resolves", case.harness));
        let parsed = adapter.parse_event(&input).expect("parse ok");
        match &case.check {
            Check::Custom(f) => f(&parsed),
            Check::Tool(session, tool) => check_tool(&parsed, session, tool),
        }

        // Round-trip through the daemon hold/respond cycle.
        let raw = tokio::time::timeout(Duration::from_secs(15), hook_client.send_raw_hook(&input))
            .await
            .expect("gate releases")
            .expect("send ok")
            .expect("decision json");
        let v: serde_json::Value = serde_json::from_str(&raw).expect("decision parses");
        assert_eq!(
            v.get("decision").and_then(|d| d.as_str()),
            Some(case.expect_decision)
        );

        // Harness-dialect rendering of the same verdict.
        let decision: PromptDecision = serde_json::from_str(&raw).expect("decision shape");
        let rendered = adapter.format_response(&decision);
        // Non-allow verdicts must render a real dialect (never silence): the
        // reason has to reach the harness. Allow is exempt — silence IS the
        // allow dialect on strict harnesses (codex `gate.allow` is `{}`).
        if case.expect_decision != "allow" {
            assert_ne!(
                rendered,
                serde_json::json!({}),
                "{} renders a {} dialect",
                case.harness,
                case.expect_decision
            );
        }
        if case.harness == "opencode" {
            // opencode's permission.ask status is "ask" | "deny" | "allow", so
            // an escalation survives as an escalation. It used to render as
            // deny, which blocked a call the decider only wanted confirmed.
            assert_eq!(
                rendered,
                serde_json::json!({"decision": "ask", "reason": "Proceed?", "reply": "ask"})
            );
        }
    }

    decider_handle.await.expect("decider done");
    daemon_handle.abort();
}
