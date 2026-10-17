use agent_hooks_adapters::manifest::AdapterManifest;
use agent_hooks_adapters::{declarative::DeclarativeAdapter, HarnessAdapter};
use agent_hooks_protocol::{AgentLifecycle, EventContext, RawHookInput};

fn make_input(event_hint: &str, payload: &str) -> RawHookInput {
    RawHookInput {
        harness_hint: "cursor-test".to_string(),
        event_hint: Some(event_hint.to_string()),
        raw_payload: payload.to_string(),
        context: EventContext::default(),
        request_id: None,
        passthrough_env: Default::default(),
    }
}

const CURSOR_LIKE_MANIFEST: &str = r#"
{
  "id": "cursor-test",
  "display_name": "Cursor Test",
  "aliases": ["cursor-test-alias"],
  "default_version": "v1",
  "detect": {
    "harness_hint": ["cursor-test"],
    "payload_keys_any": [["conversation_id"]],
    "event_names": ["preToolUse", "stop"]
  },
  "versions": {
    "v1": {
      "install": [
        {
          "op": "json_hooks",
          "paths": ["~/.cursor/hooks.json", ".cursor/hooks.json"],
          "create": true,
          "root": "hooks.{event}",
          "entry_shape": "flat",
          "fixed_fields": {"version": 1},
          "entry": {"command": "{command} --event {event}", "timeout": 10},
          "dedupe": "command_subset",
          "entry_overrides": {"stop": {"loop_limit": 10}}
        }
      ],
      "events": {
        "preToolUse": {"lifecycle": "PreToolUse", "capability": "gate"},
        "postToolUse": {"lifecycle": "PostToolUse", "capability": "observe"},
        "stop": {"lifecycle": "Stop", "capability": "continue"}
      },
      "extraction": {
        "session_id": ["conversation_id", "session_id"],
        "prompt": ["prompt"],
        "tool_name": ["toolCall.name", "tool_name"],
        "tool_input": ["toolCall.args", "tool_input"],
        "tool_output": ["tool_output"],
        "error": ["error"],
        "success": ["success"],
        "summary": ["reason"]
      },
      "extraction_overrides": {
        "Stop": {"summary": ["reason", "terminationReason"]}
      },
      "response": {
        "mode": "json",
        "gate": {
          "allow": {"permission": "allow"},
          "deny": {"permission": "deny", "user_message": "{reason}", "agent_message": "{reason}"}
        },
        "observe": {},
        "continue": {
          "continue": {"followup_message": "{reason}"},
          "stop": {}
        },
        "empty_on_undecided": false
      },
      "unavailable": ["PermissionRequest", "Notification"],
      "notes": {"post_install": ["Restart Cursor"]}
    }
  }
}
"#;

#[test]
fn parse_cursor_like_manifest_succeeds() {
    let m = AdapterManifest::parse(CURSOR_LIKE_MANIFEST).expect("parse should succeed");
    assert_eq!(m.id, "cursor-test");
    assert_eq!(m.default_version(), "v1");
    assert!(m.get_version("v1").is_some());
    assert!(m.get_version("v2").is_none());
}

#[test]
fn bad_manifest_unknown_field_fails() {
    let bad = r#"
    {
      "id": "bad",
      "display_name": "Bad",
      "default_version": "v1",
      "unknown_field": "oops",
      "versions": {
        "v1": {
          "install": [],
          "events": {},
          "extraction": {},
          "response": {},
          "unavailable": []
        }
      }
    }
    "#;
    let res = AdapterManifest::parse(bad);
    assert!(
        res.is_err(),
        "unknown field must fail with deny_unknown_fields"
    );
}

#[test]
fn declarative_parse_event_and_format_response_roundtrip() {
    let manifest = AdapterManifest::parse(CURSOR_LIKE_MANIFEST).unwrap();
    let adapter = DeclarativeAdapter::new(manifest, "v1".to_string()).expect("create adapter");

    // 1. preToolUse with conversation_id + dotted toolCall.name
    let payload = r#"{"conversation_id": "conv-123", "toolCall": {"name": "Bash", "args": {"command": "ls"}}}"#;
    let input = make_input("preToolUse", payload);
    let event = adapter.parse_event(&input).expect("parse preToolUse");
    assert_eq!(event.session_id, "conv-123");
    assert_eq!(event.harness, "cursor-test");
    match event.lifecycle {
        AgentLifecycle::PreToolUse {
            tool_name, input, ..
        } => {
            assert_eq!(tool_name, "Bash");
            assert_eq!(input.get("command").and_then(|v| v.as_str()), Some("ls"));
        }
        ref other => panic!("expected PreToolUse, got {:?}", other),
    }

    // gate allow/deny
    let allow_decision = agent_hooks_protocol::PromptDecision::allow();
    let v = adapter.format_response(&allow_decision);
    assert_eq!(v.get("permission").and_then(|x| x.as_str()), Some("allow"));

    let deny_decision =
        agent_hooks_protocol::PromptDecision::deny(Some("need approval".to_string()));
    let v2 = adapter.format_response(&deny_decision);
    assert_eq!(v2.get("permission").and_then(|x| x.as_str()), Some("deny"));
    assert_eq!(
        v2.get("user_message").and_then(|x| x.as_str()),
        Some("need approval")
    );
    assert_eq!(
        v2.get("agent_message").and_then(|x| x.as_str()),
        Some("need approval")
    );

    // 2. stop event with summary
    let stop_payload = r#"{"conversation_id": "conv-123", "reason": "task done"}"#;
    let stop_input = make_input("stop", stop_payload);
    let stop_event = adapter.parse_event(&stop_input).expect("parse stop");
    match stop_event.lifecycle {
        AgentLifecycle::Stop { summary, success } => {
            assert_eq!(summary.as_deref(), Some("task done"));
            assert!(success);
        }
        ref other => panic!("expected Stop, got {:?}", other),
    }

    // continue: deny = keep going => followup_message, allow => stop {}
    let deny_cont = agent_hooks_protocol::PromptDecision::deny(Some("continue please".to_string()));
    let v_cont = adapter.format_response(&deny_cont);
    assert_eq!(
        v_cont.get("followup_message").and_then(|x| x.as_str()),
        Some("continue please")
    );

    let allow_cont = agent_hooks_protocol::PromptDecision::allow();
    let v_stop = adapter.format_response(&allow_cont);
    assert_eq!(v_stop, serde_json::json!({}));
}

#[test]
fn unknown_version_fails() {
    let manifest = AdapterManifest::parse(CURSOR_LIKE_MANIFEST).unwrap();
    let res = DeclarativeAdapter::new(manifest, "v9".to_string());
    assert!(res.is_err());
}
