use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::context::EventContext;
use crate::lifecycle::{AgentLifecycle, NormalizedStatus};

/// Raw hook input received from an agent harness CLI hook.
///
/// Wire tolerance: unknown JSON fields are ignored (serde default), so newer
/// shims may add fields without breaking older CLIs — never drop on unknown.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RawHookInput {
    /// Agent identifier passed by hook (e.g. "claude", "openai-codex", "gemini-cli")
    pub harness_hint: String,

    /// Event name hint passed by hook args (e.g. "--event Stop" or "Notification")
    #[serde(default)]
    pub event_hint: Option<String>,

    /// Raw JSON or text payload passed via stdin or argv
    #[serde(default)]
    pub raw_payload: String,

    /// Captured context at invocation time (minimal, harness-agnostic).
    /// Heavy terminal/mux resolution is the consumer's responsibility
    /// via `EventEnricher` / `extensions` — see `agent-hooks-core::enricher`.
    /// Wire tolerance: a payload without `context` fills `EventContext::default`
    /// instead of erroring (older/minimal shims omit it) — never drop on missing.
    #[serde(default)]
    pub context: EventContext,

    /// Unique request ID for request-reply prompt cycles
    #[serde(default)]
    pub request_id: Option<String>,

    /// Opaque passthrough of selected env vars at hook time (e.g. TERM_PROGRAM,
    /// KITTY_WINDOW_ID, WEZTERM_PANE, GHOSTTY_RESOURCES_DIR, CMUX_*, HERDR_*,
    /// ZELLIJ_*, ITERM_SESSION_ID). Never interpreted by agent-hooks; forwarded
    /// so consumers (e.g. Back2Vibing) can resolve terminal/mux targets
    /// without spawning work in the <2ms shim.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub passthrough_env: HashMap<String, String>,
}

/// Extension key carrying the install-time `--origin` label (see
/// `agent-hooks install --origin`). A consumer with several remotes uses it to
/// tell which one an event came from.
pub const ORIGIN_EXTENSION: &str = "agent_hooks.origin";

/// Extension key carrying the hook occurrence's id, stamped by the shim from
/// its own locally-parsed `event_id`. A daemon recording a forwarded event
/// uses it as the history delivery id, so a retried send is stored once.
/// Consumed by the daemon; never reaches subscribers. Reserved: never set by
/// harness payloads.
pub const DELIVERY_ID_EXTENSION: &str = "agent_hooks.delivery_id";

/// Longest accepted delivery id. Anything longer is not a UUID-like token and
/// is ignored rather than stored.
pub const MAX_DELIVERY_ID_LEN: usize = 128;

/// Native identity facts for the opt-in history store. Captured by the
/// adapter *before* synthetic fallbacks (random session/turn ids) are
/// applied, so the store can tell what the harness really said.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct HistoryIdentity {
    /// The harness payload carried a session id. `false` means `session_id`
    /// is a generated ephemeral UUID that must never key stored history.
    #[serde(default)]
    pub native_session: bool,
    /// Turn id exactly as the harness sent it (no `event_id` fallback).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_turn_id: Option<String>,
    /// Harness-native message id, only from manifest-declared extraction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_message_id: Option<String>,
    /// Harness-native tool call id, only from manifest-declared extraction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_tool_call_id: Option<String>,
    /// Validated `agent_hooks.delivery_id` from transport metadata (daemon).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery_id: Option<String>,
}

/// Delivery id carried on a parsed hook line, if present and sane.
pub fn hook_line_delivery_id(
    extensions: &serde_json::Map<String, serde_json::Value>,
) -> Option<String> {
    extensions
        .get(DELIVERY_ID_EXTENSION)
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty() && s.len() <= MAX_DELIVERY_ID_LEN)
        .map(str::to_string)
}

/// Top-level key on the shim -> daemon hook line that carries shim-side
/// extensions (e.g. `hook --context-cmd` output). It rides next to the
/// `RawHookInput` fields instead of inside the struct so every existing
/// `RawHookInput { .. }` literal and older daemon keeps working: older
/// daemons ignore the unknown key, newer ones merge it into
/// `AgentEvent.extensions` before the `EventEnricher` runs.
pub const HOOK_LINE_EXTENSIONS_KEY: &str = "extensions";

/// Serialize a hook line for the daemon, attaching `extensions` under
/// [`HOOK_LINE_EXTENSIONS_KEY`] when non-empty. With no extensions the line
/// is byte-identical to `serde_json::to_string(input)`.
pub fn hook_line_with_extensions(
    input: &RawHookInput,
    extensions: &serde_json::Map<String, serde_json::Value>,
) -> serde_json::Result<String> {
    if extensions.is_empty() {
        return serde_json::to_string(input);
    }
    let mut value = serde_json::to_value(input)?;
    if let Some(obj) = value.as_object_mut() {
        obj.insert(
            HOOK_LINE_EXTENSIONS_KEY.to_string(),
            serde_json::Value::Object(extensions.clone()),
        );
    }
    serde_json::to_string(&value)
}

/// Shim-side extensions carried on a parsed hook line, if any. Non-object
/// values are ignored (a malformed sender never breaks the event).
pub fn hook_line_extensions(
    line: &serde_json::Value,
) -> Option<&serde_json::Map<String, serde_json::Value>> {
    line.get(HOOK_LINE_EXTENSIONS_KEY)
        .and_then(|v| v.as_object())
        .filter(|m| !m.is_empty())
}

/// Agent event: one harness hook call, normalized to a shared shape.
///
/// Wire tolerance: unknown JSON fields are ignored (serde default) so schema
/// evolution is additive-only — newer senders may add fields without breaking
/// older readers. Never drop a connection/event on unknown or missing optional
/// data; required identity fields (`event_id`, `harness`, `session_id`,
/// `lifecycle`) still error loudly when absent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentEvent {
    /// Protocol schema version (currently 1).
    ///
    /// Compatibility rule: additive fields only, readers ignore unknown.
    /// Events with an older or newer `schema_version` still parse — consumers
    /// warn on mismatch but never drop the event/connection. A missing version
    /// (pre-versioned sender) fills the current version instead of erroring.
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,

    /// Unique event ID (UUID v4)
    pub event_id: String,

    /// Timestamp in epoch milliseconds (UTC).
    /// Wire tolerance: missing fills 0 (unknown time), never an error.
    #[serde(default)]
    pub timestamp_ms: u64,

    /// Normalized harness identifier (e.g. "claude-code", "codex", "gemini-cli")
    pub harness: String,

    /// Detected harness version if known (e.g. "0.2.29")
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_version: Option<String>,

    /// Root session identifier
    pub session_id: String,

    /// Parent session identifier if this event belongs to a subagent
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,

    /// Turn identifier within the session if provided by harness
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,

    /// Session title the harness itself pushed (pi/omp `getSessionName`,
    /// opencode `session.updated` info.title). Absent = no title known;
    /// consumers keep whatever title they had rather than clearing it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_title: Option<String>,

    /// Optional display text from the native hook, bounded to 300 characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flavor_text: Option<String>,

    /// Contextual metadata (TTY, CWD, Tmux, Git, SSH) — minimal baseline.
    /// Consumers needing precise terminal/mux targets should use
    /// `extensions` populated by their `EventEnricher`.
    /// Wire tolerance: missing fills `EventContext::default`, never an error.
    #[serde(default)]
    pub context: EventContext,

    /// Normalized lifecycle event
    pub lifecycle: AgentLifecycle,

    /// Derived status of the session.
    /// Wire tolerance: missing fills `Running` (safe non-terminal busy signal)
    /// instead of erroring — an older sender may omit it since it is derivable
    /// from `lifecycle`.
    #[serde(default = "default_status")]
    pub status: NormalizedStatus,

    /// Raw event JSON for backwards compatibility or debugging
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_event: Option<String>,

    /// Set by the daemon on a delivered event when the agent is BLOCKED on
    /// this event awaiting a verdict. Present = you are being asked, answer
    /// it. Absent = observation, do not answer.
    ///
    /// The daemon stamps this because only the daemon knows: a decider that
    /// re-derived it from capability tables and scope rules would be a second
    /// implementation of the hold rule, free to drift from the first. Never
    /// set on the shim->daemon leg.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hold: Option<crate::rpc::PromptHold>,

    /// Explicit consumer signal mirroring `hold`: `true` = this event is
    /// being held for a verdict, answer it via `prompt.respond` with a
    /// [`crate::prompt::PromptDecision`] (include `answers[]` when
    /// `prompt.kind` is `question`; `custom_input` carries the deny reason;
    /// `ask` needs an `ask` entry in `hold.verdicts`). `false` = observation,
    /// do not answer. Stamped by the daemon alongside `hold`; never set on
    /// the shim->daemon leg. Wire tolerance: missing fills `false`.
    #[serde(default)]
    pub answerable: bool,

    /// Opaque passthrough env snapshot from shim time. Always forwarded
    /// even when no `EventEnricher` is registered, so out-of-process
    /// subscribers (e.g. Back2Vibing via `subscribe`) can do client-side
    /// enrichment without a daemon-side callback. The daemon never
    /// interprets this map.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub passthrough_env: HashMap<String, String>,

    /// Consumer-populated enrichment (e.g. Back2Vibing's `terminal_target`,
    /// `mux_target`, `ssh_forward`, `bundle_id`). Empty when no enricher
    /// is registered. Consumers may also do client-side enrichment after
    /// subscribing — this field is the in-daemon rendezvous for
    /// in-process embeddings.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub extensions: HashMap<String, serde_json::Value>,

    /// Native identity facts for the opt-in history store. Absent on events
    /// from older senders; never interpreted by live consumers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_identity: Option<HistoryIdentity>,
}

/// Wire tolerance: fills the current schema version when an inbound event
/// predates versioning — a missing version is not a corrupt event.
fn default_schema_version() -> u32 {
    AgentEvent::SCHEMA_VERSION
}

/// Wire tolerance: fills a safe non-terminal status when an inbound event omits
/// it — `status` is derivable from `lifecycle`, so its absence never drops.
fn default_status() -> NormalizedStatus {
    NormalizedStatus::Running
}

impl AgentEvent {
    pub const SCHEMA_VERSION: u32 = 1;

    pub fn new(
        harness: impl Into<String>,
        session_id: impl Into<String>,
        lifecycle: AgentLifecycle,
        context: EventContext,
    ) -> Self {
        let status = lifecycle.derived_status();
        let now_ms = chrono::Utc::now().timestamp_millis() as u64;

        Self {
            schema_version: Self::SCHEMA_VERSION,
            event_id: uuid::Uuid::new_v4().to_string(),
            timestamp_ms: now_ms,
            harness: harness.into(),
            harness_version: None,
            session_id: session_id.into(),
            parent_session_id: None,
            turn_id: None,
            session_title: None,
            flavor_text: None,
            context,
            status,
            lifecycle,
            raw_event: None,
            hold: None,
            answerable: false,
            passthrough_env: Default::default(),
            extensions: Default::default(),
            history_identity: None,
        }
    }

    /// The question a human is already being asked on this event, if any.
    ///
    /// This is the signal that distinguishes a *natural* hold from an
    /// introduced one. A permission request or a question means the harness
    /// has already stopped and is waiting on a person — intercepting it costs
    /// nothing and only widens who may answer. Every other event is
    /// mid-flight: nobody is waiting, so holding it manufactures a stall that
    /// would not otherwise exist.
    pub fn prompt_request(&self) -> Option<&crate::prompt::PromptRequest> {
        match &self.lifecycle {
            crate::lifecycle::AgentLifecycle::PermissionRequest { prompt } => Some(prompt),
            crate::lifecycle::AgentLifecycle::PreToolUse { prompt, .. } => prompt.as_ref(),
            _ => None,
        }
    }

    /// Whether the harness is already blocked on a human for this event.
    pub fn is_interactive(&self) -> bool {
        self.prompt_request().is_some()
    }
}

/// Truncate `s` to `n` chars, appending `...[truncated]` when cut.
pub fn cap_chars(s: &str, n: usize) -> String {
    if s.chars().count() > n {
        s.chars().take(n).collect::<String>() + "...[truncated]"
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod answerable_tests {
    use super::*;

    #[test]
    fn fresh_events_are_not_answerable_and_old_json_stays_valid() {
        let e = AgentEvent::new(
            "muse",
            "s1",
            AgentLifecycle::ToolUseStart,
            EventContext::default(),
        );
        assert!(!e.answerable);
        assert!(e.hold.is_none());
        // Missing field fills false: older senders still parse.
        let v: serde_json::Value = serde_json::from_str(
            r#"{"schema_version":1,"event_id":"e","timestamp_ms":0,"harness":"muse","session_id":"s","lifecycle":{"type":"ToolUseStart"},"status":"running"}"#,
        )
        .unwrap();
        let back: AgentEvent = serde_json::from_value(v).unwrap();
        assert!(!back.answerable);
    }
}

#[cfg(test)]
mod hook_line_tests {
    use super::*;

    fn input() -> RawHookInput {
        RawHookInput {
            harness_hint: "claude".into(),
            event_hint: Some("Stop".into()),
            raw_payload: "{}".into(),
            context: Default::default(),
            request_id: None,
            passthrough_env: Default::default(),
        }
    }

    #[test]
    fn empty_extensions_keep_the_line_byte_identical() {
        let line = hook_line_with_extensions(&input(), &Default::default()).unwrap();
        assert_eq!(line, serde_json::to_string(&input()).unwrap());
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert!(hook_line_extensions(&v).is_none());
    }

    #[test]
    fn extensions_round_trip_and_line_still_parses_as_raw_input() {
        let mut ext = serde_json::Map::new();
        ext.insert(
            "b2v.runtime_context".into(),
            serde_json::json!({"tty": "/dev/ttys001"}),
        );
        ext.insert("b2v.disabled".into(), serde_json::json!(false));
        let line = hook_line_with_extensions(&input(), &ext).unwrap();
        // Older daemons parse the same line as a plain RawHookInput.
        let raw: RawHookInput = serde_json::from_str(&line).unwrap();
        assert_eq!(raw.harness_hint, "claude");
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(hook_line_extensions(&v), Some(&ext));
    }

    #[test]
    fn non_object_extensions_are_ignored() {
        let v = serde_json::json!({"harness_hint": "x", "extensions": "nope"});
        assert!(hook_line_extensions(&v).is_none());
    }
}

#[cfg(test)]
mod session_title_tests {
    use super::*;

    fn event() -> AgentEvent {
        AgentEvent::new(
            "pi",
            "s1",
            AgentLifecycle::TurnStart { turn_index: None },
            Default::default(),
        )
    }

    // Plugin-pushed title (legacy b2v-pi.ts sessionTitle) rides the typed event.
    #[test]
    fn session_title_round_trips() {
        let mut ev = event();
        ev.session_title = Some("Fix login flow".into());
        let json = serde_json::to_string(&ev).unwrap();
        let back: AgentEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back.session_title.as_deref(), Some("Fix login flow"));
    }

    // Additive field: absent on the wire when unknown, and older senders parse.
    #[test]
    fn session_title_absent_is_skipped_and_defaults_none() {
        let json = serde_json::to_string(&event()).unwrap();
        assert!(!json.contains("session_title"));
        let back: AgentEvent = serde_json::from_str(&json).unwrap();
        assert!(back.session_title.is_none());
    }
}
