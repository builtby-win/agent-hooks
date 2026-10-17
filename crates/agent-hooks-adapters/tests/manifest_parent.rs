use agent_hooks_adapters::manifest::AdapterManifest;
use agent_hooks_adapters::{declarative::DeclarativeAdapter, HarnessAdapter};
use agent_hooks_protocol::{AgentLifecycle, EventContext, RawHookInput};

const OPENCODE: &str = include_str!("../builtin/opencode/adapter.json");
const CLAUDE: &str = include_str!("../builtin/claude-code/adapter.json");

fn make_input(harness_hint: &str, event_hint: &str, payload: &str) -> RawHookInput {
    RawHookInput {
        harness_hint: harness_hint.to_string(),
        event_hint: Some(event_hint.to_string()),
        raw_payload: payload.to_string(),
        context: EventContext::default(),
        request_id: None,
        passthrough_env: Default::default(),
    }
}

fn opencode_adapter() -> DeclarativeAdapter {
    let manifest = AdapterManifest::parse(OPENCODE).expect("builtin opencode manifest parses");
    DeclarativeAdapter::new(manifest, "v1".to_string()).expect("create opencode adapter")
}

fn claude_adapter() -> DeclarativeAdapter {
    let manifest = AdapterManifest::parse(CLAUDE).expect("builtin claude manifest parses");
    DeclarativeAdapter::new(manifest, "v1".to_string()).expect("create claude adapter")
}

#[test]
fn parent_passthrough_top_level_and_nested() {
    let adapter = opencode_adapter();

    // Top-level variant key passes through verbatim.
    let event = adapter
        .parse_event(&make_input(
            "opencode",
            "tool.execute.before",
            r#"{"sessionID": "ses-child", "parentID": "ses-parent", "tool_name": "Bash", "tool_input": {}}"#,
        ))
        .expect("parse with parentID");
    assert_eq!(event.session_id, "ses-child");
    assert_eq!(event.parent_session_id.as_deref(), Some("ses-parent"));

    // Nested session object resolves via dotted source.
    let event = adapter
        .parse_event(&make_input(
            "opencode",
            "tool.execute.before",
            r#"{"sessionID": "ses-child", "session": {"parent_id": "ses-parent"}, "tool_name": "Bash"}"#,
        ))
        .expect("parse with nested parent");
    assert_eq!(event.parent_session_id.as_deref(), Some("ses-parent"));

    // Absent parent stays None — never fabricated, never a UUID.
    let event = adapter
        .parse_event(&make_input(
            "opencode",
            "tool.execute.before",
            r#"{"sessionID": "ses-solo", "tool_name": "Bash"}"#,
        ))
        .expect("parse without parent");
    assert_eq!(event.parent_session_id, None);
}

#[test]
fn subagent_start_extracts_child_id_without_inventing_parent() {
    let adapter = claude_adapter();
    let event = adapter
        .parse_event(&make_input(
            "claude",
            "SubagentStart",
            r#"{"session_id": "sess-main", "agent_id": "sub-1"}"#,
        ))
        .expect("parse SubagentStart");
    match event.lifecycle {
        AgentLifecycle::SubagentStart { subagent_id, .. } => assert_eq!(subagent_id, "sub-1"),
        ref other => panic!("expected SubagentStart, got {other:?}"),
    }
    // Parent linkage is the tracker's job (inferred from subagent_id);
    // parse leaves the explicit field empty.
    assert_eq!(event.session_id, "sess-main");
    assert_eq!(event.parent_session_id, None);
}
