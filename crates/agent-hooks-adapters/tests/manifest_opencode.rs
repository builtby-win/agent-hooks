use agent_hooks_adapters::declarative::DeclarativeAdapter;
use agent_hooks_adapters::manifest::AdapterManifest;
use agent_hooks_adapters::registry::AdapterRegistry;
use agent_hooks_adapters::HarnessAdapter;
use agent_hooks_protocol::{AgentLifecycle, EventContext, RawHookInput};

const BUILTIN: &str = include_str!("../builtin/opencode/adapter.json");

fn make_input(event_hint: &str, payload: &str) -> RawHookInput {
    make_input_with_request_id(event_hint, payload, None)
}

fn make_input_with_request_id(
    event_hint: &str,
    payload: &str,
    request_id: Option<String>,
) -> RawHookInput {
    RawHookInput {
        harness_hint: "opencode".to_string(),
        event_hint: Some(event_hint.to_string()),
        raw_payload: payload.to_string(),
        context: EventContext::default(),
        request_id,
        passthrough_env: Default::default(),
    }
}

fn load_adapter() -> DeclarativeAdapter {
    let manifest = AdapterManifest::parse(BUILTIN).expect("builtin opencode.json parses");
    assert_eq!(manifest.id, "opencode");
    DeclarativeAdapter::new(manifest, "v1".to_string()).expect("v1 exists")
}

#[test]
fn opencode_manifest_gate_pretooluse_legacy_dialect() {
    let adapter = load_adapter();

    // tool.execute.before with opencode-native sessionID + camelCase tool fields
    let input = make_input(
        "tool.execute.before",
        r#"{"sessionID": "ses-1", "toolName": "Bash", "toolInput": {"command": "cargo test"}}"#,
    );
    let event = adapter
        .parse_event(&input)
        .expect("parse tool.execute.before");
    assert_eq!(event.session_id, "ses-1");
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

    // legacy JSON dialect: {"decision": ...}
    let allow = adapter.format_response(&agent_hooks_protocol::PromptDecision::allow());
    assert_eq!(allow, serde_json::json!({"decision": "allow"}));

    let deny = adapter.format_response(&agent_hooks_protocol::PromptDecision::deny(Some(
        "needs approval".to_string(),
    )));
    assert_eq!(
        deny,
        serde_json::json!({"decision": "deny", "reason": "needs approval"})
    );
}

#[test]
fn opencode_payload_cwd_overrides_hook_process_cwd() {
    let adapter = load_adapter();
    let input = make_input(
        "session.created",
        r#"{"sessionID":"ses-cwd","cwd":"/workspace/project"}"#,
    );
    let event = adapter.parse_event(&input).expect("parse session.created");
    assert_eq!(
        event.context.cwd.as_deref(),
        Some(std::path::Path::new("/workspace/project"))
    );
}

#[test]
fn opencode_manifest_session_lifecycle_and_stop_observe() {
    let adapter = load_adapter();

    // session.created -> SessionStart (observe => {} regardless of decision)
    let input = make_input("session.created", r#"{"sessionID": "ses-2"}"#);
    let event = adapter.parse_event(&input).expect("parse session.created");
    assert_eq!(event.session_id, "ses-2");
    assert!(matches!(
        event.lifecycle,
        AgentLifecycle::SessionStart { .. }
    ));
    let v = adapter.format_response(&agent_hooks_protocol::PromptDecision::allow());
    assert_eq!(v, serde_json::json!({}));

    // chat.message -> UserPromptSubmit (gate: the real prompt-blocking hook)
    let prompt_input = make_input(
        "chat.message",
        r#"{"sessionID": "ses-2", "message": "Fix bug in auth.ts"}"#,
    );
    let prompt_event = adapter
        .parse_event(&prompt_input)
        .expect("parse chat.message");
    match prompt_event.lifecycle {
        AgentLifecycle::UserPromptSubmit { user_prompt } => {
            assert_eq!(user_prompt.as_deref(), Some("Fix bug in auth.ts"));
        }
        ref other => panic!("expected UserPromptSubmit, got {:?}", other),
    }
    let gate_allow = adapter.format_response(&agent_hooks_protocol::PromptDecision::allow());
    assert_eq!(gate_allow, serde_json::json!({"decision": "allow"}));
    let gate_deny = adapter.format_response(&agent_hooks_protocol::PromptDecision::deny(Some(
        "blocked".to_string(),
    )));
    assert_eq!(
        gate_deny,
        serde_json::json!({"decision": "deny", "reason": "blocked"})
    );

    // session.idle -> Stop (observe: bus-event subscribers return void, cannot gate)
    let stop_input = make_input(
        "session.idle",
        r#"{"sessionID": "ses-2", "summary": "All tests pass"}"#,
    );
    let stop_event = adapter
        .parse_event(&stop_input)
        .expect("parse session.idle");
    match stop_event.lifecycle {
        AgentLifecycle::Stop { summary, success } => {
            assert_eq!(summary.as_deref(), Some("All tests pass"));
            assert!(success);
        }
        ref other => panic!("expected Stop, got {:?}", other),
    }
    // observe renders {} for every verdict — no continue block remains.
    let keep_going = adapter.format_response(&agent_hooks_protocol::PromptDecision::deny(Some(
        "keep going".to_string(),
    )));
    assert_eq!(keep_going, serde_json::json!({}));
    let done = adapter.format_response(&agent_hooks_protocol::PromptDecision::allow());
    assert_eq!(done, serde_json::json!({}));

    // phantom vendor hooks are gone: turn.start no longer resolves.
    assert!(adapter
        .parse_event(&make_input("turn.start", r#"{"sessionID": "ses-2"}"#))
        .is_err());
}

#[test]
fn opencode_manifest_permission_and_observe_and_registry_pickup() {
    let adapter = load_adapter();

    let perm_input = make_input_with_request_id(
        "permission.ask",
        r#"{"sessionID": "ses-3", "toolName": "Bash"}"#,
        Some("p-1".to_string()),
    );
    let perm_event = adapter
        .parse_event(&perm_input)
        .expect("parse permission.ask");
    match perm_event.lifecycle {
        AgentLifecycle::PermissionRequest { prompt } => {
            assert_eq!(prompt.prompt_id, "p-1");
            assert_eq!(prompt.tool_name.as_deref(), Some("Bash"));
        }
        ref other => panic!("expected PermissionRequest, got {:?}", other),
    }
    // permission.ask takes "ask" | "deny" | "allow" (opencode's own Hooks
    // type), so escalation is a real outcome here and must NOT degrade to
    // deny — that would block a call the decider only wanted confirmed.
    let ask = adapter.format_response(&agent_hooks_protocol::PromptDecision::ask(Some(
        "Proceed?".to_string(),
    )));
    assert_eq!(
        ask,
        serde_json::json!({"decision": "ask", "reason": "Proceed?", "reply": "ask"})
    );

    // tool.execute.before carries opencode's real tool-args key `args`.
    let args_input = make_input(
        "tool.execute.before",
        r#"{"sessionID": "ses-3", "toolName": "Bash", "args": {"command": "cargo test"}}"#,
    );
    let args_event = adapter
        .parse_event(&args_input)
        .expect("parse tool.execute.before with args");
    match args_event.lifecycle {
        AgentLifecycle::PreToolUse {
            tool_name, input, ..
        } => {
            assert_eq!(tool_name, "Bash");
            assert_eq!(input, serde_json::json!({"command": "cargo test"}));
        }
        ref other => panic!("expected PreToolUse, got {:?}", other),
    }

    // tool.execute.after -> PostToolUse (observe => {})
    let post_input = make_input(
        "tool.execute.after",
        r#"{"sessionID": "ses-3", "toolName": "Bash", "toolOutput": "ok"}"#,
    );
    let post_event = adapter
        .parse_event(&post_input)
        .expect("parse tool.execute.after");
    match post_event.lifecycle {
        AgentLifecycle::PostToolUse {
            tool_name,
            output_preview,
        } => {
            assert_eq!(tool_name, "Bash");
            assert_eq!(output_preview.as_deref(), Some("ok"));
        }
        ref other => panic!("expected PostToolUse, got {:?}", other),
    }
    let v = adapter.format_response(&agent_hooks_protocol::PromptDecision::allow());
    assert_eq!(v, serde_json::json!({}));

    // registry must resolve the builtin manifest, not the legacy Rust adapter:
    // legacy deny is bare {"decision":"deny"}; the manifest deny carries the reason.
    let reg = AdapterRegistry::get_versioned("opencode", None).expect("opencode registered");
    let gate_input = make_input(
        "tool.execute.before",
        r#"{"sessionID": "ses-9", "toolName": "Bash", "toolInput": {}}"#,
    );
    reg.parse_event(&gate_input)
        .expect("registry parse tool.execute.before");
    let deny = reg.format_response(&agent_hooks_protocol::PromptDecision::deny(Some(
        "blocked".to_string(),
    )));
    assert_eq!(
        deny,
        serde_json::json!({"decision": "deny", "reason": "blocked"})
    );
}

#[test]
fn opencode_question_asked_lifts_a_question_and_answers_structurally() {
    // opencode answers questions through its own `question.asked` event and
    // `client.question.reply({answers: string[][]})` — never through a tool
    // call, which is why `ask_tools` was the wrong mechanism for it.
    let adapter = load_adapter();
    let input = make_input(
        "question.asked",
        r#"{"sessionID":"o1","requestID":"req-9","questions":[
             {"id":"q1","question":"Which README?","options":[{"label":"Root"},{"label":"Package"}]},
             {"id":"q2","question":"Run tests?","options":[{"label":"Yes"},{"label":"No"}]}
           ]}"#,
    );
    let event = adapter.parse_event(&input).expect("parse question.asked");
    let prompt = event
        .prompt_request()
        .expect("question.asked carries a prompt");
    assert_eq!(prompt.kind, agent_hooks_protocol::PromptKind::Question);
    assert_eq!(prompt.questions.len(), 2);
    assert!(
        event.is_interactive(),
        "a question the user is being asked must read as interactive"
    );

    // Structured picks must survive as string[][] — one group per question,
    // custom input appended. A comma-joined string could not say which pick
    // belongs to which question.
    let decision = agent_hooks_protocol::PromptDecision {
        allow: true,
        ask: false,
        custom_input: None,
        answers: vec![
            agent_hooks_protocol::PromptAnswer {
                question_id: "q1".to_string(),
                selected_options: vec!["Root".to_string()],
                custom_input: None,
            },
            agent_hooks_protocol::PromptAnswer {
                question_id: "q2".to_string(),
                selected_options: vec!["Yes".to_string()],
                custom_input: Some("and lint".to_string()),
            },
        ],
        allow_always: false,
        suggestion: None,
        defer: false,
        additional_context: None,
    };
    let out = adapter.format_response(&decision);
    assert_eq!(
        out,
        serde_json::json!({
            "decision": "allow",
            "reply": "allow",
            "answers": [["Root"], ["Yes", "and lint"]]
        })
    );

    // No picks: the key is omitted rather than sent empty.
    adapter.parse_event(&input).expect("re-parse");
    let bare = adapter.format_response(&agent_hooks_protocol::PromptDecision::allow());
    assert!(
        bare.get("answers").is_none(),
        "absent beats empty, got {bare}"
    );
}

#[test]
fn opencode_permission_carries_the_reply_verb() {
    // opencode distinguishes approving this call from approving every later
    // one ("once" vs "always"). The in-band status cannot say the second, so
    // the verb rides on the rendered decision and the plugin turns it into
    // the SDK call. Without it a decider can only ever approve one call at a
    // time, forever.
    let adapter = load_adapter();
    let input = make_input("permission.ask", r#"{"sessionID":"o1","toolName":"Bash"}"#);

    adapter.parse_event(&input).expect("parse permission.ask");
    let once = adapter.format_response(&agent_hooks_protocol::PromptDecision::allow());
    assert_eq!(once["decision"], "allow");
    assert_eq!(once["reply"], "allow");

    adapter.parse_event(&input).expect("re-parse");
    let always = adapter.format_response(&agent_hooks_protocol::PromptDecision::allow_always());
    assert_eq!(
        always["decision"], "allow",
        "a persistent grant still allows"
    );
    assert_eq!(
        always["reply"], "allow_always",
        "and is distinguishable from a one-shot allow"
    );
    assert_ne!(once, always);

    adapter.parse_event(&input).expect("re-parse");
    let deny = adapter.format_response(&agent_hooks_protocol::PromptDecision::deny(Some(
        "nope".to_string(),
    )));
    assert_eq!(deny["reply"], "deny");
}
