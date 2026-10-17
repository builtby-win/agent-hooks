//! `PromptDecision::additional_context` reaches the model through each
//! adapter's `inject` template, and only on a non-blocking verdict.

use agent_hooks_adapters::manifest::AdapterManifest;
use agent_hooks_adapters::{declarative::DeclarativeAdapter, HarnessAdapter};
use agent_hooks_protocol::{EventContext, PromptDecision, RawHookInput};
use serde_json::json;

fn adapter(manifest: &str) -> DeclarativeAdapter {
    let manifest = AdapterManifest::parse(manifest).expect("builtin manifest parses");
    let version = manifest.default_version().to_string();
    DeclarativeAdapter::new(manifest, version).expect("adapter")
}

fn parse(adapter: &DeclarativeAdapter, hint: &str, event: &str, payload: &str) {
    let input = RawHookInput {
        harness_hint: hint.to_string(),
        event_hint: Some(event.to_string()),
        raw_payload: payload.to_string(),
        context: EventContext::default(),
        request_id: None,
        passthrough_env: Default::default(),
    };
    adapter.parse_event(&input).expect("parse event");
}

fn claude_shape(event: &str) -> serde_json::Value {
    json!({"hookSpecificOutput": {"hookEventName": event, "additionalContext": "User is new to Docker."}})
}

fn ctx(decision: PromptDecision) -> PromptDecision {
    decision.with_additional_context("User is new to Docker.")
}

#[test]
fn claude_family_injects_on_session_start_and_prompt_submit() {
    let cases = [
        ("claude-code", include_str!("../builtin/claude-code/adapter.json"), "claude"),
        ("codex", include_str!("../builtin/codex/adapter.json"), "codex"),
        ("qwen-code", include_str!("../builtin/qwen-code/adapter.json"), "qwen"),
        ("qoder", include_str!("../builtin/qoder/adapter.json"), "qoder"),
    ];
    for (name, manifest, hint) in cases {
        let a = adapter(manifest);
        for event in ["SessionStart", "UserPromptSubmit"] {
            parse(&a, hint, event, r#"{"session_id":"s","prompt":"hi"}"#);
            // Observe-style defer (no verdict) and an explicit allow both inject.
            assert_eq!(a.format_response(&ctx(PromptDecision::defer())), claude_shape(event), "{name} {event} defer");
            assert_eq!(a.format_response(&ctx(PromptDecision::allow())), claude_shape(event), "{name} {event} allow");
        }
    }
}

#[test]
fn gemini_injects_on_before_agent_and_session_start() {
    let a = adapter(include_str!("../builtin/gemini-cli/adapter.json"));
    for event in ["SessionStart", "BeforeAgent"] {
        parse(&a, "gemini", event, r#"{"session_id":"s","prompt":"hi"}"#);
        assert_eq!(a.format_response(&ctx(PromptDecision::defer())), claude_shape(event));
    }
}

#[test]
fn cursor_injects_on_session_start_only() {
    let a = adapter(include_str!("../builtin/cursor/adapter.json"));
    parse(&a, "cursor", "sessionStart", r#"{"conversation_id":"c"}"#);
    assert_eq!(
        a.format_response(&ctx(PromptDecision::defer())),
        json!({"additional_context": "User is new to Docker."})
    );
    // beforeSubmitPrompt has no context channel: the verdict renders as usual.
    parse(&a, "cursor", "beforeSubmitPrompt", r#"{"conversation_id":"c","prompt":"hi"}"#);
    assert_eq!(a.format_response(&ctx(PromptDecision::allow())), json!({"continue": true}));
}

#[test]
fn context_never_rides_a_deny_or_ask() {
    let a = adapter(include_str!("../builtin/claude-code/adapter.json"));
    parse(&a, "claude", "UserPromptSubmit", r#"{"session_id":"s","prompt":"hi"}"#);
    let out = a.format_response(&ctx(PromptDecision::deny(Some("no".to_string()))));
    assert_eq!(out, json!({"decision": "block", "reason": "no"}));
}

#[test]
fn events_without_a_template_drop_context() {
    let a = adapter(include_str!("../builtin/claude-code/adapter.json"));
    parse(&a, "claude", "PostToolUse", r#"{"session_id":"s","tool_name":"Bash","tool_input":{}}"#);
    let with = a.format_response(&ctx(PromptDecision::defer()));
    let without = a.format_response(&PromptDecision::defer());
    assert_eq!(with, without);
}

#[test]
fn blank_context_is_not_injected() {
    let a = adapter(include_str!("../builtin/claude-code/adapter.json"));
    parse(&a, "claude", "SessionStart", r#"{"session_id":"s"}"#);
    assert_eq!(
        a.format_response(&PromptDecision::defer().with_additional_context("  \n")),
        a.format_response(&PromptDecision::defer())
    );
}

#[test]
fn pi_and_omp_hand_context_to_the_bridge() {
    // The TS bridge reads `additional_context` off the shim's reply and returns
    // it from before_agent_start; there is no host-native dialect to mimic.
    for (manifest, hint) in [
        (include_str!("../builtin/pi/adapter.json"), "pi"),
        (include_str!("../builtin/omp/adapter.json"), "omp"),
    ] {
        let a = adapter(manifest);
        for event in ["SessionStart", "UserPromptSubmit"] {
            parse(&a, hint, event, r#"{"session_id":"s","prompt":"hi"}"#);
            assert_eq!(
                a.format_response(&ctx(PromptDecision::allow())),
                json!({"additional_context": "User is new to Docker."}),
                "{hint} {event}"
            );
        }
        parse(&a, hint, "PreToolUse", r#"{"session_id":"s","toolName":"bash"}"#);
        assert_eq!(
            a.format_response(&ctx(PromptDecision::allow())),
            a.format_response(&PromptDecision::allow()),
            "{hint}: tool gates carry no context"
        );
    }
}
