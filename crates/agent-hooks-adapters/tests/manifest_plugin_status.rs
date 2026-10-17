//! Status parity for the plugin harnesses (pi, omp, opencode, amp, hermes).
//!
//! Each case feeds the payload the staged plugin asset actually sends (event
//! name = the `--event` it passes) and asserts the typed lifecycle plus the
//! derived status B2V consumes, against legacy B2V `decide_status`
//! (src-tauri/src/agent_event/lifecycle.rs). Legacy busy/idle/waiting map to
//! Running/Idle/WaitingForInput. Where legacy yields None (no change) the
//! manifest picks a per-cycle lifecycle (TurnEnd / PostToolUse) that B2V's
//! busy gate treats as noise.
use agent_hooks_adapters::manifest::{AdapterManifest, InstallOp};
use agent_hooks_adapters::{declarative::DeclarativeAdapter, HarnessAdapter};
use agent_hooks_protocol::{
    AgentEvent, AgentLifecycle, EventContext, NormalizedStatus, RawHookInput,
};

fn adapter(raw: &str) -> DeclarativeAdapter {
    let m = AdapterManifest::parse(raw).expect("manifest parses");
    DeclarativeAdapter::new(m, "v1".to_string()).expect("v1")
}

fn parse(a: &DeclarativeAdapter, harness: &str, event: &str, payload: &str) -> AgentEvent {
    a.parse_event(&RawHookInput {
        harness_hint: harness.to_string(),
        event_hint: Some(event.to_string()),
        raw_payload: payload.to_string(),
        context: EventContext::default(),
        request_id: None,
        passthrough_env: Default::default(),
    })
    .unwrap_or_else(|e| panic!("{harness} {event}: {e}"))
}

fn pi() -> DeclarativeAdapter {
    adapter(include_str!("../builtin/pi/adapter.json"))
}
fn omp() -> DeclarativeAdapter {
    adapter(include_str!("../builtin/omp/adapter.json"))
}
fn opencode() -> DeclarativeAdapter {
    adapter(include_str!("../builtin/opencode/adapter.json"))
}
fn amp() -> DeclarativeAdapter {
    adapter(include_str!("../builtin/amp/adapter.json"))
}
fn hermes() -> DeclarativeAdapter {
    adapter(include_str!("../builtin/hermes/adapter.json"))
}

use NormalizedStatus::{Closed, Idle, Running, WaitingForInput};

// ---------------------------------------------------------------- pi / omp

#[test]
fn pi_and_omp_status_table() {
    // (event the asset sends, payload, lifecycle name, status)
    // legacy decide_status generic branch: agent_start/turn_start busy,
    // agent_end idle, permissionrequest waiting.
    let cases: &[(&str, &str, &str, NormalizedStatus)] = &[
        (
            "UserPromptSubmit",
            r#"{"session_id":"s","prompt":"go"}"#,
            "UserPromptSubmit",
            Running,
        ),
        (
            "TurnStart",
            r#"{"session_id":"s","turnIndex":0}"#,
            "TurnStart",
            Running,
        ),
        (
            "PreToolUse",
            r#"{"session_id":"s","toolName":"bash","input":{"command":"ls"}}"#,
            "PreToolUse",
            Running,
        ),
        (
            "PostToolUse",
            r#"{"session_id":"s","toolName":"bash"}"#,
            "PostToolUse",
            Running,
        ),
        ("TurnEnd", r#"{"session_id":"s"}"#, "TurnEnd", Running),
        (
            "PermissionRequest",
            r#"{"session_id":"s","toolName":"bash","toolCallId":"c1"}"#,
            "PermissionRequest",
            WaitingForInput,
        ),
        ("Stop", r#"{"session_id":"s"}"#, "Stop", Idle),
        ("SessionEnd", r#"{"session_id":"s"}"#, "SessionEnd", Closed),
    ];
    for (harness, a) in [("pi", pi()), ("omp", omp())] {
        for (event, payload, lifecycle, status) in cases {
            let ev = parse(&a, harness, event, payload);
            assert_eq!(ev.lifecycle.name(), *lifecycle, "{harness} {event}");
            assert_eq!(ev.status, *status, "{harness} {event}");
        }
    }
}

// b2v-pi.ts isTerminalAgentEnd (e9043f2d7): an agent_end that will continue
// (auto-retry / queued follow-up) is not the end of the run. The asset drops
// it; the manifest demotes it too in case a shim forwards it anyway.
#[test]
fn omp_agent_end_will_continue_is_not_terminal() {
    let a = omp();
    for event in ["Stop", "agent_end"] {
        let ev = parse(
            &a,
            "omp",
            event,
            r#"{"session_id":"s","willContinue":true}"#,
        );
        assert!(
            matches!(ev.lifecycle, AgentLifecycle::TurnEnd { .. }),
            "{event}"
        );
        assert_eq!(ev.status, Running);
        // Terminal (false or absent) stays a real Stop.
        let ev = parse(
            &a,
            "omp",
            event,
            r#"{"session_id":"s","willContinue":false}"#,
        );
        assert_eq!(ev.status, Idle, "{event}");
        let ev = parse(&a, "omp", event, r#"{"session_id":"s"}"#);
        assert_eq!(ev.status, Idle, "{event}");
    }
    let ev = parse(
        &pi(),
        "pi",
        "Stop",
        r#"{"session_id":"s","willContinue":true}"#,
    );
    assert_eq!(ev.status, Running, "pi demotes the same way");
}

// b2v-pi.ts plan approval (f5c918940): plan_approval_requested -> waiting.
#[test]
fn pi_plan_approval_waits_for_input() {
    for (harness, a) in [("pi", pi()), ("omp", omp())] {
        let ev = parse(
            &a,
            harness,
            "plan_approval_requested",
            r#"{"session_id":"s","reason":"plan_approval","notification_type":"question","message":"Plan ready for approval."}"#,
        );
        match &ev.lifecycle {
            AgentLifecycle::Notification {
                message,
                notification_type,
                ..
            } => {
                assert_eq!(message, "Plan ready for approval.");
                assert_eq!(notification_type.as_deref(), Some("question"));
            }
            other => panic!("{harness}: expected Notification, got {other:?}"),
        }
        assert_eq!(ev.status, WaitingForInput, "{harness}");
    }
}

// b2v-pi.ts sessionTitle: getSessionName is pushed on every event.
#[test]
fn pi_session_title_passthrough() {
    let a = pi();
    let ev = parse(
        &a,
        "pi",
        "TurnStart",
        r#"{"session_id":"s","session_title":"Fix login"}"#,
    );
    assert_eq!(ev.session_title.as_deref(), Some("Fix login"));
    let ev = parse(
        &a,
        "pi",
        "TurnStart",
        r#"{"session_id":"s","session_title":"  "}"#,
    );
    assert!(
        ev.session_title.is_none(),
        "blank title is unknown, not a rename"
    );
    let ev = parse(&a, "pi", "TurnStart", r#"{"session_id":"s"}"#);
    assert!(ev.session_title.is_none());
}

// b2v-pi.ts gatePiToolCall remembers allow_always; the asset needs `reply`.
#[test]
fn pi_gate_renders_reply_and_answer_groups() {
    use agent_hooks_protocol::{PromptAnswer, PromptDecision};
    let a = omp();
    parse(
        &a,
        "omp",
        "PermissionRequest",
        r#"{"session_id":"s","toolName":"bash"}"#,
    );
    assert_eq!(
        a.format_response(&PromptDecision::allow_always()),
        serde_json::json!({"decision": "allow", "reply": "allow_always"})
    );
    parse(
        &a,
        "omp",
        "PreToolUse",
        r#"{"session_id":"s","toolName":"ask","input":{"questions":[{"id":"q1","question":"Color?","options":[{"label":"Red"},{"label":"Blue"}]}]}}"#,
    );
    let mut d = PromptDecision::allow();
    d.answers = vec![PromptAnswer {
        question_id: "q1".into(),
        selected_options: vec!["Blue".into()],
        custom_input: None,
    }];
    assert_eq!(
        a.format_response(&d),
        serde_json::json!({"decision": "allow", "reply": "allow", "answers": [["Blue"]]})
    );
    // Deny keeps the legacy dialect.
    assert_eq!(
        a.format_response(&PromptDecision::deny(Some("no".into()))),
        serde_json::json!({"decision": "deny", "reason": "no"})
    );
}

// ---------------------------------------------------------------- opencode

#[test]
fn opencode_status_table() {
    let a = opencode();
    // legacy decide_status opencode branch + b2v-hooks.ts getLifecycleSignal.
    let cases: &[(&str, &str, &str, NormalizedStatus)] = &[
        // session.status busy/retry -> busy (asset sends session.busy). A status
        // poll is a busy signal, not a prompt submit, so it never lands on
        // UserPromptSubmit — `chat.message` (below) is this harness's submit.
        (
            "session.busy",
            r#"{"sessionID":"ses_1","status":{"type":"busy"}}"#,
            "TurnStart",
            Running,
        ),
        (
            "session.busy",
            r#"{"sessionID":"ses_1","status":{"type":"retry"}}"#,
            "TurnStart",
            Running,
        ),
        // session.idle / session.status idle -> idle
        ("session.idle", r#"{"sessionID":"ses_1"}"#, "Stop", Idle),
        (
            "tool.execute.before",
            r#"{"sessionID":"ses_1","tool":"bash","callID":"c"}"#,
            "PreToolUse",
            Running,
        ),
        (
            "tool.execute.after",
            r#"{"sessionID":"ses_1","tool":"bash","callID":"c"}"#,
            "PostToolUse",
            Running,
        ),
        (
            "permission.ask",
            r#"{"sessionID":"ses_1","id":"per_1","type":"bash"}"#,
            "PermissionRequest",
            WaitingForInput,
        ),
        (
            "chat.message",
            r#"{"sessionID":"ses_1","messageID":"msg_1"}"#,
            "UserPromptSubmit",
            Running,
        ),
        (
            "session.deleted",
            r#"{"info":{"id":"ses_1"}}"#,
            "SessionEnd",
            Closed,
        ),
        // legacy session.title push -> None: status-neutral per-cycle noise.
        (
            "session.title",
            r#"{"sessionID":"ses_1","session_title":"Refactor auth"}"#,
            "TurnEnd",
            Running,
        ),
    ];
    for (event, payload, lifecycle, status) in cases {
        let ev = parse(&a, "opencode", event, payload);
        assert_eq!(ev.lifecycle.name(), *lifecycle, "{event}");
        assert_eq!(ev.status, *status, "{event}");
        assert_eq!(ev.session_id, "ses_1", "{event}");
    }
}

// b2v-hooks.ts extractSessionId: session.created carries the id only as info.id.
#[test]
fn opencode_session_created_uses_info_id_and_parent() {
    let a = opencode();
    let ev = parse(
        &a,
        "opencode",
        "session.created",
        r#"{"info":{"id":"ses_child","parentID":"ses_parent","title":"Explore"},"parent_session_id":"ses_parent","session_title":"Explore"}"#,
    );
    assert_eq!(ev.session_id, "ses_child");
    assert_eq!(ev.parent_session_id.as_deref(), Some("ses_parent"));
    assert_eq!(ev.session_title.as_deref(), Some("Explore"));
    // A later child event: the asset stamps the remembered parent.
    let ev = parse(
        &a,
        "opencode",
        "session.idle",
        r#"{"sessionID":"ses_child","parent_session_id":"ses_parent"}"#,
    );
    assert_eq!(ev.parent_session_id.as_deref(), Some("ses_parent"));
    assert_eq!(ev.status, Idle);
    // Negative: a top-level session has no parent.
    let ev = parse(
        &a,
        "opencode",
        "session.created",
        r#"{"info":{"id":"ses_top"}}"#,
    );
    assert_eq!(ev.session_id, "ses_top");
    assert!(ev.parent_session_id.is_none());
}

// ---------------------------------------------------------------- amp

#[test]
fn amp_status_table() {
    let a = amp();
    // legacy decide_status is_amp_agent branch.
    let cases: &[(&str, &str, &str, NormalizedStatus)] = &[
        (
            "agent.start",
            r#"{"session_id":"T-1","message":"hi"}"#,
            "UserPromptSubmit",
            Running,
        ),
        (
            "tool.call",
            r#"{"session_id":"T-1","tool_name":"Bash","tool_input":{"cmd":"ls"}}"#,
            "PreToolUse",
            Running,
        ),
        (
            "tool.result",
            r#"{"session_id":"T-1","tool_name":"Bash","status":"done"}"#,
            "PostToolUse",
            Running,
        ),
        // "Amp emits agent.end when the visible turn has finished; no later
        // idle event follows, so this is the terminal status."
        (
            "agent.end",
            r#"{"session_id":"T-1","status":"done"}"#,
            "Stop",
            Idle,
        ),
        // legacy: "stop" => busy — a mid-turn signal, never terminal.
        ("stop", r#"{"session_id":"T-1"}"#, "TurnEnd", Running),
        ("Stop", r#"{"session_id":"T-1"}"#, "TurnEnd", Running),
        (
            "session.start",
            r#"{"session_id":"T-1"}"#,
            "SessionStart",
            Idle,
        ),
    ];
    for (event, payload, lifecycle, status) in cases {
        let ev = parse(&a, "amp", event, payload);
        assert_eq!(ev.lifecycle.name(), *lifecycle, "{event}");
        assert_eq!(ev.status, *status, "{event}");
        assert_eq!(ev.session_id, "T-1", "{event}");
    }
}

// b2v-amp.ts sessionIdFrom: the Amp thread id is the session.
#[test]
fn amp_thread_id_is_session() {
    let a = amp();
    let ev = parse(
        &a,
        "amp",
        "agent.end",
        r#"{"thread":{"id":"T-9","title":"Ship it"}}"#,
    );
    assert_eq!(ev.session_id, "T-9");
    assert_eq!(ev.session_title.as_deref(), Some("Ship it"));
}

// Amp has no hooks.json; legacy installed a plugin into ~/.config/amp/plugins.
#[test]
fn amp_installs_plugin_asset() {
    let m = AdapterManifest::parse(include_str!("../builtin/amp/adapter.json")).unwrap();
    let vs = m.get_version("v1").unwrap();
    match &vs.install[..] {
        [InstallOp::StageAssets {
            stage_dir, assets, ..
        }] => {
            assert_eq!(stage_dir, "~/.config/amp/plugins");
            let key = assets[0].embedded.as_deref().unwrap();
            let src = agent_hooks_adapters::embedded_asset("amp", key).expect("asset embedded");
            assert!(src.contains("amp.on(\"agent.end\""));
            assert!(src.contains("\"__AGENT_HOOKS_BIN__\""));
        }
        other => panic!("expected one stage_assets op, got {other:?}"),
    }
}

// ---------------------------------------------------------------- hermes

#[test]
fn hermes_status_table() {
    let a = hermes();
    // legacy decide_status is_hermes_agent branch.
    let cases: &[(&str, &str, &str, NormalizedStatus)] = &[
        (
            "pre_llm_call",
            r#"{"session_id":"h1","is_first_turn":true}"#,
            "TurnStart",
            Running,
        ),
        (
            "pre_tool_call",
            r#"{"session_id":"h1","tool_name":"terminal","tool_input":{"command":"ls"}}"#,
            "PreToolUse",
            Running,
        ),
        (
            "post_tool_call",
            r#"{"session_id":"h1","tool_name":"terminal","status":"ok"}"#,
            "PostToolUse",
            Running,
        ),
        (
            "post_approval_response",
            r#"{"session_id":"h1","choice":"once"}"#,
            "PostToolUse",
            Running,
        ),
        (
            "pre_approval_request",
            r#"{"session_id":"h1","command":"rm -rf build","description":"dangerous command"}"#,
            "PermissionRequest",
            WaitingForInput,
        ),
        // on_session_end is per run: legacy idle, not closed.
        (
            "on_session_end",
            r#"{"session_id":"h1","completed":true,"interrupted":false}"#,
            "Stop",
            Idle,
        ),
        (
            "subagent_stop",
            r#"{"session_id":"h1","subagent_id":"child-1","summary":"done"}"#,
            "SubagentStop",
            Running,
        ),
        (
            "on_session_start",
            r#"{"session_id":"h1"}"#,
            "SessionStart",
            Idle,
        ),
    ];
    for (event, payload, lifecycle, status) in cases {
        let ev = parse(&a, "hermes", event, payload);
        assert_eq!(ev.lifecycle.name(), *lifecycle, "{event}");
        assert_eq!(ev.status, *status, "{event}");
        assert_eq!(ev.session_id, "h1", "{event}");
    }
}

#[test]
fn hermes_approval_request_describes_the_command() {
    let ev = parse(
        &hermes(),
        "hermes",
        "pre_approval_request",
        r#"{"session_id":"h1","command":"rm -rf build","description":"dangerous command","request_id":"call_1"}"#,
    );
    match ev.lifecycle {
        AgentLifecycle::PermissionRequest { prompt } => {
            assert_eq!(prompt.description.as_deref(), Some("dangerous command"));
        }
        other => panic!("expected PermissionRequest, got {other:?}"),
    }
}

// Child sessions: pre_llm_call carries the parent (Hermes kwargs).
#[test]
fn hermes_child_llm_call_carries_parent() {
    let ev = parse(
        &hermes(),
        "hermes",
        "pre_llm_call",
        r#"{"session_id":"child","parent_session_id":"root"}"#,
    );
    assert_eq!(ev.parent_session_id.as_deref(), Some("root"));
    let ev = parse(
        &hermes(),
        "hermes",
        "pre_llm_call",
        r#"{"session_id":"root"}"#,
    );
    assert!(ev.parent_session_id.is_none());
}

// Hermes has no hooks.json; legacy installed a python plugin dir.
#[test]
fn hermes_installs_python_plugin() {
    let m = AdapterManifest::parse(include_str!("../builtin/hermes/adapter.json")).unwrap();
    let vs = m.get_version("v1").unwrap();
    match &vs.install[..] {
        [InstallOp::StageAssets {
            stage_dir,
            assets,
            managed_marker,
            ..
        }] => {
            assert_eq!(stage_dir, "~/.hermes/plugins/agent-hooks");
            // A `//` marker would be a Python syntax error.
            assert_eq!(managed_marker.as_deref(), Some("# managed by agent-hooks"));
            let paths: Vec<_> = assets.iter().map(|x| x.path.as_str()).collect();
            assert_eq!(paths, ["__init__.py", "plugin.yaml"]);
            let init = agent_hooks_adapters::embedded_asset("hermes", "__init__.py").unwrap();
            assert!(init.contains("def register(ctx):"));
            assert!(init.contains("INSTALL_BIN = \"__AGENT_HOOKS_BIN__\""));
        }
        other => panic!("expected one stage_assets op, got {other:?}"),
    }
}
