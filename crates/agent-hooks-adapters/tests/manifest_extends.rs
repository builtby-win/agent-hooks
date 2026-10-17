use agent_hooks_adapters::manifest::{AdapterManifest, Capability, ExtractionSource, LifecycleRef};

fn paths(v: &[&str]) -> Vec<ExtractionSource> {
    v.iter()
        .map(|s| ExtractionSource::Path(s.to_string()))
        .collect()
}

const BASE: &str = r#"
{
  "id": "base-harness",
  "display_name": "Base Harness",
  "aliases": ["base-alias"],
  "default_version": "v1",
  "min_agent_hooks": "1.0.0",
  "detect": {
    "harness_hint": ["base-harness"],
    "payload_keys_any": [["session_id"]],
    "event_names": ["PreToolUse"]
  },
  "versions": {
    "v1": {
      "install": [{"op": "run_command", "command": "base-install"}],
      "events": {
        "PreToolUse": {"lifecycle": "PreToolUse", "capability": "gate"},
        "PostToolUse": {"lifecycle": "PostToolUse", "capability": "observe"}
      },
      "extraction": {
        "session_id": ["session_id"],
        "prompt": ["prompt"],
        "tool_name": ["tool_name"]
      },
      "extraction_overrides": {
        "Stop": {"summary": ["reason"]}
      },
      "response": {
        "mode": "json",
        "gate": {"allow": {"a": 1}, "deny": {"d": 1}},
        "observe": {"o": true},
        "continue": {"continue": {"c": 1}, "stop": {"s": 1}},
        "response_overrides": {
          "Stop": {"allow": {"a": 1}, "deny": {"d": 1}}
        },
        "empty_on_undecided": true
      },
      "unavailable": ["Notification"],
      "ask_tools": ["AskUserQuestion"],
      "notes": {"post_install": ["Restart"]}
    }
  }
}
"#;

const CHILD: &str = r#"
{
  "id": "child-harness",
  "display_name": "Child Harness",
  "default_version": "v1",
  "extends": "base-harness",
  "versions": {
    "v1": {
      "events": {
        "PostToolUse": {"lifecycle": "PostToolUse", "capability": "gate"}
      },
      "extraction": {
        "prompt": ["description"]
      },
      "extraction_overrides": {
        "Stop": {"summary": ["detail"]},
        "PreToolUse": {"prompt": ["question"]}
      },
      "response": {
        "response_overrides": {
          "Stop": {"allow": {"a": 2}, "deny": {"d": 2}}
        }
      },
      "unavailable": ["TurnStart"],
      "ask_tools": []
    }
  }
}
"#;

fn parse_all(jsons: &[&str]) -> Vec<AdapterManifest> {
    jsons
        .iter()
        .map(|s| AdapterManifest::parse(s).expect("fixture parses"))
        .collect()
}

fn resolve(jsons: &[&str]) -> Vec<AdapterManifest> {
    AdapterManifest::resolve_all(&parse_all(jsons)).expect("resolution succeeds")
}

fn child_of(base_extra: &[&str]) -> AdapterManifest {
    let mut jsons = vec![BASE, CHILD];
    jsons.extend(base_extra.iter());
    resolve(&jsons)
        .into_iter()
        .find(|m| m.id == "child-harness")
        .expect("child resolves")
}

// (a) child inherits events/extraction, overriding one event capability +
// one extraction field.
#[test]
fn child_inherits_and_overrides_event_and_extraction() {
    let child = child_of(&[]);
    let v1 = child.get_version("v1").expect("v1 present");

    // Overridden event capability (whole EventSpec replaced).
    let post = v1.events.get("PostToolUse").expect("PostToolUse present");
    assert_eq!(post.capability, Capability::Gate);
    // Inherited sibling event untouched.
    let pre = v1.events.get("PreToolUse").expect("PreToolUse inherited");
    assert_eq!(pre.capability, Capability::Gate);
    assert_eq!(pre.lifecycle, LifecycleRef::PreToolUse);

    // Overridden extraction field; siblings inherited.
    assert_eq!(v1.extraction.prompt, paths(&["description"]));
    assert_eq!(v1.extraction.session_id, paths(&["session_id"]));
    assert_eq!(v1.extraction.tool_name, paths(&["tool_name"]));

    // extraction_overrides merge per key: child key replaced, new key added.
    let stop = v1.extraction_overrides.get("Stop").expect("Stop override");
    assert_eq!(stop.get("summary").expect("summary"), &paths(&["detail"]));
    assert!(v1.extraction_overrides.contains_key("PreToolUse"));

    // Top-level inheritance: aliases/detect/min_agent_hooks from base,
    // id/display_name stay local.
    assert_eq!(child.aliases, vec!["base-alias".to_string()]);
    assert!(child.detect.is_some());
    assert_eq!(child.min_agent_hooks.as_deref(), Some("1.0.0"));
    assert_eq!(child.display_name, "Child Harness");

    // response_overrides merge per key (local wins); gate/observe/continue
    // inherited; empty_on_undecided OR-combined (base true wins).
    let stop_resp = v1.response.response_overrides.get("Stop").expect("Stop");
    assert_eq!(stop_resp.allow, serde_json::json!({"a": 2}));
    assert!(v1.response.gate.is_some());
    assert!(v1.response.observe.is_some());
    assert!(v1.response.continue_response.is_some());
    assert!(v1.response.empty_on_undecided);

    // notes inherited (local None).
    let notes = v1.notes.as_ref().expect("notes inherited");
    assert_eq!(notes.post_install, vec!["Restart".to_string()]);
}

// (b) three-level chain resolves transitively.
#[test]
fn three_level_chain_resolves() {
    const MID: &str = r#"
    {
      "id": "mid-harness",
      "display_name": "Mid Harness",
      "default_version": "v1",
      "extends": "base-harness",
      "versions": {
        "v1": {
          "events": {},
          "extraction": {"tool_name": ["mid_tool"]},
          "response": {},
          "unavailable": []
        }
      }
    }
    "#;
    const LEAF: &str = r#"
    {
      "id": "leaf-harness",
      "display_name": "Leaf Harness",
      "default_version": "v1",
      "extends": "mid-harness",
      "versions": {
        "v1": {
          "events": {},
          "extraction": {},
          "response": {},
          "unavailable": []
        }
      }
    }
    "#;
    let resolved = resolve(&[BASE, MID, LEAF]);
    let leaf = resolved
        .iter()
        .find(|m| m.id == "leaf-harness")
        .expect("leaf");
    let v1 = leaf.get_version("v1").expect("v1");
    // mid's override flows through; base's fields survive both hops.
    assert_eq!(v1.extraction.tool_name, paths(&["mid_tool"]));
    assert_eq!(v1.extraction.session_id, paths(&["session_id"]));
    assert_eq!(leaf.aliases, vec!["base-alias".to_string()]);
    assert!(leaf.extends.is_none());
}

// (c) cycles error naming the chain, including self-extends.
#[test]
fn cycle_errors_naming_chain() {
    const A: &str = r#"{
      "id": "cycle-a", "display_name": "A", "default_version": "v1",
      "extends": "cycle-b", "versions": {"v1": {}}
    }"#;
    const B: &str = r#"{
      "id": "cycle-b", "display_name": "B", "default_version": "v1",
      "extends": "cycle-a", "versions": {"v1": {}}
    }"#;
    let err = AdapterManifest::resolve_all(&parse_all(&[A, B])).expect_err("cycle must fail");
    let msg = err.to_string();
    assert!(msg.contains("cycle-a"), "names a: {msg}");
    assert!(msg.contains("cycle-b"), "names b: {msg}");

    const SELF: &str = r#"{
      "id": "self-harness", "display_name": "S", "default_version": "v1",
      "extends": "self-harness", "versions": {"v1": {}}
    }"#;
    let err = AdapterManifest::resolve_all(&parse_all(&[SELF])).expect_err("self cycle must fail");
    assert!(err.to_string().contains("self-harness"));
}

// (d) unknown base errors naming it.
#[test]
fn unknown_base_errors() {
    const ORPHAN: &str = r#"{
      "id": "orphan", "display_name": "O", "default_version": "v1",
      "extends": "missing-base", "versions": {"v1": {}}
    }"#;
    let err =
        AdapterManifest::resolve_all(&parse_all(&[ORPHAN])).expect_err("unknown base must fail");
    let msg = err.to_string();
    assert!(msg.contains("missing-base"), "names base: {msg}");
    assert!(msg.contains("orphan"), "names referrer: {msg}");
}

// (e) manifests without extends resolve identically.
#[test]
fn manifests_without_extends_resolve_identically() {
    let input = parse_all(&[BASE]);
    let resolved = AdapterManifest::resolve_all(&input).expect("resolves");
    assert_eq!(resolved, input);
}

// (f) install/unavailable/ask_tools inherit-or-replace.
#[test]
fn list_fields_inherit_or_replace() {
    let child = child_of(&[]);
    let v1 = child.get_version("v1").expect("v1");

    // Non-empty local replaces.
    assert_eq!(v1.unavailable, vec!["TurnStart".to_string()]);
    // Empty local inherits (child declares ask_tools: []).
    assert_eq!(v1.ask_tools.len(), 1);
    assert_eq!(v1.ask_tools[0].name(), "AskUserQuestion");
    // Absent local install inherits whole.
    assert_eq!(v1.install.len(), 1);

    // A local install replaces the base install.
    const WITH_INSTALL: &str = r#"{
      "id": "installer", "display_name": "I", "default_version": "v1",
      "extends": "base-harness",
      "versions": {"v1": {"install": [{"op": "run_command", "command": "child-install"}]}}
    }"#;
    let resolved = resolve(&[BASE, WITH_INSTALL]);
    let m = resolved
        .iter()
        .find(|m| m.id == "installer")
        .expect("installer");
    let v1 = m.get_version("v1").expect("v1");
    assert_eq!(v1.install.len(), 1);
    let json = serde_json::to_value(&v1.install[0]).expect("serializes");
    assert_eq!(
        json.get("command"),
        Some(&serde_json::json!("child-install"))
    );
    // ...while events/extraction still inherit.
    assert!(v1.events.contains_key("PreToolUse"));
    assert_eq!(v1.extraction.prompt, paths(&["prompt"]));
}

// (g) resolved manifests are fully materialized (extends: None, omitted
// from serialization).
#[test]
fn resolved_manifest_has_no_extends() {
    let resolved = resolve(&[BASE, CHILD]);
    for m in &resolved {
        assert!(m.extends.is_none(), "{} keeps extends", m.id);
        let json = serde_json::to_value(m).expect("serializes");
        assert!(
            json.get("extends").is_none(),
            "extends omitted from JSON for {}",
            m.id
        );
    }
}

// A child may inherit its default version's content from the base.
#[test]
fn child_can_inherit_default_version_content() {
    const SLIM: &str = r#"{
      "id": "slim", "display_name": "Slim", "default_version": "v1",
      "extends": "base-harness", "versions": {}
    }"#;
    let resolved = resolve(&[BASE, SLIM]);
    let slim = resolved.iter().find(|m| m.id == "slim").expect("slim");
    let v1 = slim.get_version("v1").expect("inherited v1");
    assert!(v1.events.contains_key("PreToolUse"));
}

// Version keys missing locally are taken whole from the base.
#[test]
fn missing_version_key_taken_whole() {
    const EXTRA_BASE: &str = r#"{
      "id": "two-ver", "display_name": "T", "default_version": "v1",
      "versions": {
        "v1": {"events": {"PreToolUse": {"lifecycle": "PreToolUse", "capability": "gate"}}},
        "v2": {"events": {"Stop": {"lifecycle": "Stop", "capability": "continue"}}}
      }
    }"#;
    const EXTRA_CHILD: &str = r#"{
      "id": "two-ver-child", "display_name": "TC", "default_version": "v1",
      "extends": "two-ver",
      "versions": {"v1": {"unavailable": ["Notification"]}}
    }"#;
    let resolved = resolve(&[EXTRA_BASE, EXTRA_CHILD]);
    let child = resolved
        .iter()
        .find(|m| m.id == "two-ver-child")
        .expect("child");
    // v2 inherited whole; v1 merged (base events + local unavailable).
    let v2 = child.get_version("v2").expect("v2 inherited");
    assert!(v2.events.contains_key("Stop"));
    let v1 = child.get_version("v1").expect("v1");
    assert!(v1.events.contains_key("PreToolUse"));
    assert_eq!(v1.unavailable, vec!["Notification".to_string()]);
}
