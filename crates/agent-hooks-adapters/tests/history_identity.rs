use agent_hooks_adapters::{
    declarative::DeclarativeAdapter, manifest::AdapterManifest, HarnessAdapter,
};
use agent_hooks_protocol::{AgentEvent, EventContext, RawHookInput};
use serde_json::{json, Value};

fn parse(event: &str, payload: Value) -> AgentEvent {
    let manifest = include_str!("../builtin/claude-code/adapter.json");
    let adapter =
        DeclarativeAdapter::new(AdapterManifest::parse(manifest).unwrap(), "v1".into()).unwrap();
    adapter
        .parse_event(&RawHookInput {
            harness_hint: "claude-code".into(),
            event_hint: Some(event.into()),
            raw_payload: payload.to_string(),
            context: EventContext::default(),
            request_id: None,
            passthrough_env: Default::default(),
        })
        .unwrap()
}

#[test]
fn native_session_turn_and_tool_call_are_captured_before_fallbacks() {
    let ev = parse(
        "PreToolUse",
        json!({"session_id":"s-1","turn_id":"t-1","tool_name":"Bash","tool_use_id":"toolu_1","tool_input":{}}),
    );
    let id = ev.history_identity.expect("identity");
    assert!(id.native_session);
    assert_eq!(id.native_turn_id.as_deref(), Some("t-1"));
    assert_eq!(id.native_tool_call_id.as_deref(), Some("toolu_1"));
    assert_eq!(id.native_message_id, None);
}

#[test]
fn generated_session_and_turn_ids_are_not_reported_as_native() {
    let ev = parse("Stop", json!({"last_assistant_message":"done"}));
    let id = ev.history_identity.expect("identity");
    // Live behavior unchanged: a UUID session and an event_id turn still exist.
    assert!(!ev.session_id.is_empty());
    assert_eq!(ev.turn_id.as_deref(), Some(ev.event_id.as_str()));
    assert!(!id.native_session);
    assert_eq!(id.native_turn_id, None);
}

#[test]
fn unrelated_id_fields_are_not_mistaken_for_native_ids() {
    let ev = parse(
        "PreToolUse",
        json!({"session_id":"s","tool_name":"Bash","tool_input":{"id":"not-a-call-id","tool_use_id":"nested"},"id":"x"}),
    );
    let id = ev.history_identity.expect("identity");
    assert_eq!(id.native_tool_call_id, None);
}

fn parse_with(manifest: &str, harness: &str, event: &str, payload: Value) -> AgentEvent {
    let adapter =
        DeclarativeAdapter::new(AdapterManifest::parse(manifest).unwrap(), "v1".into()).unwrap();
    adapter
        .parse_event(&RawHookInput {
            harness_hint: harness.into(),
            event_hint: Some(event.into()),
            raw_payload: payload.to_string(),
            context: EventContext::default(),
            request_id: None,
            passthrough_env: Default::default(),
        })
        .unwrap()
}

#[test]
fn opencode_before_and_after_share_the_native_call_id_and_distinct_calls_differ() {
    let manifest = include_str!("../builtin/opencode/adapter.json");
    let id_of = |event: &str, payload: Value| {
        parse_with(manifest, "opencode", event, payload)
            .history_identity
            .expect("identity")
            .native_tool_call_id
    };
    let before = id_of(
        "tool.execute.before",
        json!({"sessionID":"ses_1","tool":"bash","callID":"call-A","args":{"command":"ls"}}),
    );
    let after = id_of(
        "tool.execute.after",
        json!({"sessionID":"ses_1","tool":"bash","callID":"call-A","output":"ok","title":"ls"}),
    );
    let other = id_of(
        "tool.execute.before",
        json!({"sessionID":"ses_1","tool":"bash","callID":"call-B","args":{"command":"pwd"}}),
    );
    assert_eq!(before.as_deref(), Some("call-A"));
    assert_eq!(after, before);
    assert_eq!(other.as_deref(), Some("call-B"));
    assert_ne!(other, before);
}

#[test]
fn opencode_unrelated_ids_are_not_native_call_ids() {
    let manifest = include_str!("../builtin/opencode/adapter.json");
    let ev = parse_with(
        manifest,
        "opencode",
        "tool.execute.before",
        json!({"sessionID":"ses_1","tool":"bash","id":"x","messageID":"msg_1","args":{"id":"nested","callID":"nested"}}),
    );
    assert_eq!(
        ev.history_identity.expect("identity").native_tool_call_id,
        None
    );
}

#[test]
fn antigravity_pre_and_post_share_the_tool_call_id_and_distinct_calls_differ() {
    let manifest = include_str!("../builtin/antigravity/adapter.json");
    let id_of = |event: &str, payload: Value| {
        parse_with(manifest, "antigravity", event, payload)
            .history_identity
            .expect("identity")
            .native_tool_call_id
    };
    let pre = id_of(
        "PreToolUse",
        json!({"conversationId":"c1","toolCall":{"id":"tc-1","name":"run_command","args":{"CommandLine":"ls"}}}),
    );
    let post = id_of(
        "PostToolUse",
        json!({"conversationId":"c1","toolCall":{"id":"tc-1","name":"run_command","args":{"CommandLine":"ls"}},"output":"ok"}),
    );
    let other = id_of(
        "PreToolUse",
        json!({"conversationId":"c1","toolCall":{"id":"tc-2","name":"run_command","args":{"CommandLine":"pwd"}}}),
    );
    assert_eq!(pre.as_deref(), Some("tc-1"));
    assert_eq!(post, pre);
    assert_eq!(other.as_deref(), Some("tc-2"));
}

#[test]
fn antigravity_ids_inside_user_tool_arguments_are_not_native_call_ids() {
    let manifest = include_str!("../builtin/antigravity/adapter.json");
    let ev = parse_with(
        manifest,
        "antigravity",
        "PreToolUse",
        json!({
            "conversationId": "c1",
            "id": "top-level",
            "toolCall": {
                "name": "run_command",
                "args": {"id": "arg", "callID": "arg", "tool_use_id": "arg",
                         "toolCall": {"id": "nested"}}
            }
        }),
    );
    assert_eq!(
        ev.history_identity.expect("identity").native_tool_call_id,
        None
    );
}
