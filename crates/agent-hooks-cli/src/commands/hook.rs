use crate::cli::HookArgs;
use agent_hooks_protocol::{
    AgentEvent, EventContext, ORIGIN_EXTENSION, PromptDecision, RawHookInput,
};
use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Written when nothing can be rendered in the harness dialect: `{}` is "no
/// hook output" for every JSON harness, so the harness keeps its own
/// handling. Never a bare allow — that approves calls nobody decided.
const FALLBACK_PASSTHROUGH: &str = "{}";

/// Socket read budget for observe/continue events: the daemon answers these
/// immediately, so waiting longer only stalls the agent.
const FAST_WAIT: Duration = Duration::from_millis(500);
/// Forward-retry policy (shim -> daemon): gate-capability events get up to
/// this many attempts with `FORWARD_RETRY_BACKOFF` between them, all inside
/// the existing hold/timeout budget (`GATE_WAIT`); observe/continue events
/// get exactly 1 attempt (fail fast). Retries fire ONLY on transport failure
/// (connect refused/reset/empty reply) — a received daemon verdict is final
/// and never re-requested. Exhaustion fails open to the local decision.
const GATE_MAX_ATTEMPTS: u32 = 3;
const FORWARD_RETRY_BACKOFF: Duration = Duration::from_millis(50);

/// Ceiling on a gate-event socket read when the manifest declares no harness
/// timeout. Normally the budget below governs instead.
const GATE_WAIT: Duration = Duration::from_secs(330);

/// Head start the shim keeps over the harness's own hook timeout.
///
/// Smaller than the daemon's `HARNESS_MARGIN` (5s) on purpose: the daemon
/// must resolve its hold and answer while we are still reading, so the
/// verdict arrives in-band. If we gave up first we would fail open to the
/// local decision and throw away a verdict that was about to arrive.
const SHIM_MARGIN: Duration = Duration::from_secs(2);

/// Everything one event costs — local hooks plus the daemon round trip — has
/// to fit in the window the harness allows, or our output is discarded and
/// the harness applies its own failure semantics instead of our decision.
/// Tracks what is left of that window.
#[derive(Clone, Copy)]
struct Budget {
    deadline: Option<Instant>,
}

impl Budget {
    fn new(total: Option<Duration>) -> Self {
        Self {
            deadline: total.map(|t| {
                Instant::now() + t.saturating_sub(SHIM_MARGIN).max(Duration::from_secs(1))
            }),
        }
    }

    fn remaining(&self) -> Option<Duration> {
        self.deadline
            .map(|d| d.saturating_duration_since(Instant::now()))
    }

    fn clamp(&self, want: Duration) -> Duration {
        match self.remaining() {
            Some(left) => want.min(left),
            None => want,
        }
    }

    fn spent(&self) -> bool {
        self.remaining() == Some(Duration::ZERO)
    }
}

/// Default socket locations.
pub fn default_socket_path() -> PathBuf {
    PathBuf::from(agent_hooks_protocol::default_socket_path())
}

/// Entry point for `agent-hooks hook`.
pub fn run(args: HookArgs) -> anyhow::Result<()> {
    // Resolve harness_hint / raw_payload with JSON-first-arg quirk handling.
    // See `resolve_harness_and_payload`.
    let (harness_hint, raw_payload) = resolve_harness_and_payload(&args);
    // Best-effort fixture capture for `adapter new` workflows: snapshot the raw
    // payload BEFORE forwarding so offline `adapter test` fixtures stay faithful.
    // Write errors are ignored — capture must never block the agent.
    if let Some(dir) = args.capture_fixtures.as_ref() {
        write_capture_fixture(dir, &harness_hint, args.event.as_deref(), &raw_payload);
    }

    // 2. Capture context from environment (minimal, harness-agnostic).
    // Heavy terminal/mux resolution (bundle_id, kitty/wezterm/herdr/cmux
    // targets) is the consumer's job via `EventEnricher` / `extensions`.
    // We only forward opaque env vars so the consumer can resolve lazily
    // without adding latency to the <2ms shim.
    let mut env_policy = super::user_hooks::EnvCapturePolicy::default();
    let (merged_cfg, cfg_warnings) = super::user_hooks::load_merged(&mut env_policy);
    for warning in &cfg_warnings {
        eprintln!("agent-hooks: {warning}");
    }
    let requested = requested_env_names(args.socket.as_deref());
    let context = capture_context();
    let mut passthrough_env = capture_passthrough_env();
    // Subscription is consent: a connected subscriber's `passthrough_env`
    // request is enough — no `allowed_passthrough_env` allow-list gate.
    // `passthrough_env` config remains as always-capture names.
    for name in env_policy.always.iter().chain(requested.iter()) {
        if !passthrough_env.contains_key(name) {
            if let Ok(value) = std::env::var(name) {
                if !value.is_empty() {
                    passthrough_env.insert(name.clone(), value);
                }
            }
        }
    }

    let input = RawHookInput {
        harness_hint,
        event_hint: args.event.clone(),
        raw_payload,
        context,
        request_id: args.request_id.clone(),
        passthrough_env,
    };

    // 3. Resolve the adapter once and parse locally. The normalized event
    //    drives daemonless local hooks AND pre-populates the adapter's
    //    response-dialect state for format_response below.
    let adapter = resolve_adapter(&input, args.adapter_version.as_deref());
    let parsed = adapter.as_ref().map(|a| a.parse_event(&input));
    // Unknown/unmapped host events degrade to observe/no-op: no local hooks
    // (no event, no match), best-effort daemon forward, observe/empty dialect
    // below, exit 0. Never error-exit on unknown events.
    if let Some(msg) = parse_failure_warning(parsed.as_ref(), args.event.as_deref()) {
        eprintln!("{msg}");
    }
    let event = parsed.and_then(|r| r.ok());

    // 4. Deterministic local hooks from ~/.agent-hooks.json overlaid with
    //    .agent-hooks/hooks.json — no daemon required. Config warnings go
    //    to stderr but never fail the agent.
    // One budget for the whole event, taken from what this harness says it
    // will wait for this hook. Local scripts spend from it first, then the
    // daemon round trip spends what is left.
    let budget = Budget::new(
        adapter
            .as_ref()
            .zip(event.as_ref())
            .and_then(|(a, e)| a.entry_timeout_for(e)),
    );
    let local = event.as_ref().and_then(|e| {
        super::user_hooks::run_local_hooks(
            e,
            super::user_hooks::entries_for(&merged_cfg, e),
            budget.remaining(),
        )
    });

    // History capture is independent of the daemon and of the verdict: the
    // store existing is the opt-in, and the decision never depends on it. It
    // commits before the daemon sees the event, so a subscriber reacting to
    // that event (and a held gate event, which can wait on a human) always
    // finds it in the store.
    record_history(event.as_ref(), args.origin.as_deref());

    // 5. Best-effort daemon forward (subscribers + live policy). Gate events
    //    wait long enough for a human decision and retry transport failures
    //    (up to GATE_MAX_ATTEMPTS, ~50ms backoff, inside the hold budget);
    //    everything else gets exactly 1 attempt (fail fast). A received
    //    verdict is final, never re-requested. Exhaustion fails open to the
    //    local decision with one stderr warning — never block the agent.
    let socket_path = args.socket.clone().unwrap_or_else(default_socket_path);
    let attempts = if is_gate_event(adapter.as_ref(), event.as_ref()) {
        GATE_MAX_ATTEMPTS
    } else {
        1
    };
    // Consumer context (`--context-cmd`) only matters when a daemon send will
    // happen: skipped once the budget is gone or no daemon socket exists.
    let mut extensions = match args.context_cmd.as_deref() {
        Some(cmd) if !budget.spent() && daemon_socket_present(&socket_path) => {
            let harness_id = adapter
                .as_ref()
                .map(|a| a.id().to_string())
                .unwrap_or_else(|| input.harness_hint.clone());
            // Only a session id the harness actually sent: adapters mint a
            // UUID for session-less payloads, and a consumer keying per-session
            // state on that would treat every event as a new session.
            let session = event
                .as_ref()
                .map(|e| e.session_id.as_str())
                .filter(|id| !id.is_empty() && input.raw_payload.contains(*id));
            run_context_cmd(
                cmd,
                &harness_id,
                args.event.as_deref().unwrap_or(""),
                session,
                budget.clamp(context_cmd_timeout()),
            )
        }
        _ => serde_json::Map::new(),
    };
    if let Some(origin) = args
        .origin
        .as_deref()
        .map(str::trim)
        .filter(|o| !o.is_empty())
    {
        extensions.insert(
            ORIGIN_EXTENSION.to_string(),
            serde_json::Value::String(origin.to_string()),
        );
        // The shim's own event id doubles as the delivery id, so a retried
        // send of this occurrence is recorded once by a daemon.
        if let Some(event) = event.as_ref() {
            extensions.insert(
                agent_hooks_protocol::DELIVERY_ID_EXTENSION.to_string(),
                serde_json::Value::String(event.event_id.clone()),
            );
        }
    }
    // Whatever the scripts (and context command) already spent comes out of
    // the daemon's share.
    let wait = forward_timeout(adapter.as_ref(), event.as_ref(), budget);
    // Budget already gone to local scripts: skip the daemon rather than
    // overrun the harness and have the whole decision discarded. Whatever the
    // scripts returned still stands; nothing returned still means proceed.
    let daemon_raw: Option<String> = if budget.spent() {
        eprintln!("agent-hooks: harness budget spent by local hooks; skipping daemon forward");
        None
    } else {
        match agent_hooks_protocol::hook_line_with_extensions(&input, &extensions) {
            Ok(line) => forward_with_retries(&socket_path, &line, wait, attempts),
            Err(e) => {
                eprintln!("agent-hooks: cannot serialize hook input: {e}; skipping daemon forward");
                None
            }
        }
    };
    let daemon = daemon_raw.as_deref().map(parse_decision);

    // 6. Deny wins from either side: a dead daemon degrades to the local
    //    scripts instead of blanket-allow; a live daemon keeps full veto.
    let decision = super::user_hooks::merge_decisions(local, daemon);

    // 7. Render the merged decision in the harness dialect.
    match adapter {
        Some(adapter) => {
            if event.is_none() {
                // Populate response-dialect state best-effort (unchanged).
                let _ = adapter.parse_event(&input);
            }
            // Exit 2 ONLY for deny-equivalent in `exit2` response mode.
            // `PromptDecision::ask` sets `allow: false`, so ask also exits 2
            // here with the question (`custom_input`) on stderr — fail-safe.
            // Every infra failure (daemon down, parse failure, local hook
            // spawn failure) resolves to allow above and exits 0.
            if adapter.response_mode() == agent_hooks_adapters::ResponseMode::Exit2
                && !decision.allow
                && !decision.defer
            {
                if let Some(reason) = decision.custom_input.clone() {
                    if !reason.is_empty() {
                        eprintln!("{}", reason);
                    }
                }
                std::process::exit(2);
            }
            let daemon_stdout = extract_daemon_stdout(daemon_raw.as_deref());
            let out_str = if decision.allow && daemon_stdout.is_some() {
                daemon_stdout.unwrap()
            } else {
                let formatted = adapter.format_response(&decision);
                serde_json::to_string(&formatted).unwrap_or_else(|_| {
                    daemon_raw
                        .clone()
                        .unwrap_or_else(|| FALLBACK_PASSTHROUGH.to_string())
                })
            };
            let stdout = std::io::stdout();
            let mut out = stdout.lock();
            let _ = writeln!(out, "{}", out_str);
        }
        None => {
            // No adapter resolves: legacy passthrough (daemon raw or allow).
            let stdout = std::io::stdout();
            let mut out = stdout.lock();
            let out_str = extract_daemon_stdout(daemon_raw.as_deref()).unwrap_or_else(|| {
                daemon_raw
                    .clone()
                    .unwrap_or_else(|| FALLBACK_PASSTHROUGH.to_string())
            });
            let _ = writeln!(out, "{}", out_str);
        }
    }
    // Always exit 0 — never fail the agent's tool execution due to hook infra.
    Ok(())
}

fn resolve_adapter(
    input: &RawHookInput,
    adapter_version: Option<&str>,
) -> Option<Box<dyn agent_hooks_adapters::HarnessAdapter>> {
    use agent_hooks_adapters::AdapterRegistry;
    if !input.harness_hint.is_empty() {
        if let Some(a) = AdapterRegistry::get_versioned(&input.harness_hint, adapter_version) {
            return Some(a);
        }
    }
    if let Some(detected) = AdapterRegistry::detect_from_input(input) {
        let id = detected.id().to_string();
        if let Some(versioned) = AdapterRegistry::get_versioned(&id, adapter_version) {
            return Some(versioned);
        }
        return Some(detected);
    }
    None
}

fn extract_daemon_stdout(raw: Option<&str>) -> Option<String> {
    let raw = raw?;
    let val = serde_json::from_str::<serde_json::Value>(raw).ok()?;
    let target = val.get("result").unwrap_or(&val);
    target
        .get("stdout")
        .and_then(|s| s.as_str())
        .map(|s| s.to_string())
}

/// A harness-native verdict (`hookSpecificOutput` as Claude Code prints it),
/// for daemons that relay a harness's own stdout instead of a verdict line.
fn native_decision(v: &serde_json::Value) -> Option<PromptDecision> {
    let hso = v.get("hookSpecificOutput")?;
    let text = |x: Option<&serde_json::Value>| x.and_then(|x| x.as_str()).map(str::to_string);
    if let Some(perm) = hso.get("permissionDecision").and_then(|x| x.as_str()) {
        let reason = text(hso.get("permissionDecisionReason"));
        match perm.to_ascii_lowercase().as_str() {
            "deny" => return Some(PromptDecision::deny(reason)),
            "allow" => return Some(PromptDecision::allow()),
            "ask" => return Some(PromptDecision::ask(reason)),
            _ => {}
        }
    }
    let dec = hso.get("decision")?.as_object()?;
    let behavior = dec.get("behavior")?.as_str()?;
    if behavior.eq_ignore_ascii_case("deny") {
        Some(PromptDecision::deny(text(dec.get("message"))))
    } else if behavior.eq_ignore_ascii_case("allow") {
        Some(PromptDecision::allow())
    } else {
        None
    }
}

/// Decode a daemon reply into the canonical [`PromptDecision`].
///
/// The daemon's verdict line is a serialized `PromptDecision` plus a
/// redundant `decision` verb, so serde reads every field the renderer needs
/// (`allow_always`, `suggestion`, `custom_input`, `answers`, …). Only a
/// defer — which has no `allow` — is recognized by its verb. `result` /
/// `stdout` envelopes and harness-native `hookSpecificOutput` are unwrapped
/// for daemons that relay harness output.
fn parse_decision_from_value(v: &serde_json::Value) -> Option<PromptDecision> {
    if v.get("allow").is_none()
        && v.get("decision")
            .and_then(|x| x.as_str())
            .is_some_and(|d| d.eq_ignore_ascii_case("defer"))
    {
        return Some(PromptDecision::defer());
    }
    if let Some(d) = v.get("result").and_then(parse_decision_from_value) {
        return Some(d);
    }
    if let Some(d) = v
        .get("stdout")
        .and_then(|x| x.as_str())
        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
        .and_then(|inner| parse_decision_from_value(&inner))
    {
        return Some(d);
    }
    if let Some(mut d) = native_decision(v) {
        d.answers = v
            .get("answers")
            .and_then(|a| serde_json::from_value(a.clone()).ok())
            .unwrap_or_default();
        return Some(d);
    }
    // The daemon's verdict line is a PromptDecision plus a redundant
    // `decision` verb (an envelope key, not a decision field); everything
    // else is parsed by the protocol type itself.
    let mut decision = v.as_object()?.clone();
    decision.remove("decision");
    serde_json::from_value::<PromptDecision>(serde_json::Value::Object(decision)).ok()
}

fn parse_decision(trimmed: &str) -> PromptDecision {
    serde_json::from_str::<serde_json::Value>(trimmed)
        .ok()
        .as_ref()
        .and_then(parse_decision_from_value)
        // Unparseable daemon reply: no verdict was reached, so hand the call
        // back to the harness rather than approving it.
        .unwrap_or_else(PromptDecision::defer)
}

/// Resolve harness_hint and raw_payload from HookArgs, handling the
/// codex JSON-first-arg quirk (`agent-hooks hook '{"key":...}' --event X`).
///
/// - If `harness_override` (`--harness`) is present and non-empty, it always
///   wins as `harness_hint`. If the positional `harness` looks like JSON
///   (trimmed starts with '{'), it is treated as payload.
/// - Otherwise, if positional `harness` trimmed starts with '{', treat it as
///   the raw JSON payload with unknown harness (`""`) and let registry
///   detection sniff the adapter (codex matches on payload).
/// - Otherwise, normal path: harness_hint = positional, payload = trailing
///   args or stdin — byte-identical to prior behavior.
fn resolve_harness_and_payload(args: &HookArgs) -> (String, String) {
    let jcode_payload = std::env::var(JCODE_PAYLOAD_ENV).ok();
    if let Some(resolved) = jcode_env_payload(args, jcode_payload.as_deref()) {
        return resolved;
    }
    resolve_harness_and_payload_with(args, &mut read_piped_stdin)
}

/// jcode observer hooks get no stdin; the canonical JSON payload is exported
/// in this env var instead.
const JCODE_PAYLOAD_ENV: &str = "JCODE_HOOK_PAYLOAD";

/// jcode's env payload wins over argv/stdin (legacy B2V
/// resources/b2v-agent-event.sh: `JCODE_HOOK_PAYLOAD` overrides PAYLOAD_ARG).
/// Checked before stdin is touched: a jcode hook may leave stdin as an
/// unclosed pipe, and reading it would stall the agent.
fn jcode_env_payload(args: &HookArgs, env_payload: Option<&str>) -> Option<(String, String)> {
    let payload = normalize_payload(env_payload?);
    if payload.is_empty() {
        return None;
    }
    let harness = args
        .harness_override
        .as_deref()
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| args.harness.trim());
    harness
        .eq_ignore_ascii_case("jcode")
        .then(|| (harness.to_string(), payload))
}

/// Lock-wait budget for the shim's history write. Parallel agents each spawn
/// a shim, and SQLite allows one writer at a time; stays well inside the
/// harness hook timeouts.
const HISTORY_LOCK_WAIT: Duration = Duration::from_millis(200);

/// Persist this event into the opt-in history store, if one exists. A missing
/// store is the off state and stays silent; any other failure is a stderr
/// warning (never content) and never changes the hook outcome.
fn record_history(event: Option<&AgentEvent>, origin: Option<&str>) {
    use agent_hooks_history::{Capture, HistoryError, HistoryWriter, resolve_history_dir};
    let Some(event) = event else { return };
    let origin = origin
        .map(str::trim)
        .filter(|o| !o.is_empty())
        .unwrap_or(agent_hooks_protocol::LOCAL_ORIGIN);
    let Some(capture) = Capture::from_event(event, origin) else {
        return;
    };
    let mut writer = match HistoryWriter::open(&resolve_history_dir(None)) {
        Ok(w) => w,
        Err(HistoryError::Disabled(_)) => return,
        Err(e) => {
            eprintln!("agent-hooks: history capture skipped: {e}");
            return;
        }
    };
    let result = writer
        .set_busy_timeout(HISTORY_LOCK_WAIT)
        .and_then(|()| writer.record(&capture).map(drop));
    if let Err(e) = result {
        eprintln!("agent-hooks: history write failed: {e}");
    }
}

/// Stderr line for an event the adapter could not turn into an AgentEvent.
/// `Ignored` is manifest-declared noise the daemon defers on — expected, so
/// silent unless `AGENT_HOOKS_DEBUG` is set.
fn parse_failure_warning(
    parsed: Option<&Result<AgentEvent, agent_hooks_adapters::AdapterError>>,
    event_hint: Option<&str>,
) -> Option<String> {
    let hint = event_hint.unwrap_or("(none)");
    match parsed {
        Some(Ok(_)) => None,
        Some(Err(agent_hooks_adapters::AdapterError::Ignored(why))) => {
            debug_enabled().then(|| format!("agent-hooks: ignored event '{hint}': {why}"))
        }
        _ => Some(format!(
            "agent-hooks: unknown event '{hint}'; forwarding best-effort with no local hooks"
        )),
    }
}

/// Payload normalization: BOM-strip + trim + double-encoded-JSON unwrap.
/// Pure string function so unit tests cover it without touching stdin.
/// Testable core below takes an injected `read_stdin` so unit tests never
/// touch real stdin — under `cargo test` stdin is often a held-open pipe,
/// where `read_to_string` would block forever.
/// Empty-after-trim stays empty (explicit empty-payload path downstream).
fn normalize_payload(raw: &str) -> String {
    let t = raw.strip_prefix('\u{FEFF}').unwrap_or(raw).trim();
    if t.len() >= 2 && t.starts_with('"') {
        if let Ok(inner) = serde_json::from_str::<String>(t) {
            let inner = inner.strip_prefix('\u{FEFF}').unwrap_or(&inner).trim();
            if inner.starts_with('{') || inner.starts_with('[') {
                return inner.to_string();
            }
        }
    }
    t.to_string()
}

fn resolve_harness_and_payload_with(
    args: &HookArgs,
    read_stdin: &mut dyn FnMut() -> Option<String>,
) -> (String, String) {
    // Explicit `--harness` always wins over sniffing.
    if let Some(explicit) = &args.harness_override {
        let explicit_trimmed = explicit.trim();
        if !explicit_trimmed.is_empty() {
            let pos_norm = normalize_payload(&args.harness);
            if pos_norm.starts_with('{') {
                let trailing = collect_payload(&args.payload, read_stdin);
                let payload = if trailing.is_empty() {
                    pos_norm
                } else {
                    format!("{pos_norm} {trailing}")
                };
                return (explicit_trimmed.to_string(), payload);
            }
            return (
                explicit_trimmed.to_string(),
                collect_payload(&args.payload, read_stdin),
            );
        }
    }

    let pos_norm = normalize_payload(&args.harness);
    if pos_norm.starts_with('{') {
        let trailing = collect_payload(&args.payload, read_stdin);
        let payload = if trailing.is_empty() {
            pos_norm
        } else {
            format!("{pos_norm} {trailing}")
        };
        return (String::new(), payload);
    }
    (pos_norm, collect_payload(&args.payload, read_stdin))
}

fn collect_payload(trailing: &[String], read_stdin: &mut dyn FnMut() -> Option<String>) -> String {
    // Existing order kept: trailing argv wins when present, else piped stdin.
    // Both paths are BOM-stripped, trimmed, and double-decode tolerant;
    // empty-after-trim flows through as the explicit empty-payload path
    // (fail-open allow downstream, never a sniffing crash).
    if !trailing.is_empty() {
        return normalize_payload(&trailing.join(" "));
    }
    match read_stdin() {
        Some(s) => normalize_payload(&s),
        None => String::new(),
    }
}

/// Production stdin source: terminal (interactive, no pipe) yields `None`
/// instead of blocking; otherwise read the pipe fully as bytes and decode
/// lossily — invalid UTF-8 never panics, it degrades to U+FFFD and the
/// (likely unparseable) payload fails open to allow downstream.
fn read_piped_stdin() -> Option<String> {
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        return None;
    }
    let mut buf = Vec::new();
    let mut handle = stdin.lock();
    match handle.read_to_end(&mut buf) {
        Ok(_) => Some(String::from_utf8_lossy(&buf).into_owned()),
        Err(_) => Some(String::new()),
    }
}

/// Best-effort fixture snapshot for `agent-hooks hook --capture-fixtures <dir>`.
///
/// Writes `<dir>/<harness>-<event>-<unixts>.json` containing
/// `{"harness", "event_hint", "stdin"}`. All errors are swallowed — capture
/// must never fail or block the agent's hook execution.
fn write_capture_fixture(dir: &Path, harness: &str, event: Option<&str>, payload: &str) {
    let sanitize = |s: &str| -> String {
        let cleaned: String = s
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let cleaned = cleaned.trim_matches('_').to_string();
        if cleaned.is_empty() {
            "unknown".to_string()
        } else {
            cleaned
        }
    };
    let harness = sanitize(harness);
    let event = event.map(sanitize).unwrap_or_else(|| "event".to_string());
    let unixts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let stdin: serde_json::Value = serde_json::from_str(payload)
        .unwrap_or_else(|_| serde_json::Value::String(payload.to_string()));
    let doc = serde_json::json!({
        "harness": harness,
        "event_hint": event,
        "stdin": stdin,
    });
    let _ = (|| -> anyhow::Result<()> {
        std::fs::create_dir_all(dir)?;
        let text = serde_json::to_string_pretty(&doc)?;
        std::fs::write(dir.join(format!("{harness}-{event}-{unixts}.json")), text)?;
        Ok(())
    })();
}

fn capture_context() -> EventContext {
    let cwd = std::env::current_dir().ok();
    let git_worktree = cwd
        .as_deref()
        .and_then(find_git_root)
        .or_else(|| cwd.clone());

    let tty = detect_tty();
    let pid = Some(std::process::id());
    let ppid = get_ppid();

    // TMUX detection via env vars — minimal baseline only. Rich mux
    // targeting (session_name precision, window_id, client_pid via
    // `tmux display-message`) is consumer-enriched, not shim-resolved.
    let tmux_pane_id = std::env::var("TMUX_PANE").ok();
    let tmux_session_name = std::env::var("TMUX_SESSION").ok();
    let tmux_window_index = std::env::var("TMUX_WINDOW")
        .ok()
        .and_then(|v| v.parse().ok());
    let tmux_pane_index = std::env::var("TMUX_PANE_INDEX")
        .ok()
        .and_then(|v| v.parse().ok());
    let tmux_client_pid = std::env::var("TMUX_CLIENT_PID")
        .ok()
        .and_then(|v| v.parse().ok());

    let is_ssh = std::env::var("SSH_CONNECTION").is_ok()
        || std::env::var("SSH_CLIENT").is_ok()
        || std::env::var("SSH_TTY").is_ok();
    let ssh_connection = std::env::var("SSH_CONNECTION").ok();
    let ssh_tty = std::env::var("SSH_TTY").ok();

    EventContext {
        cwd,
        git_worktree,
        tty,
        pid,
        ppid,
        tmux_pane_id,
        tmux_session_name,
        tmux_window_index,
        tmux_pane_index,
        tmux_client_pid,
        is_ssh,
        ssh_connection,
        ssh_tty,
    }
}

/// Opaque env passthrough for consumer-side enrichment. The shim never
/// interprets these — it just forwards whatever is present so Back2Vibing
/// (or any consumer) can resolve `bundle_id`, `terminal_tab_id`, precise
/// tmux chain, etc. without paying for a spawn in the hot path.
fn capture_passthrough_env() -> std::collections::HashMap<String, String> {
    const KEYS: &[&str] = &[
        // terminal identity
        "TERM",
        "TERM_PROGRAM",
        "TERM_PROGRAM_VERSION",
        "TERM_SESSION_ID",
        "LC_TERMINAL",
        "ITERM_SESSION_ID",
        "ITERM_PROFILE",
        "KITTY_WINDOW_ID",
        "KITTY_LISTEN_ON",
        "WEZTERM_PANE",
        "WEZTERM_UNIX_SOCKET",
        "GHOSTTY_RESOURCES_DIR",
        "ZELLIJ_PANE_ID",
        "ZELLIJ_SESSION_NAME",
        "CMUX_WORKSPACE_ID",
        "CMUX_SURFACE_ID",
        "CMUX_SOCKET_PATH",
        "HERDR_PANE_ID",
        "HERDR_TAB_ID",
        "HERDR_WORKSPACE_ID",
        "HERDR_SOCKET_PATH",
        "VSCODE_GIT_ASKPASS_NODE",
        "CURSOR_TRACE_ID",
        "__CFBundleIdentifier",
        "BUNDLE_ID",
        // passthrough for debugging / SSH
        "SSH_CONNECTION",
        "SSH_TTY",
        "SSH_CLIENT",
        "TMUX",
        "TMUX_PANE",
    ];
    let mut out = std::collections::HashMap::new();
    for &k in KEYS {
        if let Ok(v) = std::env::var(k) {
            if !v.is_empty() {
                out.insert(k.to_string(), v);
            }
        }
    }
    // Also forward any env prefixed with these namespaces (e.g. CMUX_*, HERDR_*)
    for (key, value) in std::env::vars_os() {
        let (Ok(k), Ok(v)) = (key.into_string(), value.into_string()) else {
            continue;
        };
        if v.is_empty() {
            continue;
        }
        if k.starts_with("CMUX_") || k.starts_with("HERDR_") || k.starts_with("GHOSTTY_") {
            out.entry(k).or_insert(v);
        }
    }
    out
}

/// Bound the entire lookup, including connect and any client transport retries.
/// Subscription is consent: whatever the daemon reports as requested is
/// captured (plus always-capture names). No allow-list gate.
fn requested_env_names(socket: Option<&Path>) -> Vec<String> {
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return Vec::new();
    };
    let path = socket
        .map(Path::to_path_buf)
        .unwrap_or_else(default_socket_path);
    let client = agent_hooks_client::AgentHooksClient::new(path.to_string_lossy().into_owned());
    runtime.block_on(async {
        tokio::time::timeout(Duration::from_millis(100), client.requested_env_names())
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default()
    })
}

fn detect_tty() -> Option<String> {
    // Prefer explicit env.
    if let Ok(v) = std::env::var("TTY") {
        if !v.is_empty() {
            return Some(v);
        }
    }
    // Try to query via `tty` on Unix (best effort, non-fatal).
    // Avoid spawning for speed — only if needed and cheap.
    // Fallback: check if stdout/stderr is a tty and report /dev/tty.
    if std::io::stdout().is_terminal() || std::io::stderr().is_terminal() {
        // We don't know the exact pts path without libc; return generic.
        // Callers can still correlate via pid.
        // Attempt to read /proc/self/fd/0 symlink on Linux.
        #[cfg(unix)]
        {
            if let Ok(p) = std::fs::read_link("/proc/self/fd/0") {
                return Some(p.to_string_lossy().to_string());
            }
            if let Ok(p) = std::fs::read_link("/proc/self/fd/1") {
                return Some(p.to_string_lossy().to_string());
            }
        }
        return Some("/dev/tty".to_string());
    }
    None
}

#[cfg(unix)]
fn get_ppid() -> Option<u32> {
    // Use libc via std? Avoid extra dep — read /proc/self/stat on Linux
    // and fallback to None on macOS.
    #[cfg(target_os = "linux")]
    {
        if let Ok(stat) = std::fs::read_to_string("/proc/self/stat") {
            // ppid is 4th field
            if let Some(ppid) = stat.split_whitespace().nth(3) {
                if let Ok(v) = ppid.parse() {
                    return Some(v);
                }
            }
        }
        None
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

#[cfg(windows)]
fn get_ppid() -> Option<u32> {
    None
}

fn find_git_root(start: &Path) -> Option<PathBuf> {
    let mut cur = Some(start);
    while let Some(dir) = cur {
        if dir.join(".git").exists() {
            return Some(dir.to_path_buf());
        }
        cur = dir.parent();
    }
    None
}
/// Attempt to connect to the daemon and exchange a single request/response.
/// Returns Ok(Some(response)) on success, Ok(None) if daemon returned empty,
/// Err on connection failure (caller falls back to allow).
/// `--context-cmd` default timeout; the child is killed past it.
const CONTEXT_CMD_TIMEOUT: Duration = Duration::from_millis(1500);
const CONTEXT_CMD_TIMEOUT_ENV: &str = "AGENT_HOOKS_CONTEXT_TIMEOUT_MS";
/// Stdout cap for the context command: runtime context is a few hundred
/// bytes; anything past this is a misbehaving command, not context.
const CONTEXT_CMD_STDOUT_CAP: u64 = 256 * 1024;

fn context_cmd_timeout() -> Duration {
    parse_context_timeout(std::env::var(CONTEXT_CMD_TIMEOUT_ENV).ok().as_deref())
}

/// Unset or unparseable override falls back to the default, never to "no timeout".
fn parse_context_timeout(raw: Option<&str>) -> Duration {
    raw.and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(CONTEXT_CMD_TIMEOUT)
}

/// Opt-in shim diagnostics (`AGENT_HOOKS_DEBUG=1`). The shim has no tracing
/// subscriber — its stderr belongs to the harness — so debug lines are gated.
fn debug_enabled() -> bool {
    std::env::var("AGENT_HOOKS_DEBUG").is_ok_and(|v| !v.is_empty() && v != "0")
}

/// Whether a daemon could be listening. Unix sockets are files, so a missing
/// path means the forward would fail anyway; named pipes can't be probed
/// cheaply, so non-unix always says yes.
fn daemon_socket_present(socket_path: &Path) -> bool {
    #[cfg(unix)]
    {
        socket_path.exists()
    }
    #[cfg(not(unix))]
    {
        let _ = socket_path;
        true
    }
}

/// Run `<cmd> hook-context --harness <id> --event <name>` and return its
/// JSON-object stdout as extensions. Every failure (spawn, timeout, non-zero
/// exit, non-JSON or non-object stdout) is one stderr warning and an empty
/// map — the event is still delivered, just without consumer context.
fn run_context_cmd(
    cmd: &Path,
    harness: &str,
    event: &str,
    session: Option<&str>,
    timeout: Duration,
) -> serde_json::Map<String, serde_json::Value> {
    let start = Instant::now();
    let result = context_cmd_output(cmd, harness, event, session, timeout);
    if debug_enabled() {
        eprintln!(
            "agent-hooks: context-cmd {} took {:?} ({})",
            cmd.display(),
            start.elapsed(),
            if result.is_ok() { "ok" } else { "failed" }
        );
    }
    match result {
        Ok(map) => map,
        Err(reason) => {
            eprintln!(
                "agent-hooks: context-cmd {}: {reason}; continuing without context",
                cmd.display()
            );
            serde_json::Map::new()
        }
    }
}

fn context_cmd_output(
    cmd: &Path,
    harness: &str,
    event: &str,
    session: Option<&str>,
    timeout: Duration,
) -> Result<serde_json::Map<String, serde_json::Value>, String> {
    use std::process::{Command, Stdio};
    let mut command = Command::new(cmd);
    command.args(["hook-context", "--harness", harness, "--event", event]);
    if let Some(session) = session {
        command.args(["--session", session]);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Own process group so a timeout also kills grandchildren that would
        // otherwise hold the stdout pipe (and the agent) open.
        command.process_group(0);
    }
    let mut child = command.spawn().map_err(|e| format!("spawn failed: {e}"))?;
    let kill = |child: &mut std::process::Child| {
        #[cfg(unix)]
        super::user_hooks::kill_process_group(child.id());
        let _ = child.kill();
        let _ = child.wait();
    };

    // Read stdout off-thread so a full pipe can't deadlock the wait and a
    // stuck command can't outlive the timeout.
    let mut stdout = child.stdout.take().ok_or("no stdout pipe")?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let res = (&mut stdout)
            .take(CONTEXT_CMD_STDOUT_CAP + 1)
            .read_to_end(&mut buf)
            .map(|_| buf);
        let _ = tx.send(res);
    });

    let deadline = Instant::now() + timeout;
    let out = match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(Ok(buf)) => buf,
        Ok(Err(e)) => {
            kill(&mut child);
            return Err(format!("reading stdout failed: {e}"));
        }
        Err(_) => {
            kill(&mut child);
            return Err(format!("timed out after {timeout:?}, killed"));
        }
    };
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(2)),
            Ok(None) => {
                kill(&mut child);
                return Err(format!("timed out after {timeout:?}, killed"));
            }
            Err(e) => {
                kill(&mut child);
                return Err(format!("wait failed: {e}"));
            }
        }
    };
    if !status.success() {
        return Err(format!("exited with {status}"));
    }
    if out.len() as u64 > CONTEXT_CMD_STDOUT_CAP {
        return Err(format!("stdout exceeds {CONTEXT_CMD_STDOUT_CAP} bytes"));
    }
    parse_context_output(&out)
}

/// Context stdout must be exactly one JSON object; its top-level keys become
/// extensions. Empty stdout is "no context", not an error.
fn parse_context_output(out: &[u8]) -> Result<serde_json::Map<String, serde_json::Value>, String> {
    let text = String::from_utf8_lossy(out);
    let text = text.trim();
    if text.is_empty() {
        return Ok(serde_json::Map::new());
    }
    match serde_json::from_str::<serde_json::Value>(text) {
        Ok(serde_json::Value::Object(map)) => Ok(map),
        Ok(_) => Err("stdout is JSON but not an object".to_string()),
        Err(e) => Err(format!("stdout is not JSON: {e}")),
    }
}

/// Gate-capability predicate shared by the wait budget and the retry budget:
/// only gate events (`PreToolUse`, `PermissionRequest`, …) may wait long and
/// retry; observe/continue (and unparseable inputs) fail fast with 1 attempt.
fn is_gate_event(
    adapter: Option<&Box<dyn agent_hooks_adapters::HarnessAdapter>>,
    event: Option<&AgentEvent>,
) -> bool {
    match (adapter, event) {
        (Some(a), Some(e)) => {
            matches!(
                a.capability_for(e),
                Some(agent_hooks_adapters::manifest::Capability::Gate)
            )
        }
        _ => false,
    }
}

/// How long the shim waits for the daemon's reply: gate events may be held
/// open for a human verdict, so they out-wait the daemon's own prompt
/// timeout; observe/continue (and unparseable inputs) fail fast.
fn forward_timeout(
    adapter: Option<&Box<dyn agent_hooks_adapters::HarnessAdapter>>,
    event: Option<&AgentEvent>,
    budget: Budget,
) -> Duration {
    let want = if is_gate_event(adapter, event) {
        GATE_WAIT
    } else {
        FAST_WAIT
    };
    // Never read past the harness's patience: a verdict that arrives after it
    // kills us is not a late verdict, it is a discarded one.
    budget.clamp(want)
}

/// True only for transport failures worth retrying (connect refused/reset,
/// aborted, broken pipe, not connected, unexpected EOF). Anything else —
/// serialization errors, unsupported platform — is *not* retried: another
/// attempt would fail identically.
fn forward_error_retryable(err: &anyhow::Error) -> bool {
    if let Some(ioe) = err.downcast_ref::<std::io::Error>() {
        use std::io::ErrorKind as K;
        return matches!(
            ioe.kind(),
            K::ConnectionRefused
                | K::ConnectionReset
                | K::ConnectionAborted
                | K::BrokenPipe
                | K::NotConnected
                | K::UnexpectedEof
        );
    }
    let msg = err.to_string().to_lowercase();
    msg.contains("refused")
        || msg.contains("reset")
        || msg.contains("broken pipe")
        || msg.contains("not connected")
}

/// Bounded forward with retries around `try_forward`.
///
/// - `attempts`: gate events pass `GATE_MAX_ATTEMPTS` (3), observe/continue
///   pass exactly 1 (fail fast).
/// - Retries fire ONLY on transport failure (retryable connect error or
///   empty reply); a received daemon verdict (non-empty) is final and
///   returned immediately, never re-requested.
/// - Every attempt (plus the ~50ms backoff between attempts) is carved out
///   of the same `wait` budget, so the total never exceeds the existing
///   hold/timeout budget; once the budget is spent, remaining attempts are
///   skipped.
/// - Exhaustion fails open (`None` → local decision, allow when nothing
///   matches) with exactly one stderr warning.
fn forward_with_retries(
    socket_path: &Path,
    line: &str,
    wait: Duration,
    attempts: u32,
) -> Option<String> {
    let start = Instant::now();
    let attempts = attempts.max(1);
    for attempt in 1..=attempts {
        if attempt > 1 {
            // Backoff inside the same budget; budget spent → stop retrying.
            let left = wait.saturating_sub(start.elapsed());
            if left.is_zero() {
                break;
            }
            std::thread::sleep(FORWARD_RETRY_BACKOFF.min(left));
            if wait.saturating_sub(start.elapsed()).is_zero() {
                break;
            }
        }
        let per = wait.saturating_sub(start.elapsed());
        match try_forward_inner(socket_path, line, per) {
            Ok(Some(response)) => {
                let trimmed = response.trim();
                if trimmed.is_empty() {
                    // Empty reply = transport failure: retry while attempts remain.
                    if attempt == attempts {
                        break;
                    }
                } else {
                    return Some(trimmed.to_string());
                }
            }
            Ok(None) => {
                // Empty reply (daemon closed/timeout with no verdict): retry
                // while gate attempts remain; budget checks above bound a
                // post-timeout retry storm.
                if attempt == attempts {
                    break;
                }
            }
            Err(e) => {
                if attempt == attempts || !forward_error_retryable(&e) {
                    break;
                }
            }
        }
    }
    eprintln!(
        "agent-hooks: daemon unreachable after {attempts} attempt(s); failing open to local decision"
    );
    None
}

#[cfg(unix)]
fn try_forward_inner(
    socket_path: &Path,
    payload: &str,
    wait: Duration,
) -> anyhow::Result<Option<String>> {
    try_forward_unix(socket_path, payload, wait)
}

#[cfg(windows)]
fn try_forward_inner(
    socket_path: &Path,
    payload: &str,
    wait: Duration,
) -> anyhow::Result<Option<String>> {
    try_forward_windows(socket_path, payload, wait)
}

#[cfg(not(any(unix, windows)))]
fn try_forward_inner(
    _socket_path: &Path,
    _payload: &str,
    _wait: Duration,
) -> anyhow::Result<Option<String>> {
    anyhow::bail!("unsupported platform")
}

#[cfg(unix)]
fn try_forward_unix(
    socket_path: &Path,
    payload: &str,
    wait: Duration,
) -> anyhow::Result<Option<String>> {
    use std::os::unix::net::UnixStream;

    let mut stream = UnixStream::connect(socket_path).map_err(|e| anyhow::anyhow!(e))?;
    // Write budget stays short; the read budget is capability-aware
    // (FAST_WAIT for observe, GATE_WAIT for human permission dialogs).
    stream.set_read_timeout(Some(wait)).ok();
    stream.set_write_timeout(Some(FAST_WAIT)).ok();

    // NDJSON framing: one JSON line.
    let mut to_send = payload.to_string();
    if !to_send.ends_with('\n') {
        to_send.push('\n');
    }
    stream.write_all(to_send.as_bytes())?;
    stream.flush()?;

    // Single read with timeout; first line is the decision. On timeout the
    // daemon is still deciding (or gone) — caller falls back, harness waits
    // on us per its own entry timeout.
    let mut buf = vec![0u8; 64 * 1024];
    let n = match stream.read(&mut buf) {
        Ok(0) => return Ok(None),
        Ok(n) => n,
        Err(e)
            if e.kind() == std::io::ErrorKind::WouldBlock
                || e.kind() == std::io::ErrorKind::TimedOut =>
        {
            return Ok(None);
        }
        Err(e) => return Err(anyhow::anyhow!(e)),
    };
    Ok(first_response_line(&buf[..n]))
}
#[cfg(not(any(unix, windows)))]
fn try_forward_unix(
    _socket_path: &Path,
    _payload: &str,
    _wait: Duration,
) -> anyhow::Result<Option<String>> {
    anyhow::bail!("unix socket not supported on this platform")
}

/// First non-empty line of a daemon reply. Non-JSON passes through as-is
/// (daemon may send plain text); empty means no decision.
fn first_response_line(bytes: &[u8]) -> Option<String> {
    let resp = String::from_utf8_lossy(bytes).trim().to_string();
    if resp.is_empty() {
        return None;
    }
    let first_line = resp.lines().next().unwrap_or("").trim().to_string();
    if first_line.is_empty() {
        return None;
    }
    Some(first_line)
}
#[cfg(windows)]
fn try_forward_windows(
    socket_path: &Path,
    payload: &str,
    wait: Duration,
) -> anyhow::Result<Option<String>> {
    use interprocess::local_socket::prelude::*;
    use interprocess::local_socket::{GenericFilePath, ToFsName};
    use std::io::{Read, Write};

    let raw = socket_path.to_string_lossy();
    let name = raw
        .to_fs_name::<GenericFilePath>()
        .map_err(|e| anyhow::anyhow!("invalid pipe name: {}", e))?;

    let mut stream = interprocess::local_socket::Stream::connect(name)
        .map_err(|e| anyhow::anyhow!("connect error: {}", e))?;

    let mut to_send = payload.to_string();
    if !to_send.ends_with('\n') {
        to_send.push('\n');
    }
    stream.write_all(to_send.as_bytes())?;
    stream.flush()?;

    // Named pipes here have no read timeout; bound the wait on a reader
    // thread instead. A timed-out thread lingers only until the daemon's
    // (always eventual) reply arrives and the pipe closes.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = vec![0u8; 64 * 1024];
        let out = match stream.read(&mut buf) {
            Ok(n) => Ok(buf[..n].to_vec()),
            Err(e) => Err(e.to_string()),
        };
        let _ = tx.send(out);
    });
    match rx.recv_timeout(wait) {
        Ok(Ok(bytes)) => Ok(first_response_line(&bytes)),
        Ok(Err(e)) => Err(anyhow::anyhow!(e)),
        Err(_) => Ok(None),
    }
}

#[cfg(unix)]
#[allow(dead_code)]
fn _connect_with_timeout(
    path: &Path,
    timeout: Duration,
) -> std::io::Result<std::os::unix::net::UnixStream> {
    use std::os::unix::net::UnixStream;
    // UnixStream::connect_timeout not stable; emulate with non-blocking connect.
    // For now use blocking connect with set timeout via alarm-like approach:
    // just use connect (fast failure if missing) — timeout matters only when
    // daemon is overloaded and backlog full.
    let _ = timeout;
    UnixStream::connect(path)
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::HookArgs;
    use std::path::PathBuf;

    fn hook_args(
        harness: &str,
        harness_override: Option<&str>,
        event: Option<&str>,
        payload: Vec<&str>,
    ) -> HookArgs {
        HookArgs {
            harness: harness.to_string(),
            harness_override: harness_override.map(|s| s.to_string()),
            event: event.map(|s| s.to_string()),
            request_id: None,
            socket: None::<PathBuf>,
            adapter_version: None,
            capture_fixtures: None,
            context_cmd: None,
            origin: None,
            payload: payload.into_iter().map(String::from).collect(),
        }
    }
    /// Stdin stub: no piped content (terminal case). Keeps tests hermetic —
    /// they must never block on real stdin.
    fn no_stdin() -> Option<String> {
        None
    }

    /// Stdin stub that must never be consulted: trailing args short-circuit
    /// before stdin.
    fn panic_stdin() -> Option<String> {
        panic!("trailing args present: must not read stdin")
    }

    #[test]
    fn json_first_arg_routes_to_codex_with_payload_intact() {
        let json = r#"{"thread_id":"th_123","hook_event_name":"Stop"}"#;
        let args = hook_args(json, None, Some("Stop"), vec![]);
        let (hint, payload) = resolve_harness_and_payload_with(&args, &mut no_stdin);
        assert_eq!(
            hint, "",
            "JSON-first-arg should yield empty harness_hint for sniffing"
        );
        assert_eq!(payload, json, "payload intact");

        // Registry should sniff to codex
        let input = agent_hooks_protocol::RawHookInput {
            harness_hint: hint,
            event_hint: args.event.clone(),
            raw_payload: payload.clone(),
            context: agent_hooks_protocol::EventContext::default(),
            request_id: None,
            passthrough_env: Default::default(),
        };
        let detected = agent_hooks_adapters::registry::AdapterRegistry::detect_from_input(&input)
            .expect("should detect adapter");
        assert_eq!(
            detected.id(),
            "codex",
            "JSON-first-arg must resolve to codex"
        );

        // Codex parse should succeed
        let event = detected
            .parse_event(&input)
            .expect("codex parse should succeed");
        assert_eq!(event.session_id, "th_123");
    }

    #[test]
    fn json_first_arg_with_whitespace_prefix_routes_to_codex() {
        let json = r#"  {"thread_id":"ws_trim","event":"Stop"}"#;
        let args = hook_args(json, None, Some("Stop"), vec![]);
        let (hint, payload) = resolve_harness_and_payload_with(&args, &mut no_stdin);
        assert_eq!(hint, "");
        assert_eq!(payload, json.trim(), "trimmed JSON payload preserved");
        let input = agent_hooks_protocol::RawHookInput {
            harness_hint: hint,
            event_hint: args.event.clone(),
            raw_payload: payload,
            context: agent_hooks_protocol::EventContext::default(),
            request_id: None,
            passthrough_env: Default::default(),
        };
        assert_eq!(
            agent_hooks_adapters::registry::AdapterRegistry::detect_from_input(&input)
                .unwrap()
                .id(),
            "codex"
        );
    }

    #[test]
    fn explicit_harness_still_wins_over_sniffing() {
        let json = r#"{"thread_id":"th_explicit","hook_event_name":"Stop"}"#;
        // JSON first arg would normally sniff to codex, but explicit says claude
        let args = hook_args(json, Some("claude"), Some("Stop"), vec![]);
        let (hint, payload) = resolve_harness_and_payload_with(&args, &mut no_stdin);
        assert_eq!(hint, "claude");
        assert_eq!(payload, json);

        let input = agent_hooks_protocol::RawHookInput {
            harness_hint: hint.clone(),
            event_hint: args.event.clone(),
            raw_payload: payload,
            context: agent_hooks_protocol::EventContext::default(),
            request_id: None,
            passthrough_env: Default::default(),
        };
        let detected =
            agent_hooks_adapters::registry::AdapterRegistry::detect_from_input(&input).unwrap();
        assert_eq!(detected.id(), "claude-code", "explicit harness must win");

        // Also test explicit codex with JSON payload
        let args2 = hook_args(json, Some("codex"), Some("Stop"), vec![]);
        let (hint2, payload2) = resolve_harness_and_payload_with(&args2, &mut no_stdin);
        assert_eq!(hint2, "codex");
        assert_eq!(payload2, json);
        let input2 = agent_hooks_protocol::RawHookInput {
            harness_hint: hint2,
            event_hint: args2.event.clone(),
            raw_payload: payload2,
            context: agent_hooks_protocol::EventContext::default(),
            request_id: None,
            passthrough_env: Default::default(),
        };
        assert_eq!(
            agent_hooks_adapters::registry::AdapterRegistry::detect_from_input(&input2)
                .unwrap()
                .id(),
            "codex"
        );
    }

    #[test]
    fn plain_harness_unchanged_regression() {
        // Normal path must be byte-identical
        let args = hook_args("codex", None, Some("Stop"), vec![]);
        let (hint, payload) = resolve_harness_and_payload_with(&args, &mut no_stdin);
        assert_eq!(hint, "codex");
        assert_eq!(payload, "", "no trailing -> empty payload (stdin path)");

        let args2 = hook_args("claude", None, Some("PreToolUse"), vec![r#"{"foo":"bar"}"#]);
        let (hint2, payload2) = resolve_harness_and_payload_with(&args2, &mut panic_stdin);
        assert_eq!(hint2, "claude");
        assert_eq!(payload2, r#"{"foo":"bar"}"#);

        // Detection still works for normal hints
        let input = agent_hooks_protocol::RawHookInput {
            harness_hint: hint2,
            event_hint: args2.event.clone(),
            raw_payload: payload2,
            context: agent_hooks_protocol::EventContext::default(),
            request_id: None,
            passthrough_env: Default::default(),
        };
        assert_eq!(
            agent_hooks_adapters::registry::AdapterRegistry::detect_from_input(&input)
                .unwrap()
                .id(),
            "claude-code"
        );
    }

    #[test]
    fn json_plus_trailing_payload_joined() {
        let json = r#"{"thread_id":"t1"}"#;
        let args = hook_args(json, None, Some("Stop"), vec![r#"extra"#]);
        let (hint, payload) = resolve_harness_and_payload_with(&args, &mut panic_stdin);
        assert_eq!(hint, "");
        assert_eq!(payload, format!("{} {}", json, "extra"));
    }

    #[test]
    fn non_json_harness_not_treated_as_payload() {
        let args = hook_args("openai-codex", None, Some("Stop"), vec![]);
        let (hint, payload) = resolve_harness_and_payload_with(&args, &mut no_stdin);
        assert_eq!(hint, "openai-codex");
        assert_eq!(payload, "");
    }
    #[test]
    fn piped_stdin_used_when_no_trailing_payload() {
        let args = hook_args("claude", None, Some("PreToolUse"), vec![]);
        let (hint, payload) =
            resolve_harness_and_payload_with(&args, &mut || Some("piped-body".to_string()));
        assert_eq!(hint, "claude");
        assert_eq!(payload, "piped-body");
    }
    fn wait_test_adapter() -> Box<dyn agent_hooks_adapters::HarnessAdapter> {
        let text = serde_json::json!({
            "id": "testwait",
            "display_name": "Test Wait",
            "aliases": [],
            "default_version": "v1",
            "detect": {"harness_hint": ["testwait"]},
            "versions": {"v1": {
                "install": [],
                "events": {
                    "preToolUse": {"lifecycle": "PreToolUse", "capability": "gate"},
                    "postToolUse": {"lifecycle": "PostToolUse", "capability": "observe"}
                },
                "extraction": {
                    "session_id": ["session_id"],
                    "prompt": ["prompt"],
                    "tool_name": ["tool_name"],
                    "tool_input": ["tool_input"],
                    "tool_output": ["tool_output"],
                    "error": ["error"],
                    "success": ["success"]
                },
                "response": {
                    "mode": "json",
                    "gate": {"allow": {"a": 1}, "deny": {"d": 1}},
                    "observe": {},
                    "continue": {"continue": {"c": 1}, "stop": {}}
                }
            }}
        })
        .to_string();
        let manifest = agent_hooks_adapters::manifest::AdapterManifest::parse(&text).unwrap();
        let adapter =
            agent_hooks_adapters::DeclarativeAdapter::new(manifest, "v1".to_string()).unwrap();
        Box::new(adapter)
    }

    fn wait_test_input(event: &str) -> agent_hooks_protocol::RawHookInput {
        agent_hooks_protocol::RawHookInput {
            harness_hint: "testwait".to_string(),
            event_hint: Some(event.to_string()),
            raw_payload: r#"{"session_id":"s","tool_name":"Bash","tool_input":{}}"#.to_string(),
            context: agent_hooks_protocol::EventContext::default(),
            request_id: None,
            passthrough_env: Default::default(),
        }
    }

    #[test]
    fn gate_events_wait_long_observe_fails_fast() {
        let adapter = wait_test_adapter();
        let gate = adapter.parse_event(&wait_test_input("preToolUse")).unwrap();
        assert_eq!(
            adapter.capability_for(&gate),
            Some(agent_hooks_adapters::manifest::Capability::Gate)
        );
        let unbudgeted = Budget::new(None);
        assert_eq!(
            forward_timeout(Some(&adapter), Some(&gate), unbudgeted),
            GATE_WAIT
        );
        assert!(GATE_WAIT > Duration::from_secs(300));

        let adapter = wait_test_adapter();
        let observe = adapter
            .parse_event(&wait_test_input("postToolUse"))
            .unwrap();
        assert_eq!(
            adapter.capability_for(&observe),
            Some(agent_hooks_adapters::manifest::Capability::Observe)
        );
        assert_eq!(
            forward_timeout(Some(&adapter), Some(&observe), unbudgeted),
            FAST_WAIT
        );

        assert_eq!(forward_timeout(None, None, unbudgeted), FAST_WAIT);

        // A declared harness timeout caps the gate wait: we must answer
        // before the harness stops listening, not after. (Re-parse first —
        // capability_for reports the adapter's most recent parse.)
        let adapter = wait_test_adapter();
        let gate = adapter.parse_event(&wait_test_input("preToolUse")).unwrap();
        let budgeted = Budget::new(Some(Duration::from_secs(30)));
        let capped = forward_timeout(Some(&adapter), Some(&gate), budgeted);
        assert!(
            capped <= Duration::from_secs(28) && capped >= Duration::from_secs(27),
            "30s harness budget should cap the gate wait near 28s, got {capped:?}"
        );
    }
    #[test]
    fn parse_decision_defer_shapes() {
        assert!(parse_decision(r#"{"decision":"defer"}"#).defer);
        assert!(parse_decision(r#"{"result":{"decision":"defer"}}"#).defer);
        assert!(parse_decision(&serde_json::to_string(&PromptDecision::defer()).unwrap()).defer);
        // Garbage is no verdict, not an approval.
        let d = parse_decision("not json");
        assert!(d.defer && !d.allow);
    }
    #[test]
    fn parse_decision_ask_shapes() {
        // The daemon's verdict line: a PromptDecision plus the `decision` verb.
        let d = parse_decision(
            r#"{"decision":"ask","allow":false,"ask":true,"custom_input":"allow deploy?","answers":[]}"#,
        );
        assert!(d.ask && d.custom_input.as_deref() == Some("allow deploy?"));
        // Round-trip through the wire struct.
        let d = parse_decision(
            &serde_json::to_string(&PromptDecision::ask(Some("q".to_string()))).unwrap(),
        );
        assert!(d.ask);
        let d = parse_decision(r#"{"allow":false,"custom_input":"no"}"#);
        assert!(d.is_deny() && d.custom_input.as_deref() == Some("no"));
        // Verb-only / pre-PromptDecision shapes are no verdict: defer, never allow.
        let d = parse_decision(r#"{"decision":"allow","reason":"legacy"}"#);
        assert!(d.defer && !d.allow);
    }

    #[test]
    fn parse_decision_keeps_answers_and_native_shapes() {
        let mut allow = PromptDecision::allow();
        allow.answers = vec![agent_hooks_protocol::PromptAnswer {
            question_id: "q".to_string(),
            selected_options: vec!["A".to_string()],
            custom_input: None,
        }];
        let d = parse_decision(&serde_json::to_string(&allow).unwrap());
        assert_eq!(d, allow);
        let d = parse_decision(
            r#"{"stdout":"{\"hookSpecificOutput\":{\"permissionDecision\":\"deny\",\"permissionDecisionReason\":\"native\"}}"}"#,
        );
        assert!(d.is_deny() && d.custom_input.as_deref() == Some("native"));
    }

    // --- --context-cmd -------------------------------------------------

    #[cfg(unix)]
    fn script(dir: &Path, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("ctx.sh");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn parse_context_output_shapes() {
        let m = parse_context_output(br#" {"b2v.disabled": true} "#).unwrap();
        assert_eq!(m.get("b2v.disabled"), Some(&serde_json::json!(true)));
        assert!(parse_context_output(b"").unwrap().is_empty());
        assert!(parse_context_output(b"\n  \n").unwrap().is_empty());
        assert!(parse_context_output(b"[1,2]").is_err());
        assert!(parse_context_output(b"\"str\"").is_err());
        assert!(parse_context_output(b"tty=/dev/ttys001").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn context_cmd_receives_contract_argv_and_returns_object() {
        let dir = tempfile::tempdir().unwrap();
        // Echo argv back as JSON so the contract's argv shape is asserted.
        let cmd = script(
            dir.path(),
            r#"printf '{"argv":"%s %s %s %s %s","b2v.disabled":false}' "$1" "$2" "$3" "$4" "$5""#,
        );
        let m = context_cmd_output(
            &cmd,
            "claude-code",
            "PreToolUse",
            None,
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(
            m.get("argv"),
            Some(&serde_json::json!(
                "hook-context --harness claude-code --event PreToolUse"
            ))
        );
        assert_eq!(m.get("b2v.disabled"), Some(&serde_json::json!(false)));
    }

    /// The daemon's verdict line must round-trip every field the renderer
    /// reads (live parity e2e: AllowAlways rendered as a plain allow).
    #[test]
    fn daemon_verdict_keeps_allow_always_and_custom_input() {
        let mut always = PromptDecision::allow_always();
        always.custom_input = Some("typed".to_string());
        let v = serde_json::to_value(&always).unwrap();
        let parsed = parse_decision_from_value(&v).expect("parses");
        assert!(parsed.allow);
        assert!(parsed.allow_always);
        assert_eq!(parsed.custom_input.as_deref(), Some("typed"));

        let plain =
            parse_decision_from_value(&serde_json::to_value(PromptDecision::allow()).unwrap())
                .expect("parses");
        assert!(plain.allow && !plain.allow_always);
    }

    /// Consumers key per-session state (b2v's identity markers) on it.
    #[cfg(unix)]
    #[test]
    fn context_cmd_receives_session_when_known() {
        let dir = tempfile::tempdir().unwrap();
        let cmd = script(
            dir.path(),
            r#"printf '{"argv":"%s %s %s %s %s %s %s"}' "$1" "$2" "$3" "$4" "$5" "$6" "$7""#,
        );
        let m = context_cmd_output(
            &cmd,
            "claude-code",
            "Stop",
            Some("abc-123"),
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(
            m.get("argv"),
            Some(&serde_json::json!(
                "hook-context --harness claude-code --event Stop --session abc-123"
            ))
        );
    }

    #[cfg(unix)]
    #[test]
    fn context_cmd_stdin_is_null() {
        let dir = tempfile::tempdir().unwrap();
        // `cat` on a null stdin hits EOF immediately instead of hanging.
        let cmd = script(dir.path(), r#"cat >/dev/null; echo '{"ok":1}'"#);
        let m = context_cmd_output(&cmd, "x", "y", None, Duration::from_secs(5)).unwrap();
        assert_eq!(m.get("ok"), Some(&serde_json::json!(1)));
    }

    #[cfg(unix)]
    #[test]
    fn context_cmd_failures_are_errors_not_panics() {
        let dir = tempfile::tempdir().unwrap();
        let nonzero = script(dir.path(), r#"echo '{"a":1}'; exit 3"#);
        let err = context_cmd_output(&nonzero, "x", "y", None, Duration::from_secs(5)).unwrap_err();
        assert!(err.contains("exited"), "{err}");

        let garbage = script(dir.path(), "echo not-json");
        let err = context_cmd_output(&garbage, "x", "y", None, Duration::from_secs(5)).unwrap_err();
        assert!(err.contains("not JSON"), "{err}");

        let missing = dir.path().join("does-not-exist");
        let err = context_cmd_output(&missing, "x", "y", None, Duration::from_secs(5)).unwrap_err();
        assert!(err.contains("spawn"), "{err}");
        // The wrapper degrades every failure to "no extensions".
        assert!(run_context_cmd(&missing, "x", "y", None, Duration::from_secs(1)).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn context_cmd_timeout_kills_child_and_grandchildren() {
        let dir = tempfile::tempdir().unwrap();
        // Backgrounded grandchild keeps stdout open: only a group kill frees us.
        let cmd = script(dir.path(), "sleep 5 & sleep 5; echo '{\"late\":true}'");
        let start = Instant::now();
        let err = context_cmd_output(&cmd, "x", "y", None, Duration::from_millis(200)).unwrap_err();
        assert!(err.contains("timed out"), "{err}");
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "timeout must not wait for the child: {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn context_cmd_timeout_env_override() {
        assert_eq!(parse_context_timeout(None), Duration::from_millis(1500));
        assert_eq!(
            parse_context_timeout(Some("200")),
            Duration::from_millis(200)
        );
        assert_eq!(
            parse_context_timeout(Some(" 50 ")),
            Duration::from_millis(50)
        );
        assert_eq!(parse_context_timeout(Some("soon")), CONTEXT_CMD_TIMEOUT);
        assert_eq!(parse_context_timeout(Some("-1")), CONTEXT_CMD_TIMEOUT);
    }

    #[cfg(unix)]
    #[test]
    fn missing_socket_means_no_daemon_send() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!daemon_socket_present(&dir.path().join("nope.sock")));
        assert!(daemon_socket_present(dir.path()));
    }

    // --- jcode env payload / ignored events ------------------------------

    // Legacy B2V resources/b2v-agent-event.sh: JCODE_HOOK_PAYLOAD overrides
    // any positional/stdin payload for jcode.
    const JCODE_TURN_END: &str =
        r#"{"hook_event_name":"turn_end","session_id":"jc-1","cwd":"/tmp/repo"}"#;

    #[test]
    fn jcode_env_payload_wins_over_argv() {
        let args = hook_args("jcode", None, Some("turn_end"), vec![r#"{"stale":true}"#]);
        let (h, p) = jcode_env_payload(&args, Some(JCODE_TURN_END)).expect("env payload used");
        assert_eq!(h, "jcode");
        assert_eq!(p, JCODE_TURN_END);
        // Explicit --harness jcode counts too; double-encoded env still unwraps.
        let args = hook_args("x", Some("jcode"), Some("turn_end"), vec![]);
        let encoded = serde_json::to_string(JCODE_TURN_END).unwrap();
        let (_, p) = jcode_env_payload(&args, Some(&encoded)).unwrap();
        assert_eq!(p, JCODE_TURN_END);
    }

    #[test]
    fn jcode_env_payload_ignored_when_empty_or_other_harness() {
        let args = hook_args("jcode", None, Some("turn_end"), vec![]);
        assert!(jcode_env_payload(&args, None).is_none());
        assert!(jcode_env_payload(&args, Some("  ")).is_none());
        // Only jcode reads it: a leaked env var must not hijack other harnesses.
        let args = hook_args("claude", None, Some("Stop"), vec![]);
        assert!(jcode_env_payload(&args, Some(JCODE_TURN_END)).is_none());
    }

    #[test]
    fn ignored_events_do_not_warn_unknown() {
        let ignored: Result<AgentEvent, agent_hooks_adapters::AdapterError> = Err(
            agent_hooks_adapters::AdapterError::Ignored("codex realtime voice".into()),
        );
        // Silent by default (the debug variant only prints with AGENT_HOOKS_DEBUG).
        if !debug_enabled() {
            assert!(parse_failure_warning(Some(&ignored), Some("notify")).is_none());
        } else {
            let msg = parse_failure_warning(Some(&ignored), Some("notify")).unwrap();
            assert!(!msg.contains("unknown event"), "{msg}");
        }
        // Genuinely unmapped events and missing adapters still warn.
        let unknown: Result<AgentEvent, agent_hooks_adapters::AdapterError> = Err(
            agent_hooks_adapters::AdapterError::UnsupportedEvent("Bogus".into()),
        );
        assert!(
            parse_failure_warning(Some(&unknown), Some("Bogus"))
                .unwrap()
                .contains("unknown event 'Bogus'")
        );
        assert!(
            parse_failure_warning(None, None)
                .unwrap()
                .contains("unknown event")
        );
    }
}
