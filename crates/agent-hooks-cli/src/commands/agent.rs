use crate::cli::{AgentLaunchArgs, ModelsArgs};
use agent_hooks_adapters::manifest::{AdapterManifest, AgentCliSpec, ModelsCommandSpec};
use agent_hooks_adapters::traits::normalize_hint;
use std::process::Command;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Start,
    Exec,
}

/// Exact id/alias match only: launching a loosely matched agent is worse than failing.
fn agent_cli(harness: &str) -> anyhow::Result<(String, AgentCliSpec)> {
    let manifests = AdapterManifest::load_tiered_from(&AdapterManifest::trusted_tier_dirs());
    let norm = normalize_hint(harness);
    let Some(manifest) = manifests.iter().find(|m| {
        normalize_hint(&m.id) == norm || m.aliases.iter().any(|a| normalize_hint(a) == norm)
    }) else {
        let mut known: Vec<&str> = manifests
            .iter()
            .filter(|m| {
                m.versions
                    .get(&m.default_version)
                    .is_some_and(|v| v.agent_cli.is_some())
            })
            .map(|m| m.id.as_str())
            .collect();
        known.sort();
        anyhow::bail!(
            "unknown harness '{harness}'. Launchable: {}",
            known.join(", ")
        );
    };
    let manifest = manifest.clone();
    let spec = manifest
        .versions
        .get(&manifest.default_version)
        .and_then(|vs| vs.agent_cli.clone())
        .ok_or_else(|| anyhow::anyhow!("no agent_cli metadata recorded for '{}'", manifest.id))?;
    Ok((manifest.id, spec))
}

/// Substitute `{prompt}` and splice launch options after the executable and
/// any leading subcommand (`codex exec`, `opencode run`), since subcommands own
/// their flags.
fn build_argv(
    id: &str,
    spec: &AgentCliSpec,
    mode: Mode,
    prompt: &str,
    model: Option<&str>,
    yolo: bool,
) -> anyhow::Result<Vec<String>> {
    let (label, recipe) = match mode {
        Mode::Start => ("start", &spec.start),
        Mode::Exec => ("exec", &spec.exec),
    };
    let recipe = recipe
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("no {label} recipe recorded for '{id}'"))?;
    let mut argv: Vec<String> = recipe
        .argv
        .iter()
        .map(|arg| arg.replace("{prompt}", prompt))
        .collect();
    let mut launch_args = Vec::new();
    if let Some(model) = model {
        if recipe.model_unsupported {
            anyhow::bail!(
                "{id} {label} has no model option; use exec, or set the model in {id}'s own config"
            );
        }
        if spec.model_args.is_empty() {
            anyhow::bail!("no model_args recorded for '{id}'; cannot pass --model");
        }
        launch_args.extend(spec.model_args.iter().map(|a| a.replace("{model}", model)));
    }
    if yolo {
        let mode_args = match mode {
            Mode::Start if !spec.start_yolo_args.is_empty() => &spec.start_yolo_args,
            Mode::Exec if !spec.exec_yolo_args.is_empty() => &spec.exec_yolo_args,
            _ => &spec.yolo_args,
        };
        if mode_args.is_empty() {
            anyhow::bail!("no YOLO launch arguments recorded for '{id} {label}'");
        }
        launch_args.extend(mode_args.iter().cloned());
    }
    if !launch_args.is_empty() {
        let at = (1..argv.len())
            .find(|&i| argv[i].starts_with('-') || recipe.argv[i].contains("{prompt}"))
            .unwrap_or(argv.len());
        argv.splice(at..at, launch_args);
    }
    Ok(argv)
}

fn spawn_error(program: &str, e: std::io::Error) -> anyhow::Error {
    match e.kind() {
        std::io::ErrorKind::NotFound => {
            anyhow::anyhow!("'{program}' is not installed or not on PATH")
        }
        _ => anyhow::anyhow!("failed to run '{program}': {e}"),
    }
}

/// On Unix the agent replaces this process, so the terminal, signals and exit
/// code are the agent's own. Elsewhere, spawn and propagate the exit code.
fn launch_argv(argv: &[String]) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let e = Command::new(&argv[0]).args(&argv[1..]).exec();
        Err(spawn_error(&argv[0], e))
    }
    #[cfg(not(unix))]
    run_argv(argv)
}

fn run_argv(argv: &[String]) -> anyhow::Result<()> {
    let status = Command::new(&argv[0])
        .args(&argv[1..])
        .status()
        .map_err(|e| spawn_error(&argv[0], e))?;
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}

pub fn launch(mode: Mode, args: AgentLaunchArgs) -> anyhow::Result<()> {
    let (id, spec) = agent_cli(&args.harness)?;
    if args.prompt.trim().is_empty() {
        anyhow::bail!("prompt is empty");
    }
    // A leading '-' would reach the agent as an option, not as the prompt.
    if args.prompt.starts_with('-') {
        anyhow::bail!("prompt must not start with '-' (the agent would parse it as an option)");
    }
    if args
        .model
        .as_deref()
        .is_some_and(|m| m.is_empty() || m.starts_with('-'))
    {
        anyhow::bail!("model must be a model name, not an option");
    }
    if let Some(model) = args.model.as_deref().filter(|_| !args.unchecked) {
        check_model(&id, &spec, model)?;
    }
    let mut argv = build_argv(
        &id,
        &spec,
        mode,
        &args.prompt,
        args.model.as_deref(),
        args.yolo,
    )?;
    argv.extend(args.native_args);
    if args.dry_run {
        println!("{}", serde_json::to_string(&argv)?);
        return Ok(());
    }
    launch_argv(&argv)
}

/// Ids the agent's own CLI reports (or its fixed aliases); `None` when the
/// harness has no way to say.
fn available_models(spec: &AgentCliSpec) -> anyhow::Result<Option<Vec<String>>> {
    if let Some(cmd) = &spec.models {
        let out = Command::new(&cmd.argv[0])
            .args(&cmd.argv[1..])
            .stdin(std::process::Stdio::null())
            .output()
            .map_err(|e| anyhow::anyhow!("failed to run '{}': {e}", cmd.argv.join(" ")))?;
        if !out.status.success() {
            anyhow::bail!(
                "'{}' failed ({}): {}",
                cmd.argv.join(" "),
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        return Ok(Some(parse_model_ids(
            &String::from_utf8_lossy(&out.stdout),
            cmd,
        )));
    }
    if !spec.model_aliases.is_empty() {
        return Ok(Some(spec.model_aliases.clone()));
    }
    Ok(None)
}

fn parse_model_ids(stdout: &str, cmd: &ModelsCommandSpec) -> Vec<String> {
    fn collect(v: &serde_json::Value, fields: &[String], out: &mut Vec<String>) {
        match v {
            serde_json::Value::Object(map) => {
                for (k, v) in map {
                    match v.as_str() {
                        Some(id) if fields.contains(k) => out.push(id.to_string()),
                        _ => collect(v, fields, out),
                    }
                }
            }
            serde_json::Value::Array(items) => items.iter().for_each(|i| collect(i, fields, out)),
            _ => {}
        }
    }
    if !cmd.id_fields.is_empty() {
        let mut out = Vec::new();
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(stdout) {
            collect(&v, &cmd.id_fields, &mut out);
        }
        return out;
    }
    let col = cmd.id_column.unwrap_or(0);
    let mut out = Vec::new();
    let rows = stdout
        .lines()
        .skip(cmd.skip_lines)
        .map(|l| l.split_whitespace().collect::<Vec<_>>());
    for cols in rows {
        let Some(id) = cols.get(col) else { continue };
        match cmd.provider_column.map(|c| cols.get(c)) {
            Some(Some(provider)) => out.push(format!("{provider}/{id}")),
            Some(None) => {}
            None => out.push(id.to_string()),
        }
    }
    out
}

fn summarize(ids: &[String]) -> String {
    const SHOWN: usize = 15;
    match ids.len().checked_sub(SHOWN) {
        Some(rest) if rest > 0 => format!(
            "{}, ... {rest} more (see `agent-hooks models`)",
            ids[..SHOWN].join(", ")
        ),
        _ => ids.join(", "),
    }
}

fn check_model(id: &str, spec: &AgentCliSpec, model: &str) -> anyhow::Result<()> {
    match available_models(spec) {
        Ok(Some(ids)) if ids.is_empty() => anyhow::bail!(
            "{id}'s model list was empty or in an unrecognized format, so '{model}' cannot be verified (use --unchecked to pass it through anyway)"
        ),
        Ok(Some(ids)) if ids.iter().any(|m| m == model) => Ok(()),
        Ok(Some(ids)) => anyhow::bail!(
            "'{model}' is not a model {id} reports. Available: {}\n(use --unchecked to pass it through anyway)",
            summarize(&ids)
        ),
        Ok(None) => anyhow::bail!(
            "{id} cannot list its models, so '{model}' cannot be verified (use --unchecked to pass it through)"
        ),
        Err(e) => anyhow::bail!(
            "could not verify '{model}' for {id}: {e}\n(use --unchecked to pass it through anyway)"
        ),
    }
}

pub fn models(args: ModelsArgs) -> anyhow::Result<()> {
    let (id, spec) = agent_cli(&args.harness)?;
    if args.json {
        let models = available_models(&spec)?.ok_or_else(|| {
            anyhow::anyhow!(
                "'{id}' has no model-listing command; pass --model with a name from its docs"
            )
        })?;
        println!(
            "{}",
            serde_json::to_string(&serde_json::json!({"harness": id, "models": models}))?
        );
        return Ok(());
    }
    if spec.models.is_none() && !spec.model_aliases.is_empty() {
        println!("{}", spec.model_aliases.join("\n"));
        return Ok(());
    }
    let models = spec.models.ok_or_else(|| {
        anyhow::anyhow!(
            "'{id}' has no model-listing command; pass --model with a name from its docs"
        )
    })?;
    run_argv(&models.argv)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_hooks_adapters::manifest::{AgentCliSpec, PromptCommandSpec};

    fn spec() -> AgentCliSpec {
        AgentCliSpec {
            executables: vec!["x".into()],
            documentation: vec![],
            recipe_source: "t".into(),
            resume: None,
            start: Some(PromptCommandSpec {
                argv: vec!["x".into(), "{prompt}".into()],
                model_unsupported: false,
            }),
            exec: None,
            models: None,
            model_aliases: vec![],
            model_args: vec!["--model={model}".into()],
            yolo_args: vec![],
            start_yolo_args: vec![],
            exec_yolo_args: vec![],
        }
    }

    #[test]
    fn splices_model_after_executable_and_keeps_prompt_one_arg() {
        let argv = build_argv("x", &spec(), Mode::Start, "a b; rm", Some("m1"), false).unwrap();
        assert_eq!(argv, ["x", "--model=m1", "a b; rm"]);
    }

    #[test]
    fn model_args_follow_leading_subcommand_only() {
        let mut s = spec();
        s.exec = Some(PromptCommandSpec {
            argv: vec!["x".into(), "run".into(), "{prompt}".into()],
            model_unsupported: false,
        });
        let argv = build_argv("x", &s, Mode::Exec, "p", Some("m1"), false).unwrap();
        assert_eq!(argv, ["x", "run", "--model=m1", "p"]);
    }

    #[test]
    fn yolo_args_can_be_exec_only() {
        let mut s = spec();
        s.exec = Some(PromptCommandSpec {
            argv: vec!["x".into(), "exec".into(), "{prompt}".into()],
            model_unsupported: false,
        });
        s.exec_yolo_args = vec!["--skip-permissions-unsafe".into()];

        assert!(
            build_argv("x", &s, Mode::Start, "p", None, true)
                .unwrap_err()
                .to_string()
                .contains("no YOLO launch arguments recorded for 'x start'")
        );
        let argv = build_argv("x", &s, Mode::Exec, "p", None, true).unwrap();
        assert_eq!(argv, ["x", "exec", "--skip-permissions-unsafe", "p"]);
    }

    fn cmd(fields: &[&str], col: Option<usize>) -> ModelsCommandSpec {
        ModelsCommandSpec {
            argv: vec!["x".into()],
            id_fields: fields.iter().map(|f| f.to_string()).collect(),
            id_column: col,
            provider_column: None,
            skip_lines: 0,
        }
    }

    #[test]
    fn parses_ids_from_json_and_columns() {
        let json = r#"{"models":[{"slug":"a","description":"high"},{"slug":"b"}]}"#;
        assert_eq!(parse_model_ids(json, &cmd(&["slug"], None)), ["a", "b"]);
        let table = "provider model\nanthropic opus\nopenai gpt";
        assert_eq!(
            parse_model_ids(table, &cmd(&[], Some(1))),
            ["model", "opus", "gpt"]
        );
        assert_eq!(
            parse_model_ids("m1\tName One\nm2\tTwo", &cmd(&[], None)),
            ["m1", "m2"]
        );
    }

    /// Trimmed captures of each CLI's real listing output; a format change
    /// upstream should fail here, not at a user's prompt.
    #[test]
    fn parses_captured_listing_formats() {
        let codex = r#"{"models":[{"slug":"gpt-6.1-sol","display_name":"GPT-6.1-Sol","description":"Latest","supported_reasoning_levels":[{"effort":"low","description":"Fast"}]},{"slug":"gpt-5.6-luna","display_name":"x"}]}"#;
        assert_eq!(
            parse_model_ids(codex, &cmd(&["slug"], None)),
            ["gpt-6.1-sol", "gpt-5.6-luna"]
        );
        let omp = r#"{"models":[{"provider":"anthropic","kind":"chat","id":"claude-haiku-4-5","selector":"anthropic/claude-haiku-4-5","name":"Haiku"}]}"#;
        assert_eq!(
            parse_model_ids(omp, &cmd(&["id", "selector"], None)),
            ["claude-haiku-4-5", "anthropic/claude-haiku-4-5"]
        );
        let agy = "gemini-4-argon-medium\tGemini 4 Argon (Medium)\ngemini-3.8-flash-low\tGemini 3.8 Flash (Low)\n";
        assert_eq!(
            parse_model_ids(agy, &cmd(&[], None)),
            ["gemini-4-argon-medium", "gemini-3.8-flash-low"]
        );
        let opencode = "alibaba/glm-5.2\nalibaba/qwen-flash\n";
        assert_eq!(
            parse_model_ids(opencode, &cmd(&[], None)),
            ["alibaba/glm-5.2", "alibaba/qwen-flash"]
        );
        let pi = "provider   model             context\nanthropic  claude-haiku-4-5  200K\n";
        assert_eq!(
            parse_model_ids(pi, &cmd(&[], Some(1))),
            ["model", "claude-haiku-4-5"]
        );
        let mut with_provider = cmd(&[], Some(1));
        with_provider.provider_column = Some(0);
        with_provider.skip_lines = 1;
        assert_eq!(
            parse_model_ids(pi, &with_provider),
            ["anthropic/claude-haiku-4-5"]
        );
        assert!(parse_model_ids("not json", &cmd(&["slug"], None)).is_empty());
    }

    #[test]
    fn rejects_unknown_model_and_unlistable_agents() {
        let mut s = spec();
        s.model_aliases = vec!["opus".into()];
        assert!(check_model("x", &s, "opus").is_ok());
        let err = check_model("x", &s, "nope").unwrap_err().to_string();
        assert!(err.contains("Available: opus"), "{err}");
        s.model_aliases.clear();
        assert!(check_model("x", &s, "opus").is_err());
    }

    #[test]
    fn modes_without_a_model_option_refuse_one() {
        let mut s = spec();
        s.start.as_mut().unwrap().model_unsupported = true;
        let err = build_argv("x", &s, Mode::Start, "p", Some("m"), false).unwrap_err();
        assert!(err.to_string().contains("no model option"), "{err}");
        assert!(build_argv("x", &s, Mode::Start, "p", None, false).is_ok());
    }

    #[test]
    fn missing_recipe_or_model_args_errors() {
        assert!(build_argv("x", &spec(), Mode::Exec, "p", None, false).is_err());
        let mut s = spec();
        s.model_args.clear();
        assert!(build_argv("x", &s, Mode::Start, "p", Some("m"), false).is_err());
    }
}
