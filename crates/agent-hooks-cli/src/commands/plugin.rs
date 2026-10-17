//! Plugin management CLI commands: list, install, uninstall, enable, disable, validate.

use std::fs;
use std::path::Path;

use agent_hooks_core::plugin::PluginManager;
use anyhow::{bail, Context, Result};
use serde_json::json;

use crate::cli::{
    PluginArgs, PluginCommand, PluginDisableArgs, PluginEnableArgs, PluginInstallArgs,
    PluginListArgs, PluginUninstallArgs, PluginValidateArgs,
};
use crate::commands::user_hooks::{
    global_plugins_dir, project_config_path, project_plugins_dir, user_config_path,
    KNOWN_LIFECYCLES,
};

pub fn run(args: PluginArgs) -> Result<()> {
    match args.command {
        PluginCommand::List(args) => list(args),
        PluginCommand::Install(args) => install(args),
        PluginCommand::Uninstall(args) => uninstall(args),
        PluginCommand::Enable(args) => enable(args),
        PluginCommand::Disable(args) => disable(args),
        PluginCommand::Validate(args) => validate(args),
    }
}

fn load_disabled_plugins() -> (Vec<String>, Vec<String>) {
    let mut user_dis = Vec::new();
    let mut proj_dis = Vec::new();

    if let Some(user_path) = user_config_path() {
        if let Ok(content) = fs::read_to_string(&user_path) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&content) {
                if let Some(arr) = v.get("disabled_plugins").and_then(|a| a.as_array()) {
                    user_dis.extend(arr.iter().filter_map(|x| x.as_str().map(String::from)));
                }
            }
        }
    }

    if let Some(proj_path) = project_config_path() {
        if let Ok(content) = fs::read_to_string(&proj_path) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&content) {
                if let Some(arr) = v.get("disabled_plugins").and_then(|a| a.as_array()) {
                    proj_dis.extend(arr.iter().filter_map(|x| x.as_str().map(String::from)));
                }
            }
        }
    }

    (user_dis, proj_dis)
}

fn list(args: PluginListArgs) -> Result<()> {
    let global_dir = global_plugins_dir();
    let project_dir = project_plugins_dir();

    let (user_disabled, proj_disabled) = load_disabled_plugins();
    let all_disabled: Vec<String> = user_disabled.into_iter().chain(proj_disabled).collect();

    let mut mgr = PluginManager::new();
    mgr.set_disabled(all_disabled);

    if !args.project {
        if let Some(dir) = &global_dir {
            mgr.load_from_plugins_dir(dir, false);
        }
    }
    if !args.global {
        if let Some(dir) = &project_dir {
            mgr.load_from_plugins_dir(dir, true);
        }
    }

    let plugins = mgr.list();

    if args.json {
        let items: Vec<serde_json::Value> = plugins
            .iter()
            .map(|p| {
                json!({
                    "id": p.id,
                    "version": p.manifest.version,
                    "description": p.manifest.description,
                    "enabled": p.enabled,
                    "scope": if p.is_project_local { "project" } else { "global" },
                    "path": p.root_dir,
                    "hooks": p.manifest.hooks,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&items)?);
        return Ok(());
    }

    if plugins.is_empty() {
        println!("No plugins installed.");
        println!("\nInstall a plugin with: agent-hooks plugin install <path>");
        return Ok(());
    }

    println!("{:<24} {:<10} {:<10} {:<10} {}", "NAME", "VERSION", "SCOPE", "STATUS", "DESCRIPTION");
    println!("{:-<24} {:-<10} {:-<10} {:-<10} {:-<30}", "", "", "", "", "");

    for p in plugins {
        let scope = if p.is_project_local { "project" } else { "global" };
        let status = if p.enabled { "enabled" } else { "disabled" };
        let desc = p.manifest.description.as_deref().unwrap_or("-");
        println!(
            "{:<24} {:<10} {:<10} {:<10} {}",
            p.id, p.manifest.version, scope, status, desc
        );

        if args.verbose {
            println!("  Path: {}", p.root_dir.display());
            if !p.manifest.hooks.is_empty() {
                println!("  Hooks:");
                for (event, entries) in &p.manifest.hooks {
                    for entry in entries {
                        println!("    - {}: {}", event, entry.command);
                    }
                }
            }
            println!();
        }
    }

    Ok(())
}

fn install(args: PluginInstallArgs) -> Result<()> {
    let src = fs::canonicalize(&args.source)
        .with_context(|| format!("Plugin source path '{}' not found", args.source.display()))?;

    let manifest = PluginManager::read_manifest(&src)
        .with_context(|| format!("No valid plugin manifest found in '{}'", src.display()))?;

    let dest_dir = if args.project {
        project_plugins_dir().context("Cannot determine project plugins directory")?
    } else {
        global_plugins_dir().context("Cannot determine global plugins directory")?
    };

    let target = dest_dir.join(&manifest.name);
    if (target.exists() || fs::symlink_metadata(&target).is_ok()) && !args.force {
        bail!(
            "Plugin '{}' already exists at {}. Pass --force to overwrite.",
            manifest.name,
            target.display()
        );
    }

    let installed = PluginManager::install_from_dir(&src, &dest_dir, args.link)
        .map_err(|e| anyhow::anyhow!(e))?;

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "id": installed.id,
                "version": installed.manifest.version,
                "path": installed.root_dir,
                "linked": args.link,
                "scope": if args.project { "project" } else { "global" },
            }))?
        );
    } else {
        println!(
            "{} plugin '{}' v{} -> {}",
            if args.link { "Linked" } else { "Installed" },
            installed.id,
            installed.manifest.version,
            installed.root_dir.display()
        );
    }

    Ok(())
}

fn uninstall(args: PluginUninstallArgs) -> Result<()> {
    let dest_dir = if args.project {
        project_plugins_dir().context("Cannot determine project plugins directory")?
    } else {
        global_plugins_dir().context("Cannot determine global plugins directory")?
    };

    let removed = PluginManager::uninstall(&args.name, &dest_dir).map_err(|e| anyhow::anyhow!(e))?;

    if !removed {
        bail!("Plugin '{}' not found in {}", args.name, dest_dir.display());
    }

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "id": args.name,
                "uninstalled": true,
                "scope": if args.project { "project" } else { "global" },
            }))?
        );
    } else {
        println!(
            "Uninstalled plugin '{}' from {}",
            args.name,
            dest_dir.display()
        );
    }

    Ok(())
}

fn mutate_disabled_in_config(config_path: &Path, name: &str, disable: bool) -> Result<()> {
    if let Some(parent) = config_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut doc: serde_json::Value = if config_path.exists() {
        let content = fs::read_to_string(config_path)?;
        serde_json::from_str(&content).unwrap_or_else(|_| json!({ "version": 1, "hooks": {} }))
    } else {
        json!({ "version": 1, "hooks": {} })
    };

    let mut disabled_list: Vec<String> = doc
        .get("disabled_plugins")
        .and_then(|a| a.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    if disable {
        if !disabled_list.iter().any(|d| d == name) {
            disabled_list.push(name.to_string());
        }
    } else {
        disabled_list.retain(|d| d != name);
    }

    if let Some(obj) = doc.as_object_mut() {
        if disabled_list.is_empty() {
            obj.remove("disabled_plugins");
        } else {
            obj.insert(
                "disabled_plugins".to_string(),
                serde_json::to_value(disabled_list)?,
            );
        }
    }

    fs::write(config_path, serde_json::to_string_pretty(&doc)? + "\n")?;
    Ok(())
}

fn enable(args: PluginEnableArgs) -> Result<()> {
    let config_path = if args.project {
        project_config_path().context("Cannot determine project config path")?
    } else {
        user_config_path().context("Cannot determine user config path")?
    };

    mutate_disabled_in_config(&config_path, &args.name, false)?;
    println!("Enabled plugin '{}' (updated {})", args.name, config_path.display());
    Ok(())
}

fn disable(args: PluginDisableArgs) -> Result<()> {
    let config_path = if args.project {
        project_config_path().context("Cannot determine project config path")?
    } else {
        user_config_path().context("Cannot determine user config path")?
    };

    mutate_disabled_in_config(&config_path, &args.name, true)?;
    println!("Disabled plugin '{}' (updated {})", args.name, config_path.display());
    Ok(())
}

fn validate(args: PluginValidateArgs) -> Result<()> {
    let path = fs::canonicalize(&args.path)
        .with_context(|| format!("Directory '{}' does not exist", args.path.display()))?;

    let mut checks: Vec<(String, bool, String)> = Vec::new();

    let manifest = match PluginManager::read_manifest(&path) {
        Some(m) => {
            checks.push(("manifest".into(), true, format!("found valid manifest for '{}'", m.name)));
            m
        }
        None => {
            checks.push(("manifest".into(), false, "no plugin.json, .claude-plugin/plugin.json, or package.json found".into()));
            return render_validation_results(&checks, args.json);
        }
    };

    // 1. Name check
    if manifest.name.trim().is_empty() {
        checks.push(("name".into(), false, "plugin name cannot be empty".into()));
    } else if manifest.name.contains('/') || manifest.name.contains('\\') || manifest.name.contains("..") {
        checks.push(("name".into(), false, "plugin name contains illegal path traversal characters".into()));
    } else {
        checks.push(("name".into(), true, format!("name '{}' is valid", manifest.name)));
    }

    // 2. Version check
    if manifest.version.trim().is_empty() {
        checks.push(("version".into(), false, "version cannot be empty".into()));
    } else {
        checks.push(("version".into(), true, format!("version '{}'", manifest.version)));
    }

    // 3. Hooks checks
    if manifest.hooks.is_empty() {
        checks.push(("hooks".into(), true, "no hooks declared (manifest is valid but idle)".into()));
    } else {
        for (event, entries) in &manifest.hooks {
            let canon = KNOWN_LIFECYCLES.iter().find(|k| k.eq_ignore_ascii_case(event));
            match canon {
                Some(c) => {
                    checks.push((
                        format!("hook:{}", event),
                        true,
                        format!("event matches known lifecycle '{}'", c),
                    ));
                }
                None => {
                    checks.push((
                        format!("hook:{}", event),
                        false,
                        format!(
                            "unknown lifecycle event '{}' (expected one of: {})",
                            event,
                            KNOWN_LIFECYCLES.join(", ")
                        ),
                    ));
                }
            }

            for entry in entries {
                if entry.command.trim().is_empty() {
                    checks.push((
                        format!("entry:{}", event),
                        false,
                        "hook entry command cannot be empty".into(),
                    ));
                } else {
                    let first_token = entry.command.split_whitespace().next().unwrap_or("");
                    let cand = path.join(first_token);
                    if cand.exists() {
                        checks.push((
                            format!("entrypoint:{}", first_token),
                            true,
                            format!("executable script found at {}", cand.display()),
                        ));
                    }
                }
            }
        }
    }

    render_validation_results(&checks, args.json)
}

fn render_validation_results(checks: &[(String, bool, String)], json_out: bool) -> Result<()> {
    let all_ok = checks.iter().all(|(_, ok, _)| *ok);

    if json_out {
        let items: Vec<serde_json::Value> = checks
            .iter()
            .map(|(name, ok, msg)| {
                json!({
                    "check": name,
                    "passed": ok,
                    "message": msg,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "valid": all_ok,
                "checks": items,
            }))?
        );
    } else {
        for (name, ok, msg) in checks {
            if *ok {
                println!("  ok: [{}] {}", name, msg);
            } else {
                eprintln!("  error: [{}] {}", name, msg);
            }
        }
        if all_ok {
            println!("\nPlugin validation passed.");
        } else {
            bail!("Plugin validation failed.");
        }
    }

    Ok(())
}
