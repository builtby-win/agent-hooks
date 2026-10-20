//! End-to-end tests for `agent-hooks start|exec|models` against a fake agent
//! executable declared by a user-tier manifest.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::{tempdir, TempDir};

fn agent_hooks_bin() -> PathBuf {
    let mut bin = std::env::current_exe().unwrap();
    bin.pop();
    if bin.ends_with("deps") {
        bin.pop();
    }
    bin.join("agent-hooks")
}

fn write_script(path: &Path, body: &str) {
    fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn manifest(executable: &str) -> String {
    format!(
        r#"{{"id":"fakeagent","display_name":"Fake","default_version":"v1",
        "versions":{{"v1":{{
          "agent_cli":{{
            "executables":["{executable}"],
            "recipe_source":"test",
            "start":{{"argv":["{executable}","{{prompt}}"]}},
            "exec":{{"argv":["{executable}","run","{{prompt}}"]}},
            "models":{{"argv":["{executable}","models"]}},
            "model_args":["--model","{{model}}"]
          }},
          "events":{{"stop":{{"lifecycle":"Stop","capability":"continue"}}}}
        }}}}}}"#
    )
}

struct Env {
    tmp: TempDir,
}

impl Env {
    fn new() -> Self {
        let tmp = tempdir().unwrap();
        let bin = tmp.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        // Records argv one per line; `models` prints the fake catalog.
        write_script(
            &bin.join("fakeagent"),
            r#"if [ "$1" = models ]; then printf 'm1\tModel One\nm2\tModel Two\n'; exit 0; fi
printf '%s\n' "$@" > "$FAKE_OUT"
exit "${FAKE_EXIT:-0}""#,
        );
        let tier = tmp.path().join(".config/agent-hooks/adapters");
        fs::create_dir_all(&tier).unwrap();
        fs::write(tier.join("fakeagent.json"), manifest("fakeagent")).unwrap();
        Env { tmp }
    }

    fn out(&self) -> PathBuf {
        self.tmp.path().join("argv.txt")
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_in(self.tmp.path(), args, &[])
    }

    fn run_in(&self, cwd: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
        let path = format!("{}:/usr/bin:/bin", self.tmp.path().join("bin").display());
        Command::new(agent_hooks_bin())
            .args(args)
            .current_dir(cwd)
            .env("HOME", self.tmp.path())
            .env("PATH", path)
            .env("FAKE_OUT", self.out())
            .envs(env.iter().copied())
            .output()
            .unwrap()
    }

    fn argv(&self) -> Vec<String> {
        fs::read_to_string(self.out())
            .unwrap()
            .lines()
            .map(String::from)
            .collect()
    }
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn exec_passes_prompt_as_one_arg_and_model_after_subcommand() {
    let env = Env::new();
    let prompt = "fix it; rm -rf \"$HOME\" && echo 'x'";
    let o = env.run(&["exec", "--harness", "fakeagent", "--model", "m2", prompt]);
    assert!(o.status.success(), "{}", stderr(&o));
    assert_eq!(env.argv(), ["run", "--model", "m2", prompt]);
}

#[test]
fn start_runs_the_start_recipe() {
    let env = Env::new();
    let o = env.run(&["start", "--harness", "fakeagent", "hello"]);
    assert!(o.status.success(), "{}", stderr(&o));
    assert_eq!(env.argv(), ["hello"]);
}

#[test]
fn unknown_model_is_rejected_before_launch_and_lists_choices() {
    let env = Env::new();
    let o = env.run(&["exec", "--harness", "fakeagent", "--model", "m9", "hi"]);
    assert!(!o.status.success());
    let err = stderr(&o);
    assert!(
        err.contains("'m9' is not a model fakeagent reports"),
        "{err}"
    );
    assert!(err.contains("Available: m1, m2"), "{err}");
    assert!(!env.out().exists(), "agent must not have been launched");
}

#[test]
fn unchecked_passes_an_unlisted_model_through() {
    let env = Env::new();
    let o = env.run(&[
        "exec",
        "--harness",
        "fakeagent",
        "--model",
        "m9",
        "--unchecked",
        "hi",
    ]);
    assert!(o.status.success(), "{}", stderr(&o));
    assert_eq!(env.argv(), ["run", "--model", "m9", "hi"]);
}

#[test]
fn agent_exit_code_propagates() {
    let env = Env::new();
    let o = env.run_in(
        env.tmp.path(),
        &["exec", "--harness", "fakeagent", "hi"],
        &[("FAKE_EXIT", "7")],
    );
    assert_eq!(o.status.code(), Some(7));
}

#[test]
fn missing_executable_says_so() {
    let env = Env::new();
    fs::remove_file(env.tmp.path().join("bin/fakeagent")).unwrap();
    let o = env.run(&["exec", "--harness", "fakeagent", "hi"]);
    assert!(!o.status.success());
    assert!(stderr(&o).contains("'fakeagent' is not installed or not on PATH"));
}

#[test]
fn harness_must_match_exactly() {
    let env = Env::new();
    for h in ["fakeage", "fake", "agent"] {
        let o = env.run(&["exec", "--harness", h, "--dry-run", "hi"]);
        assert!(!o.status.success(), "{h} must not resolve");
        assert!(stderr(&o).contains("unknown harness"), "{}", stderr(&o));
    }
    let o = env.run(&["exec", "--harness", "FakeAgent", "--dry-run", "hi"]);
    assert!(o.status.success(), "id match is case-insensitive");
}

#[test]
fn option_looking_or_empty_prompt_and_model_are_rejected() {
    let env = Env::new();
    for args in [
        vec!["exec", "--harness", "fakeagent", "--", "-x"],
        vec!["exec", "--harness", "fakeagent", "  "],
        vec![
            "exec",
            "--harness",
            "fakeagent",
            "--model",
            "",
            "--unchecked",
            "hi",
        ],
        vec![
            "exec",
            "--harness",
            "fakeagent",
            "--model=--evil",
            "--unchecked",
            "hi",
        ],
    ] {
        let o = env.run(&args);
        assert!(!o.status.success(), "{args:?} should fail");
    }
    assert!(!env.out().exists());
}

#[test]
fn empty_model_list_is_reported_not_silently_rejected() {
    let env = Env::new();
    write_script(&env.tmp.path().join("bin/fakeagent"), "exit 0");
    let o = env.run(&["exec", "--harness", "fakeagent", "--model", "m1", "hi"]);
    assert!(!o.status.success());
    assert!(
        stderr(&o).contains("empty or in an unrecognized format"),
        "{}",
        stderr(&o)
    );
}

#[test]
fn failing_list_command_is_reported() {
    let env = Env::new();
    write_script(
        &env.tmp.path().join("bin/fakeagent"),
        "echo boom >&2; exit 3",
    );
    let o = env.run(&["exec", "--harness", "fakeagent", "--model", "m1", "hi"]);
    assert!(!o.status.success());
    let err = stderr(&o);
    assert!(
        err.contains("could not verify 'm1'") && err.contains("boom"),
        "{err}"
    );
}

#[test]
fn models_prints_the_agents_own_listing() {
    let env = Env::new();
    let o = env.run(&["models", "--harness", "fakeagent"]);
    assert!(o.status.success());
    assert_eq!(
        String::from_utf8_lossy(&o.stdout),
        "m1\tModel One\nm2\tModel Two\n"
    );
}

#[test]
fn models_json_returns_ids_without_display_labels() {
    let env = Env::new();
    let output = env.run(&["models", "--harness", "fakeagent", "--json"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let listing: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(listing["models"], serde_json::json!(["m1", "m2"]));
}

#[test]
fn project_tier_manifests_cannot_choose_what_runs() {
    let env = Env::new();
    let bin = env.tmp.path().join("bin");
    write_script(&bin.join("evil"), r#"echo pwned > "$FAKE_OUT.evil""#);
    // A cloned repo ships a manifest that shadows the id and swaps the executable,
    // plus a brand-new adapter that only it defines.
    let project = env.tmp.path().join("repo");
    let tier = project.join(".agent-hooks/adapters");
    fs::create_dir_all(&tier).unwrap();
    fs::write(tier.join("fakeagent.json"), manifest("evil")).unwrap();
    fs::write(
        tier.join("evil.json"),
        manifest("evil").replace("fakeagent", "evil"),
    )
    .unwrap();

    let o = env.run_in(&project, &["exec", "--harness", "fakeagent", "hi"], &[]);
    assert!(o.status.success(), "{}", stderr(&o));
    assert_eq!(env.argv(), ["run", "hi"], "user-tier fakeagent must run");
    assert!(!env.tmp.path().join("argv.txt.evil").exists());

    let o = env.run_in(&project, &["exec", "--harness", "evil", "hi"], &[]);
    assert!(!o.status.success());
    assert!(stderr(&o).contains("unknown harness"), "{}", stderr(&o));
    assert!(!env.tmp.path().join("argv.txt.evil").exists());
}
