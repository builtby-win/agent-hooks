use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::event::AgentEvent;
use crate::prompt::PromptDecision;

/// Standard JSON-RPC 2.0 Request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: Value,
    pub method: String,
    #[serde(default)]
    pub params: Option<Value>,
}

impl JsonRpcRequest {
    pub fn new(id: impl Into<Value>, method: impl Into<String>, params: Option<Value>) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id: id.into(),
            method: method.into(),
            params,
        }
    }
}

/// Standard JSON-RPC 2.0 Response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// Standard JSON-RPC 2.0 Notification (No ID)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcNotification {
    pub jsonrpc: String,
    pub method: String,
    pub params: Value,
}

impl JsonRpcNotification {
    pub fn event(event: AgentEvent) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            method: "event".to_string(),
            params: serde_json::to_value(event).unwrap_or_default(),
        }
    }
}

/// Subscription filter parameters for consumers
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SubscriptionFilter {
    /// Filter by specific harnesses (e.g. ["claude-code", "codex"])
    #[serde(default)]
    pub harnesses: Vec<String>,

    /// Filter by specific session IDs
    #[serde(default)]
    pub session_ids: Vec<String>,

    /// Filter by lifecycle event types (e.g. ["PermissionRequest", "Stop"])
    #[serde(default)]
    pub event_types: Vec<String>,

    /// What this subscriber will answer. Gating is opt-in AND scoped: the
    /// daemon holds a gate event only when some connected decider declared
    /// it would answer *that* event. See [`Decide`].
    #[serde(default)]
    pub decide: Decide,

    /// Env var names this subscriber wants in `event.passthrough_env`.
    /// Declaration only — the daemon never interprets the values. The shim
    /// treats a subscription as consent and captures the active union
    /// (`env.requested`) with no allow-list gate. Empty means no additional
    /// request; it does not filter the values delivered in events.
    #[serde(default)]
    pub passthrough_env: Vec<String>,
}

/// Primary JSON-RPC method returning the active union of requested env names.
pub const ENV_REQUESTED_METHOD: &str = "env.requested";

/// Response payload for [`ENV_REQUESTED_METHOD`]: the sorted, de-duplicated
/// union of valid requested names across all connected subscribers.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct EnvRequestedResponse {
    #[serde(default)]
    pub passthrough_env: Vec<String>,
}

/// Whether `name` is a usable env var name: non-empty ASCII, starts with a
/// letter or underscore, remaining chars alphanumeric or underscore.
pub fn is_valid_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Normalize a requested-name list: trim, drop invalid/empty, de-duplicate,
/// sort. Applied identically at subscribe time and when serving the union,
/// so an invalid name is excluded everywhere rather than rejected in one
/// place and honored in another.
pub fn normalize_env_names(names: &[String]) -> Vec<String> {
    let mut out: Vec<String> = names
        .iter()
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty() && is_valid_env_name(n))
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Which gate events this subscriber will answer — declared up front because the hold is decided at dispatch, before any verdict could arrive.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Decide {
    /// `false` — pure observer, never held.
    ///
    /// `true` — answer only events the harness already blocked on; never manufactures a wait.
    Enabled(bool),
    /// Answer exactly what this scope names, including mid-flight events — you own the stall you introduce.
    Scope(DecideScope),
}

impl Default for Decide {
    fn default() -> Self {
        Decide::Enabled(false)
    }
}

impl Decide {
    /// Whether this subscriber decides anything at all.
    pub fn enabled(&self) -> bool {
        !matches!(self, Decide::Enabled(false))
    }

    /// Resolve to a concrete scope; `true` inherits the subscription filter — you cannot answer what you were never sent.
    pub fn resolve(&self, filter: &SubscriptionFilter) -> Option<DecideScope> {
        match self {
            Decide::Enabled(false) => None,
            Decide::Enabled(true) => Some(DecideScope {
                harnesses: filter.harnesses.clone(),
                session_ids: filter.session_ids.clone(),
                event_types: filter.event_types.clone(),
                tools: Vec::new(),
                interactive_only: true,
            }),
            Decide::Scope(s) => Some(s.clone()),
        }
    }
}

/// The events a decider will answer. Empty vec = no constraint on that axis.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DecideScope {
    #[serde(default)]
    pub harnesses: Vec<String>,
    #[serde(default)]
    pub session_ids: Vec<String>,
    #[serde(default)]
    pub event_types: Vec<String>,
    /// Tool names (harness `matcher` concept); only events that name a tool can match.
    #[serde(default)]
    pub tools: Vec<String>,
    /// Claim only events the harness already blocked on; explicit scopes default to `false` because you own the stall you name.
    #[serde(default)]
    pub interactive_only: bool,
}

impl DecideScope {
    /// Whether this decider claims the given event.
    pub fn claims(&self, event: &AgentEvent) -> bool {
        if self.interactive_only && !event.is_interactive() {
            return false;
        }
        if !self.harnesses.is_empty() && !self.harnesses.iter().any(|h| h == &event.harness) {
            return false;
        }
        if !self.session_ids.is_empty() && !self.session_ids.iter().any(|s| s == &event.session_id)
        {
            return false;
        }
        if !self.event_types.is_empty() {
            let name = event.lifecycle.name();
            if !self.event_types.iter().any(|t| t == name) {
                return false;
            }
        }
        if !self.tools.is_empty() {
            let Some(tool) = event_tool_name(event) else {
                return false;
            };
            if !self.tools.iter().any(|t| t == tool) {
                return false;
            }
        }
        true
    }
}

/// Stamped by the daemon onto a delivered event when the agent is blocked on
/// it awaiting a verdict.
///
/// Its presence IS the flag. Everything a decider needs to answer is here, so
/// it never has to consult a capability table, re-derive its own scope, or
/// know that `PermissionRequest` correlates on a different id than every
/// other event.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PromptHold {
    /// Answer with this id via `prompt.respond`. Already resolved — callers
    /// never reconstruct it from `event_id`.
    pub prompt_id: String,
    /// Epoch ms after which the hold resolves itself (fail-closed to deny).
    /// Answer before this or don't bother.
    pub deadline_ms: u64,
    /// The verdicts this harness accepts for this event, from its manifest.
    /// Answering outside this set gets coerced — e.g. Claude Code takes
    /// allow/deny on `UserPromptSubmit` but has no `ask` there.
    #[serde(default)]
    pub verdicts: Vec<String>,
    /// True when the harness had already stopped for a human (a permission
    /// request or a question) — answering costs the agent nothing. False when
    /// a decider claimed a mid-flight event and the agent is stalled waiting.
    /// UIs should surface the difference; it is the difference between
    /// answering a question and being in the way.
    pub interactive: bool,
}

/// Tool named by an event, when it names one.
fn event_tool_name(event: &AgentEvent) -> Option<&str> {
    use crate::lifecycle::AgentLifecycle;
    match &event.lifecycle {
        AgentLifecycle::PreToolUse { tool_name, .. }
        | AgentLifecycle::PostToolUse { tool_name, .. }
        | AgentLifecycle::PostToolUseFailure { tool_name, .. } => Some(tool_name),
        AgentLifecycle::PermissionRequest { prompt } => prompt.tool_name.as_deref(),
        _ => None,
    }
}

/// Shared subscription filter: empty vec = no constraint on that axis.
pub fn event_passes_filter(event: &AgentEvent, filter: &SubscriptionFilter) -> bool {
    if !filter.harnesses.is_empty() && !filter.harnesses.iter().any(|h| h == &event.harness) {
        return false;
    }
    if !filter.session_ids.is_empty() && !filter.session_ids.iter().any(|s| s == &event.session_id)
    {
        return false;
    }
    if !filter.event_types.is_empty() {
        let name = event.lifecycle.name();
        if !filter.event_types.iter().any(|t| t == name) {
            return false;
        }
    }
    true
}

/// Parameters for prompt.respond RPC method
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptRespondParams {
    pub prompt_id: String,
    pub decision: PromptDecision,
}

/// Default Unix Domain Socket path.
pub const DEFAULT_UNIX_SOCKET_PATH: &str = "/tmp/agent-hooks.sock";

/// Default Windows Named Pipe path.
pub const DEFAULT_WINDOWS_PIPE_PATH: &str = r"\\.\pipe\agent-hooks";

/// Environment variable to override the default socket path.
pub const ENV_AGENT_HOOKS_SOCKET: &str = "AGENT_HOOKS_SOCKET";

/// Resolves the default socket or named pipe path for the current platform,
/// respecting the `AGENT_HOOKS_SOCKET` environment variable when present.
pub fn default_socket_path() -> String {
    if let Ok(val) = std::env::var(ENV_AGENT_HOOKS_SOCKET) {
        let trimmed = val.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    if cfg!(windows) {
        DEFAULT_WINDOWS_PIPE_PATH.to_string()
    } else {
        DEFAULT_UNIX_SOCKET_PATH.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_socket_path_and_env_override() {
        // Save existing env
        let orig = std::env::var(ENV_AGENT_HOOKS_SOCKET).ok();

        std::env::remove_var(ENV_AGENT_HOOKS_SOCKET);
        let default_path = default_socket_path();
        if cfg!(windows) {
            assert_eq!(default_path, DEFAULT_WINDOWS_PIPE_PATH);
        } else {
            assert_eq!(default_path, DEFAULT_UNIX_SOCKET_PATH);
        }

        // Test override
        std::env::set_var(ENV_AGENT_HOOKS_SOCKET, "/custom/path.sock");
        assert_eq!(default_socket_path(), "/custom/path.sock");

        // Restore env
        if let Some(val) = orig {
            std::env::set_var(ENV_AGENT_HOOKS_SOCKET, val);
        } else {
            std::env::remove_var(ENV_AGENT_HOOKS_SOCKET);
        }
    }

    #[test]
    fn env_name_validation_and_normalization_exclude_invalid_consistently() {
        assert!(is_valid_env_name("TERM_PROGRAM"));
        assert!(is_valid_env_name("_X1"));
        assert!(!is_valid_env_name(""));
        assert!(!is_valid_env_name("1ABC"));
        assert!(!is_valid_env_name("HAS-DASH"));
        assert!(!is_valid_env_name("HAS SPACE"));
        assert!(!is_valid_env_name("lower.dot"));
        let normalized = normalize_env_names(&[
            "TMUX".to_string(),
            " TERM_PROGRAM ".to_string(),
            "TMUX".to_string(),
            "bad-name!".to_string(),
            "".to_string(),
            "1NOPE".to_string(),
        ]);
        assert_eq!(normalized, vec!["TERM_PROGRAM", "TMUX"]);
    }
}
