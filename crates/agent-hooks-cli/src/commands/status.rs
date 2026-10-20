use crate::cli::StatusArgs;
use crate::paths::{expand_env_vars, expand_op_path, expand_tilde};
use agent_hooks_adapters::manifest::{AdapterManifest, InstallOp};
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::time::Duration;

#[derive(Debug, serde::Serialize)]
struct StatusReport {
    daemon: DaemonStatus,
    installed_hooks: Vec<HarnessStatus>,
}

#[derive(Debug, serde::Serialize)]
struct DaemonStatus {
    socket: String,
    running: bool,
    version: Option<String>,
    binary_version: String,
    version_mismatch: bool,
    /// From the daemon's `ping` reply (`history_enabled`). Absent on older
    /// daemons, which reads as `false`: same version but no capture.
    history_enabled: bool,
    latency_ms: Option<u64>,
    error: Option<String>,
}

#[derive(Debug, serde::Serialize)]
struct HarnessStatus {
    id: String,
    display_name: String,
    unverified: bool,
    config_path: String,
    exists: bool,
    installed: bool,
    hook_count: usize,
    warnings: Vec<String>,
}
pub fn run(args: StatusArgs) -> anyhow::Result<()> {
    let socket_path = args
        .socket
        .unwrap_or_else(|| PathBuf::from(agent_hooks_protocol::default_socket_path()));
    let daemon = check_daemon(&socket_path);
    let installed_hooks = check_installed_hooks(&socket_path);

    if args.json {
        let report = StatusReport {
            daemon,
            installed_hooks,
        };
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    println!("agent-hooks status\n");

    println!("Daemon:");
    println!("  Socket: {}", daemon.socket);
    if daemon.running {
        println!("  Status: ✓ running");
        if let Some(v) = &daemon.version {
            println!("  Version: {} (binary {})", v, daemon.binary_version);
        } else {
            println!("  Version: unknown (binary {})", daemon.binary_version);
        }
        println!(
            "  History: {}",
            if daemon.history_enabled {
                "enabled"
            } else {
                "disabled (restart the daemon with --history to capture forwarded events)"
            }
        );
        if daemon.version_mismatch {
            println!(
                "  Warning: version mismatch — binary {} vs daemon {}",
                daemon.binary_version,
                daemon.version.as_deref().unwrap_or("unknown")
            );
        }
    } else {
        println!("  Status: ✗ not running");
        if let Some(e) = &daemon.error {
            println!("  Error: {}", e);
        }
        println!("  Hint: run `agent-hooks daemon` to start");
    }

    println!("\nInstalled hooks:");
    let mut any_installed = false;
    for h in &installed_hooks {
        let icon = if h.installed { "✓" } else { "-" };
        let detail = if h.exists {
            if h.installed {
                any_installed = true;
                format!("{} hook(s) in {}", h.hook_count, h.config_path)
            } else {
                format!("config exists but no hooks — {}", h.config_path)
            }
        } else {
            format!("no config — {}", h.config_path)
        };
        let name = if h.unverified {
            format!("{} [unverified contract]", h.display_name)
        } else {
            h.display_name.clone()
        };
        println!("  {} {} ({}): {}", icon, name, h.id, detail);
        if !h.warnings.is_empty() {
            println!(
                "    ! {} ({}): {}",
                h.display_name,
                h.id,
                h.warnings.join("; ")
            );
        }
    }

    if !any_installed {
        println!("\n  No hooks installed. Run `agent-hooks install --harness all` to install.");
    }

    println!("\nAdapters: run `agent-hooks list-adapters` for all supported harnesses.");

    Ok(())
}

fn check_daemon(socket_path: &Path) -> DaemonStatus {
    let socket_str = socket_path.display().to_string();
    let binary_version = env!("CARGO_PKG_VERSION").to_string();
    let start = std::time::Instant::now();

    match ping_daemon(socket_path) {
        Ok(ping) => {
            let version_mismatch = ping.version.as_ref().is_some_and(|v| v != &binary_version);
            DaemonStatus {
                socket: socket_str,
                running: true,
                version: ping.version,
                binary_version,
                version_mismatch,
                history_enabled: ping.history_enabled,
                latency_ms: Some(start.elapsed().as_millis() as u64),
                error: None,
            }
        }
        Err(e) => DaemonStatus {
            socket: socket_str,
            running: false,
            version: None,
            binary_version,
            version_mismatch: false,
            history_enabled: false,
            latency_ms: None,
            error: Some(e.to_string()),
        },
    }
}

/// What the daemon's `ping` reply tells us. Older daemons omit
/// `history_enabled`, which reads as `false` so a same-version daemon started
/// without `--history` is never mistaken for a capturing one.
struct PingInfo {
    version: Option<String>,
    history_enabled: bool,
}

fn ping_daemon(socket_path: &Path) -> anyhow::Result<PingInfo> {
    if cfg!(windows) {
        anyhow::bail!("windows named pipe check not yet implemented");
    }

    #[cfg(unix)]
    {
        use std::io::{Read, Write};
        use std::os::unix::net::UnixStream;

        let mut stream = UnixStream::connect(socket_path).map_err(|e| anyhow::anyhow!(e))?;
        stream
            .set_read_timeout(Some(Duration::from_millis(800)))
            .ok();
        stream
            .set_write_timeout(Some(Duration::from_millis(500)))
            .ok();

        let req = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "ping",
            "params": {}
        });
        let mut payload = req.to_string();
        payload.push('\n');
        stream.write_all(payload.as_bytes())?;
        stream.flush()?;

        let mut buf = vec![0u8; 4096];
        let n = stream.read(&mut buf)?;
        if n == 0 {
            return Ok(PingInfo {
                version: None,
                history_enabled: false,
            });
        }
        let resp_str = String::from_utf8_lossy(&buf[..n]).to_string();
        let val: serde_json::Value =
            serde_json::from_str(resp_str.trim()).unwrap_or(serde_json::Value::Null);
        let result = val.get("result");
        Ok(PingInfo {
            version: result
                .and_then(|r| r.get("version"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            history_enabled: result
                .and_then(|r| r.get("history_enabled"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
        })
    }

    #[cfg(not(unix))]
    {
        let _ = socket_path;
        anyhow::bail!("unsupported platform")
    }
}

// ---------------------------------------------------------------------------
// Manifest-driven hook inventory
// ---------------------------------------------------------------------------

/// Derive the harness list, config paths, and installed state from the
/// builtin manifests (default versions): config paths come from the
/// `json_hooks`/`toml_hooks` op paths (plus `stage_assets` dirs for
/// plugin-bridge harnesses), and installed state from scanning those
/// locations for our hook commands / staged assets.
fn check_installed_hooks(checked_socket: &Path) -> Vec<HarnessStatus> {
    let mut manifests = AdapterManifest::load_tiered();
    manifests.sort_by(|a, b| a.id.cmp(&b.id));
    let mut out = Vec::new();
    for m in &manifests {
        let Some(vs) = m.versions.get(&m.default_version) else {
            continue;
        };
        // Candidate locations in op order: config files first, then stage dirs.
        let mut file_paths: Vec<PathBuf> = Vec::new();
        let mut stage_dirs: Vec<PathBuf> = Vec::new();
        for op in &vs.install {
            match op {
                InstallOp::JsonHooks { paths, .. }
                | InstallOp::TomlHooks { paths, .. }
                | InstallOp::TomlListHooks { paths, .. }
                | InstallOp::JsonEdit { paths, .. } => {
                    for raw in paths {
                        let p = expand_op_path(raw);
                        if !file_paths.contains(&p) {
                            file_paths.push(p);
                        }
                    }
                }
                InstallOp::StageAssets { stage_dir, .. } => {
                    let d = expand_op_path(stage_dir);
                    if !stage_dirs.contains(&d) {
                        stage_dirs.push(d);
                    }
                }
                InstallOp::RunCommand { .. } => {}
            }
        }
        let mut exists = false;
        let mut installed = false;
        let mut hook_count = 0usize;
        let mut found_path: Option<PathBuf> = None;
        let mut existing_files: Vec<PathBuf> = Vec::new();

        for p in &file_paths {
            if p.exists() {
                exists = true;
                found_path.get_or_insert_with(|| p.clone());
                existing_files.push(p.clone());
                let (inst, count) = count_hooks_in_file(p);
                if inst {
                    installed = true;
                    hook_count += count;
                }
            }
        }
        for d in &stage_dirs {
            if d.is_dir() {
                exists = true;
                found_path.get_or_insert_with(|| d.clone());
                let count = count_staged_assets(d);
                if count > 0 {
                    installed = true;
                    hook_count += count;
                }
            }
        }

        let display_path = found_path
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| {
                file_paths
                    .first()
                    .or(stage_dirs.first())
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "(no install paths)".to_string())
            });

        out.push(HarnessStatus {
            id: m.id.clone(),
            display_name: m.display_name.clone(),
            unverified: m.unverified,
            config_path: display_path,
            exists,
            installed,
            hook_count,
            warnings: detect_drift(&existing_files, checked_socket),
        });
    }
    out
}

/// Count staged asset files that look managed (marker or agent-hooks
/// reference). Foreign files in shared plugin dirs don't count.
fn count_staged_assets(dir: &Path) -> usize {
    let mut count = 0usize;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let entries = match std::fs::read_dir(&d) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.is_file() {
                let content = std::fs::read_to_string(&path).unwrap_or_default();
                if content.contains("managed by agent-hooks") || content.contains("agent-hooks") {
                    count += 1;
                }
            }
        }
    }
    count
}

fn count_hooks_in_file(path: &Path) -> (bool, usize) {
    let data = match std::fs::read_to_string(path) {
        Ok(d) => d,
        Err(_) => return (false, 0),
    };
    // Try JSON: recursively count `command` strings referencing agent-hooks.
    // Covers flat, matcher_group, and named-bundle shapes.
    if let Ok(val) = serde_json::from_str::<serde_json::Value>(&data) {
        let mut count = 0usize;
        count_commands(&val, &mut count);
        if count > 0 {
            return (true, count);
        }
        // Fallback: raw marker search.
        let raw = data.matches("agent-hooks").count();
        return (raw > 0, raw);
    }
    // Plain/TOML
    let count = data.matches("agent-hooks").count();
    (count > 0, count)
}

fn count_commands(value: &serde_json::Value, count: &mut usize) {
    match value {
        serde_json::Value::Object(map) => {
            if map
                .get("command")
                .and_then(|c| c.as_str())
                .is_some_and(|s| s.contains("agent-hooks"))
            {
                *count += 1;
            }
            for v in map.values() {
                count_commands(v, count);
            }
        }
        serde_json::Value::Array(arr) => {
            for v in arr {
                count_commands(v, count);
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Drift detection: compare embedded hook commands against this binary/socket
// ---------------------------------------------------------------------------

/// Compare each agent-hooks hook command embedded in the harness's existing
/// config files against the running binary and the checked socket. Returns
/// additive warning strings (`binary-mismatch`, `socket-mismatch`,
/// `dangling-command`); empty when the install matches this binary/socket.
fn detect_drift(existing_files: &[PathBuf], checked_socket: &Path) -> Vec<String> {
    let current_exe = std::env::current_exe().ok();
    let mut commands: Vec<String> = Vec::new();
    for path in existing_files {
        for cmd in collect_hook_commands_from_file(path) {
            if !commands.contains(&cmd) {
                commands.push(cmd);
            }
        }
    }
    let mut warnings: Vec<String> = Vec::new();
    for cmd in &commands {
        let Some((binary, socket)) = parse_hook_command(cmd) else {
            continue;
        };
        let expanded_bin = expand_tilde(&expand_env_vars(&binary))
            .display()
            .to_string();
        let expanded_bin_path = expand_tilde(&expand_env_vars(&binary));
        // Dangling: the embedded binary path does not resolve to anything.
        if !binary_resolves(&binary) {
            push_unique(
                &mut warnings,
                format!("dangling-command: embedded binary '{expanded_bin}' does not exist"),
            );
        }
        // Binary mismatch: embedded path differs from the running binary,
        // or names a file that is not executable.
        let mismatch = match &current_exe {
            Some(exe) => !same_binary(&expanded_bin, exe),
            None => !is_executable_path(&expanded_bin_path),
        } || !is_executable_path(&expanded_bin_path);
        if mismatch {
            match &current_exe {
                Some(exe) => push_unique(
                    &mut warnings,
                    format!(
                        "binary-mismatch: embedded binary '{expanded_bin}' != current binary '{}'",
                        exe.display()
                    ),
                ),
                None => push_unique(
                    &mut warnings,
                    format!("binary-mismatch: embedded binary '{expanded_bin}' is not executable"),
                ),
            }
        }
        // Socket mismatch: embedded --socket differs from the checked socket.
        if let Some(sock) = socket {
            let expanded_sock = expand_tilde(&expand_env_vars(&sock));
            if expanded_sock != checked_socket.to_path_buf()
                && !same_path_canon(&expanded_sock, checked_socket)
            {
                push_unique(
                    &mut warnings,
                    format!(
                        "socket-mismatch: embedded socket '{}' != checked socket '{}'",
                        expanded_sock.display(),
                        checked_socket.display()
                    ),
                );
            }
        }
    }
    warnings
}

fn push_unique(warnings: &mut Vec<String>, warning: String) {
    if !warnings.contains(&warning) {
        warnings.push(warning);
    }
}

/// Collect every `command` string from a config file. JSON configs are walked
/// structurally; anything else (TOML/plain) is scanned for quoted segments
/// mentioning hooks. Shape validation happens in [`parse_hook_command`].
fn collect_hook_commands_from_file(path: &Path) -> Vec<String> {
    let data = match std::fs::read_to_string(path) {
        Ok(d) => d,
        Err(_) => return Vec::new(),
    };
    let mut out: Vec<String> = Vec::new();
    if let Ok(val) = serde_json::from_str::<serde_json::Value>(&data) {
        collect_commands_str(&val, &mut out);
    }
    if out.is_empty() {
        extract_quoted_commands(&data, &mut out);
    }
    out
}

fn collect_commands_str(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(s) = map.get("command").and_then(|c| c.as_str()) {
                if !out.contains(&s.to_string()) {
                    out.push(s.to_string());
                }
            }
            for v in map.values() {
                collect_commands_str(v, out);
            }
        }
        serde_json::Value::Array(arr) => {
            for v in arr {
                collect_commands_str(v, out);
            }
        }
        _ => {}
    }
}

/// Scan raw text for single/double-quoted segments that look like hook
/// commands (covers TOML `command = "..."` lines and plain-text configs).
fn extract_quoted_commands(text: &str, out: &mut Vec<String>) {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let q = bytes[i];
        if q != b'"' && q != b'\'' {
            i += 1;
            continue;
        }
        let mut seg = String::new();
        let mut j = i + 1;
        let mut closed = false;
        while j < bytes.len() {
            let c = bytes[j];
            if c == b'\\' && q == b'"' && j + 1 < bytes.len() {
                let n = bytes[j + 1];
                match n {
                    b'n' => seg.push('\n'),
                    b't' => seg.push('\t'),
                    b'r' => seg.push('\r'),
                    b'\\' => seg.push('\\'),
                    b'"' => seg.push('"'),
                    _ => {
                        seg.push('\\');
                        seg.push(n as char);
                    }
                }
                j += 2;
                continue;
            }
            if c == q {
                closed = true;
                break;
            }
            seg.push(c as char);
            j += 1;
        }
        if closed && seg.contains("hook") && !out.contains(&seg) {
            out.push(seg);
        }
        i = if closed { j + 1 } else { bytes.len() };
    }
}

/// Split an embedded binary + socket out of a hook command string.
/// Shape: `<binary> hook <harness> --event <Event> [--socket <path>] ...`
/// The binary token is validated by invocation shape (not by name) so a
/// hand-edited bogus binary still yields a `dangling-command` warning.
/// Returns the raw (still quoted/expanded-later) binary token and socket.
fn parse_hook_command(cmd: &str) -> Option<(String, Option<String>)> {
    let tokens = shell_split(cmd);
    if tokens.is_empty() {
        return None;
    }
    let hook_idx = tokens.iter().position(|t| t == "hook")?;
    if hook_idx == 0 {
        return None;
    }
    if !tokens
        .iter()
        .any(|t| t == "--event" || t.starts_with("--event="))
    {
        return None;
    }
    let binary = tokens[hook_idx - 1].clone();
    let mut socket: Option<String> = None;
    let mut k = 0;
    while k < tokens.len() {
        if tokens[k] == "--socket" && k + 1 < tokens.len() {
            socket = Some(tokens[k + 1].clone());
        } else if let Some(rest) = tokens[k].strip_prefix("--socket=") {
            socket = Some(rest.to_string());
        }
        k += 1;
    }
    Some((binary, socket))
}

/// Minimal shell-word split honoring single/double quotes. Backslashes only
/// escape adjacent whitespace/quotes (Windows paths stay intact).
fn shell_split(cmd: &str) -> Vec<String> {
    fn escapable(c: char) -> bool {
        c.is_whitespace() || c == '"' || c == '\'' || c == '\\'
    }
    let mut tokens: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut in_tok = false;
    let mut quote: Option<char> = None;
    let mut chars = cmd.chars();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                } else if c == '\\' && q == '"' {
                    if let Some(n) = chars.next() {
                        if n == '"' || n == '\\' || n == '$' || n == '`' {
                            cur.push(n);
                        } else {
                            cur.push('\\');
                            cur.push(n);
                        }
                    } else {
                        cur.push('\\');
                    }
                } else {
                    cur.push(c);
                }
            }
            None => {
                if c == '"' || c == '\'' {
                    quote = Some(c);
                    in_tok = true;
                } else if c == '\\' {
                    if let Some(n) = chars.next() {
                        if escapable(n) {
                            cur.push(n);
                        } else {
                            cur.push('\\');
                            cur.push(n);
                        }
                        in_tok = true;
                    } else {
                        cur.push('\\');
                        in_tok = true;
                    }
                } else if c.is_whitespace() {
                    if in_tok {
                        tokens.push(std::mem::take(&mut cur));
                        in_tok = false;
                    }
                } else {
                    cur.push(c);
                    in_tok = true;
                }
            }
        }
    }
    if in_tok {
        tokens.push(cur);
    }
    tokens
}

/// True when the embedded binary token resolves to an existing file:
/// an explicit path must exist; a bare name must be found on `PATH`.
fn binary_resolves(raw: &str) -> bool {
    let expanded = expand_tilde(&expand_env_vars(raw));
    if raw.contains('/') || raw.contains('\\') {
        expanded.exists()
    } else {
        find_on_path(raw).is_some()
    }
}

fn is_executable_path(path: &Path) -> bool {
    if !path.is_file() {
        return find_on_path(&path.display().to_string()).is_some();
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let cand = dir.join(name);
        if is_executable_file(&cand) {
            return Some(cand);
        }
        #[cfg(windows)]
        {
            let exe = dir.join(format!("{name}.exe"));
            if exe.is_file() {
                return Some(exe);
            }
        }
    }
    None
}

fn is_executable_file(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Compare an embedded binary string against the running exe: equal after
/// canonicalization (symlink-tolerant), else exact string equality.
fn same_binary(embedded_expanded: &str, exe: &Path) -> bool {
    if embedded_expanded == exe.display().to_string() {
        return true;
    }
    #[cfg(windows)]
    if embedded_expanded.eq_ignore_ascii_case(&exe.display().to_string()) {
        return true;
    }
    let emb = Path::new(embedded_expanded);
    if let (Ok(a), Ok(b)) = (std::fs::canonicalize(emb), std::fs::canonicalize(exe)) {
        return a == b;
    }
    false
}

/// Canonical-compare two paths when both resolve; else false (caller also
/// compares the raw path strings first).
fn same_path_canon(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}
