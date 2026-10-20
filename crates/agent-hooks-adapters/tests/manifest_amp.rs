use agent_hooks_adapters::declarative::DeclarativeAdapter;
use agent_hooks_adapters::manifest::AdapterManifest;
use agent_hooks_adapters::HarnessAdapter;
use agent_hooks_protocol::{
    AgentLifecycle, EventContext, PromptAnswer, PromptDecision, RawHookInput,
};

const BUILTIN: &str = include_str!("../builtin/amp/adapter.json");

fn make_input(event_hint: &str, payload: &str) -> RawHookInput {
    RawHookInput {
        harness_hint: "amp".to_string(),
        event_hint: Some(event_hint.to_string()),
        raw_payload: payload.to_string(),
        context: EventContext::default(),
        request_id: Some("request-q-1".to_string()),
        passthrough_env: Default::default(),
    }
}

fn load_adapter() -> DeclarativeAdapter {
    let manifest = AdapterManifest::parse(BUILTIN).expect("builtin amp manifest parses");
    assert_eq!(manifest.id, "amp");
    DeclarativeAdapter::new(manifest, "v1".to_string()).expect("v1 exists")
}

#[test]
fn amp_question_event_normalizes_permission_and_structured_answers() {
    let adapter = load_adapter();
    let event = adapter
        .parse_event(&make_input(
            "question",
            r#"{"session_id":"amp-session","request_id":"request-q-1","tool_name":"agent_hooks_ask","questions":[
                {"id":"q1","question":"Which target?","custom":false,"options":[{"label":"Root"},{"label":"Package"}]},
                {"id":"q2","question":"Which checks?","multiple":true,"custom":true,"options":[{"label":"Tests"},{"label":"Lint"}]}
            ]}"#,
        ))
        .expect("parse normalized question event");

    assert_eq!(event.session_id, "amp-session");
    match &event.lifecycle {
        AgentLifecycle::PermissionRequest { prompt } => {
            assert_eq!(prompt.kind, agent_hooks_protocol::PromptKind::Question);
            assert_eq!(prompt.prompt_id, "request-q-1");
            assert_eq!(prompt.tool_name.as_deref(), Some("agent_hooks_ask"));
            assert_eq!(prompt.questions.len(), 2);
            assert_eq!(prompt.questions[0].id, "q1");
            assert_eq!(prompt.questions[0].question, "Which target?");
            assert_eq!(prompt.questions[0].options[0].label, "Root");
            assert!(!prompt.questions[0].multiple);
            assert_eq!(prompt.questions[1].id, "q2");
            assert!(prompt.questions[1].multiple);
            assert!(!prompt.questions[0].custom);
            assert!(prompt.questions[1].custom);
        }
        other => panic!("expected PermissionRequest, got {other:?}"),
    }

    let answered = PromptDecision {
        allow: true,
        ask: false,
        custom_input: None,
        answers: vec![
            PromptAnswer {
                question_id: "q1".to_string(),
                selected_options: vec!["Package".to_string()],
                custom_input: None,
            },
            PromptAnswer {
                question_id: "q2".to_string(),
                selected_options: vec!["Tests".to_string(), "Lint".to_string()],
                custom_input: Some("include docs checks".to_string()),
            },
        ],
        allow_always: false,
        suggestion: None,
        defer: false,
        additional_context: None,
    };
    assert_eq!(
        adapter.format_response(&answered),
        serde_json::json!({
            "decision": "allow",
            "answers": [
                {"question_id":"q1","selected_options":["Package"]},
                {"question_id":"q2","selected_options":["Tests","Lint"],"custom_input":"include docs checks"}
            ]
        })
    );
}

#[test]
fn amp_tool_gate_allows_denies_and_asks() {
    let adapter = load_adapter();
    let input = make_input(
        "tool.call",
        r#"{"thread":{"id":"amp-session"},"toolUseID":"permission-1","tool":"shell_command","input":{"command":"printf safe"},"description":"run command"}"#,
    );
    let event = adapter.parse_event(&input).expect("parse tool call");
    let AgentLifecycle::PreToolUse { tool_name, input, .. } = event.lifecycle else {
        panic!("tool.call must retain PreToolUse policy matching");
    };
    assert_eq!(tool_name, "shell_command");
    assert_eq!(input["command"], "printf safe");

    assert_eq!(
        adapter.format_response(&PromptDecision::allow()),
        serde_json::json!({"decision":"allow"})
    );
    assert_eq!(
        adapter.format_response(&PromptDecision::deny(Some("blocked by policy".to_string()))),
        serde_json::json!({"decision":"deny","reason":"blocked by policy"})
    );
    assert_eq!(
        adapter.format_response(&PromptDecision::ask(Some("need human input".to_string()))),
        serde_json::json!({"decision":"ask","reason":"need human input"})
    );
}
