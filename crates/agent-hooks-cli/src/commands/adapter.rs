use super::user_hooks::KNOWN_LIFECYCLES;
use crate::cli::{AdapterAddArgs, AdapterArgs, AdapterCommands, AdapterListArgs, AdapterNewArgs};
use agent_hooks_adapters::manifest::{AdapterManifest, Capability, InstallOp};
use agent_hooks_adapters::{DeclarativeAdapter, HarnessAdapter};
use agent_hooks_protocol::{EventContext, PromptDecision, RawHookInput};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub fn run(args: AdapterArgs) -> anyhow::Result<()> {
    match args.command {
        AdapterCommands::New(a) => cmd_new(a),
        AdapterCommands::Validate(a) => cmd_validate(&a.target),
        AdapterCommands::Test(a) => cmd_test(&a.target),
        AdapterCommands::Add(a) => cmd_add(a),
        AdapterCommands::List(a) => cmd_list(a),
    }
}

// ---------------------------------------------------------------------------
// discovery tiers (design doc §5): project > user > embedded
// ---------------------------------------------------------------------------

fn home_dir() -> Option<PathBuf> {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()
        .map(PathBuf::from)
}

fn user_adapters_dir() -> Option<PathBuf> {
    home_dir().map(|h| h.join(".config/agent-hooks/adapters"))
}

/// Scan a tier directory. Both layouts are accepted, path-sorted:
/// - flat: `<id>.json` (`.meta.json` sidecars skipped; `adapter add` form)
/// - folder: `<id>/adapter.json` or `<id>/<id>.json`, with the adapter's
///   `fixtures/` sibling (`adapter new` form)
fn scan_tier_dir(dir: &Path) -> Vec<(PathBuf, AdapterManifest)> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return out,
    };
    let mut paths: Vec<PathBuf> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
    paths.sort();
    for path in paths {
        let candidates: Vec<PathBuf> = if path.is_dir() {
            let adapter_json = path.join("adapter.json");
            if adapter_json.is_file() {
                vec![adapter_json]
            } else if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
                let named = path.join(format!("{name}.json"));
                if named.is_file() {
                    vec![named]
                } else {
                    Vec::new()
                }
            } else {
                Vec::new()
            }
        } else {
            let skip_meta = path
                .file_stem()
                .and_then(|s| s.to_str())
                .map(|s| s.ends_with(".meta"))
                .unwrap_or(false);
            let is_json = path.extension().and_then(|e| e.to_str()) == Some("json");
            if is_json && !skip_meta {
                vec![path]
            } else {
                Vec::new()
            }
        };
        for manifest_path in candidates {
            if let Ok(text) = std::fs::read_to_string(&manifest_path) {
                if let Ok(manifest) = AdapterManifest::parse(&text) {
                    out.push((manifest_path, manifest));
                }
            }
        }
    }
    out
}

/// Resolve `target` (file path or adapter id) to a manifest.
/// Returns the manifest plus the on-disk manifest path when the target was a file.
fn load_target(target: &str) -> anyhow::Result<(AdapterManifest, Option<PathBuf>)> {
    let path = Path::new(target);
    if path.is_file() {
        let text = std::fs::read_to_string(path)?;
        let manifest = AdapterManifest::parse(&text)?;
        return Ok((manifest, Some(path.to_path_buf())));
    }
    // By id/alias across all tiers — project > user > embedded. Tier hits
    // keep their on-disk path so `adapter test` can find the sibling
    // `fixtures/` dir.
    let norm = target.to_ascii_lowercase().replace(['_', ' '], "-");
    let id_matches = |manifest: &AdapterManifest| {
        manifest.id == *target
            || manifest.id.to_ascii_lowercase() == norm
            || manifest
                .aliases
                .iter()
                .any(|a| *a == *target || a.to_ascii_lowercase() == norm)
    };
    for dir in AdapterManifest::tier_dirs() {
        for (path, manifest) in scan_tier_dir(&dir) {
            if id_matches(&manifest) {
                return Ok((manifest, Some(path)));
            }
        }
    }
    for manifest in AdapterManifest::load_embedded() {
        if id_matches(&manifest) {
            return Ok((manifest, None));
        }
    }
    anyhow::bail!(
        "unknown adapter '{}': not a file path or a known builtin/user/project adapter id",
        target
    )
}

// ---------------------------------------------------------------------------
// `adapter new`
// ---------------------------------------------------------------------------

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

fn scaffold_template(id: &str) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "display_name": id,
        "aliases": [],
        "default_version": "v1",
        "detect": {
            "harness_hint": [id],
            "payload_keys_any": [],
            "event_names": ["preToolUse", "postToolUse", "stop"]
        },
        "versions": {
            "v1": {
                "install": [
                    {
                        "op": "json_hooks",
                        "paths": [format!("~/.{id}/hooks.json"), format!(".{id}/hooks.json")],
                        "create": true,
                        "root": "hooks.{event}",
                        "entry_shape": "flat",
                        "entry": {"command": format!("agent-hooks hook {id} --event {{event}}")},
                        "dedupe": "command_subset",
                        "entry_overrides": {}
                    }
                ],
                "events": {
                    "preToolUse": {"lifecycle": "PreToolUse", "capability": "gate"},
                    "postToolUse": {"lifecycle": "PostToolUse", "capability": "observe"},
                    "stop": {"lifecycle": "Stop", "capability": "continue"}
                },
                "extraction": {
                    "session_id": ["session_id", "conversation_id", "sessionId", "conversationId"],
                    "prompt": ["prompt", "user_prompt", "message", "input"],
                    "tool_name": ["tool_name", "toolCall.name"],
                    "tool_input": ["tool_input", "toolCall.args", "args"],
                    "tool_output": ["tool_output", "tool_response", "result_json", "output"],
                    "error": ["error_message", "error", "failure_type"],
                    "success": ["success"],
                    "summary": ["reason", "summary", "message"]
                },
                "response": {
                    "mode": "json",
                    "gate": {
                        "allow": {"decision": "allow"},
                        "deny": {"decision": "deny", "reason": "{reason}"}
                    },
                    "observe": {},
                    "continue": {
                        "continue": {"decision": "continue", "reason": "{reason}"},
                        "stop": {}
                    }
                },
                "unavailable": ["PermissionRequest", "Notification", "TurnStart", "TurnEnd"]
            }
        }
    })
}

fn cmd_new(args: AdapterNewArgs) -> anyhow::Result<()> {
    if !valid_id(&args.id) {
        anyhow::bail!(
            "invalid adapter id '{}': use lowercase letters, digits, '-' or '_'",
            args.id
        );
    }
    let tier = user_adapters_dir()
        .ok_or_else(|| anyhow::anyhow!("cannot determine home directory for user adapter tier"))?;
    // Per-adapter folder: manifest + its fixtures travel together, so two
    // custom adapters in the same tier never collide.
    let dir = tier.join(&args.id);
    std::fs::create_dir_all(dir.join("fixtures"))?;
    let dest = dir.join("adapter.json");
    if dest.exists() {
        anyhow::bail!("{} already exists — refusing to overwrite", dest.display());
    }
    let template = scaffold_template(&args.id);
    // Template must itself validate (fail fast on schema drift).
    let text = serde_json::to_string_pretty(&template)?;
    let manifest = AdapterManifest::parse(&text)?;
    let failures = validate_manifest(&manifest);
    let errors: Vec<&String> = failures
        .iter()
        .filter_map(|(_, r)| r.as_ref().err())
        .collect();
    if !errors.is_empty() {
        anyhow::bail!("generated template failed validation: {}", errors[0]);
    }
    std::fs::write(&dest, format!("{}\n", text))?;
    println!("wrote {}", dest.display());
    println!("next steps:");
    println!(
        "  1. capture real payloads: agent-hooks hook --capture-fixtures {} --harness {} ...",
        dir.join("fixtures").display(),
        args.id
    );
    println!("  2. annotate fixtures in {}/fixtures/", dir.display());
    println!("  3. agent-hooks adapter validate {}", dest.display());
    println!("  4. agent-hooks adapter test {}", dest.display());
    Ok(())
}

// ---------------------------------------------------------------------------
// validation (shared by `validate` and `add`)
// ---------------------------------------------------------------------------

/// One named check plus its outcome. `run` prints `ok: <name>` /
/// `error: <name>: <reason>` per entry and fails when any is `Err`.
fn validate_manifest(manifest: &AdapterManifest) -> Vec<(String, Result<(), String>)> {
    let mut checks: Vec<(String, Result<(), String>)> = Vec::new();

    // default_version resolves
    checks.push((
        "default_version resolves".to_string(),
        if manifest.versions.contains_key(&manifest.default_version) {
            Ok(())
        } else {
            Err(format!(
                "default_version '{}' not in versions map",
                manifest.default_version
            ))
        },
    ));

    let Some(vs) = manifest.versions.get(&manifest.default_version) else {
        return checks;
    };

    // every events value maps to a known lifecycle
    for (host_event, spec) in &vs.events {
        let lifecycle = spec.lifecycle.as_str();
        checks.push((
            format!("events[{host_event}].lifecycle is known"),
            if KNOWN_LIFECYCLES.contains(&lifecycle) {
                Ok(())
            } else {
                Err(format!("unknown lifecycle '{lifecycle}'"))
            },
        ));
    }

    // install paths expand; install ops render parseable output
    for (i, op) in vs.install.iter().enumerate() {
        match op {
            InstallOp::JsonHooks {
                paths,
                root,
                entry,
                entry_overrides,
                ..
            } => {
                for path in paths {
                    checks.push((
                        format!("install[{i}].json_hooks path expands: {path}"),
                        if path.trim().is_empty() {
                            Err("empty path".to_string())
                        } else {
                            Ok(())
                        },
                    ));
                }
                checks.push((
                    format!("install[{i}].json_hooks root interpolates event: {root}"),
                    if root.contains("{event}") {
                        Ok(())
                    } else {
                        Err("root must contain {event}".to_string())
                    },
                ));
                // Dry-run: render one patched JSON doc per host event, prove it serializes.
                match render_json_hooks_sample(vs.events.keys(), root, entry, entry_overrides) {
                    Ok(doc) => match serde_json::to_string_pretty(&doc) {
                        Ok(s) => match serde_json::from_str::<serde_json::Value>(&s) {
                            Ok(_) => checks.push((
                                format!("install[{i}].json_hooks renders parseable JSON"),
                                Ok(()),
                            )),
                            Err(e) => checks.push((
                                format!("install[{i}].json_hooks renders parseable JSON"),
                                Err(e.to_string()),
                            )),
                        },
                        Err(e) => checks.push((
                            format!("install[{i}].json_hooks renders parseable JSON"),
                            Err(e.to_string()),
                        )),
                    },
                    Err(e) => checks.push((
                        format!("install[{i}].json_hooks renders parseable JSON"),
                        Err(e),
                    )),
                }
            }
            InstallOp::TomlHooks {
                paths, root, entry, ..
            } => {
                for path in paths {
                    checks.push((
                        format!("install[{i}].toml_hooks path expands: {path}"),
                        if path.trim().is_empty() {
                            Err("empty path".to_string())
                        } else {
                            Ok(())
                        },
                    ));
                }
                // Dry-run: serialize a sample table via toml_edit, prove it re-parses.
                match render_toml_hooks_sample(vs.events.keys(), root, entry) {
                    Ok(text) => match text.parse::<toml_edit::Document>() {
                        Ok(_) => checks.push((
                            format!("install[{i}].toml_hooks renders parseable TOML"),
                            Ok(()),
                        )),
                        Err(e) => checks.push((
                            format!("install[{i}].toml_hooks renders parseable TOML"),
                            Err(e.to_string()),
                        )),
                    },
                    Err(e) => checks.push((
                        format!("install[{i}].toml_hooks renders parseable TOML"),
                        Err(e),
                    )),
                }
            }
            InstallOp::TomlListHooks {
                paths,
                list_key,
                entry,
                entry_overrides,
                ..
            } => {
                for path in paths {
                    checks.push((
                        format!("install[{i}].toml_list_hooks path expands: {path}"),
                        if path.trim().is_empty() {
                            Err("empty path".to_string())
                        } else {
                            Ok(())
                        },
                    ));
                }
                checks.push((
                    format!("install[{i}].toml_list_hooks list key is non-empty: {list_key}"),
                    if list_key.trim().is_empty() {
                        Err("empty list_key".to_string())
                    } else {
                        Ok(())
                    },
                ));
                // Dry-run: render one [[list_key]] row per host event
                // into a real array-of-tables document, prove it
                // re-parses. Non-table rows fail the check.
                let mut arr = toml_edit::ArrayOfTables::new();
                let mut render_err: Option<String> = None;
                for host_event in vs.events.keys() {
                    let mut handler = entry.clone();
                    if let (Some(h), Some(o)) = (
                        handler.as_object_mut(),
                        entry_overrides.get(host_event).and_then(|v| v.as_object()),
                    ) {
                        for (k, v) in o {
                            h.insert(k.clone(), v.clone());
                        }
                    }
                    render_handler_template(
                        &mut handler,
                        host_event,
                        "agent-hooks hook <id> --event <event>",
                    );
                    match json_to_toml_item(&handler) {
                        Ok(toml_edit::Item::Table(row)) => arr.push(row),
                        Ok(_) => {
                            render_err = Some(format!(
                                "event '{host_event}' does not render to a TOML table"
                            ));
                            break;
                        }
                        Err(e) => {
                            render_err = Some(e);
                            break;
                        }
                    }
                }
                match render_err {
                    Some(e) => checks.push((
                        format!("install[{i}].toml_list_hooks renders parseable TOML"),
                        Err(e),
                    )),
                    None => {
                        let mut sample = toml_edit::Document::new();
                        sample
                            .as_table_mut()
                            .insert(list_key, toml_edit::Item::ArrayOfTables(arr));
                        match sample.to_string().parse::<toml_edit::Document>() {
                            Ok(_) => checks.push((
                                format!("install[{i}].toml_list_hooks renders parseable TOML"),
                                Ok(()),
                            )),
                            Err(e) => checks.push((
                                format!("install[{i}].toml_list_hooks renders parseable TOML"),
                                Err(e.to_string()),
                            )),
                        }
                    }
                }
            }
            InstallOp::StageAssets { assets, .. } => {
                for asset in assets {
                    let ok = asset.embedded.is_some() || asset.template.is_some();
                    checks.push((
                        format!(
                            "install[{i}].stage_assets asset has embedded key or template: {}",
                            asset.path
                        ),
                        if ok {
                            Ok(())
                        } else {
                            Err("asset needs `embedded` or `template`".to_string())
                        },
                    ));
                }
            }
            InstallOp::RunCommand { command, .. } => {
                checks.push((
                    format!("install[{i}].run_command is non-empty"),
                    if command.trim().is_empty() {
                        Err("empty command".to_string())
                    } else {
                        Ok(())
                    },
                ));
            }
            InstallOp::JsonEdit { paths, pointer, .. } => {
                for path in paths {
                    checks.push((
                        format!("install[{i}].json_edit path expands: {path}"),
                        if path.trim().is_empty() {
                            Err("empty path".to_string())
                        } else {
                            Ok(())
                        },
                    ));
                }
                checks.push((
                    format!("install[{i}].json_edit pointer is absolute: {pointer}"),
                    if pointer.starts_with('/') || pointer.is_empty() {
                        Ok(())
                    } else {
                        Err("pointer must start with '/'".to_string())
                    },
                ));
            }
        }
    }

    // response templates may use only the placeholders `check_placeholders` lists
    let mut templates: Vec<(&str, &serde_json::Value)> = Vec::new();
    if let Some(gate) = &vs.response.gate {
        templates.push(("gate.allow", &gate.allow));
        templates.push(("gate.deny", &gate.deny));
        if let Some(ask) = &gate.ask {
            templates.push(("gate.ask", ask));
        }
    }
    if let Some(cont) = &vs.response.continue_response {
        templates.push(("continue.continue", &cont.continue_));
        templates.push(("continue.stop", &cont.stop));
    }
    for (name, template) in templates {
        checks.push((
            format!("response.{name} uses only supported placeholders"),
            check_placeholders(template),
        ));
    }
    for (lifecycle, ov) in &vs.response.response_overrides {
        checks.push((
            format!("response.overrides[{lifecycle}].allow uses only supported placeholders"),
            check_placeholders(&ov.allow),
        ));
        checks.push((
            format!("response.overrides[{lifecycle}].deny uses only supported placeholders"),
            check_placeholders(&ov.deny),
        ));
        if let Some(ask) = &ov.ask {
            checks.push((
                format!("response.overrides[{lifecycle}].ask uses only supported placeholders"),
                check_placeholders(ask),
            ));
        }
    }
    // verdicts/ask-template contradiction: a dead ask template mislabels the contract.
    // Effective ask = the lifecycle's override ask when an override exists, else the
    // base gate ask (mirrors DeclarativeAdapter resolution, where an override fully
    // replaces the base gate for its lifecycle). Verdicts that allow (or omit) ask
    // with no ask template are fine: the deny fallback is legitimate, stays silent.
    for (host_event, spec) in &vs.events {
        if spec.capability != Capability::Gate {
            continue;
        }
        let override_gate = vs
            .response
            .response_overrides
            .get(host_event)
            .or_else(|| vs.response.response_overrides.get(spec.lifecycle.as_str()));
        let effective_ask = match override_gate {
            Some(gate) => gate.ask.as_ref(),
            None => vs.response.gate.as_ref().and_then(|gate| gate.ask.as_ref()),
        };
        if effective_ask.is_some() && !vs.ask_allowed(host_event) {
            checks.push((
                format!("events[{host_event}].verdicts contradict ask template"),
                Err(format!(
                    "lifecycle '{}' disallows ask but defines an ask template (dead template mislabels the contract)",
                    spec.lifecycle.as_str()
                )),
            ));
        }
    }

    checks
}

fn render_json_hooks_sample<'a>(
    host_events: impl Iterator<Item = &'a String>,
    root: &str,
    entry: &serde_json::Value,
    entry_overrides: &BTreeMap<String, serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let mut doc = serde_json::json!({});
    for host_event in host_events {
        let mut handler = entry.clone();
        render_handler_template(
            &mut handler,
            host_event,
            "agent-hooks hook <id> --event <event>",
        );
        if let Some(ov) = entry_overrides.get(host_event) {
            merge_json(&mut handler, ov);
        }
        let path = root.replace("{event}", host_event);
        set_dotted(&mut doc, &path, handler);
    }
    Ok(doc)
}

/// Recursively replace `{event}` / `{command}` placeholders in string leaves.
fn render_handler_template(value: &mut serde_json::Value, event: &str, command: &str) {
    match value {
        serde_json::Value::String(s) => {
            *s = s.replace("{event}", event).replace("{command}", command);
        }
        serde_json::Value::Array(items) => {
            for item in items {
                render_handler_template(item, event, command);
            }
        }
        serde_json::Value::Object(map) => {
            for (_, v) in map.iter_mut() {
                render_handler_template(v, event, command);
            }
        }
        _ => {}
    }
}

fn merge_json(base: &mut serde_json::Value, overlay: &serde_json::Value) {
    match (base, overlay) {
        (serde_json::Value::Object(b), serde_json::Value::Object(o)) => {
            for (k, v) in o {
                match b.get_mut(k) {
                    Some(existing) => merge_json(existing, v),
                    None => {
                        b.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        (base, overlay) => {
            *base = overlay.clone();
        }
    }
}

fn set_dotted(doc: &mut serde_json::Value, dotted: &str, leaf: serde_json::Value) {
    let mut node = doc;
    let parts: Vec<&str> = dotted.split('.').collect();
    for part in &parts[..parts.len().saturating_sub(1)] {
        if !node.is_object() {
            *node = serde_json::json!({});
        }
        node = node
            .as_object_mut()
            .unwrap()
            .entry((*part).to_string())
            .or_insert(serde_json::json!({}));
    }
    if let Some(last) = parts.last() {
        if !node.is_object() {
            *node = serde_json::json!({});
        }
        let slot = node
            .as_object_mut()
            .unwrap()
            .entry(last.to_string())
            .or_insert(serde_json::json!([]));
        if let Some(arr) = slot.as_array_mut() {
            arr.push(leaf);
        } else {
            *slot = serde_json::json!([leaf]);
        }
    }
}

fn render_toml_hooks_sample<'a>(
    host_events: impl Iterator<Item = &'a String>,
    root: &str,
    entry: &serde_json::Value,
) -> Result<String, String> {
    // Build a sample `hooks.<event>` table and serialize it with toml_edit.
    let mut table = toml_edit::Table::new();
    for host_event in host_events {
        let mut handler = entry.clone();
        render_handler_template(
            &mut handler,
            host_event,
            "agent-hooks hook <id> --event <event>",
        );
        let item = json_to_toml_item(&handler)?;
        let event_key = root.replace("{event}", host_event);
        // root is dotted (e.g. `hooks.PreToolUse`); nest accordingly.
        let parts: Vec<&str> = event_key.split('.').collect();
        let mut node = &mut table;
        for part in &parts {
            if !node.contains_key(part) {
                node.insert(part, toml_edit::Item::Table(toml_edit::Table::new()));
            }
            node = node
                .get_mut(part)
                .and_then(|i| i.as_table_mut())
                .ok_or_else(|| format!("root segment '{part}' is not a table"))?;
        }
        if !node.contains_key("hooks") {
            node.insert(
                "hooks",
                toml_edit::Item::ArrayOfTables(toml_edit::ArrayOfTables::new()),
            );
        }
        let aot = node
            .get_mut("hooks")
            .and_then(|i| i.as_array_of_tables_mut())
            .ok_or_else(|| "hooks is not an array of tables".to_string())?;
        let row = match item {
            toml_edit::Item::Table(t) => t,
            other => {
                let mut t = toml_edit::Table::new();
                t.insert("value", other);
                t
            }
        };
        aot.push(row);
    }
    Ok(table.to_string())
}

fn json_to_toml_item(value: &serde_json::Value) -> Result<toml_edit::Item, String> {
    let v: toml_edit::Value = match value {
        serde_json::Value::String(s) => s.as_str().into(),
        serde_json::Value::Bool(b) => (*b).into(),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i.into()
            } else if let Some(f) = n.as_f64() {
                f.into()
            } else {
                return Err("unsupported number in toml entry".to_string());
            }
        }
        serde_json::Value::Array(items) => {
            let mut arr = toml_edit::Array::new();
            for item in items {
                arr.push(json_to_toml_value(item)?);
            }
            toml_edit::Value::Array(arr)
        }
        serde_json::Value::Object(map) => {
            let mut t = toml_edit::Table::new();
            for (k, v) in map {
                t.insert(k, json_to_toml_item(v)?);
            }
            return Ok(toml_edit::Item::Table(t));
        }
        serde_json::Value::Null => return Err("null has no TOML representation".to_string()),
    };
    Ok(toml_edit::Item::Value(v))
}

fn json_to_toml_value(value: &serde_json::Value) -> Result<toml_edit::Value, String> {
    match json_to_toml_item(value)? {
        toml_edit::Item::Value(v) => Ok(v),
        _ => Err("nested tables not allowed in TOML arrays".to_string()),
    }
}

/// Error when a template uses a placeholder unsupported by the renderer.
fn check_placeholders(template: &serde_json::Value) -> Result<(), String> {
    fn walk(value: &serde_json::Value, bad: &mut Vec<String>) {
        match value {
            serde_json::Value::String(s) => {
                let mut rest = s.as_str();
                while let Some(open) = rest.find('{') {
                    rest = &rest[open + 1..];
                    if let Some(close) = rest.find('}') {
                        let name = &rest[..close];
                        if !matches!(
                            name,
                            "" | "reason"
                                | "reason?"
                                | "event"
                                | "answers"
                                | "reply"
                                | "answer_groups"
                                | "answers_structured"
                                | "updated_input"
                                | "updated_permissions"
                        ) {
                            bad.push(format!("{{{name}}}"));
                        }
                        rest = &rest[close + 1..];
                    } else {
                        break;
                    }
                }
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    walk(item, bad);
                }
            }
            serde_json::Value::Object(map) => {
                for (_, v) in map {
                    walk(v, bad);
                }
            }
            _ => {}
        }
    }
    let mut bad = Vec::new();
    walk(template, &mut bad);
    if bad.is_empty() {
        Ok(())
    } else {
        Err(format!("unknown placeholders: {}", bad.join(", ")))
    }
}

fn cmd_validate(target: &str) -> anyhow::Result<()> {
    let (manifest, _) = load_target(target)?;
    // `extends` manifests may omit `versions` (inherited from the base), so
    // validate the fully materialized form against the loaded tier set.
    let manifest = match manifest.extends.as_deref() {
        None => manifest,
        Some(_) => {
            let all = AdapterManifest::load_tiered();
            AdapterManifest::resolve_all(&all)?
                .into_iter()
                .find(|m| m.id == manifest.id)
                .ok_or_else(|| {
                    anyhow::anyhow!("extends base resolution failed for '{}'", manifest.id)
                })?
        }
    };
    let checks = validate_manifest(&manifest);
    let mut failed = 0;
    for (name, result) in &checks {
        match result {
            Ok(()) => println!("ok: {name}"),
            Err(reason) => {
                failed += 1;
                println!("error: {name}: {reason}");
            }
        }
    }
    println!("{} ({} failed)", manifest.id, failed);
    if failed > 0 {
        anyhow::bail!("validation failed with {failed} error(s)");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// `adapter test`
// ---------------------------------------------------------------------------

fn cmd_test(target: &str) -> anyhow::Result<()> {
    let (mut manifest, manifest_path) = load_target(target)?;
    // Materialize `extends` against the full tier set before building the
    // adapter — an extends manifest may have no local `versions` map.
    if manifest.extends.is_some() {
        let all = AdapterManifest::load_tiered();
        manifest = AdapterManifest::resolve_all(&all)?
            .into_iter()
            .find(|m| m.id == manifest.id)
            .ok_or_else(|| {
                anyhow::anyhow!("extends base resolution failed for '{}'", manifest.id)
            })?;
    }
    let version = manifest.default_version.clone();
    let adapter = DeclarativeAdapter::new(manifest.clone(), version.clone())?;

    let fixtures_dir: Option<PathBuf> = manifest_path
        .as_ref()
        .and_then(|p| p.parent().map(|parent| parent.join("fixtures")));
    let Some(dir) = fixtures_dir else {
        println!(
            "note: builtin adapter '{}' ships no fixtures dir — nothing to replay",
            manifest.id
        );
        return Ok(());
    };
    if !dir.is_dir() {
        anyhow::bail!(
            "no fixtures dir at {} — capture payloads first (see `adapter new` next steps)",
            dir.display()
        );
    }

    let mut entries: Vec<PathBuf> = std::fs::read_dir(&dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("json"))
        .collect();
    entries.sort();
    if entries.is_empty() {
        println!("note: no fixtures in {} — nothing to replay", dir.display());
        return Ok(());
    }

    let mut failed = 0;
    let mut passed = 0;
    for path in &entries {
        match run_fixture(&adapter, &manifest.id, path) {
            Ok(()) => {
                passed += 1;
                println!(
                    "ok: {}",
                    path.file_name().unwrap_or_default().to_string_lossy()
                );
            }
            Err(e) => {
                failed += 1;
                println!("FAIL: {}: {e:#}", path.display());
            }
        }
    }
    println!("{passed} passed, {failed} failed");
    if failed > 0 {
        anyhow::bail!("{failed} fixture(s) failed");
    }
    Ok(())
}

fn run_fixture(adapter: &DeclarativeAdapter, id: &str, path: &Path) -> anyhow::Result<()> {
    let text = std::fs::read_to_string(path)?;
    let fixture: serde_json::Value = serde_json::from_str(&text)?;

    let stdin_value = fixture
        .get("stdin")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let raw_payload = match &stdin_value {
        serde_json::Value::String(s) => s.clone(),
        _ => serde_json::to_string(&stdin_value)?,
    };
    // event_hint comes from the payload's own event field, like a real shim call.
    let event_hint = ["event", "event_hint", "hook_event_name", "hookEventName"]
        .iter()
        .find_map(|k| {
            stdin_value
                .get(*k)
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        });

    let input = RawHookInput {
        harness_hint: id.to_string(),
        event_hint,
        raw_payload,
        context: EventContext::default(),
        request_id: None,
        passthrough_env: Default::default(),
    };
    let event = adapter
        .parse_event(&input)
        .map_err(|e| anyhow::anyhow!("parse_event: {e}"))?;

    // expect_event: {"type": "<LifecycleVariant>", ...} (+ optional session_id)
    if let Some(expect) = fixture.get("expect_event") {
        if let Some(want_type) = expect.get("type").and_then(|v| v.as_str()) {
            let lifecycle_json = serde_json::to_value(&event.lifecycle)?;
            let got_type = lifecycle_json
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            if got_type != want_type {
                anyhow::bail!("event type mismatch: expected {want_type}, got {got_type}");
            }
        }
        if let Some(want_session) = expect.get("session_id").and_then(|v| v.as_str()) {
            if event.session_id != want_session {
                anyhow::bail!(
                    "session_id mismatch: expected {want_session}, got {}",
                    event.session_id
                );
            }
        }
    }

    // expect_response: rendered with the fixture's decision (default: allow).
    if let Some(expect_response) = fixture.get("expect_response") {
        // The fixture's `decision` is a canonical PromptDecision.
        let decision = match fixture.get("decision") {
            Some(d) => serde_json::from_value::<PromptDecision>(d.clone())
                .map_err(|e| anyhow::anyhow!("fixture `decision`: {e}"))?,
            None => PromptDecision::allow(),
        };
        let got = adapter.format_response(&decision);
        if &got != expect_response {
            anyhow::bail!("response mismatch: {}", json_diff(expect_response, &got));
        }
    }

    Ok(())
}

/// One-line JSON diff: first path where expected and actual differ.
fn json_diff(expected: &serde_json::Value, got: &serde_json::Value) -> String {
    fn walk(path: &str, e: &serde_json::Value, g: &serde_json::Value) -> Option<String> {
        if e == g {
            return None;
        }
        match (e, g) {
            (serde_json::Value::Object(em), serde_json::Value::Object(gm)) => {
                for (k, ev) in em {
                    let p = if path.is_empty() {
                        k.clone()
                    } else {
                        format!("{path}.{k}")
                    };
                    match gm.get(k) {
                        Some(gv) => {
                            if let Some(d) = walk(&p, ev, gv) {
                                return Some(d);
                            }
                        }
                        None => return Some(format!("{p}: expected {ev}, missing")),
                    }
                }
                for k in gm.keys() {
                    if !em.contains_key(k) {
                        let p = if path.is_empty() {
                            k.clone()
                        } else {
                            format!("{path}.{k}")
                        };
                        return Some(format!("{p}: unexpected {}", gm[k]));
                    }
                }
                None
            }
            _ => Some(format!("{path}: expected {e}, got {g}")),
        }
    }
    walk("", expected, got).unwrap_or_else(|| format!("expected {expected}, got {got}"))
}

// ---------------------------------------------------------------------------
// `adapter add`
// ---------------------------------------------------------------------------

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut s = String::with_capacity(digest.len() * 2);
    for b in digest {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn temp_workdir(prefix: &str) -> anyhow::Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!(
        "agent-hooks-{prefix}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Find the first valid manifest among `*.json` files under `dir` (recursive, sorted).
fn discover_manifest(dir: &Path) -> anyhow::Result<(PathBuf, Vec<u8>, AdapterManifest)> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    collect_json_files(dir, &mut candidates);
    candidates.sort();
    for path in candidates {
        if path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(|s| s.ends_with(".meta"))
            .unwrap_or(false)
        {
            continue;
        }
        if let Ok(bytes) = std::fs::read(&path) {
            if let Ok(text) = std::str::from_utf8(&bytes) {
                if let Ok(manifest) = AdapterManifest::parse(text) {
                    return Ok((path, bytes, manifest));
                }
            }
        }
    }
    anyhow::bail!(
        "no valid adapter manifest (*.json) found under {}",
        dir.display()
    )
}

fn collect_json_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        // Skip VCS and dependency dirs.
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            if name == ".git" || name == "node_modules" || name == "target" {
                continue;
            }
        }
        if path.is_dir() {
            collect_json_files(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("json") {
            out.push(path);
        }
    }
}

fn cmd_add(args: AdapterAddArgs) -> anyhow::Result<()> {
    let source_path = Path::new(&args.source);
    let (bytes, manifest): (Vec<u8>, AdapterManifest) = if source_path.is_file() {
        let bytes = std::fs::read(source_path)?;
        let text = std::str::from_utf8(&bytes)
            .map_err(|_| anyhow::anyhow!("manifest is not valid UTF-8"))?;
        let manifest = AdapterManifest::parse(text)?;
        (bytes, manifest)
    } else if source_path.is_dir() {
        let (_, bytes, manifest) = discover_manifest(source_path)?;
        (bytes, manifest)
    } else if args.source.ends_with(".tar.gz")
        || args.source.ends_with(".tgz")
        || args.source.ends_with(".tar")
    {
        let workdir = temp_workdir("tarball")?;
        let archive = workdir.join("pkg.tar");
        let status = std::process::Command::new("curl")
            .args(["-sSL", args.source.as_str(), "-o"])
            .arg(&archive)
            .status()?;
        if !status.success() {
            anyhow::bail!("curl failed to download {}", args.source);
        }
        let unpacked = workdir.join("unpacked");
        std::fs::create_dir_all(&unpacked)?;
        let status = std::process::Command::new("tar")
            .args(["xf"])
            .arg(&archive)
            .arg("-C")
            .arg(&unpacked)
            .status()?;
        if !status.success() {
            anyhow::bail!("tar failed to extract {}", args.source);
        }
        let (_, bytes, manifest) = discover_manifest(&unpacked)?;
        (bytes, manifest)
    } else {
        // Assume git URL: shallow-clone via the git CLI.
        let workdir = temp_workdir("git")?;
        let checkout = workdir.join("repo");
        let status = std::process::Command::new("git")
            .args(["clone", "--depth", "1", args.source.as_str()])
            .arg(&checkout)
            .status()?;
        if !status.success() {
            anyhow::bail!("git clone failed for {}", args.source);
        }
        let (_, bytes, manifest) = discover_manifest(&checkout)?;
        (bytes, manifest)
    };

    // Validate before installing into the user tier.
    let checks = validate_manifest(&manifest);
    let errors: Vec<String> = checks
        .iter()
        .filter_map(|(name, r)| r.as_ref().err().map(|e| format!("{name}: {e}")))
        .collect();
    if !errors.is_empty() {
        anyhow::bail!("manifest failed validation:\n  {}", errors.join("\n  "));
    }

    let name = args.name.clone().unwrap_or_else(|| manifest.id.clone());
    if !valid_id(&name) {
        anyhow::bail!("invalid name '{name}': use lowercase letters, digits, '-' or '_'");
    }
    let dir = user_adapters_dir()
        .ok_or_else(|| anyhow::anyhow!("cannot determine home directory for user adapter tier"))?;
    std::fs::create_dir_all(&dir)?;
    let dest = dir.join(format!("{name}.json"));
    std::fs::write(&dest, &bytes)?;
    let meta = serde_json::json!({
        "source": args.source,
        "sha256": sha256_hex(&bytes),
        "fetched_at": chrono::Utc::now().to_rfc3339(),
    });
    std::fs::write(
        dir.join(format!("{name}.meta.json")),
        serde_json::to_string_pretty(&meta)?,
    )?;
    println!("installed {name} → {}", dest.display());
    Ok(())
}

// ---------------------------------------------------------------------------
// `adapter list`
// ---------------------------------------------------------------------------

fn cmd_list(args: AdapterListArgs) -> anyhow::Result<()> {
    struct Row {
        id: String,
        source: &'static str,
        default_version: String,
        events: usize,
        ops: usize,
    }
    let mut rows: Vec<Row> = Vec::new();
    for manifest in AdapterManifest::load_embedded() {
        let vs = manifest.versions.get(&manifest.default_version);
        rows.push(Row {
            id: manifest.id,
            source: "builtin",
            default_version: manifest.default_version,
            events: vs.map(|v| v.events.len()).unwrap_or(0),
            ops: vs.map(|v| v.install.len()).unwrap_or(0),
        });
    }
    if let Some(dir) = user_adapters_dir() {
        for (_, manifest) in scan_tier_dir(&dir) {
            let vs = manifest.versions.get(&manifest.default_version);
            rows.push(Row {
                id: manifest.id,
                source: "user",
                default_version: manifest.default_version,
                events: vs.map(|v| v.events.len()).unwrap_or(0),
                ops: vs.map(|v| v.install.len()).unwrap_or(0),
            });
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        for (_, manifest) in scan_tier_dir(&cwd.join(".agent-hooks/adapters")) {
            let vs = manifest.versions.get(&manifest.default_version);
            rows.push(Row {
                id: manifest.id,
                source: "project",
                default_version: manifest.default_version,
                events: vs.map(|v| v.events.len()).unwrap_or(0),
                ops: vs.map(|v| v.install.len()).unwrap_or(0),
            });
        }
    }
    rows.sort_by(|a, b| a.id.cmp(&b.id));

    if args.json {
        let list: Vec<serde_json::Value> = rows
            .iter()
            .map(|r| {
                serde_json::json!({
                    "id": r.id,
                    "source": r.source,
                    "default_version": r.default_version,
                    "events": r.events,
                    "install_ops": r.ops,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&list)?);
        return Ok(());
    }

    println!(
        "{:<20} {:<10} {:<10} {:<8} ops",
        "ID", "SOURCE", "VERSION", "EVENTS"
    );
    println!("{}", "-".repeat(60));
    for r in &rows {
        println!(
            "{:<20} {:<10} {:<10} {:<8} {}",
            r.id, r.source, r.default_version, r.events, r.ops
        );
    }
    Ok(())
}
