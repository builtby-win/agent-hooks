//! Daemonless local hooks: `~/.agent-hooks/hooks.json` + `<project>/.agent-hooks/hooks.json`.
//!
//! Maps **normalized** lifecycle names (`PreToolUse`, `Stop`, … — see
//! `AgentLifecycle::name()`) to shell commands the hook shim runs
//! deterministically with no daemon present. The normalized [`AgentEvent`]
//! JSON goes to the script's stdin; session id, harness, and lifecycle ride
//! along as `AGENT_HOOKS_*` env vars.
//!
//! Script protocol (boring, mirrors the harness exit-2 convention). Exit
//! codes keep precedence over stdout:
//! - exit `0` → no opinion unless stdout carries JSON (see below); continues
//!   to the next entry / daemon.
//! - exit `2` → deny; trimmed stderr (capped) becomes the deny reason.
//! - exit `3` → ask; trimmed stderr (capped) is the question for the
//!   decider (native harness dialog where supported, else deny fallback).
//! - any other exit, spawn failure, or timeout → fail-open allow + stderr
//!   warning. Local hooks must never block the agent.
//!
//! Exit-0 stdout JSON, in order:
//! - a canonical [`PromptDecision`] (the protocol type, parsed by serde with
//!   unknown fields rejected — identical to the RPC and `respond` schema;
//!   `allow` is required). JSON with an `allow` key that does not parse, or
//!   fails answer validation, fails closed (deny with a clear reason), never
//!   an implicit approval.
//! - a context-only object (`additional_context`, `additionalContext`, or
//!   Claude Code's `hookSpecificOutput.additionalContext`): a non-verdict.
//! - anything else is ignored (JSON objects of neither kind warn).
//! - stdout past 1 MiB is never parsed or truncated: it is a deny with a
//!   size error.
//!
//! A config entry's `respond` is the same canonical [`PromptDecision`]. Every
//! local decision is checked with [`PromptDecision::resolve_questions`]
//! against the event's questions before it is accepted; an invalid answer set
//! fails closed rather than routing wrong answers.

use crate::cli::InitArgs;
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use agent_hooks_protocol::event::cap_chars;
use agent_hooks_protocol::{AgentEvent, AgentLifecycle, PromptDecision, PromptQuestion};
use serde::{Deserialize, Serialize};

/// Config file version we understand.
pub const CONFIG_VERSION: u32 = 1;

/// Default per-command timeout when the entry omits `timeout` and the harness
/// declares no hook window of its own.
pub const DEFAULT_TIMEOUT_SECS: u64 = 10;
/// Grace margin between an inherited harness timeout and our own kill: our
/// kill fires first so the hook exits with a verdict instead of being killed
/// mid-write by the harness (whose kill discards the decision either way).
const INHERITED_TIMEOUT_GRACE_SECS: u64 = 2;

/// Env escape hatch: `AGENT_HOOKS_NO_LOCAL=1` skips local dispatch entirely.
pub const NO_LOCAL_ENV: &str = "AGENT_HOOKS_NO_LOCAL";

/// Cap on deny/ask reason text, in chars.
const MAX_REASON_LEN: usize = 2000;
/// Cap on injected context per event, summed across hooks.
const MAX_CONTEXT_LEN: usize = 4000;
/// Cap on bytes read from a script's stdout. Stdout past this is never
/// parsed or truncated into a decision: it is denied with a size error.
const MAX_STDOUT_BYTES: usize = 1024 * 1024;

/// One local hook entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserHookEntry {
    /// Shell command, run via `sh -c` (unix) / `cmd /C` (windows).
    /// Optional: entries with `respond` answer directly without spawning.
    #[serde(default)]
    pub command: String,
    /// Seconds before the command is killed (fail-open). Omitted inherits the
    /// harness hook window (minus a grace margin so our kill fires first);
    /// when the harness declares none, the 10s default applies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u64>,
    /// Optional match filter: the entry only applies when the event matches.
    /// Absent means "match everything".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<HookMatch>,
    /// Optional canned response, in canonical [`PromptDecision`] terms: when
    /// the entry matches, answer directly without spawning `command`. Unknown
    /// keys are a config error (the pre-`PromptDecision` `{decision, reason}`
    /// shape is rejected, not silently ignored).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_respond"
    )]
    pub respond: Option<PromptDecision>,
}

/// Match filter for a hook entry. Every present field must match
/// (case-insensitive); absent/empty fields impose no constraint.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookMatch {
    /// Exact tool-name match against `PreToolUse.tool_name` or
    /// `PermissionRequest.prompt.tool_name`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// Case-insensitive substring over the prompt description + all question
    /// texts (`PreToolUse` tool-input JSON as fallback).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_matches: Option<String>,
}

fn deserialize_respond<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<PromptDecision>, D::Error> {
    use serde::de::Error;
    Option::<serde_json::Value>::deserialize(deserializer)?
        .map(|v| {
            serde_json::from_value::<PromptDecision>(v)
                .map_err(|e| D::Error::custom(format!("respond: {e}")))
        })
        .transpose()
}

/// Questions the event asks, for `resolve_questions` (empty for events with
/// no prompt).
fn event_questions(event: &AgentEvent) -> &[PromptQuestion] {
    match &event.lifecycle {
        AgentLifecycle::PermissionRequest { prompt } => &prompt.questions,
        AgentLifecycle::PreToolUse {
            prompt: Some(prompt),
            ..
        } => &prompt.questions,
        _ => &[],
    }
}

/// Accept a local decision only after `resolve_questions` against the event's
/// questions. Invalid → deny with a clear reason (fail closed): wrong answers
/// are never routed to the harness. Deny/ask text is capped; allow-side
/// `custom_input` is an answer and is never truncated.
fn accept_decision(event: &AgentEvent, decision: PromptDecision, source: &str) -> PromptDecision {
    match decision.resolve_questions(event_questions(event)) {
        Ok(mut d) => {
            if !d.allow {
                d.custom_input = d.custom_input.map(|r| cap_chars(&r, MAX_REASON_LEN));
            }
            d
        }
        Err(e) => {
            let reason = format!("invalid local hook response from {source}: {e}");
            eprintln!("agent-hooks: {reason}");
            PromptDecision::deny(Some(cap_chars(&reason, MAX_REASON_LEN)))
        }
    }
}

/// Whole user-hooks file: `{version, hooks: {Lifecycle: [entries]}}`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserHooksConfig {
    #[serde(default = "config_version")]
    pub version: u32,
    #[serde(default)]
    pub hooks: BTreeMap<String, Vec<UserHookEntry>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disabled_plugins: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub passthrough_env: Vec<String>,
    /// Deprecated: subscription is consent, so the shim captures whatever a
    /// connected subscriber requests with no allow-list gate. Parsed for
    /// back-compat but ignored (warns when non-empty).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_passthrough_env: Vec<String>,
}

#[derive(Default)]
pub struct EnvCapturePolicy {
    pub always: Vec<String>,
}

fn config_version() -> u32 {
    CONFIG_VERSION
}

/// Canonical lifecycle keys, in `AgentLifecycle::name()` spelling.
pub const KNOWN_LIFECYCLES: [&str; 21] = [
    "SessionStart",
    "SessionEnd",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "PostToolUseFailure",
    "PermissionRequest",
    "Notification",
    "SubagentStart",
    "SubagentStop",
    "TurnStart",
    "TurnEnd",
    "Stop",
    "PreCompact",
    "PostCompact",
    "Interrupt",
    "PostToolBatch",
    "PreLLMCall",
    "PostLLMCall",
    "StopFailure",
    "ToolUseStart",
];

/// Normalize a config key to canonical lifecycle spelling (case-insensitive).
/// Returns `None` for unknown keys (caller warns + ignores).
pub fn normalize_key(key: &str) -> Option<&'static str> {
    let norm = key.to_ascii_lowercase();
    KNOWN_LIFECYCLES
        .iter()
        .find(|k| k.to_ascii_lowercase() == norm)
        .copied()
}

pub fn home_dir() -> Option<PathBuf> {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()
        .map(PathBuf::from)
}

/// `~/.agent-hooks/hooks.json` (user tier).
pub fn user_config_path() -> Option<PathBuf> {
    home_dir().map(|h| h.join(".agent-hooks").join("hooks.json"))
}

/// Every `.agent-hooks/` directory from `start` up to the git root,
/// outermost first (monorepo root, then nested packages). The search never
/// reaches the home directory, whose `.agent-hooks/` is the user tier, and an
/// inner repository's git root bounds it.
fn find_project_dirs(start: &Path, home: Option<&Path>) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for dir in start.ancestors() {
        if home == Some(dir) {
            break;
        }
        let candidate = dir.join(".agent-hooks");
        if candidate.is_dir() {
            dirs.push(candidate);
        }
        if dir.join(".git").exists() {
            break;
        }
    }
    dirs.reverse();
    dirs
}

/// Nearest project `.agent-hooks/` for `start`, or `<start>/.agent-hooks` when
/// none exists yet (where a new one is created). Writers (`init --project`,
/// plugin install/enable/disable) target this one.
pub fn project_dir_in(start: &Path) -> PathBuf {
    find_project_dirs(start, home_dir().as_deref())
        .pop()
        .unwrap_or_else(|| start.join(".agent-hooks"))
}

fn project_dir() -> Option<PathBuf> {
    std::env::current_dir().ok().map(|c| project_dir_in(&c))
}

/// `<project>/.agent-hooks/hooks.json` (project tier, wins per lifecycle key).
pub fn project_config_path() -> Option<PathBuf> {
    project_dir().map(|d| d.join("hooks.json"))
}

pub fn global_plugins_dir() -> Option<PathBuf> {
    home_dir().map(|h| h.join(".agent-hooks").join("plugins"))
}

pub fn project_plugins_dir() -> Option<PathBuf> {
    project_dir().map(|d| d.join("plugins"))
}

fn load_file(
    path: &Path,
    warnings: &mut Vec<String>,
    env_policy: Option<&mut EnvCapturePolicy>,
) -> (BTreeMap<String, Vec<UserHookEntry>>, Vec<String>) {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (BTreeMap::new(), Vec::new()),
        Err(e) => {
            warnings.push(format!("{}: cannot read ({}), ignoring", path.display(), e));
            return (BTreeMap::new(), Vec::new());
        }
    };
    let cfg: UserHooksConfig = match serde_json::from_str(&text) {
        Ok(c) => c,
        Err(e) => {
            warnings.push(format!(
                "{}: invalid JSON ({}), ignoring",
                path.display(),
                e
            ));
            return (BTreeMap::new(), Vec::new());
        }
    };
    if cfg.version != CONFIG_VERSION {
        warnings.push(format!(
            "{}: version {} unsupported (want {}), ignoring",
            path.display(),
            cfg.version,
            CONFIG_VERSION
        ));
        return (BTreeMap::new(), Vec::new());
    }
    if let Some(policy) = env_policy {
        policy.always = agent_hooks_protocol::normalize_env_names(&cfg.passthrough_env);
        // `allowed_passthrough_env` is deprecated (subscription is consent).
        // Still parsed so old configs load; non-empty values warn and are ignored.
        if !cfg.allowed_passthrough_env.is_empty() {
            warnings.push(format!(
                "{}: `allowed_passthrough_env` is deprecated and ignored — subscription alone authorizes env capture, remove it",
                path.display()
            ));
        }
    }
    let mut out = BTreeMap::new();
    for (key, entries) in cfg.hooks {
        match normalize_key(&key) {
            Some(canon) => {
                out.insert(canon.to_string(), entries);
            }
            None => warnings.push(format!(
                "{}: unknown lifecycle '{}', ignoring (want one of {})",
                path.display(),
                key,
                KNOWN_LIFECYCLES.join(", ")
            )),
        }
    }
    (out, cfg.disabled_plugins)
}

/// A later layer's entries run after earlier ones for the same event. The same
/// handler defined at two levels runs once, so a hook copied into a project
/// is not doubled.
fn append_layer(
    merged: &mut BTreeMap<String, Vec<UserHookEntry>>,
    layer: BTreeMap<String, Vec<UserHookEntry>>,
) {
    for (key, entries) in layer {
        let slot = merged.entry(key).or_default();
        for entry in entries {
            let value = serde_json::to_value(&entry).ok();
            if !slot.iter().any(|e| serde_json::to_value(e).ok() == value) {
                slot.push(entry);
            }
        }
    }
}

/// Load the user tier plus every project layer, and discover plugins. Layers
/// accumulate — like Claude Code's user/project/local settings — in the order
/// user, then each `.agent-hooks/` from the git root down to `project_root`
/// (default: the process cwd; a harness that runs the shim elsewhere passes
/// the event's cwd). All of an event's entries run; deny beats ask beats
/// allow. Never fails — unreadable tiers become warnings, never errors.
pub fn load_merged(
    env_policy: &mut EnvCapturePolicy,
    project_root: Option<&Path>,
) -> (BTreeMap<String, Vec<UserHookEntry>>, Vec<String>) {
    let root = match project_root {
        Some(root) => Some(root.to_path_buf()),
        None => std::env::current_dir().ok(),
    };
    let layers: Vec<PathBuf> = root
        .map(|r| find_project_dirs(&r, home_dir().as_deref()))
        .unwrap_or_default()
        .into_iter()
        .map(|d| d.join("hooks.json"))
        .collect();
    load_merged_from(user_config_path().as_deref(), &layers, env_policy)
}

/// `project`: project hooks files, outermost first.
fn load_merged_from(
    user: Option<&Path>,
    project: &[PathBuf],
    env_policy: &mut EnvCapturePolicy,
) -> (BTreeMap<String, Vec<UserHookEntry>>, Vec<String>) {
    let mut warnings = Vec::new();
    let mut merged = BTreeMap::new();
    let mut disabled_plugins = std::collections::HashSet::new();

    if let Some(p) = user {
        let (hooks, dis) = load_file(p, &mut warnings, Some(env_policy));
        append_layer(&mut merged, hooks);
        disabled_plugins.extend(dis);
    }
    for p in project {
        // Cannot authorize env capture.
        let (hooks, dis) = load_file(p, &mut warnings, None);
        append_layer(&mut merged, hooks);
        disabled_plugins.extend(dis);
    }

    // Discover plugins from ~/.agent-hooks/plugins, then each project layer's
    // plugins dir (beside its hooks file); a nearer layer overrides by id.
    let mut plugin_mgr = agent_hooks_core::plugin::PluginManager::new();
    plugin_mgr.set_disabled(disabled_plugins);
    plugin_mgr.discover(global_plugins_dir().as_deref(), None);
    for dir in project.iter().filter_map(|p| p.parent()) {
        plugin_mgr.load_from_plugins_dir(&dir.join("plugins"), true);
    }

    for (plugin, event_name, hook_def) in plugin_mgr.all_hooks() {
        if let Some(canon) = normalize_key(event_name) {
            let entry = UserHookEntry {
                command: plugin.resolve_command(&hook_def.command),
                timeout: hook_def.timeout,
                when: hook_def.when.as_ref().map(|w| HookMatch {
                    tool: w.tool.clone(),
                    prompt_matches: w.prompt_matches.clone(),
                }),
                respond: None,
            };
            merged.entry(canon.to_string()).or_default().push(entry);
        } else {
            warnings.push(format!(
                "plugin '{}': unknown lifecycle '{}', ignoring",
                plugin.id, event_name
            ));
        }
    }

    (merged, warnings)
}

/// Entries matching the event's lifecycle, or empty.
pub fn entries_for<'a>(
    merged: &'a BTreeMap<String, Vec<UserHookEntry>>,
    event: &AgentEvent,
) -> &'a [UserHookEntry] {
    merged
        .get(event.lifecycle.name())
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

/// Deny > ask > allow > defer across both sources: an explicit veto always
/// wins; an escalation beats an allow; an allow beats a defer. Nobody deciding
/// (no sources, or only defers) is defer — the harness proceeds as if no hook
/// were installed, never an allow that would bypass its own permission
/// checks. First denier's / asker's payload is kept (local first — the user's
/// explicit script beats the daemon); for two allows the answers-carrying one
/// wins so checkbox picks survive the merge.
pub fn merge_decisions(
    local: Option<PromptDecision>,
    daemon: Option<PromptDecision>,
) -> PromptDecision {
    let mut ask: Option<PromptDecision> = None;
    let mut allow: Option<PromptDecision> = None;
    let contexts: Vec<String> = [&local, &daemon]
        .into_iter()
        .flatten()
        .filter_map(|d| d.additional_context.clone())
        .collect();
    for d in [local, daemon].into_iter().flatten() {
        if d.is_deny() {
            return PromptDecision {
                additional_context: None,
                ..d
            };
        }
        let is_ask = d.ask;
        let is_allow = d.allow;
        let has_answers = !d.answers.is_empty();
        if is_ask && ask.is_none() {
            ask = Some(d);
        } else if is_allow && (allow.is_none() || has_answers) {
            allow = Some(d);
        }
    }
    let mut merged = ask.or(allow).unwrap_or_else(PromptDecision::defer);
    // Context from every source survives the merge, local first; a question
    // is not the moment to add to the model's context.
    merged.additional_context = if merged.ask {
        None
    } else {
        join_contexts(contexts)
    };
    merged
}

/// Join non-empty contexts in order, capped so a chatty hook cannot flood the
/// model's context.
fn join_contexts(contexts: Vec<String>) -> Option<String> {
    let joined = contexts
        .iter()
        .map(|c| c.trim())
        .filter(|c| !c.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    (!joined.is_empty()).then(|| cap_chars(&joined, MAX_CONTEXT_LEN))
}

/// Text a hook script asked to inject, from exit-0 stdout. Accepts
/// `{"additional_context": ".."}`, the same key camelCased, or Claude Code's
/// `{"hookSpecificOutput": {"additionalContext": ".."}}`, so scripts written
/// for a harness work unchanged. Anything else on stdout is ignored.
fn parse_context_stdout(v: &serde_json::Value) -> Option<String> {
    let text = v
        .get("additional_context")
        .or_else(|| v.get("additionalContext"))
        .or_else(|| v.pointer("/hookSpecificOutput/additionalContext"))?
        .as_str()?;
    (!text.trim().is_empty()).then(|| text.to_string())
}

/// What exit-0 stdout said.
enum Stdout {
    Silent,
    Context(String),
    /// JSON that is neither a verdict nor a context shape (the parse error
    /// explains why); warned about, never an approval or denial.
    Unrecognized(String),
    /// A structured verdict attempt: canonical [`PromptDecision`] or the
    /// parse error.
    Verdict(Result<PromptDecision, String>),
}

/// Classify exit-0 stdout. The protocol type is the schema: JSON that parses
/// as a [`PromptDecision`] is a verdict, and JSON with an `allow` key that
/// does not (typo, wrong type, stale shape) is a malformed verdict that fails
/// closed. Without `allow` the context-only shapes apply; anything else is
/// unrecognized.
fn parse_stdout(stdout: &str) -> Stdout {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(stdout.trim()) else {
        return Stdout::Silent;
    };
    let has_allow = value.as_object().is_some_and(|o| o.contains_key("allow"));
    match serde_json::from_value::<PromptDecision>(value.clone()) {
        Ok(decision) => Stdout::Verdict(Ok(decision)),
        Err(e) if has_allow => Stdout::Verdict(Err(e.to_string())),
        Err(e) => match parse_context_stdout(&value) {
            Some(text) => Stdout::Context(text),
            None if value
                .as_object()
                .is_some_and(|o| o.contains_key("additional_context")) =>
            {
                Stdout::Silent
            }
            None => Stdout::Unrecognized(e.to_string()),
        },
    }
}

/// Run matching entries in order with the normalized event on stdin.
/// Returns `Some(decision)` on the first veto (or the accumulated ask /
/// canned allow-with-answers), `None` when everything allows — including
/// when there are no entries, nothing matches, everything fails open, or
/// the `AGENT_HOOKS_NO_LOCAL=1` escape hatch is set.
/// Run local hooks inside a shared budget.
///
/// Per-entry timeouts alone do not bound the total: four entries at the 10s
/// default is 40s, past what several harnesses will wait for the whole hook.
/// Overrunning does not make the decision late, it makes it *discarded* — the
/// harness kills us and applies its own semantics. So each entry is clamped
/// to what remains, and once the budget is gone the rest are skipped.
///
/// Skipping is safe under the same rule the harness itself uses: whatever ran
/// still counts, and a hook that returned nothing was never a decision.
/// `budget: None` = unbudgeted, per-entry timeouts only.
pub fn run_local_hooks(
    event: &AgentEvent,
    entries: &[UserHookEntry],
    budget: Option<std::time::Duration>,
) -> Option<PromptDecision> {
    if std::env::var(NO_LOCAL_ENV).ok().as_deref() == Some("1") {
        return None;
    }
    if entries.is_empty() {
        return None;
    }
    let stdin_json = match serde_json::to_string_pretty(event) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("agent-hooks: cannot serialize event for local hooks: {e}");
            return None;
        }
    };

    // Plan in entry order, spawn concurrently, resolve in entry order.
    //
    // Concurrency matches the harness: Claude Code runs every matching hook
    // in parallel with its own timeout, so three 2s hooks cost 2s there and
    // must cost 2s here. Running them in sequence spends the harness budget N
    // times over and turns independent scripts into a queue.
    //
    // Resolution stays ordered regardless of which finishes first, so the
    // decision is a function of the config, not of the race. Precedence is
    // unchanged: deny > ask > canned allow-with-answers, earliest entry
    // winning within a tier.
    enum Planned {
        Canned(PromptDecision),
        Spawn { command: String, timeout: u64 },
    }

    let budget_secs = budget.map(|b| b.as_secs().max(1));
    let mut plan: Vec<Planned> = Vec::new();
    for entry in entries {
        // Match first: non-matching entries never spawn and never answer.
        if !entry_matches(event, entry.when.as_ref()) {
            continue;
        }
        // Matched `respond` answers directly — no process is spawned,
        // even when `command` is also set.
        if let Some(respond) = entry.respond.as_ref() {
            plan.push(Planned::Canned(accept_decision(
                event,
                respond.clone(),
                "`respond` entry",
            )));
            continue;
        }
        if entry.command.trim().is_empty() {
            continue;
        }
        // Each hook keeps its own timeout, capped by the shared budget. The
        // cap is what keeps the parallel total inside the harness window:
        // wall time is the slowest hook, and no hook may outlast the budget.
        // An omitted entry timeout inherits that window (minus a grace margin
        // so our kill — a clean fail-open verdict — fires before the harness
        // kill, which would discard the decision mid-write). With no declared
        // window the 10s default applies. Either way the first timer to fire
        // wins: entry timeout, inherited window, or harness kill.
        let timeout = match (entry.timeout, budget_secs) {
            (Some(t), Some(b)) => t.min(b),
            (Some(t), None) => t,
            (None, Some(b)) => b.saturating_sub(INHERITED_TIMEOUT_GRACE_SECS).max(1),
            (None, None) => DEFAULT_TIMEOUT_SECS,
        };
        plan.push(Planned::Spawn {
            command: entry.command.clone(),
            timeout,
        });
    }
    if plan.is_empty() {
        return None;
    }

    let outcomes: Vec<Option<RunOutcome>> = std::thread::scope(|scope| {
        let handles: Vec<Option<std::thread::ScopedJoinHandle<RunOutcome>>> = plan
            .iter()
            .map(|p| match p {
                Planned::Canned(_) => None,
                Planned::Spawn { command, timeout } => {
                    let (cmd, t, sj) = (command.clone(), *timeout, &stdin_json);
                    Some(scope.spawn(move || run_one(&cmd, t, sj, event)))
                }
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.map(|h| h.join().unwrap_or(RunOutcome::Allow)))
            .collect()
    });

    let mut ask: Option<PromptDecision> = None;
    let mut allow: Option<PromptDecision> = None;
    let mut contexts: Vec<String> = Vec::new();
    for (planned, outcome) in plan.iter().zip(outcomes) {
        let decision = match (planned, outcome) {
            (Planned::Canned(decision), _) => decision.clone(),
            (_, Some(RunOutcome::Decision(decision))) => decision,
            (_, Some(RunOutcome::Deny(reason))) => PromptDecision::deny(reason),
            (_, Some(RunOutcome::Ask(question))) => PromptDecision::ask(question),
            (_, Some(RunOutcome::Ignored(warn))) => {
                eprintln!("agent-hooks: local hook ignored: {warn}");
                continue;
            }
            (_, Some(RunOutcome::Context(text))) => {
                contexts.push(text);
                continue;
            }
            (_, Some(RunOutcome::Allow)) | (_, None) => continue,
        };
        // Deny always wins, whichever entry or channel it came from.
        if decision.is_deny() {
            return Some(PromptDecision {
                additional_context: None,
                ..decision
            });
        }
        contexts.extend(decision.additional_context.clone());
        if decision.ask {
            // Escalation accumulates: a later entry may still veto, and the
            // ask survives the daemon's default-allow via merge precedence.
            if ask.is_none() {
                ask = Some(decision);
            }
        } else if decision.allow {
            // First explicit allow wins; one carrying answers beats one without.
            let better = match &allow {
                None => true,
                Some(a) => a.answers.is_empty() && !decision.answers.is_empty(),
            };
            if better {
                allow = Some(decision);
            }
        }
        // A defer is no opinion: only its context (above) survives.
    }
    // Deny returned early above; ask beats an explicit allow.
    let verdict = ask.or(allow);
    let context = join_contexts(contexts);
    match (verdict, context) {
        (Some(v), _) if v.ask => Some(PromptDecision {
            additional_context: None,
            ..v
        }),
        (Some(v), Some(c)) => Some(v.with_additional_context(c)),
        (Some(v), None) => Some(PromptDecision {
            additional_context: None,
            ..v
        }),
        // Context with no opinion on the verdict: defer, not allow, so a
        // tool gate's own permission checks still run.
        (None, Some(c)) => Some(PromptDecision::defer().with_additional_context(c)),
        (None, None) => None,
    }
}

/// Whether the entry's `when` filter matches the event. No filter matches
/// everything; every present field must match (case-insensitive).
fn entry_matches(event: &AgentEvent, when: Option<&HookMatch>) -> bool {
    let Some(filter) = when else {
        return true;
    };
    if let Some(tool) = filter.tool.as_deref() {
        if !tool.trim().is_empty() {
            let event_tool = match &event.lifecycle {
                AgentLifecycle::PreToolUse { tool_name, .. } => Some(tool_name.as_str()),
                AgentLifecycle::PermissionRequest { prompt } => prompt.tool_name.as_deref(),
                _ => None,
            };
            match event_tool {
                Some(name) if name.eq_ignore_ascii_case(tool) => {}
                _ => return false,
            }
        }
    }
    if let Some(needle) = filter.prompt_matches.as_deref() {
        if !needle.trim().is_empty() {
            let Some(haystack) = prompt_haystack(event) else {
                return false;
            };
            if !haystack.to_lowercase().contains(&needle.to_lowercase()) {
                return false;
            }
        }
    }
    true
}

/// Searchable prompt text for `prompt_matches`: the description plus all
/// question texts, or the `PreToolUse` tool-input JSON as fallback.
/// Lifecycles with neither carry no prompt text (`None` = no match).
fn prompt_haystack(event: &AgentEvent) -> Option<String> {
    match &event.lifecycle {
        AgentLifecycle::PermissionRequest { prompt } => {
            let mut parts = Vec::new();
            if let Some(description) = prompt.description.as_deref() {
                parts.push(description.to_string());
            }
            for question in &prompt.questions {
                parts.push(question.question.clone());
            }
            Some(parts.join("\n"))
        }
        AgentLifecycle::PreToolUse { input, prompt, .. } => {
            let mut parts = Vec::new();
            if let Some(request) = prompt.as_ref() {
                if let Some(description) = request.description.as_deref() {
                    parts.push(description.to_string());
                }
                for question in &request.questions {
                    parts.push(question.question.clone());
                }
            }
            // Tool-input JSON as fallback (e.g. the Bash command string).
            let input_json = input.to_string();
            if !input_json.is_empty() && input_json != "null" {
                parts.push(input_json);
            }
            Some(parts.join("\n"))
        }
        _ => None,
    }
}

enum RunOutcome {
    Allow,
    /// Exit 0 with context to inject (see `parse_context_stdout`).
    Context(String),
    /// Exit 0 with a canonical stdout verdict, already accepted through
    /// `accept_decision` (invalid → a fail-closed deny).
    Decision(PromptDecision),
    Deny(Option<String>),
    Ask(Option<String>),
    Ignored(String),
}

/// Fail-open audit (this file): every infrastructure failure resolves to
/// allow/skip — spawn failure, stdin-write failure, non-2/3 exits, timeouts,
/// wait errors, `AGENT_HOOKS_NO_LOCAL=1`, empty commands, unreadable config
/// tiers, and non-UTF8 stderr (lossy-decoded, never an error). Deny/ask arise
/// ONLY from exit 2/3, a canned `respond`, or an explicit canonical stdout
/// verdict (a malformed one fails closed). Deny > ask > allow merge
/// precedence holds.
fn run_one(command: &str, timeout_secs: u64, stdin_json: &str, event: &AgentEvent) -> RunOutcome {
    let mut cmd = std::process::Command::new(shell_program());
    cmd.arg(shell_flag())
        .arg(command)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .env("AGENT_HOOKS_EVENT", event.lifecycle.name())
        .env("AGENT_HOOKS_SESSION", &event.session_id)
        .env("AGENT_HOOKS_HARNESS", &event.harness);
    // Local hooks never retry: exactly one spawn per entry. A second spawn
    // would double-apply side effects. (Shim->daemon transport retries live
    // in hook.rs, not here.)
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Own process group so a timeout can SIGKILL backgrounded
        // grandchildren too (group id == child pid).
        cmd.process_group(0);
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return RunOutcome::Ignored(format!("cannot spawn '{command}': {e}")),
    };
    // Drain stdout on its own thread: a script printing more than a pipe
    // buffer would otherwise block until the timeout kills it. Keeps the
    // first bytes (enough for a context JSON), discards the rest.
    let (stdout_tx, stdout_rx) = std::sync::mpsc::channel::<(String, bool)>();
    if let Some(mut out) = child.stdout.take() {
        std::thread::spawn(move || {
            use std::io::Read;
            let mut kept = Vec::new();
            let _ = out
                .by_ref()
                .take(MAX_STDOUT_BYTES as u64)
                .read_to_end(&mut kept);
            // Anything past the cap is drained so the script never blocks,
            // but flagged: a truncated verdict is never parsed.
            let overflow = std::io::copy(&mut out, &mut std::io::sink()).unwrap_or(0) > 0;
            let _ = stdout_tx.send((String::from_utf8_lossy(&kept).into_owned(), overflow));
        });
    }
    // Feed stdin, then close it so the script sees EOF.
    if let Some(mut stdin) = child.stdin.take() {
        if stdin.write_all(stdin_json.as_bytes()).is_err() {
            let _ = child.kill();
            return RunOutcome::Ignored(format!("cannot write stdin to '{command}'"));
        }
    }
    let timeout = Duration::from_secs(timeout_secs.max(1));
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                // Reap stderr for deny reasons (bounded read).
                let mut stderr = String::new();
                if let Some(mut err) = child.stderr.take() {
                    use std::io::Read;
                    // 4x headroom covers worst-case UTF-8 expansion; capped
                    // on chars below.
                    let mut buf = vec![0u8; MAX_REASON_LEN * 4 + 1];
                    if let Ok(n) = err.read(&mut buf) {
                        stderr = String::from_utf8_lossy(&buf[..n]).into_owned();
                    }
                }
                if status.code() == Some(2) || status.code() == Some(3) {
                    let text = stderr.trim().to_string();
                    let text = if text.is_empty() {
                        None
                    } else {
                        Some(cap_chars(&text, MAX_REASON_LEN))
                    };
                    if status.code() == Some(3) {
                        return RunOutcome::Ask(text);
                    }
                    return RunOutcome::Deny(text);
                }
                if status.success() {
                    // A backgrounded grandchild can hold the pipe open past
                    // exit; a short wait keeps that from stalling the hook.
                    let (stdout, overflow) = stdout_rx
                        .recv_timeout(Duration::from_millis(200))
                        .unwrap_or_default();
                    if overflow {
                        // Never parsed, never truncated: an oversized stdout
                        // may hold a decision (or answers) we cannot read
                        // exactly, so it is an explicit denial, not a skip.
                        let reason = format!(
                            "local hook '{command}' wrote more than {MAX_STDOUT_BYTES} bytes to stdout; \
                             a decision that large cannot be read exactly"
                        );
                        eprintln!("agent-hooks: {reason}");
                        return RunOutcome::Decision(PromptDecision::deny(Some(cap_chars(
                            &reason,
                            MAX_REASON_LEN,
                        ))));
                    }
                    return match parse_stdout(&stdout) {
                        Stdout::Silent => RunOutcome::Allow,
                        Stdout::Context(text) => RunOutcome::Context(text),
                        Stdout::Unrecognized(e) => RunOutcome::Ignored(format!(
                            "'{command}' stdout JSON is neither a PromptDecision nor context ({e})"
                        )),
                        Stdout::Verdict(Ok(decision)) => RunOutcome::Decision(accept_decision(
                            event,
                            decision,
                            &format!("'{command}' stdout"),
                        )),
                        Stdout::Verdict(Err(e)) => {
                            let reason =
                                format!("invalid local hook verdict on stdout of '{command}': {e}");
                            eprintln!("agent-hooks: {reason}");
                            RunOutcome::Decision(PromptDecision::deny(Some(cap_chars(
                                &reason,
                                MAX_REASON_LEN,
                            ))))
                        }
                    };
                }
                return RunOutcome::Ignored(format!(
                    "'{command}' exited with {status} (only 0/2/3 matter), treating as allow"
                ));
            }
            Ok(None) => {
                if start.elapsed() >= timeout {
                    // Timeout is fail-open allow + stderr warning, never deny.
                    #[cfg(unix)]
                    {
                        // Whole-group SIGKILL: backgrounded grandchildren
                        // (`sleep 300 &`, SIGTERM-ignoring scripts) die with
                        // the hook. Direct-child kill is the fallback.
                        kill_process_group(child.id());
                        let _ = child.kill();
                    }
                    // PLATFORM GAP (Windows): std has no process-group kill,
                    // so this is best-effort on the direct child only;
                    // detached grandchildren may survive. Unix reaps the
                    // whole group (see kill_process_group).
                    #[cfg(not(unix))]
                    let _ = child.kill();
                    let _ = child.wait();
                    return RunOutcome::Ignored(format!(
                        "'{command}' exceeded {timeout_secs}s, killed (fail-open)"
                    ));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => {
                let _ = child.kill();
                return RunOutcome::Ignored(format!("wait on '{command}' failed: {e}"));
            }
        }
    }
}
/// SIGKILL the whole process group `pgid` (the timed-out hook's group: the
/// leader's pid == `pgid` via `process_group(0)`). Raw FFI keeps this
/// dependency-free — libc links by default on unix. Failure is ignored: the
/// caller falls back to killing the direct child.
#[cfg(unix)]
pub(crate) fn kill_process_group(pgid: u32) {
    const SIGKILL: i32 = 9;
    extern "C" {
        fn killpg(pgrp: i32, sig: i32) -> i32;
    }
    // SAFETY: killpg with a positive pgid and a constant signal performs no
    // memory access; at worst it returns an error code, which we ignore.
    unsafe {
        killpg(pgid as i32, SIGKILL);
    }
}

#[cfg(windows)]
fn shell_program() -> &'static str {
    "cmd"
}

#[cfg(not(windows))]
fn shell_program() -> &'static str {
    "sh"
}

#[cfg(windows)]
fn shell_flag() -> &'static str {
    "/C"
}

#[cfg(not(windows))]
fn shell_flag() -> &'static str {
    "-c"
}

/// Scaffold for `agent-hooks init` — must itself parse via `load` semantics.
pub fn scaffold_json() -> String {
    serde_json::to_string_pretty(&serde_json::json!({
        "version": 1,
        "hooks": {
            "PreToolUse": [
                {"command": "~/.agent-hooks/block-dangerous.sh", "timeout": 10}
            ],
            "PostToolUse": [
                {"command": "~/.agent-hooks/log-tool.sh", "timeout": 10}
            ]
        }
    }))
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_hooks_protocol::{
        AgentLifecycle, EventContext, PromptAnswer, PromptKind, PromptOption, PromptQuestion,
        PromptRequest,
    };

    fn test_event(lifecycle: AgentLifecycle) -> AgentEvent {
        AgentEvent::new("test-harness", "sess-1", lifecycle, EventContext::default())
    }

    #[test]
    fn project_dir_search_stops_at_git_root_and_home() {
        let root = tempfile::TempDir::new().unwrap();
        let home = root.path().join("home");
        // home/.agent-hooks is the user tier, never a project.
        std::fs::create_dir_all(home.join(".agent-hooks")).unwrap();
        let repo = home.join("repo");
        let deep = repo.join("src/deep");
        std::fs::create_dir_all(&deep).unwrap();

        // No project dir anywhere below home: the walk must not return home's.
        assert!(find_project_dirs(&deep, Some(&home)).is_empty());

        // A repo-root layer and a nested package layer: outermost first.
        std::fs::create_dir_all(repo.join(".agent-hooks")).unwrap();
        let pkg = repo.join("packages/web");
        std::fs::create_dir_all(pkg.join("src")).unwrap();
        std::fs::create_dir_all(pkg.join(".agent-hooks")).unwrap();
        assert_eq!(
            find_project_dirs(&pkg.join("src"), Some(&home)),
            [repo.join(".agent-hooks"), pkg.join(".agent-hooks")]
        );
        assert_eq!(
            find_project_dirs(&deep, Some(&home)),
            [repo.join(".agent-hooks")]
        );

        // A git root bounds the search: the outer project's dir is not reached
        // through an inner repository.
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let inner = repo.join("vendor/inner");
        std::fs::create_dir_all(inner.join(".git")).unwrap();
        assert!(find_project_dirs(&inner, Some(&home)).is_empty());

        // An `.agent-hooks/` above a repository's git root is outside it.
        let outer = home.join("outer");
        std::fs::create_dir_all(outer.join(".agent-hooks")).unwrap();
        let nested_repo = outer.join("repo2");
        std::fs::create_dir_all(nested_repo.join("src")).unwrap();
        std::fs::create_dir_all(nested_repo.join(".git")).unwrap();
        assert!(find_project_dirs(&nested_repo.join("src"), Some(&home)).is_empty());
    }

    #[test]
    fn keys_normalize_case_insensitively() {
        assert_eq!(normalize_key("pretooluse"), Some("PreToolUse"));
        assert_eq!(normalize_key("Stop"), Some("Stop"));
        assert_eq!(normalize_key("SESSIONEND"), Some("SessionEnd"));
        assert_eq!(normalize_key("bogus"), None);
    }

    #[test]
    fn project_tier_adds_to_user_tier_per_event() {
        let dir = std::env::temp_dir().join(format!(
            "agent-hooks-usertest-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let user = dir.join("user.json");
        let project = dir.join("project.json");
        std::fs::write(
            &user,
            r#"{"version":1,"hooks":{"PreToolUse":[{"command":"user-cmd"}],"Stop":[{"command":"user-stop"}]}}"#,
        )
        .unwrap();
        std::fs::write(
            &project,
            r#"{"version":1,"hooks":{"PreToolUse":[{"command":"project-cmd"}]}}"#,
        )
        .unwrap();
        let (merged, warnings) = load_merged_from(
            Some(&user),
            &[project.clone()],
            &mut EnvCapturePolicy::default(),
        );
        assert!(warnings.is_empty(), "{warnings:?}");
        let pre: Vec<&str> = merged["PreToolUse"].iter().map(|e| e.command.as_str()).collect();
        assert_eq!(pre, ["user-cmd", "project-cmd"]);
        assert_eq!(merged["Stop"][0].command, "user-stop");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Monorepo layering: global, repo root and nested package all contribute.
    #[test]
    fn layers_accumulate_in_order_and_identical_handlers_run_once() {
        let dir = tempfile::tempdir().unwrap();
        let write = |name: &str, body: &str| {
            let p = dir.path().join(name);
            std::fs::write(&p, body).unwrap();
            p
        };
        let user = write(
            "user.json",
            r#"{"version":1,"hooks":{"Stop":[{"command":"user-stop"},{"command":"shared"}],"SessionStart":[{"command":"user-start"}]}}"#,
        );
        let root = write(
            "root.json",
            r#"{"version":1,"hooks":{"Stop":[{"command":"root-stop"},{"command":"shared"}]}}"#,
        );
        let pkg = write(
            "pkg.json",
            r#"{"version":1,"hooks":{"Stop":[{"command":"pkg-stop"}]}}"#,
        );
        let (merged, warnings) = load_merged_from(
            Some(&user),
            &[root, pkg],
            &mut EnvCapturePolicy::default(),
        );
        assert!(warnings.is_empty(), "{warnings:?}");
        let stops: Vec<&str> = merged["Stop"].iter().map(|e| e.command.as_str()).collect();
        assert_eq!(stops, ["user-stop", "shared", "root-stop", "pkg-stop"]);
        assert_eq!(merged["SessionStart"][0].command, "user-start");
    }

    #[test]
    fn project_cannot_authorize_environment_capture() {
        let dir = tempfile::tempdir().unwrap();
        let user = dir.path().join("user.json");
        let project = dir.path().join("project.json");
        std::fs::write(&user, r#"{"version":1,"passthrough_env":["USER_ALWAYS"],"allowed_passthrough_env":["USER_REQUESTED"]}"#).unwrap();
        std::fs::write(&project, r#"{"version":1,"passthrough_env":["PROJECT_SECRET"],"allowed_passthrough_env":["PROJECT_REQUESTED"]}"#).unwrap();
        let mut policy = EnvCapturePolicy::default();
        let (_, warnings) = load_merged_from(Some(&user), &[project.clone()], &mut policy);
        assert_eq!(policy.always, ["USER_ALWAYS"]);
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("`allowed_passthrough_env` is deprecated")),
            "{warnings:?}"
        );
    }

    #[test]
    fn bad_files_fail_open_with_warnings() {
        let dir = std::env::temp_dir().join(format!(
            "agent-hooks-usertest-bad-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let bad_json = dir.join("bad.json");
        let bad_version = dir.join("ver.json");
        let bad_key = dir.join("key.json");
        std::fs::write(&bad_json, "{not json").unwrap();
        std::fs::write(&bad_version, r#"{"version":99,"hooks":{}}"#).unwrap();
        std::fs::write(
            &bad_key,
            r#"{"version":1,"hooks":{"Bogus":[{"command":"x"}]}}"#,
        )
        .unwrap();
        let (m1, w1) = load_merged_from(Some(&bad_json), &[], &mut EnvCapturePolicy::default());
        let (m2, w2) = load_merged_from(Some(&bad_version), &[], &mut EnvCapturePolicy::default());
        let (m3, w3) = load_merged_from(Some(&bad_key), &[], &mut EnvCapturePolicy::default());
        let (m4, w4) = load_merged_from(
            Some(&dir.join("missing.json")),
            &[],
            &mut EnvCapturePolicy::default(),
        );
        assert!(m1.is_empty() && w1.len() == 1);
        assert!(m2.is_empty() && w2.len() == 1);
        assert!(m3.is_empty() && w3.len() == 1);
        assert!(m4.is_empty() && w4.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn merge_is_deny_over_ask_over_allow() {
        let deny_l = || Some(PromptDecision::deny(Some("local".to_string())));
        let deny_d = || Some(PromptDecision::deny(Some("daemon".to_string())));
        let ask_l = || Some(PromptDecision::ask(Some("local-q".to_string())));
        let ask_d = || Some(PromptDecision::ask(Some("daemon-q".to_string())));
        // Local deny wins over daemon allow/absent, keeps local reason.
        let m = merge_decisions(deny_l(), Some(PromptDecision::allow()));
        assert!(!m.allow && !m.ask && m.custom_input.as_deref() == Some("local"));
        // Daemon deny wins over local allow/absent.
        let m = merge_decisions(Some(PromptDecision::allow()), deny_d());
        assert!(!m.allow && !m.ask && m.custom_input.as_deref() == Some("daemon"));
        let m = merge_decisions(None, deny_d());
        assert!(!m.allow);
        // Deny beats ask from either side.
        let m = merge_decisions(ask_l(), deny_d());
        assert!(!m.allow && !m.ask);
        let m = merge_decisions(deny_l(), ask_d());
        assert!(!m.allow && !m.ask && m.custom_input.as_deref() == Some("local"));
        // Ask beats default-allow, keeps the question, local first.
        let m = merge_decisions(ask_l(), Some(PromptDecision::allow()));
        assert!(m.ask && m.custom_input.as_deref() == Some("local-q"));
        let m = merge_decisions(None, ask_d());
        assert!(m.ask && m.custom_input.as_deref() == Some("daemon-q"));
        // Both allow → allow; nobody deciding → defer, never allow.
        assert!(
            merge_decisions(Some(PromptDecision::allow()), Some(PromptDecision::allow())).allow
        );
        let m = merge_decisions(None, None);
        assert!(m.defer && !m.allow);
        let m = merge_decisions(None, Some(PromptDecision::defer()));
        assert!(m.defer && !m.allow);
        assert!(
            merge_decisions(Some(PromptDecision::defer()), Some(PromptDecision::allow())).allow
        );
    }

    #[test]
    fn scaffold_parses() {
        let cfg: UserHooksConfig = serde_json::from_str(&scaffold_json()).unwrap();
        assert_eq!(cfg.version, CONFIG_VERSION);
        assert!(cfg.hooks.contains_key("PreToolUse"));
    }

    #[cfg(unix)]
    #[test]
    fn protocol_exit_codes() {
        let event = test_event(AgentLifecycle::PreToolUse {
            tool_name: "Bash".to_string(),
            input: serde_json::json!({"command": "ls"}),
            prompt: None,
        });
        let allow = [UserHookEntry {
            command: "exit 0".to_string(),
            timeout: Some(5),
            when: None,
            respond: None,
        }];
        let deny = [UserHookEntry {
            command: "echo no-rm >&2; exit 2".to_string(),
            timeout: Some(5),
            when: None,
            respond: None,
        }];
        let other = [UserHookEntry {
            command: "exit 1".to_string(),
            timeout: Some(5),
            when: None,
            respond: None,
        }];
        let missing = [UserHookEntry {
            command: "definitely-not-a-real-binary-xyz".to_string(),
            timeout: Some(5),
            when: None,
            respond: None,
        }];
        assert!(run_local_hooks(&event, &allow, None).is_none());
        let d = run_local_hooks(&event, &deny, None).expect("exit 2 must deny");
        assert!(!d.allow && d.custom_input.as_deref() == Some("no-rm"));
        assert!(run_local_hooks(&event, &other, None).is_none());
        assert!(run_local_hooks(&event, &missing, None).is_none());
        assert!(run_local_hooks(&event, &[], None).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn protocol_exit_3_asks_with_question() {
        let event = test_event(AgentLifecycle::PreToolUse {
            tool_name: "Bash".to_string(),
            input: serde_json::json!({"command": "rm -rf /"}),
            prompt: None,
        });
        let ask = [UserHookEntry {
            command: "echo 'allow rm -rf?' >&2; exit 3".to_string(),
            timeout: Some(5),
            when: None,
            respond: None,
        }];
        let a = run_local_hooks(&event, &ask, None).expect("exit 3 must ask");
        assert!(a.ask && !a.allow && a.custom_input.as_deref() == Some("allow rm -rf?"));
        // Ask accumulates but a later deny still vetoes.
        let chain = [
            UserHookEntry {
                command: "exit 3".to_string(),
                timeout: Some(5),
                when: None,
                respond: None,
            },
            UserHookEntry {
                command: "exit 2".to_string(),
                timeout: Some(5),
                when: None,
                respond: None,
            },
        ];
        let d = run_local_hooks(&event, &chain, None).expect("deny must win over ask");
        assert!(!d.allow && !d.ask);
        // Bare exit 3 with no question still asks.
        let bare = [UserHookEntry {
            command: "exit 3".to_string(),
            timeout: Some(5),
            when: None,
            respond: None,
        }];
        let a = run_local_hooks(&event, &bare, None).expect("bare exit 3 must ask");
        assert!(a.ask && a.custom_input.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn timeout_kills_fail_open() {
        let event = test_event(AgentLifecycle::Stop {
            success: false,
            summary: None,
        });
        let slow = [UserHookEntry {
            command: "sleep 30".to_string(),
            timeout: Some(1),
            when: None,
            respond: None,
        }];
        let start = Instant::now();
        assert!(run_local_hooks(&event, &slow, None).is_none());
        assert!(start.elapsed() < Duration::from_secs(10));
    }

    #[cfg(unix)]
    #[test]
    fn omitted_timeout_inherits_the_harness_window() {
        // No entry timeout + a 4s harness window: our kill must fire inside
        // the window (4s minus the grace margin), not at the 10s default —
        // the first timer to fire wins, and it must be ours, not the harness's.
        let event = test_event(AgentLifecycle::Stop {
            success: false,
            summary: None,
        });
        let inherit = [UserHookEntry {
            command: "sleep 30".to_string(),
            timeout: None,
            when: None,
            respond: None,
        }];
        let start = Instant::now();
        assert!(run_local_hooks(&event, &inherit, Some(Duration::from_secs(4))).is_none());
        assert!(
            start.elapsed() < Duration::from_secs(4),
            "inherited kill must fire before the harness window ends, took {:?}",
            start.elapsed()
        );
    }

    #[cfg(unix)]
    #[test]
    fn omitted_timeout_parses_and_round_trips_absent() {
        // The config default must stay distinguishable from an explicit value:
        // absent parses to None and serializes back without the key.
        let entry: UserHookEntry =
            serde_json::from_value(serde_json::json!({"command": "exit 0"})).expect("parse");
        assert_eq!(entry.timeout, None);
        let v = serde_json::to_value(&entry).expect("serialize");
        assert!(
            v.get("timeout").is_none(),
            "absent timeout must not serialize"
        );
    }

    #[cfg(unix)]
    fn sleepers(n: usize, secs: u64) -> Vec<UserHookEntry> {
        (0..n)
            .map(|_| UserHookEntry {
                command: format!("sleep {secs}"),
                timeout: Some(10),
                when: None,
                respond: None,
            })
            .collect()
    }

    #[cfg(unix)]
    #[test]
    fn local_hooks_fan_out_in_parallel() {
        // The harness runs every matching hook concurrently, so we must too:
        // in sequence these three cost 3s and spend the budget three times
        // over for no reason.
        let event = test_event(AgentLifecycle::UserPromptSubmit {
            user_prompt: Some("hi".to_string()),
        });
        let start = Instant::now();
        assert!(run_local_hooks(&event, &sleepers(3, 1), None).is_none());
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(2500),
            "three 1s hooks must run concurrently, took {elapsed:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn budget_caps_each_hook_so_the_parallel_total_fits() {
        // Four entries declaring 10s each, under a 2s budget. Concurrency
        // makes the wall time the slowest hook rather than the sum, and the
        // cap keeps that slowest hook inside the window the harness allows —
        // overrunning does not make the decision late, it makes it discarded.
        let event = test_event(AgentLifecycle::UserPromptSubmit {
            user_prompt: Some("hi".to_string()),
        });
        let start = Instant::now();
        let decision = run_local_hooks(&event, &sleepers(4, 10), Some(Duration::from_secs(2)));
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(5),
            "budget must cap each hook, took {elapsed:?}"
        );
        // Nothing returned a verdict, so there is no decision — silence means
        // proceed, exactly as an exiting hook that printed nothing.
        assert!(decision.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn merge_follows_entry_order_not_completion_order() {
        // Concurrency must not leak into the verdict: a slow first entry
        // still outranks a fast second one, so the decision is a function of
        // the config rather than of the race.
        let event = test_event(AgentLifecycle::UserPromptSubmit {
            user_prompt: Some("hi".to_string()),
        });
        let entries = vec![
            UserHookEntry {
                command: "sleep 1; echo first >&2; exit 2".to_string(),
                timeout: Some(10),
                when: None,
                respond: None,
            },
            UserHookEntry {
                command: "echo second >&2; exit 2".to_string(),
                timeout: Some(10),
                when: None,
                respond: None,
            },
        ];
        let d = run_local_hooks(&event, &entries, None).expect("deny");
        assert!(!d.allow);
        assert_eq!(d.custom_input.as_deref(), Some("first"));
    }

    #[cfg(unix)]
    #[test]
    fn event_json_reaches_stdin_with_session_and_args() {
        let event = test_event(AgentLifecycle::PreToolUse {
            tool_name: "Edit".to_string(),
            input: serde_json::json!({"path": "/tmp/x"}),
            prompt: None,
        });
        let dir = std::env::temp_dir().join(format!(
            "agent-hooks-usertest-stdin-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("got.json");
        let entries = [UserHookEntry {
            command: format!("cat > {}", out.display()),
            timeout: Some(5),
            when: None,
            respond: None,
        }];
        assert!(run_local_hooks(&event, &entries, None).is_none());
        let got: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
        assert_eq!(got["session_id"], "sess-1");
        assert_eq!(got["lifecycle"]["data"]["tool_name"], "Edit");
        assert_eq!(got["lifecycle"]["data"]["input"]["path"], "/tmp/x");
        std::fs::remove_dir_all(&dir).ok();
    }

    fn color_question() -> PromptQuestion {
        PromptQuestion {
            id: "favorite_color".to_string(),
            header: None,
            question: "Favorite color?".to_string(),
            options: ["Red", "Blue"]
                .into_iter()
                .map(|label| PromptOption {
                    label: label.to_string(),
                    description: None,
                    preview: None,
                })
                .collect(),
            multiple: false,
            custom: false,
            recommended: None,
        }
    }

    fn blue_answer() -> PromptAnswer {
        PromptAnswer {
            question_id: "favorite_color".to_string(),
            selected_options: vec!["Blue".to_string()],
            custom_input: None,
        }
    }

    fn blue_decision() -> PromptDecision {
        let mut d = PromptDecision::allow();
        d.answers = vec![blue_answer()];
        d
    }

    fn question_event() -> AgentEvent {
        test_event(AgentLifecycle::PermissionRequest {
            prompt: PromptRequest {
                prompt_id: "prompt-1".to_string(),
                kind: PromptKind::Question,
                tool_name: Some("Bash".to_string()),
                command: None,
                file_path: None,
                description: Some("Pick a color".to_string()),
                questions: vec![color_question()],
                suggestions: Vec::new(),
            },
        })
    }

    fn bash_pre_tool_use(command: &str) -> AgentEvent {
        test_event(AgentLifecycle::PreToolUse {
            tool_name: "Bash".to_string(),
            input: serde_json::json!({"command": command}),
            prompt: None,
        })
    }

    fn respond_entry(when: Option<HookMatch>, respond: PromptDecision) -> UserHookEntry {
        UserHookEntry {
            command: String::new(),
            timeout: Some(5),
            when,
            respond: Some(respond),
        }
    }

    #[test]
    fn commandless_when_respond_entries_parse() {
        // `command` is optional now; a pure match+answer entry deserializes,
        // and `respond` is a canonical PromptDecision.
        let cfg: UserHooksConfig = serde_json::from_str(
            r#"{"version":1,"hooks":{"PermissionRequest":[{"when":{"tool":"Bash"},"respond":{"allow":true,"answers":[{"question_id":"favorite_color","selected_options":["Blue"]}]}}],"PreToolUse":[{"when":{"prompt_matches":"rm -rf"},"respond":{"allow":false,"custom_input":"rm -rf is never auto-approved"}}]}}"#,
        )
        .unwrap();
        let entry = &cfg.hooks["PermissionRequest"][0];
        assert!(entry.command.is_empty());
        assert_eq!(entry.when.as_ref().unwrap().tool.as_deref(), Some("Bash"));
        let respond = entry.respond.as_ref().unwrap();
        assert!(respond.allow);
        assert_eq!(
            respond.answers[0].selected_options,
            vec!["Blue".to_string()]
        );
        let deny = &cfg.hooks["PreToolUse"][0];
        assert_eq!(
            deny.when.as_ref().unwrap().prompt_matches.as_deref(),
            Some("rm -rf")
        );
        let respond = deny.respond.as_ref().unwrap();
        assert!(respond.is_deny());
        assert_eq!(
            respond.custom_input.as_deref(),
            Some("rm -rf is never auto-approved")
        );
    }

    #[test]
    fn legacy_respond_shape_and_typos_are_rejected_not_ignored() {
        for bad in [
            r#"{"decision":"deny","reason":"nope"}"#,
            r#"{"allow":false,"reason":"nope"}"#,
            r#"{"allow":true,"answer":[]}"#,
            r#"{"custom_input":"missing allow"}"#,
        ] {
            let text = format!(r#"{{"version":1,"hooks":{{"PreToolUse":[{{"respond":{bad}}}]}}}}"#);
            let err = serde_json::from_str::<UserHooksConfig>(&text)
                .expect_err(bad)
                .to_string();
            assert!(err.contains("respond"), "{bad}: {err}");
        }
    }

    #[test]
    fn respond_roundtrips_through_config_serialization() {
        let mut decision = PromptDecision::allow_always();
        decision.answers = vec![blue_answer()];
        decision.additional_context = Some("ctx".to_string());
        let entry = respond_entry(None, decision.clone());
        let json = serde_json::to_string(&entry).unwrap();
        let back: UserHookEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(back.respond, Some(decision));
    }

    #[test]
    fn when_tool_match_returns_canned_blue_answers_without_spawning() {
        let event = question_event();
        // `command` would deny if spawned — the canned path must not spawn.
        let entries = [UserHookEntry {
            command: "exit 2".to_string(),
            timeout: Some(5),
            when: Some(HookMatch {
                tool: Some("bash".to_string()),
                prompt_matches: None,
            }),
            respond: Some(blue_decision()),
        }];
        let d = run_local_hooks(&event, &entries, None).expect("canned allow+answers");
        assert!(d.allow && !d.ask);
        assert_eq!(d.answers, vec![blue_answer()]);
        // Non-matching tool: the entry is skipped entirely (no spawn, no deny).
        let miss = [UserHookEntry {
            command: "exit 2".to_string(),
            timeout: Some(5),
            when: Some(HookMatch {
                tool: Some("Edit".to_string()),
                prompt_matches: None,
            }),
            respond: Some(PromptDecision::deny(Some("nope".to_string()))),
        }];
        assert!(run_local_hooks(&event, &miss, None).is_none());
    }

    #[test]
    fn plain_canned_allow_is_kept() {
        let event = bash_pre_tool_use("ls");
        let entries = [respond_entry(None, PromptDecision::allow())];
        let d = run_local_hooks(&event, &entries, None).expect("canned allow is a decision");
        assert!(d.allow && !d.ask && !d.defer);
        assert!(d.answers.is_empty());
        // It survives the merge against a daemon that deferred.
        let merged = merge_decisions(Some(d), Some(PromptDecision::defer()));
        assert!(merged.allow);
    }

    #[test]
    fn canned_allow_always_and_suggestion_survive() {
        let event = bash_pre_tool_use("ls");
        let entries = [respond_entry(
            None,
            PromptDecision::allow_with_suggestion(0),
        )];
        let d = run_local_hooks(&event, &entries, None).unwrap();
        assert!(d.allow && d.allow_always && d.suggestion == Some(0));
    }

    #[test]
    fn canned_defer_is_no_opinion_but_keeps_context() {
        let event = bash_pre_tool_use("ls");
        let ctx = [respond_entry(
            None,
            PromptDecision::defer().with_additional_context("note"),
        )];
        let d = run_local_hooks(&event, &ctx, None).unwrap();
        assert!(d.defer && !d.allow);
        assert_eq!(d.additional_context.as_deref(), Some("note"));
        // A defer alone carries no verdict at all.
        let none = [respond_entry(None, PromptDecision::defer())];
        assert!(run_local_hooks(&event, &none, None).is_none());
    }

    #[test]
    fn invalid_canned_answers_fail_closed_with_reason() {
        let event = question_event();
        let mut unknown_question = PromptDecision::allow();
        unknown_question.answers = vec![PromptAnswer {
            question_id: "nope".to_string(),
            selected_options: vec!["Blue".to_string()],
            custom_input: None,
        }];
        let mut bad_label = PromptDecision::allow();
        bad_label.answers = vec![PromptAnswer {
            question_id: "favorite_color".to_string(),
            selected_options: vec!["Green".to_string()],
            custom_input: None,
        }];
        let mut conflicting = blue_decision();
        conflicting.ask = true;
        for decision in [unknown_question, bad_label, conflicting] {
            let d = run_local_hooks(&event, &[respond_entry(None, decision)], None)
                .expect("invalid response must decide");
            assert!(d.is_deny(), "{d:?}");
            assert!(d.answers.is_empty());
            assert!(
                d.custom_input
                    .as_deref()
                    .is_some_and(|r| r.contains("invalid local hook response")),
                "{d:?}"
            );
        }
        // Answers on an event that asked nothing never route either.
        let d = run_local_hooks(
            &bash_pre_tool_use("ls"),
            &[respond_entry(None, blue_decision())],
            None,
        )
        .unwrap();
        assert!(d.is_deny());
    }

    #[test]
    fn prompt_matches_substring_denies() {
        let event = bash_pre_tool_use("rm -rf /tmp/x");
        let entries = [respond_entry(
            Some(HookMatch {
                tool: None,
                prompt_matches: Some("RM -RF".to_string()),
            }),
            PromptDecision::deny(Some("rm -rf is never auto-approved".to_string())),
        )];
        let d = run_local_hooks(&event, &entries, None).expect("prompt match must deny");
        assert!(!d.allow && !d.ask);
        assert_eq!(
            d.custom_input.as_deref(),
            Some("rm -rf is never auto-approved")
        );
        // Non-matching substring: skipped, fail-open allow.
        let miss = [respond_entry(
            Some(HookMatch {
                tool: None,
                prompt_matches: Some("definitely not in the input".to_string()),
            }),
            PromptDecision::deny(Some("nope".to_string())),
        )];
        assert!(run_local_hooks(&event, &miss, None).is_none());
    }

    #[test]
    fn canned_ask_escalates_with_reason() {
        let event = bash_pre_tool_use("ls");
        let entries = [respond_entry(
            None,
            PromptDecision::ask(Some("are you sure?".to_string())),
        )];
        let a = run_local_hooks(&event, &entries, None).expect("canned ask must escalate");
        assert!(a.ask && !a.allow);
        assert_eq!(a.custom_input.as_deref(), Some("are you sure?"));
    }

    #[test]
    fn canned_ask_beats_canned_allow() {
        let event = bash_pre_tool_use("ls");
        let entries = [
            respond_entry(None, PromptDecision::allow()),
            respond_entry(None, PromptDecision::ask(Some("q?".to_string()))),
        ];
        let d = run_local_hooks(&event, &entries, None).unwrap();
        assert!(d.ask && !d.allow);
    }

    #[cfg(unix)]
    #[test]
    fn spawned_deny_vetoes_canned_ask() {
        let event = bash_pre_tool_use("ls");
        let chain = [
            respond_entry(None, PromptDecision::ask(Some("are you sure?".to_string()))),
            UserHookEntry {
                command: "exit 2".to_string(),
                timeout: Some(5),
                when: None,
                respond: None,
            },
        ];
        let d = run_local_hooks(&event, &chain, None).expect("deny must win over canned ask");
        assert!(!d.allow && !d.ask);
    }

    #[cfg(unix)]
    fn context_entry(command: &str) -> UserHookEntry {
        UserHookEntry {
            command: command.to_string(),
            timeout: Some(5),
            when: None,
            respond: None,
        }
    }

    #[cfg(unix)]
    #[test]
    fn stdout_json_context_defers_with_context() {
        let event = test_event(AgentLifecycle::SessionStart {
            resume_command: None,
        });
        let entries = [context_entry(r#"printf '{"additional_context":"hello"}'"#)];
        let d = run_local_hooks(&event, &entries, None).expect("context is a decision");
        // No verdict was given, so it must not become an allow that bypasses
        // the harness's own permission checks.
        assert!(d.defer && !d.allow);
        assert_eq!(d.additional_context.as_deref(), Some("hello"));
    }

    #[cfg(unix)]
    #[test]
    fn stdout_accepts_harness_native_shape() {
        let event = test_event(AgentLifecycle::SessionStart {
            resume_command: None,
        });
        let entries = [context_entry(
            r#"printf '{"hookSpecificOutput":{"hookEventName":"SessionStart","additionalContext":"native"}}'"#,
        )];
        let d = run_local_hooks(&event, &entries, None).unwrap();
        assert_eq!(d.additional_context.as_deref(), Some("native"));
    }

    #[cfg(unix)]
    #[test]
    fn plain_or_malformed_stdout_stays_ignored() {
        let event = test_event(AgentLifecycle::SessionStart {
            resume_command: None,
        });
        for cmd in [
            "echo just logging",
            "printf '{not json'",
            r#"printf '{"additional_context":"  "}'"#,
            // No `allow`, so not a verdict attempt: warned about and ignored.
            r#"printf '{"decision":"deny","reason":"stale shape"}'"#,
            r#"printf '{"custom_input":"no allow field"}'"#,
        ] {
            assert!(
                run_local_hooks(&event, &[context_entry(cmd)], None).is_none(),
                "{cmd}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn contexts_join_in_entry_order_and_chatty_stdout_does_not_hang() {
        let event = test_event(AgentLifecycle::SessionStart {
            resume_command: None,
        });
        let entries = [
            // 1 MB of noise first: the pipe must be drained, not block.
            context_entry("head -c 1000000 /dev/zero | tr '\\0' x; echo"),
            context_entry(r#"sleep 0.3; printf '{"additional_context":"first"}'"#),
            context_entry(r#"printf '{"additional_context":"second"}'"#),
        ];
        let start = Instant::now();
        let d = run_local_hooks(&event, &entries, None).unwrap();
        assert!(start.elapsed() < Duration::from_secs(4));
        assert_eq!(d.additional_context.as_deref(), Some("first\n\nsecond"));
    }

    #[cfg(unix)]
    #[test]
    fn deny_beats_context_and_drops_it() {
        let event = test_event(AgentLifecycle::UserPromptSubmit {
            user_prompt: Some("hi".to_string()),
        });
        let entries = [
            context_entry(r#"printf '{"additional_context":"x"}'"#),
            context_entry("echo no >&2; exit 2"),
        ];
        let d = run_local_hooks(&event, &entries, None).unwrap();
        assert!(d.is_deny());
        assert!(d.additional_context.is_none());
    }

    #[test]
    fn merge_keeps_context_from_both_sources_and_never_on_ask() {
        let local = PromptDecision::defer().with_additional_context("local");
        let daemon = PromptDecision::allow().with_additional_context("daemon");
        let merged = merge_decisions(Some(local.clone()), Some(daemon));
        assert!(merged.allow);
        assert_eq!(
            merged.additional_context.as_deref(),
            Some("local\n\ndaemon")
        );

        let asked = merge_decisions(
            Some(local),
            Some(PromptDecision::ask(Some("sure?".to_string()))),
        );
        assert!(asked.ask && asked.additional_context.is_none());
    }

    #[test]
    fn canned_respond_carries_static_context() {
        let event = test_event(AgentLifecycle::SessionStart {
            resume_command: None,
        });
        let entries = [respond_entry(
            None,
            PromptDecision::allow().with_additional_context("be brief"),
        )];
        let d = run_local_hooks(&event, &entries, None).unwrap();
        // An explicit allow stays an allow; the context rides along.
        assert!(d.allow && !d.defer);
        assert_eq!(d.additional_context.as_deref(), Some("be brief"));
    }

    #[cfg(unix)]
    fn json_stdout_entry(json: &str) -> UserHookEntry {
        context_entry(&format!("printf '%s' '{json}'"))
    }

    #[cfg(unix)]
    #[test]
    fn stdout_canonical_decision_roundtrips() {
        let event = question_event();
        let mut decision = blue_decision();
        decision.additional_context = Some("because".to_string());
        let json = serde_json::to_string(&decision).unwrap();
        let d = run_local_hooks(&event, &[json_stdout_entry(&json)], None).unwrap();
        assert_eq!(d, decision);

        for original in [
            PromptDecision::allow(),
            PromptDecision::allow_with_suggestion(1),
            PromptDecision::deny(Some("stdout says no".to_string())),
            PromptDecision::ask(Some("really?".to_string())),
            PromptDecision::defer().with_additional_context("ctx"),
        ] {
            let json = serde_json::to_string(&original).unwrap();
            let d = run_local_hooks(&bash_pre_tool_use("ls"), &[json_stdout_entry(&json)], None)
                .unwrap_or_else(|| panic!("{json} must decide"));
            assert_eq!(d, original, "{json}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn stdout_plain_allow_and_explicit_allow_with_context() {
        let event = bash_pre_tool_use("ls");
        let d = run_local_hooks(&event, &[json_stdout_entry(r#"{"allow":true}"#)], None).unwrap();
        assert!(d.allow && !d.defer);
        let d = run_local_hooks(
            &event,
            &[json_stdout_entry(
                r#"{"allow":true,"additional_context":"hi"}"#,
            )],
            None,
        )
        .unwrap();
        assert!(d.allow);
        assert_eq!(d.additional_context.as_deref(), Some("hi"));
    }

    #[cfg(unix)]
    #[test]
    fn nonzero_exit_beats_stdout_verdict() {
        let event = bash_pre_tool_use("ls");
        let deny = context_entry(r#"printf '{"allow":true}'; echo blocked >&2; exit 2"#);
        let d = run_local_hooks(&event, &[deny], None).unwrap();
        assert!(d.is_deny() && d.custom_input.as_deref() == Some("blocked"));
        let ask = context_entry(r#"printf '{"allow":true}'; echo sure? >&2; exit 3"#);
        let d = run_local_hooks(&event, &[ask], None).unwrap();
        assert!(d.ask && d.custom_input.as_deref() == Some("sure?"));
        // An ask on stdout of exit 0 still loses to a later exit-2 veto.
        let chain = [
            json_stdout_entry(r#"{"allow":false,"ask":true,"custom_input":"q"}"#),
            context_entry("exit 2"),
        ];
        assert!(run_local_hooks(&event, &chain, None).unwrap().is_deny());
    }

    #[cfg(unix)]
    #[test]
    fn malformed_stdout_verdict_fails_closed() {
        let event = question_event();
        for json in [
            // Stale local shape alongside a canonical key.
            r#"{"allow":true,"reason":"nope"}"#,
            r#"{"allow":"yes"}"#,
            r#"{"allow":true,"custom_input":1}"#,
            // Answer for a question that does not exist.
            r#"{"allow":true,"answers":[{"question_id":"zzz","selected_options":["Blue"]}]}"#,
            // Conflicting verdict flags.
            r#"{"allow":true,"ask":true}"#,
        ] {
            let d = run_local_hooks(&event, &[json_stdout_entry(json)], None)
                .unwrap_or_else(|| panic!("{json} must not be silently dropped"));
            assert!(d.is_deny(), "{json}: {d:?}");
            assert!(d.answers.is_empty());
            assert!(
                d.custom_input
                    .as_deref()
                    .is_some_and(|r| r.contains("invalid local hook")),
                "{json}: {d:?}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn oversized_stdout_is_denied_with_a_size_error_never_dropped() {
        let event = bash_pre_tool_use("ls");
        let big = context_entry(
            r#"printf '{"allow":true,"custom_input":"'; head -c 1100000 /dev/zero | tr '\0' x; printf '"}'"#,
        );
        let d = run_local_hooks(&event, &[big], None).expect("oversized stdout must decide");
        assert!(d.is_deny() && d.answers.is_empty(), "{d:?}");
        let reason = d.custom_input.as_deref().unwrap_or_default();
        assert!(
            reason.contains("1048576") && reason.contains("bytes"),
            "{reason}"
        );
    }
}

/// `agent-hooks init [--project]`: scaffold a user-hooks file.
pub fn cmd_init(args: InitArgs) -> anyhow::Result<()> {
    let dest = if args.project {
        project_config_path()
            .ok_or_else(|| anyhow::anyhow!("cannot determine current directory"))?
    } else {
        user_config_path().ok_or_else(|| anyhow::anyhow!("cannot determine home directory"))?
    };
    if dest.exists() {
        anyhow::bail!("{} already exists — refusing to overwrite", dest.display());
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&dest, format!("{}\n", scaffold_json()))?;
    println!("wrote {}", dest.display());
    println!("next steps:");
    println!("  1. edit the commands for the lifecycle events you care about");
    println!("     (keys: {})", KNOWN_LIFECYCLES.join(", "));
    println!("  2. scripts get the normalized event JSON on stdin +");
    println!("     AGENT_HOOKS_EVENT / AGENT_HOOKS_SESSION / AGENT_HOOKS_HARNESS env");
    println!(
        "  3. exit 0 = allow, exit 2 = deny (stderr = reason), exit 3 = ask; no daemon needed"
    );
    println!("     (exit-0 stdout may also be a PromptDecision JSON, e.g. {{\"allow\":true}})");
    Ok(())
}
