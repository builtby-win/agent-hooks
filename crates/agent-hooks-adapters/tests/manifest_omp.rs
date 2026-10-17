use agent_hooks_adapters::manifest::AdapterManifest;
use agent_hooks_adapters::registry::AdapterRegistry;
use agent_hooks_adapters::{declarative::DeclarativeAdapter, HarnessAdapter};
use agent_hooks_protocol::{AgentLifecycle, EventContext, RawHookInput};

const BUILTIN: &str = include_str!("../builtin/omp/adapter.json");

fn make_input(event_hint: &str, payload: &str) -> RawHookInput {
    RawHookInput {
        harness_hint: "omp".to_string(),
        event_hint: Some(event_hint.to_string()),
        raw_payload: payload.to_string(),
        context: EventContext::default(),
        request_id: Some("req-omp-1".to_string()),
        passthrough_env: Default::default(),
    }
}

fn manifest_adapter() -> DeclarativeAdapter {
    let manifest = AdapterManifest::parse(BUILTIN).expect("builtin omp manifest parses");
    assert_eq!(manifest.id, "omp");
    DeclarativeAdapter::new(manifest, "v1".to_string()).expect("create omp adapter")
}

#[test]
fn builtin_omp_manifest_parses_with_expected_shape() {
    let manifest = AdapterManifest::parse(BUILTIN).expect("builtin omp manifest parses");
    assert_eq!(manifest.default_version(), "v1");
    assert_eq!(manifest.aliases, vec!["oh-my-pi", "oh_my_pi"]);
    let vs = manifest.get_version("v1").expect("v1 exists");
    // omp v18.1.x stop signals both map to Stop
    for ev in ["agent_end", "session_stop", "agentsettled"] {
        assert!(vs.events.contains_key(ev), "missing event {ev}");
    }
    // plugin-based install: stage bridge assets, then register via omp CLI
    assert_eq!(vs.install.len(), 2);
}

#[test]
fn omp_stop_signals_derive_stop() {
    let adapter = manifest_adapter();
    for ev in ["agent_end", "session_stop"] {
        let input = make_input(ev, r#"{"session_id": "ses_omp_1"}"#);
        let event = adapter.parse_event(&input).expect("parse stop signal");
        assert_eq!(event.harness, "omp");
        assert!(
            matches!(event.lifecycle, AgentLifecycle::Stop { .. }),
            "expected Stop for {ev}"
        );
    }
}

#[test]
fn omp_registry_resolves_aliases() {
    for hint in ["omp", "oh-my-pi", "oh_my_pi"] {
        let adapter =
            AdapterRegistry::get_versioned(hint, None).expect("omp resolves from registry");
        assert_eq!(adapter.id(), "omp");
    }
    // `pi` stays its own harness — no alias bleed between the split adapters.
    let pi = AdapterRegistry::get_versioned("pi", None).expect("pi resolves from registry");
    assert_eq!(pi.id(), "pi");
}
