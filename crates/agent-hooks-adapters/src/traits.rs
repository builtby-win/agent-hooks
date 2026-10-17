use crate::error::AdapterError;
use agent_hooks_protocol::{AgentEvent, PromptDecision, RawHookInput};

/// Location of a hook definition file or directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookLocation {
    /// Global user config, e.g. `~/.claude/settings.json`
    GlobalFile(String),
    /// Project-local config, e.g. `.claude/settings.json` or `.github/hooks`
    ProjectFile(String),
    /// Arbitrary custom path
    Custom(String),
}

impl HookLocation {
    pub fn path(&self) -> &str {
        match self {
            Self::GlobalFile(p) | Self::ProjectFile(p) | Self::Custom(p) => p,
        }
    }

    pub fn is_global(&self) -> bool {
        matches!(self, Self::GlobalFile(_))
    }
}

/// Target / event type for a hook definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookEventTarget {
    /// Lifecycle event like `Stop`, `PreToolUse`, `UserPromptSubmit`
    Lifecycle(String),
    /// Tool-specific matcher
    Tool(String),
    /// Generic hook
    Generic(String),
}

impl HookEventTarget {
    pub fn event_name(&self) -> &str {
        match self {
            Self::Lifecycle(s) | Self::Tool(s) | Self::Generic(s) => s,
        }
    }
}

/// Definition of a hook registration – where to install and what to listen for.
#[derive(Debug, Clone)]
pub struct HookDefinition {
    /// Where the hook is installed on disk.
    pub location: HookLocation,
    /// What harness event triggers the hook.
    pub target: HookEventTarget,
    /// Shell command template to invoke for this hook.
    pub command: String,
    /// Optional human readable description.
    pub description: Option<String>,
}

impl HookDefinition {
    pub fn new(
        location: HookLocation,
        target: HookEventTarget,
        command: impl Into<String>,
    ) -> Self {
        Self {
            location,
            target,
            command: command.into(),
            description: None,
        }
    }

    pub fn with_description(mut self, desc: impl Into<String>) -> Self {
        self.description = Some(desc.into());
        self
    }

    pub fn event(&self) -> &str {
        self.target.event_name()
    }

    pub fn path(&self) -> &str {
        self.location.path()
    }
}

/// Core trait implemented by every harness adapter.
///
/// Object-safe so it can be used as `Box<dyn HarnessAdapter>`.
pub trait HarnessAdapter: Send + Sync {
    /// Adapter identifier, e.g. `claude-code`, `codex`, `gemini-cli`
    fn id(&self) -> &str;
    /// Human display name
    fn display_name(&self) -> &str;
    /// Alternate names that resolve to this adapter via [`crate::registry::AdapterRegistry::get`]
    fn aliases(&self) -> &[&str] {
        &[]
    }
    /// Hook definitions describing where and how to install hooks for this harness
    fn hook_definitions(&self) -> Vec<HookDefinition>;
    /// Convert a raw harness input into a normalized [`AgentEvent`]
    fn parse_event(&self, input: &RawHookInput) -> Result<AgentEvent, AdapterError>;
    /// Resolve a human decision to the typed, agent-aware normalized outcome
    /// for the most recent `parse_event`. Agent + event quirks (e.g. Codex
    /// rejects bare allow, ask unsupported on some events) resolve here, so
    /// users can match on [`agent_hooks_protocol::NormalizedResponse`]
    /// instead of parsing host JSON. Default assumes ask is supported;
    /// manifest-backed adapters derive it from per-event verdicts.
    fn normalize_response(
        &self,
        decision: &PromptDecision,
    ) -> agent_hooks_protocol::NormalizedResponse {
        agent_hooks_protocol::NormalizedResponse::from_gate_decision(decision, true)
    }
    /// Format a human decision back into the harness-specific JSON response
    fn format_response(&self, decision: &PromptDecision) -> serde_json::Value {
        if decision.defer {
            serde_json::json!({})
        } else if decision.allow {
            serde_json::json!({"decision": "allow"})
        } else {
            let mut v = serde_json::json!({"decision": "deny"});
            if let Some(reason) = &decision.custom_input {
                v["reason"] = serde_json::Value::String(reason.clone());
            }
            v
        }
    }
    /// Response mode for this adapter (json vs exit2). Default is Json.
    fn response_mode(&self) -> crate::manifest::ResponseMode {
        crate::manifest::ResponseMode::Json
    }
    /// Capability of the most recent `parse_event` on this instance.
    /// Lets the hook shim wait long on gate events (human permission
    /// dialogs) while failing fast on observe events. Default `None`
    /// (fail fast) for adapters without manifest capabilities.
    fn capability_for(&self, _event: &AgentEvent) -> Option<crate::manifest::Capability> {
        None
    }
    /// Verdicts the harness accepts for the most recent `parse_event`, from
    /// the manifest. Lets the daemon tell a decider what it may answer
    /// instead of leaving it to guess (Claude Code takes allow/deny on
    /// `UserPromptSubmit` but has no `ask` there). `None` = unconstrained.
    fn verdicts_for(&self, _event: &AgentEvent) -> Option<Vec<crate::manifest::Verdict>> {
        None
    }
    /// How long the HARNESS will wait for this hook before killing it and
    /// discarding whatever we were about to say, from the manifest's install
    /// entry for the most recent `parse_event`.
    ///
    /// This is the only budget that matters: overrun it and the verdict is
    /// not late, it is gone, and the harness applies its own failure
    /// semantics instead of ours. Everything the shim and daemon do for one
    /// event has to fit inside it. `None` = unknown, use internal defaults.
    fn entry_timeout_for(&self, _event: &AgentEvent) -> Option<std::time::Duration> {
        None
    }
    /// Whether this adapter considers the given input a match (for registry detection)
    fn matches_input(&self, input: &RawHookInput) -> bool {
        let hint = normalize_hint(&input.harness_hint);
        let id_norm = normalize_hint(self.id());
        if hint == id_norm {
            return true;
        }
        for alias in self.aliases() {
            if hint == normalize_hint(alias) {
                return true;
            }
            // substring match for convenience: "claude" should match "claude-code"
            if hint.contains(&normalize_hint(alias)) || normalize_hint(alias).contains(&hint) {
                // only if hint non-empty
                if !hint.is_empty() {
                    return true;
                }
            }
        }
        // also check if hint contains id substring
        !hint.is_empty() && (hint.contains(&id_norm) || id_norm.contains(&hint))
    }

    // -----------------------------------------------------------------------
    // Legacy shim — kept while agent-hooks-core daemon calls `parse()`.
    // Prefer `parse_event()` in new code.
    // -----------------------------------------------------------------------

    /// Legacy alias for [`Self::parse_event`].
    fn parse(&self, input: &RawHookInput) -> Result<AgentEvent, AdapterError> {
        self.parse_event(input)
    }
}

pub fn expand_tilde_path(raw: &str) -> std::path::PathBuf {
    if let Some(stripped) = raw.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")) {
            return std::path::PathBuf::from(home).join(stripped);
        }
    } else if raw == "~" {
        if let Ok(home) = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")) {
            return std::path::PathBuf::from(home);
        }
    }
    std::path::PathBuf::from(raw)
}

/// Find git repository root by walking up from `start`.
pub fn find_git_root(start: &std::path::Path) -> Option<std::path::PathBuf> {
    let mut cur = Some(start);
    while let Some(dir) = cur {
        if dir.join(".git").exists() {
            return Some(dir.to_path_buf());
        }
        cur = dir.parent();
    }
    None
}

/// Normalize an id/alias/hint: lower-case, trim, replace `_` and spaces with `-`, remove duplicate `-`
pub fn normalize_hint(raw: &str) -> String {
    let mut s = raw.trim().to_ascii_lowercase().replace(['_', ' '], "-");
    while s.contains("--") {
        s = s.replace("--", "-");
    }
    // remove leading/trailing dash
    s.trim_matches('-').to_string()
}

/// Helper: parse raw_payload as JSON Value, handling empty string and JSON-RPC envelope
pub fn parse_payload(raw: &str) -> Result<serde_json::Value, AdapterError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(serde_json::Value::Object(Default::default()));
    }
    // Try direct parse
    match serde_json::from_str::<serde_json::Value>(trimmed) {
        Ok(v) => {
            // If JSON-RPC envelope, unwrap params as payload but keep method for event extraction
            if v.get("jsonrpc").is_some() && v.get("method").is_some() {
                // keep envelope but caller should handle method; return envelope as-is
                Ok(v)
            } else {
                Ok(v)
            }
        }
        Err(e) => Err(AdapterError::Parse(e.to_string())),
    }
}

/// Extract event name from RawHookInput + payload, checking many possible fields
pub fn resolve_event_name(input: &RawHookInput, payload: &serde_json::Value) -> Option<String> {
    // 1. event_hint takes precedence if non-empty
    if let Some(hint) = &input.event_hint {
        let t = hint.trim();
        if !t.is_empty() {
            return Some(t.to_string());
        }
    }
    // 2. JSON-RPC method
    if let Some(method) = payload.get("method").and_then(|v| v.as_str()) {
        return Some(method.to_string());
    }
    // 3. various payload fields
    let candidates = [
        "hook_event_name",
        "hookEventName",
        "hook_event",
        "hookEvent",
        "event",
        "event_name",
        "eventName",
        "eventType",
        "event_type",
        "type",
        "hook",
        "lifecycle",
        "action",
        "name",
    ];
    for key in candidates {
        if let Some(v) = payload.get(key).and_then(|x| x.as_str()) {
            if !v.trim().is_empty() {
                return Some(v.to_string());
            }
        }
    }
    // nested lookups
    for container in ["payload", "params", "properties", "info", "session", "data"] {
        if let Some(obj) = payload.get(container) {
            for key in candidates {
                if let Some(v) = obj.get(key).and_then(|x| x.as_str()) {
                    if !v.trim().is_empty() {
                        return Some(v.to_string());
                    }
                }
            }
        }
    }
    // dot-notation type like "session.created"
    if let Some(t) = payload.get("type").and_then(|v| v.as_str()) {
        return Some(t.to_string());
    }
    None
}

/// Normalize event for matching: lowercase, strip separators
pub fn normalize_event(s: &str) -> String {
    s.to_ascii_lowercase().replace(['_', '-', '.'], "")
}
pub fn extract_string_field(payload: &serde_json::Value, keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Some(v) = payload.get(*key) {
            if let Some(s) = v.as_str() {
                if !s.is_empty() {
                    return Some(s.to_string());
                }
            } else if v.is_string() {
                // already handled
            }
            // numeric or other -> stringify
            if v.is_number() || v.is_boolean() {
                return Some(v.to_string());
            }
        }
        // nested containers
        for container in [
            "info",
            "session",
            "properties",
            "payload",
            "params",
            "data",
            "tool",
            "context",
        ] {
            if let Some(obj) = payload.get(container) {
                if let Some(v) = obj.get(*key) {
                    if let Some(s) = v.as_str() {
                        if !s.is_empty() {
                            return Some(s.to_string());
                        }
                    }
                    if v.is_number() || v.is_boolean() {
                        return Some(v.to_string());
                    }
                }
            }
        }
    }
    None
}

pub fn extract_session_id(payload: &serde_json::Value, input: &RawHookInput) -> Option<String> {
    // Prefer payload extraction
    let keys = [
        "session_id",
        "sessionId",
        "sessionID",
        "thread_id",
        "threadId",
        "thread-id",
        "conversation_id",
        "conversationId",
        "session",
        "id",
        "session_hash",
    ];
    if let Some(s) = extract_string_field(payload, &keys) {
        // Guard against payload being just random id; ensure not empty and not object JSON
        if s.len() >= 2 {
            return Some(s);
        }
    }
    // Check nested info/session objects more directly
    for container in ["info", "session", "properties", "properties.info", "data"] {
        let part = if container.contains('.') {
            let mut cur = payload;
            let mut found = true;
            for seg in container.split('.') {
                if let Some(n) = cur.get(seg) {
                    cur = n;
                } else {
                    found = false;
                    break;
                }
            }
            if found {
                Some(cur)
            } else {
                None
            }
        } else {
            payload.get(container)
        };
        if let Some(obj) = part {
            for k in ["id", "session_id", "sessionId", "sessionID"] {
                if let Some(v) = obj.get(k).and_then(|x| x.as_str()) {
                    if !v.is_empty() {
                        return Some(v.to_string());
                    }
                }
            }
        }
    }
    // For JSON-RPC envelope, params may hold session
    if let Some(params) = payload.get("params") {
        if let Some(s) = extract_string_field(params, &keys) {
            return Some(s);
        }
        for container in ["info", "session"] {
            if let Some(obj) = params.get(container) {
                for k in ["id", "session_id", "sessionId"] {
                    if let Some(v) = obj.get(k).and_then(|x| x.as_str()) {
                        return Some(v.to_string());
                    }
                }
            }
        }
    }
    // Fallback to request_id if present
    if let Some(rid) = &input.request_id {
        if !rid.is_empty() {
            // Not ideal but better than missing; use rid as session-ish?
            // Only fallback if seems like session
            // We'll not use it; instead return None to trigger MissingSessionId
        }
    }
    // Last resort: if payload has any id field at top level that looks like uuid or non-empty
    if let Some(v) = payload.get("id").and_then(|x| x.as_str()) {
        if !v.is_empty() {
            return Some(v.to_string());
        }
    }
    None
}

pub fn payload_value(payload: &serde_json::Value, keys: &[&str]) -> Option<serde_json::Value> {
    for key in keys {
        if let Some(v) = payload.get(*key) {
            if !v.is_null() {
                return Some(v.clone());
            }
        }
        for container in ["info", "session", "properties", "payload", "params", "data"] {
            if let Some(obj) = payload.get(container) {
                if let Some(v) = obj.get(*key) {
                    if !v.is_null() {
                        return Some(v.clone());
                    }
                }
            }
        }
    }
    // JSON-RPC params
    if let Some(params) = payload.get("params") {
        for key in keys {
            if let Some(v) = params.get(*key) {
                if !v.is_null() {
                    return Some(v.clone());
                }
            }
        }
    }
    None
}
