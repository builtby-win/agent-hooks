use crate::cli::InstallArgs;
use crate::paths::expand_op_path;
use agent_hooks_adapters::manifest::{AdapterManifest, EditMode, EntryShape, InstallOp};
use agent_hooks_adapters::traits::normalize_hint;
use agent_hooks_adapters::AdapterRegistry;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Manifest resolution
// ---------------------------------------------------------------------------

/// Find the builtin manifest for an id or alias.
///
/// Mirrors `AdapterRegistry::get_versioned` matching: exact id/alias first,
/// then lenient substring (guarded against short-hint false positives).
pub(crate) fn find_manifest(harness: &str) -> Option<AdapterManifest> {
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
    // Lenient substring fallback — skip tiny hints ("pi" must not match
    // "anthropic"/"copilot" by substring).
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

fn all_manifests_sorted() -> Vec<AdapterManifest> {
    let mut manifests = AdapterManifest::load_tiered();
    manifests.sort_by(|a, b| a.id.cmp(&b.id));
    manifests
}

/// Resolve (manifest, version) for an install target, validating through the
/// registry per the runtime contract. Errors name the available ids/versions.
fn resolve_target(
    harness: &str,
    adapter_version: Option<&str>,
) -> anyhow::Result<(AdapterManifest, String)> {
    let manifest = find_manifest(harness).ok_or_else(|| {
        anyhow::anyhow!(
            "unknown harness '{}'. Known: {}, or 'all'",
            harness,
            AdapterRegistry::ids().join(", ")
        )
    })?;
    let version = adapter_version
        .unwrap_or(&manifest.default_version)
        .to_string();
    if !manifest.versions.contains_key(&version) {
        let mut available: Vec<&str> = manifest.versions.keys().map(String::as_str).collect();
        available.sort();
        anyhow::bail!(
            "unknown adapter version '{}' for harness '{}'. Available: {}",
            version,
            manifest.id,
            available.join(", ")
        );
    }
    // Registry is the runtime source of truth — the manifest must resolve there too.
    if AdapterRegistry::get_versioned(&manifest.id, Some(&version)).is_none() {
        anyhow::bail!(
            "harness '{}' version '{}' did not resolve via the adapter registry",
            manifest.id,
            version
        );
    }
    Ok((manifest, version))
}

fn resolve_binary_path(explicit: Option<&Path>) -> PathBuf {
    if let Some(p) = explicit {
        return p.to_path_buf();
    }
    // Prefer current exe, fallback to "agent-hooks" on PATH.
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from("agent-hooks"))
}

/// Shell command that invokes the hook shim. The binary path is double-quoted
/// when it contains whitespace (`C:\Program Files\...`, `~/My Tools/...`) —
/// valid for both POSIX shells and `cmd.exe`. Unchanged otherwise so
/// reinstalls stay idempotent against previously written entries.
fn hook_command(
    binary: &Path,
    harness: &str,
    event: &str,
    socket: Option<&Path>,
    context_cmd: Option<&Path>,
    origin: Option<&str>,
    adapter_version: Option<&str>,
) -> String {
    let mut command = format!("{} hook {} --event {}", shell_path(binary), harness, event);
    if let Some(path) = socket {
        command = format!("{} --socket {}", command, shell_path(path));
    }
    if let Some(path) = context_cmd {
        command = format!("{} --context-cmd {}", command, shell_path(path));
    }
    if let Some(origin) = origin.map(str::trim).filter(|o| !o.is_empty()) {
        command = format!("{} --origin {}", command, shell_word(origin));
    }
    if let Some(v) = adapter_version {
        command = format!("{} --adapter-version {}", command, v);
    }
    command
}

/// A label as one shell word: bare when it is only `[A-Za-z0-9@._:+/-]`
/// (ssh targets, host names), otherwise double-quoted with `"`/`\\` escaped.
fn shell_word(value: &str) -> String {
    if value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "@._:+/-".contains(c))
    {
        value.to_string()
    } else {
        format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

/// A path as one shell word: double-quoted only when it contains whitespace,
/// so whitespace-free paths render exactly as they always have.
fn shell_path(path: &Path) -> String {
    let s = path.display().to_string();
    if s.chars().any(char::is_whitespace) {
        format!("\"{}\"", s)
    } else {
        s
    }
}

/// Escape a hook command for a TOML basic string. Backslashes in Windows
/// paths (`C:\tools\...`) would otherwise parse as escapes (`\t` = tab).
fn toml_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}
/// Strip JSONC affordances (`//` line comments, `/* */` block comments, and
/// trailing commas) outside string literals so commented configs parse with
/// plain `serde_json`. String contents (including escapes) pass through
/// untouched; only ASCII framing bytes are ever removed, so UTF-8 stays valid.
fn strip_jsonc(input: &str) -> String {
    let b = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    let mut in_str = false;
    let mut esc = false;
    while i < b.len() {
        let c = b[i];
        if in_str {
            out.push(c);
            if esc {
                esc = false;
            } else if c == b'\\' {
                esc = true;
            } else if c == b'"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        if c == b'"' {
            in_str = true;
            out.push(c);
            i += 1;
            continue;
        }
        if c == b'/' && i + 1 < b.len() && (b[i + 1] == b'/' || b[i + 1] == b'*') {
            if b[i + 1] == b'/' {
                i += 2;
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            i += 2;
            while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(b.len());
            continue;
        }
        if c == b',' {
            let mut j = i + 1;
            while j < b.len() && matches!(b[j], b' ' | b'\t' | b'\n' | b'\r') {
                j += 1;
            }
            if j < b.len() && (b[j] == b'}' || b[j] == b']') {
                i += 1;
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| input.to_string())
}

/// Load a JSON(C) config file. Returns `None` (after a stderr warning naming
/// the path and reason) when the content is still unparseable — callers MUST
/// skip the file, never overwrite user content with `{}`.
fn load_json_doc(path: &Path) -> Option<serde_json::Value> {
    let data = std::fs::read_to_string(path).unwrap_or_else(|_| "{}".to_string());
    match serde_json::from_str(&strip_jsonc(&data)) {
        Ok(v) => Some(v),
        Err(e) => {
            eprintln!(
                "  warning: {}: skipping unparseable config ({})",
                path.display(),
                e
            );
            None
        }
    }
}

/// Atomically write `contents` to `path`: temp file in the same directory +
/// rename, so a crash mid-write never leaves a truncated config behind.
fn write_atomic(path: &Path, contents: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let tmp = PathBuf::from(format!("{}.tmp-{}", path.display(), std::process::id()));
    std::fs::write(&tmp, contents)?;
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// True when a hook `command` string is one of ours: it invokes agent-hooks
/// for this harness id or one of its manifest aliases. Comparisons use
/// [`normalize_hint`] (case-insensitive, `_`/space/`-` equivalent).
fn entry_is_ours(command: &str, id_norm: &str, alias_norms: &[String]) -> bool {
    let norm = normalize_hint(command);
    norm.contains("agent-hooks")
        && (norm.contains(id_norm)
            || alias_norms
                .iter()
                .any(|a| !a.is_empty() && norm.contains(a.as_str())))
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub fn run(args: InstallArgs) -> anyhow::Result<()> {
    let binary = resolve_binary_path(args.binary_path.as_deref());
    let target = args.harness.to_lowercase();

    let (targets, is_all): (Vec<(AdapterManifest, String)>, bool) = if target == "all" {
        // For "all", only install into detected harnesses (config exists or binary on PATH).
        let detected: Vec<(AdapterManifest, String)> = all_manifests_sorted()
            .into_iter()
            .filter(|m| is_detected(m))
            .map(|m| {
                let v = m.default_version.clone();
                (m, v)
            })
            .collect();
        if detected.is_empty() {
            println!("No detected harnesses found. Use --harness <name> to install for a specific harness.");
            println!("Known harnesses: {}", AdapterRegistry::ids().join(", "));
            println!("Binary path: {}", binary.display());
            return Ok(());
        }
        (detected, true)
    } else {
        let resolved = resolve_target(&target, args.adapter_version.as_deref())?;
        (vec![resolved], false)
    };

    let mut installed = 0usize;
    let mut current = 0usize;
    let mut skipped = 0usize;

    for (manifest, version) in &targets {
        match install_for_manifest(
            manifest,
            version,
            &binary,
            args.socket.as_deref(),
            args.adapter_version.as_deref(),
            !is_all,
            StageVars {
                bin: args.binary_path.as_deref(),
                socket: args.socket.as_deref(),
                context_cmd: args.context_cmd.as_deref(),
                origin: args.origin.as_deref(),
            },
        ) {
            Ok(outcome) => {
                if outcome.hooks > 0 {
                    println!(
                        "✓ {} — installed {} hook(s)",
                        manifest.display_name, outcome.hooks
                    );
                    installed += 1;
                } else if outcome.up_to_date {
                    println!(
                        "✓ {} — already installed (up to date)",
                        manifest.display_name
                    );
                    current += 1;
                } else {
                    println!(
                        "- {} — no config file found (skipped)",
                        manifest.display_name
                    );
                    skipped += 1;
                }
            }
            Err(e) => {
                eprintln!("✗ {} — failed: {}", manifest.display_name, e);
            }
        }
    }

    if installed == 0 && current == 0 && skipped > 0 {
        println!(
            "\nNo harness config files found. Create one or specify --harness <name>.\nBinary path used: {}",
            binary.display()
        );
        println!(
            "Hook command template: {} hook <harness> --event <Event>",
            binary.display()
        );
    } else if installed > 0 {
        println!(
            "\nInstalled hooks for {} harness(es). Restart the agent to activate.",
            installed
        );
        println!("Binary: {}", binary.display());
    } else {
        println!(
            "\nAll {} harness(es) already installed and up to date.",
            current
        );
        println!("Binary: {}", binary.display());
    }

    Ok(())
}

/// A harness counts as detected when any of its install paths already exists
/// or its binary is on PATH (id or alias).
fn is_detected(manifest: &AdapterManifest) -> bool {
    let Some(vs) = manifest.versions.get(&manifest.default_version) else {
        return false;
    };
    for op in &vs.install {
        match op {
            InstallOp::JsonHooks { paths, .. }
            | InstallOp::TomlHooks { paths, .. }
            | InstallOp::TomlListHooks { paths, .. }
            | InstallOp::JsonEdit { paths, .. } => {
                if paths.iter().any(|p| expand_op_path(p).exists()) {
                    return true;
                }
            }
            InstallOp::StageAssets { stage_dir, .. } => {
                if expand_op_path(stage_dir).exists() {
                    return true;
                }
            }
            InstallOp::RunCommand { .. } => {}
        }
    }
    std::iter::once(manifest.id.as_str())
        .chain(manifest.aliases.iter().map(String::as_str))
        .any(|name| is_binary_on_path(name))
}

fn is_binary_on_path(name: &str) -> bool {
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let candidate = dir.join(name);
            if candidate.exists() {
                return true;
            }
            // Windows exe extension
            if cfg!(windows) && dir.join(format!("{}.exe", name)).exists() {
                return true;
            }
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Op execution
// ---------------------------------------------------------------------------

struct Ctx<'a> {
    binary: &'a Path,
    socket: Option<&'a Path>,
    context_cmd: Option<&'a Path>,
    origin: Option<&'a str>,
    adapter_version: Option<&'a str>,
    stage_dir: Option<PathBuf>,
}

/// Install-time settings baked into staged plugin assets. Each is `None`
/// unless given on the command line, so an asset falls back to its own
/// resolution (`INSTALL_BIN || resolveBinary()`) instead of pinning
/// whatever binary happened to run the install.
#[derive(Clone, Copy, Default)]
struct StageVars<'a> {
    bin: Option<&'a Path>,
    socket: Option<&'a Path>,
    context_cmd: Option<&'a Path>,
    origin: Option<&'a str>,
}

/// Outcome of installing one harness: hooks written plus whether the harness
/// is present and up to date (nothing needed rewriting). The distinction
/// matters for plugin-bridge harnesses (e.g. Pi): a no-op re-run stages 0
/// files yet the harness is installed — reporting that as "skipped, no config
/// file" is wrong and sends users hunting for a config file that doesn't exist.
struct InstallOutcome {
    hooks: usize,
    up_to_date: bool,
}

fn install_for_manifest(
    manifest: &AdapterManifest,
    version: &str,
    binary: &Path,
    socket: Option<&Path>,
    adapter_version: Option<&str>,
    explicit: bool,
    vars: StageVars,
) -> anyhow::Result<InstallOutcome> {
    let vs = manifest
        .versions
        .get(version)
        .ok_or_else(|| anyhow::anyhow!("version '{}' not found", version))?;
    let mut ctx = Ctx {
        binary,
        socket,
        context_cmd: vars.context_cmd,
        origin: vars.origin,
        adapter_version,
        stage_dir: None,
    };
    let mut total = 0usize;
    let mut touched_config = false;
    let mut up_to_date = false;

    for op in &vs.install {
        match op {
            InstallOp::JsonHooks {
                paths,
                create,
                root,
                entry_shape,
                name,
                fixed_fields,
                entry,
                dedupe,
                entry_overrides,
            } => {
                let (n, touched) = op_json_hooks(
                    manifest,
                    version,
                    paths,
                    *create,
                    root,
                    *entry_shape,
                    name.as_deref(),
                    fixed_fields.as_ref(),
                    entry,
                    dedupe.as_deref(),
                    entry_overrides,
                    &ctx,
                    explicit,
                )?;
                total += n;
                touched_config = touched_config || touched;
            }
            InstallOp::TomlHooks {
                paths,
                create,
                root,
                entry_shape,
                entry,
            } => {
                let (n, touched) = op_toml_hooks(
                    manifest,
                    version,
                    paths,
                    *create,
                    root,
                    *entry_shape,
                    entry,
                    &ctx,
                    explicit,
                )?;
                total += n;
                touched_config = touched_config || touched;
            }
            InstallOp::TomlListHooks {
                paths,
                create,
                list_key,
                entry,
                entry_overrides,
            } => {
                let (n, touched) = op_toml_list_hooks(
                    manifest,
                    version,
                    paths,
                    *create,
                    list_key,
                    entry,
                    entry_overrides,
                    &ctx,
                    explicit,
                )?;
                total += n;
                touched_config = touched_config || touched;
            }
            InstallOp::StageAssets {
                stage_dir,
                assets,
                managed_marker,
            } => {
                let (dir, n) = op_stage_assets(
                    &manifest.id,
                    stage_dir,
                    assets,
                    managed_marker.as_deref(),
                    vars,
                )?;
                up_to_date = up_to_date || dir.is_dir();
                ctx.stage_dir = Some(dir);
                total += n;
                touched_config = touched_config || n > 0;
            }
            InstallOp::RunCommand {
                command, optional, ..
            } => {
                let ran = op_run_command(
                    command,
                    *optional,
                    ctx.stage_dir.as_deref(),
                    vs.notes.as_ref(),
                )?;
                up_to_date = up_to_date || ran;
            }
            InstallOp::JsonEdit {
                paths,
                pointer,
                mode,
                value,
                restore_on_uninstall,
                only_if_sole_json,
            } => {
                if let Some(sole) = only_if_sole_json.as_deref() {
                    if dir_has_other_json(&expand_op_path(sole)) {
                        println!(
                            "  note: other agent configs exist next to {sole}; leaving {pointer} unchanged"
                        );
                        continue;
                    }
                }
                let restore = *restore_on_uninstall && *mode == EditMode::Set;
                total += op_json_edit(paths, pointer, *mode, value.as_ref(), restore)?;
            }
        }
    }

    if let Some(notes) = vs.notes.as_ref() {
        if total > 0 || touched_config {
            for line in &notes.post_install {
                println!("  note: {}", line);
            }
        }
    }

    up_to_date = up_to_date || touched_config || total > 0;
    // Non-explicit ("all") installs that touched nothing report hooks 0 so the
    // caller prints "skipped" — unless the harness is already installed and up
    // to date (plugin-bridge harnesses like Pi have no config file to touch on
    // a no-op re-run).
    if !explicit && !up_to_date {
        return Ok(InstallOutcome {
            hooks: 0,
            up_to_date: false,
        });
    }
    Ok(InstallOutcome {
        hooks: total,
        up_to_date,
    })
}

/// First existing path, else the first listed path when `create` (or the
/// install is explicit and this is the primary path).
fn pick_target_path(paths: &[String], create: bool, explicit: bool) -> Option<PathBuf> {
    for p in paths {
        let expanded = expand_op_path(p);
        if expanded.exists() {
            return Some(expanded);
        }
    }
    if create || explicit {
        paths.first().map(|p| expand_op_path(p))
    } else {
        None
    }
}

// --- json_hooks ---

/// Render the handler object for one host event: merge per-event overrides,
/// then interpolate `{event}` / `{command}` in every string. The `command`
/// field is always the canonical [`hook_command`] output (binary-aware +
/// socket/version stamped) unless the template embeds `{command}` inside a
/// larger custom command, in which case the template shape is kept.
fn render_entry(
    entry: &serde_json::Value,
    overrides: Option<&serde_json::Value>,
    host_event: &str,
    command: &str,
) -> serde_json::Value {
    let mut handler = entry.clone();
    if let (Some(h), Some(o)) = (
        handler.as_object_mut(),
        overrides.and_then(|v| v.as_object()),
    ) {
        for (k, v) in o {
            h.insert(k.clone(), v.clone());
        }
    }
    // Canonical command, unless the template builds a custom one around {command}.
    if let Some(obj) = handler.as_object_mut() {
        let custom = obj
            .get("command")
            .and_then(|c| c.as_str())
            .is_some_and(|s| s.contains("{command}"));
        if !custom {
            obj.insert(
                "command".to_string(),
                serde_json::Value::String(command.to_string()),
            );
        }
    }
    substitute_strings(&mut handler, host_event, command);
    handler
}

fn substitute_strings(value: &mut serde_json::Value, event: &str, command: &str) {
    match value {
        serde_json::Value::String(s) => {
            if s.contains("{event}") || s.contains("{command}") {
                *s = s.replace("{command}", command).replace("{event}", event);
            }
        }
        serde_json::Value::Array(arr) => {
            for v in arr {
                substitute_strings(v, event, command);
            }
        }
        serde_json::Value::Object(map) => {
            for v in map.values_mut() {
                substitute_strings(v, event, command);
            }
        }
        _ => {}
    }
}

/// Recursively collect every `command` string under `value` (descends into
/// matcher-group `hooks` arrays and named bundles).
fn collect_commands(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(cmd) = map.get("command").and_then(|c| c.as_str()) {
                out.push(cmd.to_string());
            }
            for v in map.values() {
                collect_commands(v, out);
            }
        }
        serde_json::Value::Array(arr) => {
            for v in arr {
                collect_commands(v, out);
            }
        }
        _ => {}
    }
}

/// Remove our stale entries (same harness id or manifest alias, e.g. entries
/// installed under an old id or a previous binary path) from an event array
/// in place, descending into matcher-group `hooks` arrays and dropping groups
/// left empty. Foreign entries and groups are never touched. Returns true
/// when anything was removed.
fn remove_stale_ours(
    entries: &mut Vec<serde_json::Value>,
    harness_id: &str,
    aliases: &[String],
) -> bool {
    let id_norm = normalize_hint(harness_id);
    let alias_norms: Vec<String> = aliases.iter().map(|a| normalize_hint(a)).collect();
    let is_ours = |s: &str| entry_is_ours(s, &id_norm, &alias_norms);
    let mut removed = false;
    entries.retain_mut(|entry| {
        if let Some(hooks) = entry.get_mut("hooks").and_then(|h| h.as_array_mut()) {
            hooks.retain(|h| {
                let keep = !h
                    .get("command")
                    .and_then(|c| c.as_str())
                    .is_some_and(&is_ours);
                if !keep {
                    removed = true;
                }
                keep
            });
            // Drop matcher groups left empty by stale removal; keep foreign groups.
            if hooks.is_empty() {
                removed = true;
                return false;
            }
            return true;
        }
        let ours = entry
            .get("command")
            .and_then(|c| c.as_str())
            .is_some_and(&is_ours);
        if ours {
            removed = true;
            return false;
        }
        // Named-bundle leftovers or nested shapes: recurse generically.
        if entry.is_object() || entry.is_array() {
            let mut cmds = Vec::new();
            collect_commands(entry, &mut cmds);
            if cmds.iter().any(|c| is_ours(c)) {
                removed = true;
                return false;
            }
        }
        true
    });
    removed
}

/// Effective entry shape for one host event: an event spec may override the
/// install op's shape (Antigravity lifecycle events install flat inside the
/// named bundle; its tool events keep matcher groups).
fn event_entry_shape(
    manifest: &AdapterManifest,
    version: &str,
    op_shape: EntryShape,
    host_event: &str,
) -> EntryShape {
    manifest
        .versions
        .get(version)
        .and_then(|v| v.events.get(host_event))
        .and_then(|e| e.entry_shape)
        .unwrap_or(op_shape)
}

/// True when `entries` already holds `command` in the shape `shape`
/// prescribes. Command-only matching is not enough: a same-command entry in
/// the wrong shape would otherwise pin the malformed config forever, since
/// dedupe short-circuits before stale removal can rewrite it.
fn has_entry_in_shape(entries: &[serde_json::Value], command: &str, shape: EntryShape) -> bool {
    entries.iter().any(|entry| match shape {
        EntryShape::Flat => {
            entry.get("hooks").is_none()
                && entry.get("command").and_then(|c| c.as_str()) == Some(command)
        }
        EntryShape::MatcherGroup | EntryShape::Named => entry
            .get("hooks")
            .and_then(|h| h.as_array())
            .is_some_and(|hooks| {
                hooks
                    .iter()
                    .any(|h| h.get("command").and_then(|c| c.as_str()) == Some(command))
            }),
        EntryShape::Command => entry.as_str() == Some(command),
    })
}

fn navigate_parent<'a>(
    root: &'a mut serde_json::Value,
    segments: &[String],
) -> anyhow::Result<&'a mut serde_json::Map<String, serde_json::Value>> {
    let mut node = root;
    for seg in segments {
        if !node.is_object() {
            anyhow::bail!("config node '{}' is not an object", seg);
        }
        node = node
            .as_object_mut()
            .expect("checked")
            .entry(seg.clone())
            .or_insert_with(|| serde_json::Value::Object(Default::default()));
    }
    node.as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("config node is not an object"))
}

/// See the call site in [`op_json_hooks`]. Never creates nodes.
fn prune_normalize_only_json(
    doc: &mut serde_json::Value,
    manifest: &AdapterManifest,
    version: &str,
    root: &str,
    name: Option<&str>,
) -> bool {
    let Some(vs) = manifest.versions.get(version) else {
        return false;
    };
    let group = name.unwrap_or("agent-hooks");
    let mut pruned = false;
    for (host_event, _) in vs.events.iter().filter(|(_, spec)| !spec.install) {
        let segs: Vec<String> = root
            .split('.')
            .map(|s| s.replace("{event}", host_event).replace("{name}", group))
            .collect();
        let Some((key, parents)) = segs.split_last() else {
            continue;
        };
        let pointer: String = parents
            .iter()
            .map(|seg| format!("/{}", seg.replace('~', "~0").replace('/', "~1")))
            .collect();
        let Some(parent) = doc.pointer_mut(&pointer).and_then(|v| v.as_object_mut()) else {
            continue;
        };
        let Some(arr) = parent.get_mut(key.as_str()).and_then(|v| v.as_array_mut()) else {
            continue;
        };
        if remove_stale_ours(arr, &manifest.id, &manifest.aliases) {
            pruned = true;
            if arr.is_empty() {
                parent.remove(key.as_str());
            }
        }
    }
    pruned
}

#[allow(clippy::too_many_arguments)]
fn op_json_hooks(
    manifest: &AdapterManifest,
    version: &str,
    paths: &[String],
    create: bool,
    root: &str,
    entry_shape: EntryShape,
    name: Option<&str>,
    fixed_fields: Option<&serde_json::Map<String, serde_json::Value>>,
    entry: &serde_json::Value,
    _dedupe: Option<&str>,
    entry_overrides: &std::collections::BTreeMap<String, serde_json::Value>,
    ctx: &Ctx,
    explicit: bool,
) -> anyhow::Result<(usize, bool)> {
    let Some(target) = pick_target_path(paths, create, explicit) else {
        return Ok((0, false));
    };
    let existed = target.exists();

    let mut doc: serde_json::Value = if target.exists() {
        match load_json_doc(&target) {
            Some(d) => d,
            // Unparseable configs are skipped with a warning, never clobbered.
            None => return Ok((0, existed)),
        }
    } else {
        serde_json::Value::Object(Default::default())
    };
    if !doc.is_object() {
        eprintln!(
            "  warning: {}: root is not an object, skipping",
            target.display()
        );
        return Ok((0, existed));
    }

    let mut fixed_changed = false;
    if let Some(fields) = fixed_fields {
        let obj = doc.as_object_mut().expect("checked");
        for (k, v) in fields {
            if !obj.contains_key(k) {
                obj.insert(k.clone(), v.clone());
                fixed_changed = true;
            }
        }
    }

    // Host events come from the installing version's events map (canonical spelling).
    let host_events = version_events(manifest, version);

    let mut added = 0usize;
    for host_event in &host_events {
        let command = hook_command(
            ctx.binary,
            &manifest.id,
            host_event,
            ctx.socket,
            ctx.context_cmd,
            ctx.origin,
            ctx.adapter_version,
        );
        let handler = render_entry(entry, entry_overrides.get(host_event), host_event, &command);
        let event_shape = event_entry_shape(manifest, version, entry_shape, host_event);
        let wrapped = match event_shape {
            EntryShape::Flat => handler,
            EntryShape::MatcherGroup | EntryShape::Named => serde_json::json!({
                "matcher": "*",
                "hooks": [handler]
            }),
            EntryShape::Command => {
                anyhow::bail!("entry_shape \"command\" is only supported by toml_hooks")
            }
        };

        // root like "hooks.{event}" or "{name}.{event}".
        let group = name.unwrap_or("agent-hooks");
        let segs: Vec<String> = root
            .split('.')
            .map(|s| s.replace("{event}", host_event).replace("{name}", group))
            .collect();
        let (parent_segs, event_key) = match segs.split_last() {
            Some((last, parent)) => (parent, last.clone()),
            None => anyhow::bail!("install root '{}' is empty", root),
        };
        // Per-event fault isolation: one malformed node (non-array where an
        // array is expected, unnavigable root) warns naming the host event
        // and continues with the remaining events.
        let step = (|| -> anyhow::Result<bool> {
            let parent = navigate_parent(&mut doc, parent_segs)?;
            if entry_shape == EntryShape::Named {
                parent
                    .entry("enabled".to_string())
                    .or_insert(serde_json::Value::Bool(true));
            }
            let arr = parent
                .entry(event_key)
                .or_insert_with(|| serde_json::Value::Array(Vec::new()));
            let vec = arr.as_array_mut().ok_or_else(|| {
                anyhow::anyhow!("hook entry for '{}' is not an array", host_event)
            })?;

            // dedupe "command_subset": skip when the exact command already
            // exists in the shape this event prescribes. The same command in
            // a wrong shape (e.g. matcher-wrapped lifecycle hooks the host's
            // parser rejects wholesale) must not short-circuit —
            // remove_stale_ours prunes it so the reinstall rewrites it.
            if has_entry_in_shape(vec, &command, event_shape) {
                return Ok(false);
            }
            remove_stale_ours(vec, &manifest.id, &manifest.aliases);
            vec.push(wrapped);
            Ok(true)
        })();
        match step {
            Ok(true) => added += 1,
            Ok(false) => {}
            Err(e) => eprintln!("  warning: event '{}': skipping ({})", host_event, e),
        }
    }
    // Normalize-only events (`"install": false`) may still carry our entries
    // from an older install that registered every mapped key; prune them so
    // the host only sees the keys it fires. Foreign entries are kept, and a
    // key left empty is removed.
    let pruned = prune_normalize_only_json(&mut doc, manifest, version, root, name);
    if added == 0 && !fixed_changed && !pruned {
        return Ok((0, existed));
    }
    let pretty = serde_json::to_string_pretty(&doc)?;
    write_atomic(&target, &(pretty + "\n"))?;
    Ok((added, true))
}

// --- toml_hooks ---

#[allow(clippy::too_many_arguments)]
fn op_toml_hooks(
    manifest: &AdapterManifest,
    version: &str,
    paths: &[String],
    create: bool,
    root: &str,
    entry_shape: EntryShape,
    entry: &serde_json::Value,
    ctx: &Ctx,
    explicit: bool,
) -> anyhow::Result<(usize, bool)> {
    let Some(target) = pick_target_path(paths, create, explicit) else {
        return Ok((0, false));
    };
    if entry_shape == EntryShape::Command {
        return toml_command_hooks(manifest, version, &target, root, ctx);
    }

    let existing = if target.exists() {
        std::fs::read_to_string(&target).unwrap_or_default()
    } else {
        String::new()
    };
    // Idempotent: our harness entries already present (id or any alias, so
    // reinstalls under a renamed id don't duplicate entries).
    let ours_present = std::iter::once(&manifest.id)
        .chain(manifest.aliases.iter())
        .any(|id| existing.contains(&format!("agent-hooks hook {}", id)));
    if ours_present {
        return Ok((0, true));
    }

    let host_events = version_events(manifest, version);
    let mut out = existing;
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&format!(
        "\n# Added by agent-hooks for harness '{}'\n",
        manifest.id
    ));
    for host_event in &host_events {
        let command = hook_command(
            ctx.binary,
            &manifest.id,
            host_event,
            ctx.socket,
            ctx.context_cmd,
            ctx.origin,
            ctx.adapter_version,
        );
        let handler = render_entry_toml(entry, host_event, &command);
        let dotted = root.replace("{event}", host_event);
        let event_shape = event_entry_shape(manifest, version, entry_shape, host_event);
        match event_shape {
            EntryShape::Flat => {
                out.push_str(&format!("[{}]\n", dotted));
                out.push_str(&handler);
                out.push('\n');
            }
            // Only reachable through a per-event override; the bare-string
            // form still beats a table jcode-style harnesses can't parse.
            EntryShape::Command => {
                out.push_str(&format!("{} = \"{}\"\n", dotted, toml_escape(&command)));
            }
            EntryShape::MatcherGroup | EntryShape::Named => {
                out.push_str(&format!("[[{}]]\nmatcher = \"*\"\n", dotted));
                out.push_str(&format!("[[{}.hooks]]\n", dotted));
                out.push_str(&handler);
                out.push('\n');
            }
        }
    }

    write_atomic(&target, &out)?;
    Ok((host_events.len(), true))
}

/// `op: "toml_list_hooks"`: append one `[[<list_key>]]` table per installed
/// host event to a shared TOML config whose hook format has no per-event
/// key (Kimi Code `config.toml`, Mistral Vibe `hooks.toml`). Text-append
/// under a marker comment like `toml_hooks`, so unrelated tables, rows,
/// and comments survive untouched. Idempotent via the same
/// installer-command check (own id or any alias): a re-run that finds our
/// command already present writes nothing.
#[allow(clippy::too_many_arguments)]
fn op_toml_list_hooks(
    manifest: &AdapterManifest,
    version: &str,
    paths: &[String],
    create: bool,
    list_key: &str,
    entry: &serde_json::Value,
    entry_overrides: &std::collections::BTreeMap<String, serde_json::Value>,
    ctx: &Ctx,
    explicit: bool,
) -> anyhow::Result<(usize, bool)> {
    let Some(target) = pick_target_path(paths, create, explicit) else {
        return Ok((0, false));
    };

    let existing = if target.exists() {
        std::fs::read_to_string(&target).unwrap_or_default()
    } else {
        String::new()
    };
    let ours_present = std::iter::once(&manifest.id)
        .chain(manifest.aliases.iter())
        .any(|id| existing.contains(&format!("agent-hooks hook {}", id)));
    if ours_present {
        return Ok((0, true));
    }

    let host_events = version_events(manifest, version);
    let mut out = existing;
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&format!(
        "\n# Added by agent-hooks for harness '{}'\n",
        manifest.id
    ));
    for host_event in &host_events {
        let command = hook_command(
            ctx.binary,
            &manifest.id,
            host_event,
            ctx.socket,
            ctx.context_cmd,
            ctx.origin,
            ctx.adapter_version,
        );
        let handler = render_entry(entry, entry_overrides.get(host_event), host_event, &command);
        let Some(obj) = handler.as_object() else {
            continue;
        };
        out.push_str(&format!("[[{}]]\n", list_key));
        push_toml_keys(&mut out, obj);
        out.push('\n');
    }

    write_atomic(&target, &out)?;
    Ok((host_events.len(), true))
}

/// `entry_shape: "command"`: set `<root>` (e.g. `hooks.<event>`) to the bare
/// command string, structurally, so an existing `[hooks]` table is reused
/// and user comments survive. Mirrors legacy B2V agent_config/agents/jcode.rs
/// `write_jcode_hooks`: one command per event, so a foreign command already
/// on an event is left alone (warned), ours is rewritten in place.
fn toml_command_hooks(
    manifest: &AdapterManifest,
    version: &str,
    target: &Path,
    root: &str,
    ctx: &Ctx,
) -> anyhow::Result<(usize, bool)> {
    let existed = target.exists();
    let raw = if existed {
        std::fs::read_to_string(target)?
    } else {
        String::new()
    };
    let mut doc: toml_edit::Document = match raw.parse() {
        Ok(d) => d,
        Err(e) => {
            eprintln!(
                "  warning: {}: invalid TOML ({e}), skipping",
                target.display()
            );
            return Ok((0, existed));
        }
    };
    let id_norm = normalize_hint(&manifest.id);
    let alias_norms: Vec<String> = manifest.aliases.iter().map(|a| normalize_hint(a)).collect();
    let mut changed = 0usize;
    let mut present = 0usize;
    for host_event in version_events(manifest, version) {
        let command = hook_command(
            ctx.binary,
            &manifest.id,
            &host_event,
            ctx.socket,
            ctx.context_cmd,
            ctx.origin,
            ctx.adapter_version,
        );
        let dotted = root.replace("{event}", &host_event);
        let Some((parents, key)) =
            dotted
                .split('.')
                .collect::<Vec<_>>()
                .split_last()
                .map(|(k, p)| {
                    (
                        p.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
                        k.to_string(),
                    )
                })
        else {
            anyhow::bail!("install root '{}' is empty", root);
        };
        let Some(table) = toml_table_at(doc.as_table_mut(), &parents) else {
            eprintln!(
                "  warning: {}: '{}' is not a table, skipping {host_event}",
                target.display(),
                parents.join(".")
            );
            continue;
        };
        match table.get(&key).and_then(|i| i.as_str()) {
            Some(cur) if cur == command => {
                present += 1;
                continue;
            }
            Some(cur) if !entry_is_ours(cur, &id_norm, &alias_norms) => {
                eprintln!(
                    "  warning: {}: {dotted} already runs another command (one per event); left unchanged",
                    target.display()
                );
                continue;
            }
            None if table.contains_key(&key) => {
                eprintln!(
                    "  warning: {}: {dotted} is not a command string; left unchanged",
                    target.display()
                );
                continue;
            }
            _ => {}
        }
        table.insert(&key, toml_edit::value(command));
        changed += 1;
    }
    // Normalize-only events may carry our commands from an older install
    // that registered every mapped event; drop those so the host only sees
    // the keys it supports.
    let normalize_only = manifest
        .versions
        .get(version)
        .map(|v| {
            v.events
                .iter()
                .filter(|(_, spec)| !spec.install)
                .map(|(k, _)| k.clone())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    for host_event in normalize_only {
        let dotted = root.replace("{event}", &host_event);
        let parts: Vec<String> = dotted.split('.').map(str::to_string).collect();
        let Some((key, parents)) = parts.split_last() else {
            continue;
        };
        let Some(table) = toml_table_get_mut(doc.as_table_mut(), parents) else {
            continue;
        };
        if table
            .get(key)
            .and_then(|i| i.as_str())
            .is_some_and(|cur| entry_is_ours(cur, &id_norm, &alias_norms))
        {
            table.remove(key);
            changed += 1;
        }
    }
    if changed > 0 {
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        write_atomic(target, &doc.to_string())?;
    }
    Ok((changed, changed > 0 || present > 0 || existed))
}

/// Walk existing dotted `parents` tables without creating any.
fn toml_table_get_mut<'a>(
    table: &'a mut toml_edit::Table,
    parents: &[String],
) -> Option<&'a mut toml_edit::Table> {
    let Some((first, rest)) = parents.split_first() else {
        return Some(table);
    };
    toml_table_get_mut(table.get_mut(first)?.as_table_mut()?, rest)
}

/// Walk (creating as needed) the dotted `parents` tables; `None` when a
/// segment exists but is not a table.
fn toml_table_at<'a>(
    table: &'a mut toml_edit::Table,
    parents: &[String],
) -> Option<&'a mut toml_edit::Table> {
    let Some((first, rest)) = parents.split_first() else {
        return Some(table);
    };
    let next = table
        .entry(first)
        .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()))
        .as_table_mut()?;
    toml_table_at(next, rest)
}

/// Render an entry template as TOML key lines (`command` escaped).
fn render_entry_toml(entry: &serde_json::Value, event: &str, command: &str) -> String {
    let mut handler = entry.clone();
    if let Some(obj) = handler.as_object_mut() {
        obj.insert(
            "command".to_string(),
            serde_json::Value::String(command.to_string()),
        );
    }
    substitute_strings(&mut handler, event, command);
    let mut out = String::new();
    if let Some(obj) = handler.as_object() {
        push_toml_keys(&mut out, obj);
    }
    out
}

/// Serialize one flat JSON object as TOML `key = value` lines (sorted keys).
/// Strings are quoted, numbers/bools are bare, and anything nested falls
/// back to its JSON encoding in a quoted string.
fn push_toml_keys(out: &mut String, obj: &serde_json::Map<String, serde_json::Value>) {
    let mut keys: Vec<&String> = obj.keys().collect();
    keys.sort();
    for k in keys {
        let v = &obj[k];
        match v {
            serde_json::Value::String(s) => {
                out.push_str(&format!("{} = \"{}\"\n", k, toml_escape(s)));
            }
            serde_json::Value::Number(_) | serde_json::Value::Bool(_) => {
                out.push_str(&format!("{} = {}\n", k, v));
            }
            _ => {
                out.push_str(&format!(
                    "{} = \"{}\"\n",
                    k,
                    toml_escape(&serde_json::to_string(v).unwrap_or_default())
                ));
            }
        }
    }
}

// --- stage_assets ---

fn op_stage_assets(
    harness: &str,
    stage_dir_raw: &str,
    assets: &[agent_hooks_adapters::manifest::AssetSpec],
    managed_marker: Option<&str>,
    vars: StageVars,
) -> anyhow::Result<(PathBuf, usize)> {
    let dir = expand_op_path(stage_dir_raw);
    std::fs::create_dir_all(&dir)?;
    let mut written = 0usize;
    for asset in assets {
        let mut content = if let Some(key) = asset.embedded.as_deref() {
            agent_hooks_adapters::embedded_asset(harness, key)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "unknown embedded asset '{harness}/{key}' — expected a file under \
                         crates/agent-hooks-adapters/builtin/{harness}/assets/{key}"
                    )
                })?
                .to_string()
        } else if let Some(t) = asset.template.as_ref() {
            match t {
                serde_json::Value::String(s) => s.clone(),
                _ => serde_json::to_string_pretty(t)?,
            }
        } else {
            anyhow::bail!("asset '{}' has neither embedded nor template", asset.path);
        };
        // Managed marker is a source comment (`//! ...`, `// ...`). Only valid
        // in code assets — prepending it to JSON (package.json) corrupts the
        // file and breaks consumers that parse it (`omp plugin install` ->
        // `SyntaxError: Failed to parse JSON`). Skip JSON assets.
        let is_json = std::path::Path::new(&asset.path)
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("json"));
        // Always rendered from the embedded template, so a reinstall with
        // different settings rewrites the file cleanly.
        if is_code_asset(&asset.path) {
            content = substitute_install_placeholders(&content, vars);
        }
        if let Some(marker) = managed_marker {
            if !is_json && !content.starts_with(marker) {
                content = format!("{}\n{}", marker, content);
            }
        }
        let dest = dir.join(&asset.path);
        if let Some(parent) = dest.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let existing = std::fs::read_to_string(&dest).ok();
        if existing.as_deref() != Some(content.as_str()) {
            write_atomic(&dest, &content)?;
            written += 1;
        }
    }
    Ok((dir, written))
}

/// Script assets that carry install placeholders. JSON/markdown assets are
/// never rewritten: a token there is data, not a setting.
fn is_code_asset(path: &str) -> bool {
    Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| {
            ["ts", "js", "mjs", "cjs", "mts", "cts", "py"]
                .iter()
                .any(|x| e.eq_ignore_ascii_case(x))
        })
}

/// Replace each quoted placeholder token (`"__AGENT_HOOKS_BIN__"`, same for
/// `_SOCKET__` / `_CONTEXT_CMD__`, single or double quotes) with a JSON
/// string literal — valid in JS/TS and Python alike, and escape-safe for
/// quotes/backslashes in paths. Unset settings become `""` so the asset's
/// own fallback applies.
fn substitute_install_placeholders(content: &str, vars: StageVars) -> String {
    let literal = |p: Option<&Path>| {
        serde_json::to_string(&p.map(|p| p.display().to_string()).unwrap_or_default())
            .expect("string serializes")
    };
    let mut out = content.to_string();
    for (token, value) in [
        ("__AGENT_HOOKS_BIN__", literal(vars.bin)),
        ("__AGENT_HOOKS_SOCKET__", literal(vars.socket)),
        ("__AGENT_HOOKS_CONTEXT_CMD__", literal(vars.context_cmd)),
        (
            "__AGENT_HOOKS_ORIGIN__",
            serde_json::to_string(vars.origin.unwrap_or_default()).expect("string serializes"),
        ),
    ] {
        for q in ['"', '\x27'] {
            out = out.replace(&format!("{q}{token}{q}"), &value);
        }
    }
    out
}

// --- run_command ---

/// Run a manifest `run_command` op. Returns `true` when the command actually
/// executed successfully, `false` when an optional command was skipped
/// (binary missing / failed but tolerated). Required failures still bail.
fn op_run_command(
    command_raw: &str,
    optional: bool,
    stage_dir: Option<&Path>,
    notes: Option<&agent_hooks_adapters::manifest::Notes>,
) -> anyhow::Result<bool> {
    let stage = stage_dir
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let rendered = command_raw.replace("{stage_dir}", &stage);
    let program = rendered.split_whitespace().next().unwrap_or_default();

    if !program.is_empty() && !executable_available(program) {
        if optional {
            print_notes(notes);
            println!("  (skipped '{}': '{}' not on PATH)", rendered, program);
            return Ok(false);
        }
        anyhow::bail!("could not run '{}': '{}' not on PATH", rendered, program);
    }

    match run_shell_timeout(&rendered) {
        Ok(Some(s)) if s.success() => Ok(true),
        Ok(Some(s)) => {
            print_notes(notes);
            if optional {
                println!("  (command exited with {}: {})", s, rendered);
                Ok(false)
            } else {
                anyhow::bail!("command exited with {}: {}", s, rendered)
            }
        }
        // Timeout counts as command failure: optional prints notes and skips,
        // required bails.
        Ok(None) => {
            print_notes(notes);
            if optional {
                println!("  (command timed out after 120s: {})", rendered);
                Ok(false)
            } else {
                anyhow::bail!("command timed out after 120s: {}", rendered)
            }
        }
        Err(e) => {
            print_notes(notes);
            if optional {
                println!("  (could not run '{}': {})", rendered, e);
                Ok(false)
            } else {
                anyhow::bail!("could not run '{}': {}", rendered, e)
            }
        }
    }
}

/// Run `rendered` through the platform shell, killing the child if it exceeds
/// 120s. Returns `Ok(None)` on timeout (treated as failure by the caller).
fn run_shell_timeout(rendered: &str) -> std::io::Result<Option<std::process::ExitStatus>> {
    use std::time::{Duration, Instant};
    const TIMEOUT: Duration = Duration::from_secs(120);

    #[cfg(unix)]
    let mut child = std::process::Command::new("sh")
        .args(["-c", rendered])
        .spawn()?;
    #[cfg(windows)]
    let mut child = std::process::Command::new("cmd")
        .args(["/C", rendered])
        .spawn()?;
    #[cfg(not(any(unix, windows)))]
    return Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "unsupported platform",
    ));

    let deadline = Instant::now() + TIMEOUT;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        if Instant::now() >= deadline {
            // Child kill (a unix process-group kill would also work but costs
            // a pre_exec hook); reap the child to avoid zombies.
            let _ = child.kill();
            let _ = child.wait();
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn executable_available(program: &str) -> bool {
    if program.contains('/') || program.contains('\\') {
        return Path::new(program).exists();
    }
    is_binary_on_path(program)
}

fn print_notes(notes: Option<&agent_hooks_adapters::manifest::Notes>) {
    if let Some(n) = notes {
        for line in &n.post_install {
            println!("  note: {}", line);
        }
    }
}

// --- json_edit ---

fn op_json_edit(
    paths: &[String],
    pointer: &str,
    mode: EditMode,
    value: Option<&serde_json::Value>,
    record_previous: bool,
) -> anyhow::Result<usize> {
    let mut changed = 0usize;
    for raw in paths {
        let path = expand_op_path(raw);
        if !path.exists() {
            if mode != EditMode::Set {
                continue;
            }
            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent)?;
                }
            }
            write_atomic(&path, "{}\n")?;
        }
        let data = std::fs::read_to_string(&path).unwrap_or_else(|_| "{}".to_string());
        let mut doc: serde_json::Value = match serde_json::from_str(&strip_jsonc(&data)) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("  warning: {}: not JSON, skipping ({})", path.display(), e);
                continue;
            }
        };
        let before = changed;
        let parts = parse_pointer(pointer)?;
        match mode {
            EditMode::AppendUnique => {
                let Some(v) = value else { continue };
                let node = navigate_create(&mut doc, &parts)?;
                let arr = node.as_array_mut().ok_or_else(|| {
                    anyhow::anyhow!("{}: '{}' is not an array", path.display(), pointer)
                })?;
                if !arr.contains(v) {
                    arr.push(v.clone());
                    changed += 1;
                }
            }
            EditMode::Set => {
                let Some(v) = value else { continue };
                if record_previous {
                    record_json_edit_state(&path, pointer, doc.pointer(pointer), v)?;
                }
                if set_pointer(&mut doc, &parts, v.clone()) {
                    // Legacy B2V kiro.rs `set_kiro_default_agent` keeps a
                    // backup copy of the settings file it rewrites.
                    if record_previous {
                        let _ = std::fs::copy(&path, json_edit_backup_path(&path));
                    }
                    changed += 1;
                }
            }
            EditMode::Remove => {
                if remove_pointer(&mut doc, &parts) {
                    changed += 1;
                }
            }
        }
        if changed > before {
            write_atomic(&path, &(serde_json::to_string_pretty(&doc)? + "\n"))?;
        }
    }
    Ok(changed)
}

/// Sidecar recording what a `restore_on_uninstall` edit replaced, keyed by
/// pointer: `{"<pointer>": {"set": <ours>, "previous": <old>}}`, with
/// `previous` omitted when the key did not exist.
fn json_edit_state_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".agent-hooks-state.json");
    path.with_file_name(name)
}

fn json_edit_backup_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".agent-hooks.bak");
    path.with_file_name(name)
}

fn read_json_edit_state(path: &Path) -> serde_json::Map<String, serde_json::Value> {
    std::fs::read_to_string(json_edit_state_path(path))
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

/// First install wins: a reinstall must not overwrite the user's original
/// value with our own.
fn record_json_edit_state(
    path: &Path,
    pointer: &str,
    previous: Option<&serde_json::Value>,
    ours: &serde_json::Value,
) -> anyhow::Result<()> {
    let mut state = read_json_edit_state(path);
    if state.contains_key(pointer) {
        return Ok(());
    }
    let mut entry = serde_json::Map::new();
    entry.insert("set".into(), ours.clone());
    if let Some(prev) = previous {
        entry.insert("previous".into(), prev.clone());
    }
    state.insert(pointer.to_string(), serde_json::Value::Object(entry));
    write_atomic(
        &json_edit_state_path(path),
        &(serde_json::to_string_pretty(&state)? + "\n"),
    )?;
    Ok(())
}

/// Undo a `restore_on_uninstall` json_edit. Only acts while the key still
/// holds our value (a user who changed it since keeps their choice): puts
/// back the recorded previous value, else removes the key. Returns the
/// number of files changed.
pub(crate) fn revert_json_edit(
    paths: &[String],
    pointer: &str,
    ours: &serde_json::Value,
) -> anyhow::Result<usize> {
    let mut changed = 0usize;
    for raw in paths {
        let path = expand_op_path(raw);
        let Ok(data) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(mut doc) = serde_json::from_str::<serde_json::Value>(&strip_jsonc(&data)) else {
            eprintln!("  warning: {}: not JSON, leaving {pointer}", path.display());
            continue;
        };
        let mut state = read_json_edit_state(&path);
        let recorded = state.remove(pointer);
        let ours = recorded
            .as_ref()
            .and_then(|r| r.get("set"))
            .unwrap_or(ours)
            .clone();
        if doc.pointer(pointer) == Some(&ours) {
            let parts = parse_pointer(pointer)?;
            let edited = match recorded.as_ref().and_then(|r| r.get("previous")) {
                Some(prev) => set_pointer(&mut doc, &parts, prev.clone()),
                None => remove_pointer(&mut doc, &parts),
            };
            if edited {
                write_atomic(&path, &(serde_json::to_string_pretty(&doc)? + "\n"))?;
                changed += 1;
            }
        }
        if recorded.is_some() {
            let state_path = json_edit_state_path(&path);
            if state.is_empty() {
                let _ = std::fs::remove_file(state_path);
            } else {
                write_atomic(&state_path, &(serde_json::to_string_pretty(&state)? + "\n"))?;
            }
        }
    }
    Ok(changed)
}

/// True when `file`'s directory holds a `*.json` other than `file` itself.
fn dir_has_other_json(file: &Path) -> bool {
    let Some(dir) = file.parent() else {
        return false;
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries.flatten().any(|e| {
        let p = e.path();
        p != file
            && p.extension()
                .and_then(|x| x.to_str())
                .is_some_and(|x| x.eq_ignore_ascii_case("json"))
    })
}

fn parse_pointer(pointer: &str) -> anyhow::Result<Vec<String>> {
    if pointer.is_empty() || pointer == "/" {
        return Ok(Vec::new());
    }
    if !pointer.starts_with('/') {
        anyhow::bail!("json_edit pointer must start with '/': {}", pointer);
    }
    Ok(pointer[1..]
        .split('/')
        .map(|s| s.replace("~1", "/").replace("~0", "~"))
        .collect())
}

fn navigate_create<'a>(
    doc: &'a mut serde_json::Value,
    parts: &[String],
) -> anyhow::Result<&'a mut serde_json::Value> {
    let mut node = doc;
    for part in parts {
        if node.get(part).is_none() {
            node.as_object_mut()
                .ok_or_else(|| anyhow::anyhow!("cannot descend into non-object"))?
                .insert(part.clone(), serde_json::Value::Object(Default::default()));
        }
        node = node
            .get_mut(part)
            .ok_or_else(|| anyhow::anyhow!("cannot resolve pointer segment"))?;
    }
    Ok(node)
}

/// Set the value at `parts`; returns true when the document changed.
fn set_pointer(doc: &mut serde_json::Value, parts: &[String], value: serde_json::Value) -> bool {
    if parts.is_empty() {
        let changed = *doc != value;
        *doc = value;
        return changed;
    }
    let parent = match navigate_create(doc, &parts[..parts.len() - 1]) {
        Ok(n) => n,
        Err(_) => return false,
    };
    let last = &parts[parts.len() - 1];
    if let Some(obj) = parent.as_object_mut() {
        let changed = obj.get(last) != Some(&value);
        obj.insert(last.clone(), value);
        changed
    } else if let Some(arr) = parent.as_array_mut() {
        match last.parse::<usize>() {
            Ok(i) if i < arr.len() => {
                let changed = arr[i] != value;
                arr[i] = value;
                changed
            }
            Ok(i) if i == arr.len() => {
                arr.push(value);
                true
            }
            _ => false,
        }
    } else {
        false
    }
}

fn remove_pointer(doc: &mut serde_json::Value, parts: &[String]) -> bool {
    if parts.is_empty() {
        return false;
    }
    let mut node = doc;
    for part in &parts[..parts.len() - 1] {
        match node.get_mut(part) {
            Some(next) => node = next,
            None => return false,
        }
    }
    let last = &parts[parts.len() - 1];
    if let Some(obj) = node.as_object_mut() {
        obj.remove(last).is_some()
    } else if let Some(arr) = node.as_array_mut() {
        last.parse::<usize>()
            .ok()
            .filter(|i| *i < arr.len())
            .map(|i| {
                arr.remove(i);
            })
            .is_some()
    } else {
        false
    }
}

// ---------------------------------------------------------------------------
// Manifest helpers (version-scoped events)
// ---------------------------------------------------------------------------

/// Host events `install` registers hooks for: every mapped event except
/// normalize-only ones (`"install": false`).
fn version_events(manifest: &AdapterManifest, version: &str) -> Vec<String> {
    manifest
        .versions
        .get(version)
        .map(|v| {
            v.events
                .iter()
                .filter(|(_, spec)| spec.install)
                .map(|(k, _)| k.clone())
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_command_unquoted_without_spaces() {
        let cmd = hook_command(
            Path::new("/usr/local/bin/agent-hooks"),
            "claude-code",
            "Stop",
            None,
            None,
            None,
            None,
        );
        assert_eq!(
            cmd,
            "/usr/local/bin/agent-hooks hook claude-code --event Stop"
        );
    }

    #[test]
    fn hook_command_quotes_paths_with_spaces() {
        let cmd = hook_command(
            Path::new(r"C:\Program Files\agent-hooks\agent-hooks.exe"),
            "claude-code",
            "Stop",
            None,
            None,
            None,
            None,
        );
        assert_eq!(
            cmd,
            r#""C:\Program Files\agent-hooks\agent-hooks.exe" hook claude-code --event Stop"#
        );
    }

    #[test]
    fn hook_command_with_adapter_version() {
        let cmd = hook_command(
            Path::new("/usr/local/bin/agent-hooks"),
            "cursor",
            "Stop",
            None,
            None,
            None,
            Some("v1"),
        );
        assert_eq!(
            cmd,
            "/usr/local/bin/agent-hooks hook cursor --event Stop --adapter-version v1"
        );
        // With socket and version
        let cmd2 = hook_command(
            Path::new("/usr/local/bin/agent-hooks"),
            "claude-code",
            "PreToolUse",
            Some(Path::new("/tmp/agent-hooks.sock")),
            None,
            None,
            Some("v2"),
        );
        assert_eq!(
            cmd2,
            "/usr/local/bin/agent-hooks hook claude-code --event PreToolUse --socket /tmp/agent-hooks.sock --adapter-version v2"
        );
    }

    #[test]
    fn hook_command_stamps_origin_after_context_cmd() {
        let cmd = hook_command(
            Path::new("/b/agent-hooks"),
            "claude-code",
            "Stop",
            Some(Path::new("/tmp/agent-hooks.sock")),
            None,
            Some("maintainer@example.invalid"),
            None,
        );
        assert_eq!(
            cmd,
            "/b/agent-hooks hook claude-code --event Stop --socket /tmp/agent-hooks.sock --origin maintainer@example.invalid"
        );
        // Unset or blank origin adds nothing (idempotent with older installs).
        let bare = hook_command(
            Path::new("/b/agent-hooks"),
            "x",
            "Stop",
            None,
            None,
            Some("  "),
            None,
        );
        assert_eq!(bare, "/b/agent-hooks hook x --event Stop");
        // Anything outside the safe set is quoted as one word.
        assert_eq!(shell_word("a b\"c"), "\"a b\\\"c\"");
        // Plugin assets get the label as a JSON string literal.
        let staged = substitute_install_placeholders(
            "const INSTALL_ORIGIN = \"__AGENT_HOOKS_ORIGIN__\";",
            StageVars {
                origin: Some("dev@127.0.0.1"),
                ..Default::default()
            },
        );
        assert_eq!(staged, "const INSTALL_ORIGIN = \"dev@127.0.0.1\";");
    }

    #[test]
    fn hook_command_stamps_context_cmd_like_socket() {
        let cmd = hook_command(
            Path::new("/usr/local/bin/agent-hooks"),
            "claude-code",
            "Stop",
            Some(Path::new("/tmp/agent-hooks.sock")),
            Some(Path::new("/Applications/B2V.app/Contents/MacOS/b2v")),
            None,
            Some("v1"),
        );
        assert_eq!(
            cmd,
            "/usr/local/bin/agent-hooks hook claude-code --event Stop \
             --socket /tmp/agent-hooks.sock \
             --context-cmd /Applications/B2V.app/Contents/MacOS/b2v --adapter-version v1"
        );
        // Without socket the flag still lands before the version pin.
        let cmd = hook_command(
            Path::new("/bin/agent-hooks"),
            "codex",
            "Stop",
            None,
            Some(Path::new("/opt/b2v")),
            None,
            None,
        );
        assert_eq!(
            cmd,
            "/bin/agent-hooks hook codex --event Stop --context-cmd /opt/b2v"
        );
    }

    #[test]
    fn hook_command_quotes_context_cmd_with_whitespace() {
        let cmd = hook_command(
            Path::new("/Users/me/My Tools/agent-hooks"),
            "claude-code",
            "Stop",
            Some(Path::new("/tmp/my sock.sock")),
            Some(Path::new(
                "/Applications/Back 2 Vibing.app/Contents/MacOS/b2v",
            )),
            None,
            None,
        );
        assert_eq!(
            cmd,
            r#""/Users/me/My Tools/agent-hooks" hook claude-code --event Stop --socket "/tmp/my sock.sock" --context-cmd "/Applications/Back 2 Vibing.app/Contents/MacOS/b2v""#
        );
        // The shim's own argv parser sees one token per path.
        let tokens: Vec<String> = cmd
            .split('"')
            .enumerate()
            .flat_map(|(i, part)| {
                if i % 2 == 1 {
                    vec![part.to_string()]
                } else {
                    part.split_whitespace().map(String::from).collect()
                }
            })
            .collect();
        let at = tokens.iter().position(|t| t == "--context-cmd").unwrap();
        assert_eq!(
            tokens[at + 1],
            "/Applications/Back 2 Vibing.app/Contents/MacOS/b2v"
        );
    }

    #[test]
    fn install_args_accept_bin_alias_and_context_cmd() {
        use clap::Parser;
        let cli = crate::cli::Cli::try_parse_from([
            "agent-hooks",
            "install",
            "--harness",
            "pi",
            "--bin",
            "/opt/agent-hooks",
            "--context-cmd",
            "/opt/b2v",
        ])
        .expect("parses");
        let crate::cli::Commands::Install(args) = cli.command else {
            panic!("install subcommand");
        };
        assert_eq!(
            args.binary_path.as_deref(),
            Some(Path::new("/opt/agent-hooks"))
        );
        assert_eq!(args.context_cmd.as_deref(), Some(Path::new("/opt/b2v")));
    }

    const PLACEHOLDER_ASSET: &str = r#"const INSTALL_BIN = "__AGENT_HOOKS_BIN__";
const INSTALL_SOCKET = "__AGENT_HOOKS_SOCKET__";
const INSTALL_CONTEXT_CMD = "__AGENT_HOOKS_CONTEXT_CMD__";
const SINGLE = '__AGENT_HOOKS_BIN__';
// bare mention __AGENT_HOOKS_BIN__ stays
"#;

    #[test]
    fn substitution_writes_json_string_literals() {
        let out = substitute_install_placeholders(
            PLACEHOLDER_ASSET,
            StageVars {
                bin: Some(Path::new("/opt/agent-hooks")),
                socket: Some(Path::new("/tmp/a.sock")),
                context_cmd: Some(Path::new("/opt/b2v")),
                origin: None,
            },
        );
        assert!(
            out.contains(r#"const INSTALL_BIN = "/opt/agent-hooks";"#),
            "{out}"
        );
        assert!(out.contains(r#"const INSTALL_SOCKET = "/tmp/a.sock";"#));
        assert!(out.contains(r#"const INSTALL_CONTEXT_CMD = "/opt/b2v";"#));
        assert!(out.contains(r#"const SINGLE = "/opt/agent-hooks";"#));
        // Only quoted tokens are settings; prose mentions are left alone.
        assert!(out.contains("bare mention __AGENT_HOOKS_BIN__ stays"));
    }

    #[test]
    fn substitution_escapes_quotes_and_backslashes() {
        let out = substitute_install_placeholders(
            PLACEHOLDER_ASSET,
            StageVars {
                bin: Some(Path::new(r"C:\Program Files\agent-hooks.exe")),
                socket: Some(Path::new(r#"/tmp/we"ird.sock"#)),
                context_cmd: None,
                origin: None,
            },
        );
        assert!(
            out.contains(r#"const INSTALL_BIN = "C:\\Program Files\\agent-hooks.exe";"#),
            "{out}"
        );
        assert!(
            out.contains(r#"const INSTALL_SOCKET = "/tmp/we\"ird.sock";"#),
            "{out}"
        );
        // Each substituted literal round-trips to the original value.
        let line = out.lines().next().unwrap();
        let lit = line
            .trim_start_matches("const INSTALL_BIN = ")
            .trim_end_matches(';');
        assert_eq!(
            serde_json::from_str::<String>(lit).unwrap(),
            r"C:\Program Files\agent-hooks.exe"
        );
    }

    #[test]
    fn substitution_unset_values_become_empty_strings() {
        let out = substitute_install_placeholders(PLACEHOLDER_ASSET, StageVars::default());
        assert!(out.contains(r#"const INSTALL_BIN = "";"#));
        assert!(out.contains(r#"const INSTALL_SOCKET = "";"#));
        assert!(out.contains(r#"const INSTALL_CONTEXT_CMD = "";"#));
        assert!(!out.contains("\"__AGENT_HOOKS_"));
    }

    fn stage_template(dir: &Path, path: &str, vars: StageVars) -> (PathBuf, usize) {
        let assets = vec![agent_hooks_adapters::manifest::AssetSpec {
            path: path.to_string(),
            embedded: None,
            template: Some(serde_json::Value::String(PLACEHOLDER_ASSET.to_string())),
        }];
        op_stage_assets(
            "test",
            &dir.to_string_lossy(),
            &assets,
            Some("//! managed by agent-hooks"),
            vars,
        )
        .expect("stage")
    }

    #[test]
    fn staging_substitutes_code_assets_idempotently_and_stays_managed() {
        let tmp = tempfile::tempdir().unwrap();
        let first = StageVars {
            bin: Some(Path::new("/opt/one/agent-hooks")),
            socket: None,
            context_cmd: Some(Path::new("/opt/one/b2v")),
            origin: None,
        };
        let (dir, n) = stage_template(tmp.path(), "agent-hooks.ts", first);
        assert_eq!(n, 1);
        let file = dir.join("agent-hooks.ts");
        let staged = std::fs::read_to_string(&file).unwrap();
        assert!(staged.starts_with("//! managed by agent-hooks\n"));
        assert!(staged.contains(r#"const INSTALL_CONTEXT_CMD = "/opt/one/b2v";"#));
        assert!(staged.contains(r#"const INSTALL_SOCKET = "";"#));
        // uninstall/status still recognize the substituted file as ours.
        assert!(crate::commands::uninstall::is_managed_file(
            &file,
            Some("//! managed by agent-hooks")
        ));

        // Same settings: nothing rewritten.
        let (_, n) = stage_template(tmp.path(), "agent-hooks.ts", first);
        assert_eq!(n, 0, "re-staging identical settings is a no-op");

        // New settings: rendered fresh from the template, no stale values.
        let second = StageVars {
            bin: None,
            socket: Some(Path::new("/tmp/two.sock")),
            context_cmd: Some(Path::new("/opt/two/b2v")),
            origin: None,
        };
        let (_, n) = stage_template(tmp.path(), "agent-hooks.ts", second);
        assert_eq!(n, 1);
        let restaged = std::fs::read_to_string(&file).unwrap();
        assert!(restaged.contains(r#"const INSTALL_BIN = "";"#));
        assert!(restaged.contains(r#"const INSTALL_SOCKET = "/tmp/two.sock";"#));
        assert!(restaged.contains(r#"const INSTALL_CONTEXT_CMD = "/opt/two/b2v";"#));
        assert!(!restaged.contains("/opt/one"));
        assert_eq!(restaged.matches("managed by agent-hooks").count(), 1);
    }

    #[test]
    fn staging_leaves_non_code_assets_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let vars = StageVars {
            bin: Some(Path::new("/opt/agent-hooks")),
            ..Default::default()
        };
        for name in ["README.md", "notes.txt"] {
            let (dir, _) = stage_template(tmp.path(), name, vars);
            let staged = std::fs::read_to_string(dir.join(name)).unwrap();
            assert!(staged.contains("\"__AGENT_HOOKS_BIN__\""), "{name}");
        }
        for name in ["a.js", "a.py", "a.mjs"] {
            let (dir, _) = stage_template(tmp.path(), name, vars);
            let staged = std::fs::read_to_string(dir.join(name)).unwrap();
            assert!(!staged.contains("\"__AGENT_HOOKS_"), "{name}");
        }
    }

    /// Every embedded plugin asset stages with no placeholder left behind.
    #[test]
    fn embedded_plugin_assets_leave_no_placeholders() {
        let vars = StageVars {
            bin: Some(Path::new("/opt/agent-hooks")),
            socket: Some(Path::new("/tmp/a.sock")),
            context_cmd: Some(Path::new("/opt/b2v")),
            origin: None,
        };
        for m in AdapterManifest::load_embedded() {
            for vs in m.versions.values() {
                for op in &vs.install {
                    let InstallOp::StageAssets { assets, .. } = op else {
                        continue;
                    };
                    for asset in assets {
                        let Some(key) = asset.embedded.as_deref() else {
                            continue;
                        };
                        let Some(src) = agent_hooks_adapters::embedded_asset(&m.id, key) else {
                            continue;
                        };
                        if !is_code_asset(&asset.path) {
                            continue;
                        }
                        let out = substitute_install_placeholders(src, vars);
                        // Full quoted tokens only: assets may keep a bare
                        // `"__AGENT_HOOKS_"` prefix check as an unstaged guard.
                        let leftover = [
                            "__AGENT_HOOKS_BIN__",
                            "__AGENT_HOOKS_SOCKET__",
                            "__AGENT_HOOKS_CONTEXT_CMD__",
                        ]
                        .iter()
                        .any(|t| {
                            out.contains(&format!("\"{t}\"")) || out.contains(&format!("'{t}'"))
                        });
                        assert!(
                            !leftover,
                            "{}/{key} keeps an unsubstituted placeholder",
                            m.id
                        );
                    }
                }
            }
        }
    }

    fn builtin(id: &str) -> (AdapterManifest, String) {
        let m = AdapterManifest::load_embedded()
            .into_iter()
            .find(|m| m.id == id)
            .unwrap_or_else(|| panic!("{id} builtin manifest"));
        let v = m.default_version().to_string();
        (m, v)
    }

    fn test_ctx<'a>(context_cmd: Option<&'a Path>) -> Ctx<'a> {
        Ctx {
            binary: Path::new("/usr/local/bin/agent-hooks"),
            socket: None,
            context_cmd,
            origin: None,
            adapter_version: None,
            stage_dir: None,
        }
    }

    fn install_toml(manifest: &AdapterManifest, version: &str, target: &Path, ctx: &Ctx) -> usize {
        let Some(InstallOp::TomlHooks {
            create,
            root,
            entry_shape,
            entry,
            ..
        }) = manifest.versions[version].install.first().cloned()
        else {
            panic!("{} installs via toml_hooks", manifest.id);
        };
        let paths = vec![target.to_string_lossy().into_owned()];
        op_toml_hooks(
            manifest,
            version,
            &paths,
            create,
            &root,
            entry_shape,
            &entry,
            ctx,
            true,
        )
        .expect("install")
        .0
    }

    /// jcode reads `hooks.<event> = "<shell command>"` — one string per
    /// event, never `[[hooks.<event>]]` tables. Legacy oracle: B2V
    /// src-tauri/src/agent_config/agents/jcode.rs `write_jcode_hooks`.
    #[test]
    fn jcode_installs_one_command_string_per_event() {
        let (manifest, version) = builtin("jcode");
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("config.toml");
        // User config with an existing [hooks] table and a foreign command.
        std::fs::write(
            &target,
            "# my jcode config\nmodel = \"x\"\n\n[hooks]\nsession_end = \"notify-send bye\"\n",
        )
        .unwrap();

        let added = install_toml(&manifest, &version, &target, &test_ctx(None));
        let raw = std::fs::read_to_string(&target).unwrap();
        let doc: toml_edit::Document = raw.parse().expect("valid TOML");
        let hooks = doc["hooks"].as_table().expect("[hooks] table");
        for ev in ["session_start", "turn_start", "post_tool", "turn_end"] {
            assert_eq!(
                hooks[ev].as_str(),
                Some(format!("/usr/local/bin/agent-hooks hook jcode --event {ev}").as_str()),
                "{ev} must be a bare command string:\n{raw}"
            );
        }
        // One command per event: the user's own session_end is untouched.
        assert_eq!(hooks["session_end"].as_str(), Some("notify-send bye"));
        assert!(!raw.contains("[[hooks."), "no array-of-tables:\n{raw}");
        assert!(raw.contains("# my jcode config") && raw.contains("model = \"x\""));
        assert_eq!(added, version_events(&manifest, &version).len() - 1);

        // Reinstall is a no-op; changing --context-cmd rewrites ours in place.
        assert_eq!(
            install_toml(&manifest, &version, &target, &test_ctx(None)),
            0
        );
        let ctx_cmd = Path::new("/Applications/Back 2 Vibing.app/b2v");
        assert_eq!(
            install_toml(&manifest, &version, &target, &test_ctx(Some(ctx_cmd))),
            added
        );
        let doc: toml_edit::Document = std::fs::read_to_string(&target).unwrap().parse().unwrap();
        assert_eq!(
            doc["hooks"]["turn_end"].as_str(),
            Some(
                r#"/usr/local/bin/agent-hooks hook jcode --event turn_end --context-cmd "/Applications/Back 2 Vibing.app/b2v""#
            )
        );

        // Uninstall drops only our lines and leaves valid TOML behind.
        let removed =
            crate::commands::uninstall::remove_toml_hooks(&target, &manifest.id, &manifest.aliases)
                .unwrap();
        assert_eq!(removed, added);
        let raw = std::fs::read_to_string(&target).unwrap();
        let doc: toml_edit::Document = raw.parse().expect("valid TOML after uninstall");
        assert_eq!(
            doc["hooks"]["session_end"].as_str(),
            Some("notify-send bye")
        );
        assert!(!raw.contains("agent-hooks hook"), "{raw}");
    }

    #[test]
    fn jcode_install_creates_config_from_scratch() {
        let (manifest, version) = builtin("jcode");
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("nested/config.toml");
        let added = install_toml(&manifest, &version, &target, &test_ctx(None));
        assert_eq!(added, version_events(&manifest, &version).len());
        let doc: toml_edit::Document = std::fs::read_to_string(&target).unwrap().parse().unwrap();
        assert!(doc["hooks"]["session_start"].is_str());
    }

    fn install_list_toml(
        manifest: &AdapterManifest,
        version: &str,
        target: &Path,
        ctx: &Ctx,
    ) -> usize {
        let Some(InstallOp::TomlListHooks {
            create,
            list_key,
            entry,
            entry_overrides,
            ..
        }) = manifest.versions[version].install.first().cloned()
        else {
            panic!("{} installs via toml_list_hooks", manifest.id);
        };
        let paths = vec![target.to_string_lossy().into_owned()];
        op_toml_list_hooks(
            manifest,
            version,
            &paths,
            create,
            &list_key,
            &entry,
            &entry_overrides,
            ctx,
            true,
        )
        .expect("install")
        .0
    }

    /// Flat `[[hooks]]` rows append under a marker, reinstalls are no-ops,
    /// and uninstall drops only our rows — foreign rows, tables, and
    /// comments survive with valid TOML.
    #[test]
    fn toml_list_hooks_round_trip_preserves_foreign_content() {
        let (manifest, version) = builtin("kimi-code");
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("config.toml");
        std::fs::write(
            &target,
            "# my kimi config\nmodel = \"k2\"\n\n[[hooks]]\nevent = \"Notification\"\ncommand = \"notify-send done\"\n",
        )
        .unwrap();

        let added = install_list_toml(&manifest, &version, &target, &test_ctx(None));
        assert_eq!(added, version_events(&manifest, &version).len());
        let raw = std::fs::read_to_string(&target).unwrap();
        let doc: toml_edit::Document = raw.parse().expect("valid TOML");
        let rows = doc["hooks"].as_array_of_tables().expect("[[hooks]] rows");
        // Foreign row + one row per installed event.
        assert_eq!(rows.len(), added + 1);
        assert!(raw.contains("# my kimi config") && raw.contains("model = \"k2\""));
        assert!(raw.contains("notify-send done"));
        for row in rows.iter().skip(1) {
            assert_eq!(
                row["command"].as_str(),
                Some(
                    format!(
                        "/usr/local/bin/agent-hooks hook kimi-code --event {}",
                        row["event"].as_str().unwrap()
                    )
                    .as_str()
                ),
                "row must carry the canonical command"
            );
        }

        // Reinstall is a no-op.
        assert_eq!(
            install_list_toml(&manifest, &version, &target, &test_ctx(None)),
            0
        );

        // Uninstall drops only our rows and leaves valid TOML behind.
        let removed = crate::commands::uninstall::remove_toml_list_hooks(
            &target.to_path_buf(),
            "hooks",
            &manifest.id,
            &manifest.aliases,
        )
        .unwrap();
        assert_eq!(removed, added);
        let raw = std::fs::read_to_string(&target).unwrap();
        let doc: toml_edit::Document = raw.parse().expect("valid TOML after uninstall");
        let rows = doc["hooks"].as_array_of_tables().expect("[[hooks]] rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows.iter().next().unwrap()["command"].as_str(),
            Some("notify-send done")
        );
        assert!(!raw.contains("agent-hooks hook"), "{raw}");
    }

    /// Mistral rows carry unique names and per-event hook types.
    #[test]
    fn toml_list_hooks_mistral_rows_carry_types() {
        let (manifest, version) = builtin("mistral-vibe");
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("hooks.toml");
        let added = install_list_toml(&manifest, &version, &target, &test_ctx(None));
        assert_eq!(added, 3, "pre_tool + post_tool + post_agent rows");
        let raw = std::fs::read_to_string(&target).unwrap();
        let doc: toml_edit::Document = raw.parse().expect("valid TOML");
        let rows = doc["hooks"].as_array_of_tables().expect("[[hooks]] rows");
        let types: Vec<&str> = rows.iter().map(|r| r["type"].as_str().unwrap()).collect();
        assert!(types.contains(&"pre_tool"));
        assert!(types.contains(&"post_tool"));
        assert!(types.contains(&"post_agent"));
        for row in rows.iter() {
            assert!(
                row["name"]
                    .as_str()
                    .is_some_and(|n| n.starts_with("agent-hooks-")),
                "named rows dedupe across project/user files"
            );
        }
    }

    /// `~/.kiro/agents/agent-hooks.json` must be a loadable Kiro agent:
    /// name + description + `tools: ["*"]` + flat `{command}` hooks. Legacy
    /// oracle: B2V src-tauri/src/agent_config/agents/kiro.rs (new-file branch
    /// of the writer: name/description/DEFAULT_TOOLS).
    #[test]
    fn kiro_agent_config_is_a_valid_agent() {
        let (manifest, version) = builtin("kiro");
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("agent-hooks.json");
        let Some(InstallOp::JsonHooks {
            create,
            root,
            entry_shape,
            name,
            fixed_fields,
            entry,
            dedupe,
            entry_overrides,
            ..
        }) = manifest.versions[&version].install.first().cloned()
        else {
            panic!("kiro installs via json_hooks");
        };
        let paths = vec![target.to_string_lossy().into_owned()];
        let run = |target_ctx: &Ctx| {
            op_json_hooks(
                &manifest,
                &version,
                &paths,
                create,
                &root,
                entry_shape,
                name.as_deref(),
                fixed_fields.as_ref(),
                &entry,
                dedupe.as_deref(),
                &entry_overrides,
                target_ctx,
                true,
            )
            .expect("install")
        };
        run(&test_ctx(None));
        let doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&target).unwrap()).unwrap();
        assert_eq!(doc["name"], "agent-hooks");
        assert_eq!(doc["tools"], serde_json::json!(["*"]));
        assert!(doc["description"].is_string());
        // Legacy kiro.rs installs agentSpawn/preToolUse/postToolUse/stop;
        // userPromptSubmit is parse-only (`"install": false`).
        assert!(doc["hooks"].get("userPromptSubmit").is_none());
        for ev in ["agentSpawn", "preToolUse", "postToolUse", "stop"] {
            let arr = doc["hooks"][ev]
                .as_array()
                .unwrap_or_else(|| panic!("{ev}"));
            assert_eq!(arr.len(), 1, "{ev}");
            assert_eq!(
                arr[0]["command"],
                format!("/usr/local/bin/agent-hooks hook kiro --event {ev}")
            );
            assert!(arr[0].get("hooks").is_none(), "{ev} is flat");
            // Legacy writes command-only entries; no `timeout` key.
            assert_eq!(
                arr[0].as_object().unwrap().keys().collect::<Vec<_>>(),
                vec!["command"],
                "{ev}"
            );
        }

        // A user-customized agent keeps its own name/tools on reinstall.
        let mut custom = doc.clone();
        custom["name"] = "mine".into();
        custom["tools"] = serde_json::json!(["read", "write"]);
        std::fs::write(&target, custom.to_string()).unwrap();
        run(&test_ctx(None));
        let doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&target).unwrap()).unwrap();
        assert_eq!(doc["name"], "mine");
        assert_eq!(doc["tools"], serde_json::json!(["read", "write"]));
        assert_eq!(
            doc["hooks"]["stop"].as_array().unwrap().len(),
            1,
            "no dupes"
        );
    }

    /// jcode gets exactly the 5 legacy keys (B2V agent_config/agents/jcode.rs
    /// JCODE_EVENTS); camelCase aliases are normalize-only.
    #[test]
    fn jcode_installs_only_legacy_events_and_drops_stale_aliases() {
        let (manifest, version) = builtin("jcode");
        let mut events = version_events(&manifest, &version);
        events.sort();
        assert_eq!(
            events,
            [
                "post_tool",
                "session_end",
                "session_start",
                "turn_end",
                "turn_start"
            ]
        );
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("config.toml");
        // Older install wrote an alias key; a user owns another alias key.
        std::fs::write(
            &target,
            "[hooks]\nstop = \"/usr/local/bin/agent-hooks hook jcode --event stop\"\npreToolUse = \"my-own-check\"\n",
        )
        .unwrap();
        install_toml(&manifest, &version, &target, &test_ctx(None));
        let doc: toml_edit::Document = std::fs::read_to_string(&target).unwrap().parse().unwrap();
        let hooks = doc["hooks"].as_table().unwrap();
        let mut keys: Vec<&str> = hooks.iter().map(|(k, _)| k).collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "post_tool",
                "preToolUse",
                "session_end",
                "session_start",
                "turn_end",
                "turn_start"
            ]
        );
        assert_eq!(hooks["preToolUse"].as_str(), Some("my-own-check"));
    }

    #[test]
    fn install_false_is_normalize_only() {
        let (manifest, version) = builtin("jcode");
        let spec = &manifest.versions[&version].events["stop"];
        assert!(!spec.install);
        // Still parsed: the alias keeps normalizing to a lifecycle.
        let adapter = agent_hooks_adapters::AdapterRegistry::get_versioned("jcode", None).unwrap();
        let input = agent_hooks_protocol::RawHookInput {
            harness_hint: "jcode".into(),
            event_hint: Some("stop".into()),
            raw_payload: r#"{"session_id":"jc-1"}"#.into(),
            context: Default::default(),
            request_id: None,
            passthrough_env: Default::default(),
        };
        assert!(adapter.parse_event(&input).is_ok());
    }

    /// Kiro manifest with its paths redirected under `home`.
    fn kiro_in(home: &Path) -> (AdapterManifest, String) {
        let (mut manifest, version) = builtin("kiro");
        let fix = |p: &mut String| {
            *p = p.replace('~', &home.to_string_lossy());
        };
        for op in &mut manifest.versions.get_mut(&version).unwrap().install {
            match op {
                InstallOp::JsonHooks { paths, .. } => paths.iter_mut().for_each(fix),
                InstallOp::JsonEdit {
                    paths,
                    only_if_sole_json,
                    ..
                } => {
                    paths.iter_mut().for_each(fix);
                    only_if_sole_json.iter_mut().for_each(fix);
                }
                _ => {}
            }
        }
        (manifest, version)
    }

    fn kiro_install(home: &Path) {
        let (manifest, version) = kiro_in(home);
        install_for_manifest(
            &manifest,
            &version,
            Path::new("/usr/local/bin/agent-hooks"),
            None,
            None,
            true,
            StageVars::default(),
        )
        .expect("install");
    }

    fn kiro_uninstall(home: &Path) {
        let (manifest, version) = kiro_in(home);
        for op in &manifest.versions[&version].install {
            if let InstallOp::JsonEdit {
                paths,
                pointer,
                value: Some(value),
                restore_on_uninstall: true,
                ..
            } = op
            {
                revert_json_edit(paths, pointer, value).unwrap();
            }
        }
    }

    fn kiro_settings(home: &Path) -> serde_json::Value {
        let p = home.join(".kiro/settings/cli.json");
        serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
    }

    /// Legacy B2V kiro.rs `set_kiro_default_agent`: claim chat.defaultAgent
    /// (with a backup) when ours is the only custom agent. Uninstall undoes it.
    #[test]
    fn kiro_default_agent_round_trip_without_prior_value() {
        let home = tempfile::tempdir().unwrap();
        kiro_install(home.path());
        let settings = kiro_settings(home.path());
        assert_eq!(settings["chat.defaultAgent"], "agent-hooks");
        assert!(settings.get("chat").is_none(), "dotted key is literal");

        kiro_uninstall(home.path());
        let settings = kiro_settings(home.path());
        assert!(settings.get("chat.defaultAgent").is_none(), "{settings}");
        assert!(!home
            .path()
            .join(".kiro/settings/cli.json.agent-hooks-state.json")
            .exists());
    }

    #[test]
    fn kiro_default_agent_restores_previous_value() {
        let home = tempfile::tempdir().unwrap();
        let settings_path = home.path().join(".kiro/settings/cli.json");
        std::fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
        std::fs::write(
            &settings_path,
            r#"{"chat.defaultAgent": "kiro_default", "chat.model": "m"}"#,
        )
        .unwrap();
        kiro_install(home.path());
        assert_eq!(
            kiro_settings(home.path())["chat.defaultAgent"],
            "agent-hooks"
        );
        let backup = home.path().join(".kiro/settings/cli.json.agent-hooks.bak");
        assert!(std::fs::read_to_string(backup)
            .unwrap()
            .contains("kiro_default"));
        // Reinstall must not record our own value as "previous".
        kiro_install(home.path());

        kiro_uninstall(home.path());
        let settings = kiro_settings(home.path());
        assert_eq!(settings["chat.defaultAgent"], "kiro_default");
        assert_eq!(settings["chat.model"], "m");
    }

    #[test]
    fn kiro_uninstall_keeps_a_default_the_user_changed_since() {
        let home = tempfile::tempdir().unwrap();
        kiro_install(home.path());
        let settings_path = home.path().join(".kiro/settings/cli.json");
        std::fs::write(&settings_path, r#"{"chat.defaultAgent": "their-agent"}"#).unwrap();
        kiro_uninstall(home.path());
        assert_eq!(
            kiro_settings(home.path())["chat.defaultAgent"],
            "their-agent"
        );
    }

    /// Legacy only claims the default when no other custom agent exists
    /// (it hooks those agents instead); a user's own agent keeps its seat.
    #[test]
    fn kiro_default_agent_untouched_when_user_has_own_agent() {
        let home = tempfile::tempdir().unwrap();
        let agents = home.path().join(".kiro/agents");
        std::fs::create_dir_all(&agents).unwrap();
        std::fs::write(agents.join("mine.json"), r#"{"name":"mine"}"#).unwrap();
        let settings_path = home.path().join(".kiro/settings/cli.json");
        std::fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
        std::fs::write(&settings_path, r#"{"chat.defaultAgent": "mine"}"#).unwrap();
        kiro_install(home.path());
        assert_eq!(kiro_settings(home.path())["chat.defaultAgent"], "mine");
        assert!(
            agents.join("agent-hooks.json").exists(),
            "hooks still installed"
        );
        kiro_uninstall(home.path());
        assert_eq!(kiro_settings(home.path())["chat.defaultAgent"], "mine");
    }

    /// Command-only Kiro entries give the shim no timeout; the manifest's
    /// host default (Kiro: 30s) budgets it instead.
    #[test]
    fn kiro_budget_comes_from_host_default() {
        let adapter = agent_hooks_adapters::AdapterRegistry::get_versioned("kiro", None).unwrap();
        let input = agent_hooks_protocol::RawHookInput {
            harness_hint: "kiro".into(),
            event_hint: Some("preToolUse".into()),
            raw_payload: r#"{"session_id":"k-1","hook_event_name":"preToolUse","tool_name":"fs_read","tool_input":{}}"#.into(),
            context: Default::default(),
            request_id: None,
            passthrough_env: Default::default(),
        };
        let event = adapter.parse_event(&input).expect("parse kiro preToolUse");
        assert_eq!(
            adapter.entry_timeout_for(&event),
            Some(std::time::Duration::from_secs(30))
        );
    }

    #[test]
    fn toml_escape_keeps_windows_paths_valid() {
        let escaped = toml_escape(r"C:\tools\agent-hooks.exe hook codex --event Stop");
        assert_eq!(
            escaped,
            r"C:\\tools\\agent-hooks.exe hook codex --event Stop"
        );
        // Round-trips through a TOML parser to the original command.
        let doc: toml_edit::Document = format!("command = \"{}\"\n", escaped).parse().unwrap();
        assert_eq!(
            doc["command"].as_str().unwrap(),
            r"C:\tools\agent-hooks.exe hook codex --event Stop"
        );
    }

    /// Antigravity's named bundle: lifecycle events (SessionStart, PreInvocation,
    /// PostInvocation, Stop) must be flat command objects — its parser drops
    /// the whole file when they are matcher-wrapped — while tool events keep
    /// matcher groups.
    #[test]
    fn antigravity_named_bundle_installs_flat_lifecycle_entries() {
        let mut manifest = AdapterManifest::load_embedded()
            .into_iter()
            .find(|m| m.id == "antigravity")
            .expect("antigravity builtin manifest");
        let version = manifest.default_version().to_string();
        let temp = tempfile::tempdir().expect("tempdir");
        let target = temp.path().join("hooks.json");

        let Some(InstallOp::JsonHooks {
            create,
            root,
            entry_shape,
            name,
            fixed_fields,
            entry: entry_tpl,
            dedupe,
            entry_overrides,
            ..
        }) = manifest
            .versions
            .get_mut(&version)
            .expect("version")
            .install
            .pop()
        else {
            panic!("antigravity installs via json_hooks");
        };
        let paths = vec![target.to_string_lossy().into_owned()];
        let ctx = Ctx {
            binary: Path::new("/usr/local/bin/agent-hooks"),
            socket: None,
            context_cmd: None,
            origin: None,
            adapter_version: None,
            stage_dir: None,
        };

        let (added, touched) = op_json_hooks(
            &manifest,
            &version,
            &paths,
            create,
            &root,
            entry_shape,
            name.as_deref(),
            fixed_fields.as_ref(),
            &entry_tpl,
            dedupe.as_deref(),
            &entry_overrides,
            &ctx,
            true,
        )
        .expect("install");
        assert_eq!(added, 6);
        assert!(touched);

        let doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&target).expect("read")).expect("json");
        let bundle = &doc["agent-hooks"];
        assert_eq!(bundle["enabled"], serde_json::json!(true));

        for ev in ["SessionStart", "PreInvocation", "PostInvocation", "Stop"] {
            let arr = bundle[ev]
                .as_array()
                .unwrap_or_else(|| panic!("{ev} array"));
            assert_eq!(arr.len(), 1, "{ev}");
            assert!(
                arr[0].get("hooks").is_none(),
                "{ev} must not be matcher-wrapped"
            );
            assert_eq!(arr[0]["type"], "command");
            assert!(
                arr[0]["command"]
                    .as_str()
                    .unwrap()
                    .contains(&format!("hook antigravity --event {ev}")),
                "{ev} command"
            );
        }
        for ev in ["PreToolUse", "PostToolUse"] {
            let arr = bundle[ev]
                .as_array()
                .unwrap_or_else(|| panic!("{ev} array"));
            assert_eq!(arr.len(), 1, "{ev}");
            assert_eq!(arr[0]["matcher"], "*");
            assert!(
                arr[0]["hooks"][0]["command"]
                    .as_str()
                    .unwrap()
                    .contains(&format!("hook antigravity --event {ev}")),
                "{ev} command"
            );
        }

        // Idempotent: second run finds every entry in shape and writes nothing.
        let (added2, _) = op_json_hooks(
            &manifest,
            &version,
            &paths,
            create,
            &root,
            entry_shape,
            name.as_deref(),
            fixed_fields.as_ref(),
            &entry_tpl,
            dedupe.as_deref(),
            &entry_overrides,
            &ctx,
            true,
        )
        .expect("reinstall");
        assert_eq!(added2, 0);
    }

    /// Heals configs written by the pre-fix installer: same command wrapped
    /// in matcher groups for lifecycle events. Reinstall must rewrite those
    /// flat, keep tool-event groups, and never touch foreign entries.
    #[test]
    fn antigravity_install_rewrites_wrong_shape_lifecycle_entries() {
        let mut manifest = AdapterManifest::load_embedded()
            .into_iter()
            .find(|m| m.id == "antigravity")
            .expect("antigravity builtin manifest");
        let version = manifest.default_version().to_string();
        let temp = tempfile::tempdir().expect("tempdir");
        let target = temp.path().join("hooks.json");

        let ours = |ev: &str| {
            hook_command(
                Path::new("/usr/local/bin/agent-hooks"),
                "antigravity",
                ev,
                None,
                None,
                None,
                None,
            )
        };
        let cmd_entry =
            |ev: &str| serde_json::json!({"type": "command", "command": ours(ev), "timeout": 30});
        let stale = serde_json::json!({
            "agent-hooks": {
                "SessionStart": [{"matcher": "*", "hooks": [cmd_entry("SessionStart")]}],
                "enabled": true,
                "PreInvocation": [{"matcher": "*", "hooks": [cmd_entry("PreInvocation")]}],
                "PostInvocation": [{"matcher": "*", "hooks": [cmd_entry("PostInvocation")]}],
                "Stop": [{"matcher": "*", "hooks": [cmd_entry("Stop")]}],
                "PreToolUse": [
                    {"matcher": "sql", "hooks": [{"type": "command", "command": "foreign-tool"}]},
                    {"matcher": "*", "hooks": [cmd_entry("PreToolUse")]}
                ]
            }
        });
        std::fs::write(&target, serde_json::to_string_pretty(&stale).unwrap()).unwrap();

        let Some(InstallOp::JsonHooks {
            create,
            root,
            entry_shape,
            name,
            fixed_fields,
            entry: entry_tpl,
            dedupe,
            entry_overrides,
            ..
        }) = manifest
            .versions
            .get_mut(&version)
            .expect("version")
            .install
            .pop()
        else {
            panic!("antigravity installs via json_hooks");
        };
        let paths = vec![target.to_string_lossy().into_owned()];
        let ctx = Ctx {
            binary: Path::new("/usr/local/bin/agent-hooks"),
            socket: None,
            context_cmd: None,
            origin: None,
            adapter_version: None,
            stage_dir: None,
        };
        let run = |manifest: &AdapterManifest| {
            op_json_hooks(
                manifest,
                &version,
                &paths,
                create,
                &root,
                entry_shape,
                name.as_deref(),
                fixed_fields.as_ref(),
                &entry_tpl,
                dedupe.as_deref(),
                &entry_overrides,
                &ctx,
                true,
            )
        };

        // PreToolUse is already in shape; the four lifecycle events and
        // PostToolUse get written.
        let (added, _) = run(&manifest).expect("heal");
        assert_eq!(added, 5);

        let doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&target).expect("read")).expect("json");
        let bundle = &doc["agent-hooks"];
        for ev in ["SessionStart", "PreInvocation", "PostInvocation", "Stop"] {
            let arr = bundle[ev]
                .as_array()
                .unwrap_or_else(|| panic!("{ev} array"));
            assert_eq!(arr.len(), 1, "{ev} healed to a single entry");
            assert!(arr[0].get("hooks").is_none(), "{ev} flat after heal");
            assert_eq!(arr[0]["command"], serde_json::json!(ours(ev)));
        }
        let ptu = bundle["PreToolUse"].as_array().expect("PreToolUse array");
        assert_eq!(ptu.len(), 2, "foreign matcher group preserved");
        assert_eq!(ptu[0]["matcher"], "sql");
        assert_eq!(ptu[0]["hooks"][0]["command"], "foreign-tool");
        assert_eq!(ptu[1]["matcher"], "*");
        assert_eq!(
            ptu[1]["hooks"][0]["command"],
            serde_json::json!(ours("PreToolUse"))
        );
        assert_eq!(
            bundle["PostToolUse"]
                .as_array()
                .expect("PostToolUse array")
                .len(),
            1
        );

        // Stable: another run changes nothing.
        let before = std::fs::read_to_string(&target).unwrap();
        let (added2, _) = run(&manifest).expect("stable");
        assert_eq!(added2, 0);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), before);
    }

    /// Gemini CLI only fires the events legacy B2V gemini.rs installs; the
    /// other mapped names are parse-only (`"install": false`). A reinstall
    /// prunes our stale entries for them and keeps foreign ones.
    #[test]
    fn gemini_installs_only_legacy_events_and_prunes_stale_aliases() {
        let (manifest, version) = builtin("gemini-cli");
        let mut installed = version_events(&manifest, &version);
        installed.sort();
        assert_eq!(
            installed,
            [
                "AfterAgent",
                "AfterModel",
                "AfterTool",
                "BeforeAgent",
                "BeforeModel",
                "BeforeTool",
                "Notification",
                "SessionEnd",
                "SessionStart",
            ]
        );

        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("settings.json");
        let ours = |ev: &str| format!("/old/bin/agent-hooks hook gemini-cli --event {ev}");
        let group = |cmd: String| serde_json::json!({"matcher": "*", "hooks": [{"type": "command", "command": cmd}]});
        let seed = serde_json::json!({
            "theme": "dark",
            "hooks": {
                "PreInvocation": [group(ours("PreInvocation"))],
                "UserPromptSubmit": [group(ours("UserPromptSubmit"))],
                "PostToolUse": [group(ours("PostToolUse"))],
                "Stop": [group(ours("Stop"))],
                // A foreign hook under a parse-only key survives.
                "PreToolUse": [group(ours("PreToolUse")), group("my-audit.sh".to_string())],
            }
        });
        std::fs::write(&target, serde_json::to_string_pretty(&seed).unwrap()).unwrap();

        let Some(InstallOp::JsonHooks {
            create,
            root,
            entry_shape,
            name,
            fixed_fields,
            entry,
            dedupe,
            entry_overrides,
            ..
        }) = manifest.versions[&version].install.first().cloned()
        else {
            panic!("gemini installs via json_hooks");
        };
        let paths = vec![target.to_string_lossy().into_owned()];
        let run = || {
            op_json_hooks(
                &manifest,
                &version,
                &paths,
                create,
                &root,
                entry_shape,
                name.as_deref(),
                fixed_fields.as_ref(),
                &entry,
                dedupe.as_deref(),
                &entry_overrides,
                &test_ctx(None),
                true,
            )
            .expect("install")
        };
        let (added, _) = run();
        assert_eq!(added, installed.len());

        let doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&target).unwrap()).unwrap();
        let hooks = doc["hooks"].as_object().unwrap();
        for gone in ["PreInvocation", "UserPromptSubmit", "PostToolUse", "Stop"] {
            assert!(!hooks.contains_key(gone), "{gone} must be pruned: {doc}");
        }
        assert_eq!(
            doc["hooks"]["PreToolUse"],
            serde_json::json!([group("my-audit.sh".to_string())])
        );
        let mut keys: Vec<&str> = hooks.keys().map(String::as_str).collect();
        keys.sort();
        let mut expected: Vec<&str> = installed.iter().map(String::as_str).collect();
        expected.push("PreToolUse");
        expected.sort();
        assert_eq!(keys, expected);
        assert_eq!(doc["theme"], "dark");

        // Reinstall is a no-op.
        assert_eq!(run().0, 0);
    }

    /// Installed key sets match what legacy B2V installs per harness
    /// (agent_config/agents/<harness>.rs).
    #[test]
    fn command_harnesses_install_the_legacy_event_sets() {
        for (id, expected) in [
            (
                "codex",
                &[
                    "PermissionRequest",
                    "PostToolUse",
                    "PreToolUse",
                    "SessionStart",
                    "Stop",
                    "SubagentStart",
                    "SubagentStop",
                    "UserPromptSubmit",
                ][..],
            ),
            (
                "cursor",
                &[
                    "beforeMCPExecution",
                    "beforeShellExecution",
                    "beforeSubmitPrompt",
                    "postToolUse",
                    "preToolUse",
                    "sessionEnd",
                    "sessionStart",
                    "stop",
                ][..],
            ),
            (
                "copilot",
                &[
                    "agentStop",
                    "permissionRequest",
                    "preToolUse",
                    "sessionEnd",
                    "sessionStart",
                    "userPromptSubmitted",
                ][..],
            ),
            (
                "kiro",
                &["agentSpawn", "postToolUse", "preToolUse", "stop"][..],
            ),
        ] {
            let (manifest, version) = builtin(id);
            let mut got = version_events(&manifest, &version);
            got.sort();
            assert_eq!(got, expected, "{id}");
        }
    }
}
