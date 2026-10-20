use agent_hooks_protocol::{
    AgentEvent, AgentLifecycle, HistoryEntryKind, HistoryToolStatus, NormalizedStatus,
};

/// What the store persists for one hook delivery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capture {
    pub origin: String,
    pub harness: String,
    pub session_id: String,
    pub parent_session_id: Option<String>,
    pub workspace: Option<String>,
    pub title: Option<String>,
    /// `NormalizedStatus` in snake_case.
    pub status: String,
    /// `identity.delivery_id`, else `event.event_id`.
    pub delivery_id: String,
    pub timestamp_ms: u64,
    /// `identity.native_turn_id` only; never a synthesized turn id.
    pub turn_id: Option<String>,
    pub entry: Option<CaptureEntry>,
}

/// One transient observation. `text` carries only this observation's
/// payload: a user/assistant message body, or for tools the input of a
/// `Running` observation or the output/error of a `Completed`/`Failed` one.
/// The writer routes it to the matching column; observations are never
/// concatenated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureEntry {
    pub kind: HistoryEntryKind,
    pub text: Option<String>,
    pub native_message_id: Option<String>,
    pub tool_call_id: Option<String>,
    pub tool_name: Option<String>,
    pub tool_status: Option<HistoryToolStatus>,
}

fn status_str(status: NormalizedStatus) -> &'static str {
    match status {
        NormalizedStatus::Idle => "idle",
        NormalizedStatus::Running => "running",
        NormalizedStatus::WaitingForInput => "waiting_for_input",
        NormalizedStatus::Completed => "completed",
        NormalizedStatus::Failed => "failed",
        NormalizedStatus::Closed => "closed",
    }
}

impl Capture {
    /// Project an event into a capture. `None` when the event has no history
    /// identity or its session id was not native to the harness (a generated
    /// id must never key stored history). `origin` is the
    /// `agent_hooks.origin` extension value or `LOCAL_ORIGIN`.
    pub fn from_event(event: &AgentEvent, origin: &str) -> Option<Capture> {
        let identity = event.history_identity.as_ref()?;
        if !identity.native_session {
            return None;
        }

        let entry = match &event.lifecycle {
            AgentLifecycle::UserPromptSubmit {
                user_prompt: Some(text),
            } if !text.trim().is_empty() => Some(CaptureEntry {
                kind: HistoryEntryKind::User,
                text: Some(text.clone()),
                native_message_id: identity.native_message_id.clone(),
                tool_call_id: None,
                tool_name: None,
                tool_status: None,
            }),
            AgentLifecycle::Stop {
                summary: Some(text),
                ..
            }
            | AgentLifecycle::TurnEnd {
                assistant_message: Some(text),
                ..
            } if !text.trim().is_empty() => Some(CaptureEntry {
                kind: HistoryEntryKind::Assistant,
                text: Some(text.clone()),
                native_message_id: identity.native_message_id.clone(),
                tool_call_id: None,
                tool_name: None,
                tool_status: None,
            }),
            AgentLifecycle::PreToolUse {
                tool_name, input, ..
            } => Some(tool_entry(
                tool_name,
                identity.native_tool_call_id.as_ref(),
                HistoryToolStatus::Running,
                normalize_tool_input(input),
            )),
            AgentLifecycle::PostToolUse {
                tool_name,
                output_preview,
                ..
            } => Some(tool_entry(
                tool_name,
                identity.native_tool_call_id.as_ref(),
                HistoryToolStatus::Completed,
                normalize_tool_output(output_preview.as_deref()),
            )),
            AgentLifecycle::PostToolUseFailure {
                tool_name, error, ..
            } => Some(tool_entry(
                tool_name,
                identity.native_tool_call_id.as_ref(),
                HistoryToolStatus::Failed,
                normalize_tool_output(Some(error)),
            )),
            _ => None,
        };

        Some(Capture {
            origin: origin.to_string(),
            harness: event.harness.clone(),
            session_id: event.session_id.clone(),
            parent_session_id: event.parent_session_id.clone(),
            workspace: event
                .context
                .cwd
                .as_ref()
                .map(|cwd| cwd.to_string_lossy().into_owned()),
            title: event.session_title.clone(),
            status: status_str(event.status).to_string(),
            delivery_id: identity
                .delivery_id
                .clone()
                .unwrap_or_else(|| event.event_id.clone()),
            timestamp_ms: event.timestamp_ms,
            turn_id: identity.native_turn_id.clone(),
            entry,
        })
    }
}

fn tool_entry(
    tool_name: &str,
    tool_call_id: Option<&String>,
    status: HistoryToolStatus,
    text: Option<String>,
) -> CaptureEntry {
    CaptureEntry {
        kind: HistoryEntryKind::Tool,
        text,
        native_message_id: None,
        tool_call_id: tool_call_id.cloned(),
        tool_name: Some(tool_name.to_string()),
        tool_status: Some(status),
    }
}

/// Normalized tool input: compact JSON, so every harness stores the same
/// shape. `None` when the harness sent no input (stays a bare marker row).
fn normalize_tool_input(input: &serde_json::Value) -> Option<String> {
    if input.is_null() {
        None
    } else {
        Some(input.to_string())
    }
}

/// Normalized tool output/error: verbatim, dropped when blank like
/// user/assistant text.
fn normalize_tool_output(output: Option<&str>) -> Option<String> {
    output.filter(|s| !s.trim().is_empty()).map(str::to_owned)
}
