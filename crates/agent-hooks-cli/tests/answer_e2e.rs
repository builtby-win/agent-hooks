//! Black-box tests for daemonless canonical decision validation.
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_agent-hooks"))
}

fn event(lifecycle: serde_json::Value) -> String {
    serde_json::json!({
        "event_id": "e1",
        "harness": "test",
        "session_id": "s1",
        "lifecycle": lifecycle,
    })
    .to_string()
}

fn question(id: &str, options: &[&str], custom: bool) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "question": format!("Question {id}?"),
        "options": options.iter().map(|label| serde_json::json!({"label": label})).collect::<Vec<_>>(),
        "custom": custom,
    })
}

fn run_return(event: &str, flags: &[&str]) -> std::process::Output {
    let mut child = Command::new(bin())
        .args(["answer", "--return"])
        .args(flags)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn agent-hooks answer");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(event.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn return_validates_and_orders_answers_for_permission_and_pretool_questions() {
    for lifecycle in [
        serde_json::json!({
            "type": "PermissionRequest",
            "data": {"prompt": {"prompt_id": "p1", "kind": "question", "questions": [question("first", &["A", "B"], true), question("second", &[], true)]}}
        }),
        serde_json::json!({
            "type": "PreToolUse",
            "data": {"tool_name": "request_user_input", "input": {}, "prompt": {"prompt_id": "p2", "kind": "question", "questions": [question("first", &["A", "B"], true), question("second", &[], true)]}}
        }),
    ] {
        let input = event(lifecycle);
        let decision = r#"{"allow":true,"answers":[{"question_id":"second","custom_input":"  padded text  "},{"question_id":"first","selected_options":["B"]}]}"#;
        let output = run_return(&input, &["--decision", decision]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let resolved: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(resolved["answers"][0]["question_id"], "first");
        assert_eq!(
            resolved["answers"][0]["selected_options"],
            serde_json::json!(["B"])
        );
        assert_eq!(resolved["answers"][1]["question_id"], "second");
        assert_eq!(resolved["answers"][1]["custom_input"], "  padded text  ");
    }
}

#[test]
fn return_invalid_answers_fail_closed_without_stdout() {
    let input = event(serde_json::json!({
        "type": "PermissionRequest",
        "data": {"prompt": {"prompt_id": "p1", "kind": "question", "questions": [question("color", &["Red", "Blue"], false)]}}
    }));
    let output = run_return(
        &input,
        &[
            "--decision",
            r#"{"allow":true,"answers":[{"question_id":"color","selected_options":["Green"]}]}"#,
        ],
    );
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("not an option"));
}

#[test]
fn return_missing_event_fails_instead_of_waiting() {
    let child = Command::new(bin())
        .args(["answer", "--return", "--decision", r#"{"allow":true}"#])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn agent-hooks answer");
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
}
#[test]
fn decision_modes_require_explicit_decision_and_reject_ambiguous_combinations() {
    for args in [
        vec!["answer", "--return"],
        vec!["answer", "--decision", r#"{"allow":true}"#],
        vec!["answer", "--allow"],
        vec![
            "answer",
            "--return",
            "--auto",
            "allow",
            "--decision",
            r#"{"allow":true}"#,
        ],
    ] {
        let output = Command::new(bin())
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .expect("run agent-hooks answer");
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
    }
}

#[test]
fn simple_answers_group_multi_select_and_preserve_exact_text() {
    let mut features = question("features", &["Search", "Export = CSV"], true);
    features["multiple"] = serde_json::json!(true);
    let input = event(serde_json::json!({
        "type": "PreToolUse",
        "data": {"tool_name": "ask", "input": {}, "prompt": {
            "prompt_id": "p1", "kind": "question",
            "questions": [features, question("notes", &[], true)]
        }}
    }));
    let output = run_return(
        &input,
        &[
            "--allow",
            "--custom-input",
            "notes",
            "  keep = spaces  ",
            "--select",
            "features",
            "Export = CSV",
            "--custom-input",
            "features",
            "--exact note",
            "--select",
            "features",
            "Search",
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let resolved: agent_hooks_protocol::PromptDecision =
        serde_json::from_slice(&output.stdout).unwrap();
    assert!(resolved.allow);
    assert_eq!(
        resolved.answers,
        vec![
            agent_hooks_protocol::PromptAnswer {
                question_id: "features".into(),
                selected_options: vec!["Export = CSV".into(), "Search".into()],
                custom_input: Some("--exact note".into()),
            },
            agent_hooks_protocol::PromptAnswer {
                question_id: "notes".into(),
                selected_options: vec![],
                custom_input: Some("  keep = spaces  ".into()),
            },
        ]
    );
}

#[test]
fn simple_allow_and_deny_produce_canonical_permission_decisions() {
    let input = event(serde_json::json!({
        "type": "PermissionRequest",
        "data": {"prompt": {"prompt_id": "p1", "kind": "permission", "tool_name": "Bash"}}
    }));
    for (flags, expected) in [
        (
            vec!["--allow"],
            agent_hooks_protocol::PromptDecision::allow(),
        ),
        (
            vec!["--deny", "Protected branch"],
            agent_hooks_protocol::PromptDecision::deny(Some("Protected branch".into())),
        ),
    ] {
        let output = run_return(&input, &flags);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let resolved: agent_hooks_protocol::PromptDecision =
            serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(resolved, expected);
    }
}

#[test]
fn simple_answers_reject_invalid_ids_cardinality_and_custom_input() {
    let input = event(serde_json::json!({
        "type": "PreToolUse",
        "data": {"tool_name": "ask", "input": {}, "prompt": {
            "prompt_id": "p1", "kind": "question",
            "questions": [question("choice", &["A", "B"], false), question("notes", &[], true)]
        }}
    }));
    for flags in [
        vec!["--select", "unknown", "A", "--custom-input", "notes", "valid"],
        vec![
            "--select", "choice", "A", "--select", "choice", "B", "--custom-input", "notes", "valid",
        ],
        vec!["--custom-input", "choice", "forbidden", "--custom-input", "notes", "valid"],
        vec!["--select", "choice", "A"],
        vec![
            "--select", "choice", "A", "--custom-input", "notes", "first", "--custom-input", "notes", "second",
        ],
        vec!["--select", "choice", "A", "--custom-input", "notes", ""],
    ] {
        let output = run_return(&input, &flags);
        assert_eq!(
            output.status.code(),
            Some(2),
            "{flags:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stdout.is_empty(), "{flags:?}");
    }
}

#[test]
fn simple_flags_reject_conflicting_verdicts_and_missing_values() {
    for flags in [
        vec!["--allow", "--decision", r#"{"allow":true}"#],
        vec!["--allow", "--deny", "blocked"],
        vec!["--deny", "blocked", "--select", "q", "A"],
        vec!["--select", "q", "A", "--auto", "allow"],
        vec!["--custom-input", "q", "value", "--line"],
        vec!["--select", "q"],
        vec!["--custom-input", "q"],
    ] {
        let output = run_return("", &flags);
        assert_eq!(output.status.code(), Some(2), "{flags:?}");
        assert!(output.stdout.is_empty(), "{flags:?}");
    }
}
