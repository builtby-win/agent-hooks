use crate::cli::UninstallArgs;
use crate::paths::expand_op_path;
use agent_hooks_adapters::manifest::{AdapterManifest, InstallOp};
use agent_hooks_adapters::traits::normalize_hint;
use agent_hooks_adapters::AdapterRegistry;
use std::path::{Path, PathBuf};

fn find_manifest(harness: &str) -> Option<AdapterManifest> {
    let norm = normalize_hint(harness);
    if norm.is_empty() {
        return None;
    }
    let manifests = AdapterManifest::load_tiered();
    for m in &manifests {
        if normalize_hint(&m.id) == norm || m.aliases.iter().any(|a| normalize_hint(a) == norm) {
            return Some(m.clone());
        }
    }
    if norm.len() < 3 {
        return None;
    }
    for m in &manifests {
        if normalize_hint(&m.id).contains(&norm) || norm.contains(&normalize_hint(&m.id)) {
            return Some(m.clone());
        }
        if m.aliases
            .iter()
            .any(|a| normalize_hint(a).contains(&norm) || norm.contains(&normalize_hint(a)))
        {
            return Some(m.clone());
        }
    }
    None
}

pub fn run(args: UninstallArgs) -> anyhow::Result<()> {
    let target = args.harness.to_lowercase();

    let manifests: Vec<AdapterManifest> = if target == "all" {
        let mut all = AdapterManifest::load_tiered();
        all.sort_by(|a, b| a.id.cmp(&b.id));
        all
    } else if let Some(m) = find_manifest(&target) {
        vec![m]
    } else {
        anyhow::bail!(
            "unknown harness '{}'. Known: {}, or 'all'",
            args.harness,
            AdapterRegistry::ids().join(", ")
        );
    };

    let mut removed_any = false;

    for manifest in &manifests {
        // Uninstall has no --adapter-version flag: use the default version.
        let version = manifest.default_version.clone();
        let Some(vs) = manifest.versions.get(&version) else {
            eprintln!(
                "✗ {} — no '{}' version in manifest",
                manifest.display_name, version
            );
            continue;
        };
        let staged_dir = vs.install.iter().find_map(|op| match op {
            InstallOp::StageAssets { stage_dir, .. } => Some(expand_op_path(stage_dir)),
            _ => None,
        });
        // Ops undo in reverse install order.
        let mut removed_for_harness = 0usize;
        for op in vs.install.iter().rev() {
            match op {
                InstallOp::JsonHooks { paths, .. } => {
                    for raw in paths {
                        let path = expand_op_path(raw);
                        if !path.exists() {
                            continue;
                        }
                        match remove_json_hooks(&path, &manifest.id, &manifest.aliases) {
                            Ok(n) if n > 0 => {
                                println!(
                                    "✓ {} — removed {} hook(s) from {}",
                                    manifest.display_name,
                                    n,
                                    path.display()
                                );
                                removed_for_harness += n;
                            }
                            Err(e) => eprintln!(
                                "✗ {} — failed to clean {}: {}",
                                manifest.display_name,
                                path.display(),
                                e
                            ),
                            Ok(_) => {}
                        }
                    }
                }
                InstallOp::TomlHooks { paths, .. } => {
                    for raw in paths {
                        let path = expand_op_path(raw);
                        if !path.exists() {
                            continue;
                        }
                        match remove_toml_hooks(&path, &manifest.id, &manifest.aliases) {
                            Ok(n) if n > 0 => {
                                println!(
                                    "✓ {} — removed {} hook(s) from {}",
                                    manifest.display_name,
                                    n,
                                    path.display()
                                );
                                removed_for_harness += n;
                            }
                            Err(e) => eprintln!(
                                "✗ {} — failed to clean {}: {}",
                                manifest.display_name,
                                path.display(),
                                e
                            ),
                            Ok(_) => {}
                        }
                    }
                }
                InstallOp::TomlListHooks {
                    paths, list_key, ..
                } => {
                    for raw in paths {
                        let path = expand_op_path(raw);
                        if !path.exists() {
                            continue;
                        }
                        match remove_toml_list_hooks(
                            &path,
                            list_key,
                            &manifest.id,
                            &manifest.aliases,
                        ) {
                            Ok(n) if n > 0 => {
                                println!(
                                    "✓ {} — removed {} hook(s) from {}",
                                    manifest.display_name,
                                    n,
                                    path.display()
                                );
                                removed_for_harness += n;
                            }
                            Err(e) => eprintln!(
                                "✗ {} — failed to clean {}: {}",
                                manifest.display_name,
                                path.display(),
                                e
                            ),
                            Ok(_) => {}
                        }
                    }
                }
                InstallOp::StageAssets {
                    assets,
                    managed_marker,
                    ..
                } => {
                    let Some(dir) = staged_dir.as_ref() else {
                        continue;
                    };
                    let mut removed_here = 0usize;
                    for asset in assets {
                        let dest = dir.join(&asset.path);
                        if dest.is_file() && is_managed_file(&dest, managed_marker.as_deref()) {
                            match std::fs::remove_file(&dest) {
                                Ok(()) => {
                                    println!("  removed staged asset {}", dest.display());
                                    removed_for_harness += 1;
                                    removed_here += 1;
                                }
                                Err(e) => {
                                    eprintln!("  failed to remove {}: {}", dest.display(), e)
                                }
                            }
                        }
                    }
                    // Remove directories we created only when left empty AND
                    // every removed file in them was ours (every removal
                    // above is managed-marker/agent-hooks gated): foreign
                    // files in shared plugin dirs are never deleted, and
                    // non-empty/foreign dirs stay.
                    if removed_here > 0
                        && dir.is_dir()
                        && dir
                            .read_dir()
                            .map(|mut d| d.next().is_none())
                            .unwrap_or(false)
                    {
                        let _ = std::fs::remove_dir(dir);
                    }
                }
                InstallOp::RunCommand { undo, .. } => {
                    if let Some(cmd) = undo {
                        run_undo(cmd, &manifest.display_name);
                    }
                    // Symlinked local-path plugins (omp) defeat `omp plugin
                    // uninstall` (npm-backed): unlink managed links directly.
                    if manifest.id == "pi" && manually_unlink_managed_plugin(staged_dir.as_deref())
                    {
                        removed_for_harness += 1;
                    }
                }
                InstallOp::JsonEdit {
                    paths,
                    pointer,
                    mode: agent_hooks_adapters::manifest::EditMode::Set,
                    value: Some(value),
                    restore_on_uninstall: true,
                    ..
                } => {
                    // Opt-in inverse (e.g. Kiro's chat.defaultAgent): restore
                    // the recorded previous value while the key still holds ours.
                    match crate::commands::install::revert_json_edit(paths, pointer, value) {
                        Ok(n) if n > 0 => {
                            println!("✓ {} — restored {}", manifest.display_name, pointer);
                            removed_for_harness += n;
                        }
                        Ok(_) => {}
                        Err(e) => eprintln!(
                            "✗ {} — failed to restore {}: {}",
                            manifest.display_name, pointer, e
                        ),
                    }
                }
                InstallOp::JsonEdit { .. } => {
                    // Non-inverting by design: values we appended or set
                    // belong to the user's config now; uninstall leaves them
                    // so user edits are never destroyed.
                }
            }
        }
        if removed_for_harness == 0 {
            println!("- {} — no agent-hooks entries found", manifest.display_name);
        } else {
            removed_any = true;
        }
    }

    if !removed_any {
        println!("\nNo agent-hooks entries found to remove.");
    } else {
        println!("\nUninstall complete. Restart the agent to apply.");
    }

    Ok(())
}

/// Only remove staged files we manage: marker present, or an `agent-hooks`
/// reference inside. Never delete foreign files.
pub(crate) fn is_managed_file(path: &Path, marker: Option<&str>) -> bool {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return false,
    };
    if let Some(m) = marker {
        if content.starts_with(m) || content.contains(m) {
            return true;
        }
    }
    content.contains("agent-hooks")
}

fn run_undo(command: &str, display_name: &str) {
    #[cfg(unix)]
    let status = std::process::Command::new("sh")
        .args(["-c", command])
        .status();
    #[cfg(windows)]
    let status = std::process::Command::new("cmd")
        .args(["/C", command])
        .status();
    #[cfg(not(any(unix, windows)))]
    let status: std::io::Result<std::process::ExitStatus> = Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "unsupported platform",
    ));
    match status {
        Ok(s) if s.success() => println!("✓ {} — ran undo `{}`", display_name, command),
        Ok(s) => eprintln!(
            "  warning: undo `{}` exited with {}; remove leftovers manually",
            command, s
        ),
        Err(e) => eprintln!(
            "  warning: could not run undo `{}`: {}; remove leftovers manually",
            command, e
        ),
    }
}

// ---------------------------------------------------------------------------
// Shared installer-contract helpers
// ---------------------------------------------------------------------------

/// Installer-contract removal predicate: a command is ours when it contains
/// `agent-hooks` AND the harness id or any manifest alias (case-insensitive,
/// normalized via `normalize_hint`, with a raw-lowercase fallback so `_` /
/// space / `-` separator variants match nothing foreign and miss nothing
/// of ours).
fn command_is_ours(cmd: &str, harness_id: &str, aliases: &[String]) -> bool {
    let lower = cmd.to_ascii_lowercase();
    if !lower.contains("agent-hooks") {
        return false;
    }
    std::iter::once(harness_id)
        .chain(aliases.iter().map(String::as_str))
        .filter(|a| !a.trim().is_empty())
        .any(|a| {
            let norm = normalize_hint(a);
            let raw = a.to_ascii_lowercase();
            (!norm.is_empty() && lower.contains(&norm)) || (!raw.is_empty() && lower.contains(&raw))
        })
}

/// Strip `//` line comments, `/* */` block comments, and trailing commas
/// outside string literals so JSONC configs parse with `serde_json`.
fn strip_jsonc(data: &str) -> String {
    let mut out = String::with_capacity(data.len());
    let mut chars = data.chars().peekable();
    let mut in_string = false;
    let mut escaped = false;
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
            }
            '/' => match chars.peek() {
                Some('/') => {
                    for nc in chars.by_ref() {
                        if nc == '\n' {
                            out.push('\n');
                            break;
                        }
                    }
                }
                Some('*') => {
                    chars.next();
                    let mut prev_star = false;
                    for nc in chars.by_ref() {
                        if prev_star && nc == '/' {
                            break;
                        }
                        prev_star = nc == '*';
                    }
                }
                _ => out.push(c),
            },
            _ => out.push(c),
        }
    }
    // Drop trailing commas before `}` / `]` outside strings.
    let mut cleaned = String::with_capacity(out.len());
    let mut it = out.chars().peekable();
    let mut in_s = false;
    let mut esc = false;
    while let Some(c) = it.next() {
        if in_s {
            cleaned.push(c);
            if esc {
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else if c == '"' {
                in_s = false;
            }
            continue;
        }
        match c {
            '"' => {
                in_s = true;
                cleaned.push(c);
            }
            ',' => {
                let mut next_non_ws = None;
                for nc in it.clone() {
                    if !nc.is_whitespace() {
                        next_non_ws = Some(nc);
                        break;
                    }
                }
                match next_non_ws {
                    Some('}' | ']') | None => {}
                    _ => cleaned.push(','),
                }
            }
            _ => cleaned.push(c),
        }
    }
    cleaned
}

/// Atomic config write: temp file + rename in the same dir, never a direct
/// truncate-write (crash/power-loss safe: the old config survives).
fn atomic_write(path: &Path, contents: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp-agent-hooks");
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)
}

// ---------------------------------------------------------------------------
// JSON removal (reverse of install dedupe)
// ---------------------------------------------------------------------------

/// Remove every hook entry matching the installer contract (`agent-hooks`
/// plus the harness id or any manifest alias), descending into
/// matcher-group `hooks` arrays and named bundles. Prunes newly-empty event
/// arrays and parent objects left empty by that removal so a foreign-seeded
/// config returns to its foreign content with no leftover empty `hooks: {}`
/// scaffolding. Returns the number of entries removed.
fn remove_json_hooks(
    path: &PathBuf,
    harness_id: &str,
    aliases: &[String],
) -> anyhow::Result<usize> {
    let data = std::fs::read_to_string(path)?;
    let stripped = strip_jsonc(&data);
    let mut root: serde_json::Value = match serde_json::from_str(&stripped) {
        Ok(v) => v,
        Err(e) => {
            eprintln!(
                "warning: skipping {}: unparseable config ({})",
                path.display(),
                e
            );
            return Ok(0);
        }
    };
    let mut removed = 0usize;
    if let Some(obj) = root.as_object_mut() {
        let mut empty_keys = Vec::new();
        for (key, val) in obj.iter_mut() {
            let before = removed;
            removed += remove_from_node(val, harness_id, aliases);
            // Prune only keys our removal emptied ("hooks", named bundles) —
            // never `fixed_fields` like cursor's `version`, never
            // pre-existing foreign empties, and never the file itself.
            if removed > before && is_empty_container(val) && key != "version" {
                empty_keys.push(key.clone());
            }
        }
        for k in empty_keys {
            obj.remove(&k);
        }
    }
    if removed > 0 {
        let pretty = serde_json::to_string_pretty(&root)?;
        atomic_write(path, &(pretty + "\n"))?;
    }
    Ok(removed)
}

fn is_empty_container(v: &serde_json::Value) -> bool {
    match v {
        serde_json::Value::Array(a) => a.is_empty(),
        serde_json::Value::Object(m) => {
            if m.is_empty() {
                return true;
            }
            // Named bundles shrink to just their `enabled` flag once all
            // event arrays are pruned — treat that as empty too.
            m.iter()
                .all(|(k, val)| k == "enabled" || is_empty_container(val))
        }
        _ => false,
    }
}

/// Remove our entries from any node; returns count removed. Handles:
/// - event arrays (flat handlers or matcher groups with inner `hooks`)
/// - named-bundle objects (`{"enabled": true, <Event>: [...]}`)
/// - generic nested objects (recurse + prune empties, keeping `enabled` flags)
fn remove_from_node(node: &mut serde_json::Value, harness_id: &str, aliases: &[String]) -> usize {
    match node {
        serde_json::Value::Array(arr) => {
            let mut removed = 0usize;
            arr.retain_mut(|entry| {
                if let Some(hooks) = entry.get_mut("hooks").and_then(|h| h.as_array_mut()) {
                    let h_before = hooks.len();
                    hooks.retain(|h| !is_ours(h, harness_id, aliases));
                    removed += h_before - hooks.len();
                    // Drop matcher groups left with no hooks.
                    return !hooks.is_empty();
                }
                if is_ours(entry, harness_id, aliases) {
                    removed += 1;
                    return false;
                }
                // Recurse into nested shapes (named bundles inside arrays).
                removed += remove_from_node(entry, harness_id, aliases);
                true
            });
            removed
        }
        serde_json::Value::Object(map) => {
            let mut removed = 0usize;
            let mut empty_keys = Vec::new();
            for (k, v) in map.iter_mut() {
                if k == "enabled" {
                    continue;
                }
                let before = removed;
                removed += remove_from_node(v, harness_id, aliases);
                // Prune only keys our removal emptied — never pre-existing
                // foreign empties.
                if removed > before && is_empty_container(v) {
                    empty_keys.push(k.clone());
                }
            }
            for k in empty_keys {
                map.remove(&k);
            }
            removed
        }
        _ => 0,
    }
}

fn is_ours(entry: &serde_json::Value, harness_id: &str, aliases: &[String]) -> bool {
    entry
        .get("command")
        .and_then(|c| c.as_str())
        .is_some_and(|s| command_is_ours(s, harness_id, aliases))
}

// ---------------------------------------------------------------------------
// TOML removal
// ---------------------------------------------------------------------------

/// Remove `[[hooks.<event>]]` blocks (and their `[[hooks.<event>.hooks]]`
/// children) whose command lines match the installer contract (`agent-hooks`
/// plus the harness id or any manifest alias). Returns the number of hook
/// blocks removed. The config file itself is never deleted.
pub(crate) fn remove_toml_hooks(
    path: &PathBuf,
    harness_id: &str,
    aliases: &[String],
) -> anyhow::Result<usize> {
    let data = std::fs::read_to_string(path)?;
    if !command_is_ours(&data, harness_id, aliases) {
        return Ok(0);
    }
    let lines: Vec<&str> = data.lines().collect();
    let mut out_lines: Vec<String> = Vec::new();
    let mut removed = 0usize;
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        // Only real command lines count — `# Added by agent-hooks` marker
        // comments name the harness too but are handled separately below.
        if !line.trim_start().starts_with('#') && command_is_ours(line, harness_id, aliases) {
            // Drop the surrounding block: look back for the section header.
            if !out_lines.is_empty() {
                let prev = out_lines.last().cloned().unwrap_or_default();
                if prev.trim().starts_with("[hooks.") || prev.trim().starts_with("[[hooks.") {
                    out_lines.pop();
                    if out_lines
                        .last()
                        .is_some_and(|l| l.contains("Added by agent-hooks"))
                    {
                        out_lines.pop();
                    }
                }
            }
            removed += 1;
            i += 1;
            while i < lines.len() {
                let next = lines[i].trim();
                if next.is_empty() || next.starts_with('[') || next.starts_with('#') {
                    break;
                }
                if next.starts_with("type")
                    || next.starts_with("timeout")
                    || next.starts_with("command")
                    || next.starts_with("matcher")
                {
                    i += 1;
                } else {
                    break;
                }
            }
            continue;
        }
        out_lines.push(line.to_string());
        i += 1;
    }

    if removed > 0 {
        let out = out_lines.join("\n");
        let cleaned = out
            .lines()
            .filter(|l| !l.contains("Added by agent-hooks"))
            .collect::<Vec<_>>()
            .join("\n");
        let final_out = if cleaned.trim().is_empty() {
            String::new()
        } else {
            cleaned + "\n"
        };
        // Never remove the config file itself: an emptied config stays as
        // an empty file.
        atomic_write(path, &final_out)?;
    }
    Ok(removed)
}

/// Remove flat `[[<list_key>]]` rows (e.g. Kimi Code / Mistral Vibe
/// `[[hooks]]`) whose command lines match the installer contract.
/// Only the enclosing row is dropped: scan back to its `[[<list_key>]]`
/// header (plus a directly-preceding `Added by agent-hooks` marker),
/// then skip its remaining key lines forward. Unrelated rows, tables,
/// and comments survive. The config file itself is never deleted.
pub(crate) fn remove_toml_list_hooks(
    path: &PathBuf,
    list_key: &str,
    harness_id: &str,
    aliases: &[String],
) -> anyhow::Result<usize> {
    let data = std::fs::read_to_string(path)?;
    if !data.contains("agent-hooks") {
        return Ok(0);
    }
    let header = format!("[[{}]]", list_key);
    let lines: Vec<&str> = data.lines().collect();
    let mut out_lines: Vec<String> = Vec::new();
    let mut removed = 0usize;
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if !line.trim_start().starts_with('#') && command_is_ours(line, harness_id, aliases) {
            // Pop back to the enclosing [[list_key]] header without
            // crossing a blank line or another section header.
            let mut popped_header = false;
            let mut popped: Vec<String> = Vec::new();
            while let Some(prev) = out_lines.pop() {
                let t = prev.trim();
                if t == header {
                    popped_header = true;
                    break;
                }
                if t.is_empty() || t.starts_with('[') || t.starts_with('#') {
                    popped.push(prev);
                    break;
                }
                popped.push(prev);
            }
            if popped_header {
                if out_lines
                    .last()
                    .is_some_and(|l| l.contains("Added by agent-hooks"))
                {
                    out_lines.pop();
                }
                removed += 1;
                i += 1;
                // Skip the row's remaining key lines forward.
                while i < lines.len() {
                    let next = lines[i].trim();
                    if next.is_empty() || next.starts_with('[') || next.starts_with('#') {
                        break;
                    }
                    if next.contains('=') {
                        i += 1;
                    } else {
                        break;
                    }
                }
                continue;
            }
            // No enclosing list header: not one of our rows. Restore the
            // popped lines in order and keep the current line.
            popped.reverse();
            out_lines.extend(popped);
            out_lines.push(line.to_string());
            i += 1;
            continue;
        }
        out_lines.push(line.to_string());
        i += 1;
    }

    if removed > 0 {
        let out = out_lines.join("\n");
        let cleaned = out
            .lines()
            .filter(|l| !l.contains("Added by agent-hooks"))
            .collect::<Vec<_>>()
            .join("\n");
        let final_out = if cleaned.trim().is_empty() {
            String::new()
        } else {
            cleaned + "\n"
        };
        atomic_write(path, &final_out)?;
    }
    Ok(removed)
}

// ---------------------------------------------------------------------------
// omp plugin unlink fallback (symlinked local-path installs)
// ---------------------------------------------------------------------------

/// Manual removal for symlinked local-path plugins, which
/// `omp plugin uninstall` (npm-backed) cannot remove.
///
/// Removes the `agent-hooks` symlink in `~/.omp/plugins/node_modules` when it
/// points at our staged dir (or is dangling), prunes its entries from
/// `~/.omp/plugins/package.json` + `omp-plugins.lock.json`, and returns true
/// when anything was removed. Never touches real directories.
fn manually_unlink_managed_plugin(staged_dir: Option<&Path>) -> bool {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_default();
    if home.is_empty() {
        return false;
    }
    let plugins = std::path::PathBuf::from(home).join(".omp").join("plugins");
    let link = plugins.join("node_modules").join("agent-hooks");
    let target = match std::fs::read_link(&link) {
        Ok(t) => t,
        Err(_) => return false, // not a symlink — leave alone
    };
    let ours =
        staged_dir.is_some_and(|dir| target.as_path() == dir) || std::fs::metadata(&link).is_err(); // dangling link
    if !ours {
        eprintln!(
            "  `agent-hooks` links elsewhere ({}); unlink it manually with:",
            target.display()
        );
        eprintln!(
            "    rm {:?} && omp plugin install ./crates/agent-hooks-adapters/builtin/omp/assets/oh-my-pi",
            link
        );
        return false;
    }
    let mut removed = false;
    if std::fs::remove_file(&link).is_ok() {
        println!("  removed plugin link {}", link.display());
        removed = true;
    }
    for (file, pointer) in [
        ("package.json", "/dependencies/agent-hooks"),
        ("omp-plugins.lock.json", "/plugins/agent-hooks"),
    ] {
        let path = plugins.join(file);
        if prune_json_key(&path, pointer) {
            println!("  pruned agent-hooks from {}", path.display());
            removed = true;
        }
    }
    removed
}

/// Remove a `/a/b`-style key from a JSON object file. Returns true when the
/// file was rewritten.
fn prune_json_key(path: &std::path::Path, pointer: &str) -> bool {
    let data = match std::fs::read_to_string(path) {
        Ok(d) => d,
        Err(_) => return false,
    };
    let stripped = strip_jsonc(&data);
    let mut root: serde_json::Value = match serde_json::from_str(&stripped) {
        Ok(v) => v,
        Err(_) => return false,
    };
    let mut parts: Vec<&str> = pointer.split('/').filter(|s| !s.is_empty()).collect();
    let last = match parts.pop() {
        Some(l) => l,
        None => return false,
    };
    let mut node = &mut root;
    for part in parts {
        match node.get_mut(part) {
            Some(next) => node = next,
            None => return false,
        }
    }
    if node.get(last).is_none() {
        return false;
    }
    node.as_object_mut().map(|m| m.remove(last));
    match serde_json::to_string_pretty(&root) {
        Ok(pretty) => atomic_write(path, &(pretty + "\n")).is_ok(),
        Err(_) => false,
    }
}
