use agent_hooks_adapters::{
    declarative::DeclarativeAdapter, manifest::AdapterManifest, HarnessAdapter,
};
use agent_hooks_protocol::{AgentEvent, EventContext, RawHookInput};
use serde_json::{json, Value};

fn parse(payload: Value, sources: Value, overrides: Value) -> AgentEvent {
    let mut manifest: Value =
        serde_json::from_str(include_str!("../builtin/claude-code/adapter.json")).unwrap();
    manifest["versions"]["v1"]["extraction"]["flavor_text"] = sources;
    manifest["versions"]["v1"]["extraction_overrides"]["Stop"]["flavor_text"] = overrides;
    let adapter = DeclarativeAdapter::new(
        AdapterManifest::parse(&manifest.to_string()).unwrap(),
        "v1".into(),
    )
    .unwrap();
    adapter
        .parse_event(&RawHookInput {
            harness_hint: "claude-code".into(),
            event_hint: Some("Stop".into()),
            raw_payload: payload.to_string(),
            context: EventContext::default(),
            request_id: None,
            passthrough_env: Default::default(),
        })
        .unwrap()
}

#[test]
fn hook_specific_text_wins_and_blank_sources_fall_through() {
    let event = parse(
        json!({"session_id":"s", "assistant":" Finished the fix ", "global":"wrong"}),
        json!(["global"]),
        json!(["assistant"]),
    );
    assert_eq!(event.flavor_text.as_deref(), Some("Finished the fix"));
    let event = parse(
        json!({"session_id":"s", "assistant":"  ", "global":"Fallback"}),
        json!(["global"]),
        json!(["assistant"]),
    );
    assert_eq!(event.flavor_text.as_deref(), Some("Fallback"));
}

#[test]
fn structured_text_ignores_tool_and_image_blocks() {
    let event = parse(
        json!({"session_id":"s", "blocks":[
            {"type":"tool_use", "text":"do not display", "name":"shell"},
            {"type":"text", "text":"Fixed login"},
            {"type":"image", "content":"image bytes"},
            {"type":"output_text", "text":"Tests passed"},
            {"text":"", "content":"Ready to ship"}
        ]}),
        json!(["blocks"]),
        json!([]),
    );
    assert_eq!(
        event.flavor_text.as_deref(),
        Some("Fixed login\nTests passed\nReady to ship")
    );
}

#[test]
fn unicode_text_is_bounded_and_missing_text_is_omitted_on_wire() {
    let event = parse(
        json!({"session_id":"s", "text":"🙂".repeat(400)}),
        json!(["text"]),
        json!([]),
    );
    assert_eq!(
        event.flavor_text.as_deref(),
        Some("🙂".repeat(300).as_str())
    );
    let wire = serde_json::to_value(&event).unwrap();
    let restored: AgentEvent = serde_json::from_value(wire).unwrap();
    assert_eq!(restored.flavor_text, event.flavor_text);
    let event = parse(
        json!({"session_id":"s", "text":{"command":"secret"}}),
        json!(["text"]),
        json!([]),
    );
    let wire = serde_json::to_value(&event).unwrap();
    assert!(wire.get("flavor_text").is_none());
    assert!(serde_json::from_value::<AgentEvent>(wire)
        .unwrap()
        .flavor_text
        .is_none());
}

#[test]
fn termination_metadata_does_not_hide_the_actual_error() {
    let adapter = DeclarativeAdapter::new(
        AdapterManifest::parse(include_str!("../builtin/antigravity/adapter.json")).unwrap(),
        "v1".into(),
    )
    .unwrap();
    for (payload, expected) in [
        (
            json!({"conversationId":"s", "termination_reason":"error", "error":"Context limit exceeded"}),
            Some("Context limit exceeded"),
        ),
        (
            json!({"conversationId":"s", "termination_reason":"completed"}),
            None,
        ),
    ] {
        let event = adapter
            .parse_event(&RawHookInput {
                harness_hint: "antigravity".into(),
                event_hint: Some("Stop".into()),
                raw_payload: payload.to_string(),
                context: EventContext::default(),
                request_id: None,
                passthrough_env: Default::default(),
            })
            .unwrap();
        assert_eq!(event.flavor_text.as_deref(), expected);
    }
}

#[test]
fn flavor_never_reads_transcripts_or_session_stores() {
    let event = parse(
        json!({"session_id":"s", "flavor_text":"Raw text", "transcript_path":"/does/not/exist"}),
        json!([{"transcript":"first_prompt"}]),
        json!([]),
    );
    assert_eq!(event.flavor_text.as_deref(), Some("Raw text"));
}
