//! `agent_cli` start/exec/models/model_args validation: argv runs unshelled,
//! so each template must start with a declared executable and carry its
//! placeholder exactly once.

use agent_hooks_adapters::manifest::AdapterManifest;

fn parse(agent_cli: &str) -> Result<AdapterManifest, String> {
    let raw = format!(
        r#"{{"id":"t","display_name":"T","default_version":"v1","versions":{{"v1":{{
            "agent_cli":{agent_cli},
            "events":{{"stop":{{"lifecycle":"Stop","capability":"continue"}}}}}}}}}}"#
    );
    AdapterManifest::parse(&raw).map_err(|e| e.to_string())
}

const BASE: &str = r#""executables":["t"],"recipe_source":"test""#;

#[test]
fn accepts_a_complete_spec() {
    let m = parse(&format!(
        r#"{{{BASE},
          "start":{{"argv":["t","{{prompt}}"]}},
          "exec":{{"argv":["t","run","{{prompt}}"]}},
          "models":{{"argv":["t","models"],"id_fields":["id"]}},
          "model_aliases":["fast"],
          "model_args":["--model","{{model}}"]}}"#
    ))
    .unwrap();
    let cli = m.versions["v1"].agent_cli.as_ref().unwrap();
    assert_eq!(cli.model_args, ["--model", "{model}"]);
}

#[test]
fn rejects_malformed_specs() {
    let cases = [
        ("start without prompt", r#""start":{"argv":["t","x"]}"#),
        (
            "start with two prompts",
            r#""start":{"argv":["t","{prompt}","{prompt}"]}"#,
        ),
        (
            "exec with foreign executable",
            r#""exec":{"argv":["sh","-c","{prompt}"]}"#,
        ),
        ("exec with empty argv", r#""exec":{"argv":[]}"#),
        (
            "models with foreign executable",
            r#""models":{"argv":["curl","x"]}"#,
        ),
        ("models with empty argv", r#""models":{"argv":[]}"#),
        (
            "models mixing id_fields and id_column",
            r#""models":{"argv":["t","m"],"id_fields":["id"],"id_column":1}"#,
        ),
        (
            "model_args without {model}",
            r#""model_args":["--model","x"]"#,
        ),
        (
            "model_args with two {model}",
            r#""model_args":["{model}","{model}"]"#,
        ),
        ("empty model alias", r#""model_aliases":[""]"#),
    ];
    for (name, extra) in cases {
        let err = parse(&format!("{{{BASE},{extra}}}")).expect_err(name);
        assert!(err.contains("version 'v1'"), "{name}: {err}");
    }
}

#[test]
fn builtin_agent_cli_specs_all_validate() {
    // load_embedded validates every builtin through `parse`; a bad recipe
    // would silently drop the adapter, so assert the launchable ones survive.
    let ids: Vec<String> = AdapterManifest::load_tiered_from(&[])
        .into_iter()
        .filter(|m| {
            m.versions[&m.default_version]
                .agent_cli
                .as_ref()
                .is_some_and(|c| c.exec.is_some())
        })
        .map(|m| m.id)
        .collect();
    for want in [
        "claude-code",
        "codex",
        "antigravity",
        "omp",
        "pi",
        "opencode",
        "muse",
    ] {
        assert!(ids.iter().any(|i| i == want), "{want} missing from {ids:?}");
    }
}
