//! Versioned DTOs for the read-only history query CLI (`agent-hooks history`).
//!
//! The CLI's JSON output is the consumer contract; the SQLite schema is not.

use serde::{Deserialize, Serialize};

/// `api_version` on every history response. Version 2: tool entries carry
/// independent `tool_input` / `tool_output` phases instead of one combined
/// `text`.
pub const HISTORY_API_VERSION: u32 = 2;

/// Where stored messages come from: hook events only, never transcripts.
pub const HISTORY_CAPTURE_SOURCE: &str = "hooks";

/// Assistant entries hold normalized `TurnEnd.assistant_message` or
/// `Stop.summary` text. Upstream may omit or abbreviate it; the store never
/// invents the absent native reply.
pub const HISTORY_ASSISTANT_CONTENT_SOURCE: &str = "assistant_text";

/// Origin used when a hook carries no `agent_hooks.origin` extension.
pub const LOCAL_ORIGIN: &str = "local";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryEntryKind {
    User,
    Assistant,
    Tool,
}

impl HistoryEntryKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Tool => "tool",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryToolStatus {
    Running,
    Completed,
    Failed,
    Unknown,
}

impl HistoryToolStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Unknown => "unknown",
        }
    }
}

/// Native identity of a session: `(origin, harness, session_id)`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct HistorySessionKey {
    pub origin: String,
    pub harness: String,
    pub session_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistorySession {
    #[serde(flatten)]
    pub key: HistorySessionKey,
    /// Ordering position (allocation order of the first observation).
    pub seq: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Last normalized status (`idle`, `running`, ...).
    pub status: String,
    pub first_seen_ms: u64,
    pub last_seen_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistorySessionsPage {
    pub api_version: u32,
    pub items: Vec<HistorySession>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// The newest matching user/assistant message of one session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryMessageHit {
    pub session: HistorySessionKey,
    /// `user` or `assistant`; tool entries are not searched.
    pub kind: HistoryEntryKind,
    /// The message's last matching line, windowed around the match when the
    /// line is long. Derived output: unlike stored payloads it is shortened.
    pub snippet: String,
}

/// `agent-hooks history search` result: one hit per matching session, newest
/// session first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistorySearchPage {
    pub api_version: u32,
    pub items: Vec<HistoryMessageHit>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub seq: i64,
    pub kind: HistoryEntryKind,
    /// Complete original string; never truncated. User and assistant entries
    /// carry their message here. A `tool` entry has `text` only when it was
    /// recorded before API version 2: that text is the opaque historical
    /// tool text as it was stored then, whose input/output boundary was not
    /// recorded and is not reconstructed. New tool observations never use
    /// `text`; they populate `tool_input` / `tool_output`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Exact payload of the tool's running (pre-call) observation, when one
    /// was captured. Only on `tool` entries; never truncated or combined with
    /// `tool_output`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_input: Option<String>,
    /// Exact payload of the tool's terminal (completed/failed) observation,
    /// when one was captured. Only on `tool` entries; never truncated or
    /// combined with `tool_input`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_output: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_message_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_status: Option<HistoryToolStatus>,
    pub first_seen_ms: u64,
    pub last_seen_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryCaptureInfo {
    pub capture_source: String,
    pub assistant_content_source: String,
}

impl Default for HistoryCaptureInfo {
    fn default() -> Self {
        Self {
            capture_source: HISTORY_CAPTURE_SOURCE.to_string(),
            assistant_content_source: HISTORY_ASSISTANT_CONTENT_SOURCE.to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryMessagesPage {
    pub api_version: u32,
    pub session: HistorySessionKey,
    pub capture: HistoryCaptureInfo,
    pub items: Vec<HistoryEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryStatus {
    pub api_version: u32,
    pub schema_version: u32,
    pub sessions: u64,
    pub entries: u64,
    /// Wall-clock ms of the last committed write; not a daemon health signal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_persisted_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryErrorCode {
    HistoryDisabled,
    SchemaUpgradeRequired,
    AmbiguousSession,
    NotFound,
    InvalidArgument,
    /// Store exists but is unreadable, corrupt, or from a newer release.
    StoreUnavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryErrorBody {
    pub code: HistoryErrorCode,
    pub message: String,
    /// Populated for `ambiguous_session`: the matching keys.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub candidates: Vec<HistorySessionKey>,
}

/// Error envelope printed on stdout by `agent-hooks history ...` (exit 1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryErrorResponse {
    pub api_version: u32,
    pub error: HistoryErrorBody,
}
