use serde::{Deserialize, Serialize};

/// Type of interactive prompt requiring human attention or action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptKind {
    /// Permission gate (e.g. bash execution, file modification, web search)
    Permission,
    /// Agent asking a question with possible predefined options or free text
    Question,
}

/// An option in a multiple choice question prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptOption {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
}

/// Single question inside a Question prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptQuestion {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
    pub question: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<PromptOption>,
    #[serde(default)]
    pub multiple: bool,
    #[serde(default)]
    pub custom: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recommended: Option<usize>,
}

/// What accepting a [`PromptSuggestion`] does beyond approving this one call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SuggestionKind {
    /// Remember an allow/deny rule for matching calls.
    Rule,
    /// Switch the harness's permission mode (e.g. auto, accept-edits).
    Mode,
    /// Grant access to extra directories.
    Directory,
    /// A host suggestion this protocol has no category for.
    Other,
}

/// A "remember this" choice the host offered alongside a permission gate.
///
/// Normalised across harnesses: `label` is display-ready, and the host's own
/// payload stays inside the adapter. Select one with
/// [`PromptDecision::allow_with_suggestion`], using its index in
/// [`PromptRequest::suggestions`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptSuggestion {
    pub label: String,
    pub kind: SuggestionKind,
}

/// A structured interactive prompt request from the agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptRequest {
    /// Unique identifier for this prompt (used to route responses)
    pub prompt_id: String,
    pub kind: PromptKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub questions: Vec<PromptQuestion>,
    /// Host-offered "remember this" choices, in the host's order. Empty when
    /// the harness offers none or cannot apply them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub suggestions: Vec<PromptSuggestion>,
}

/// User's answer to a Question prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptAnswer {
    pub question_id: String,
    #[serde(default)]
    pub selected_options: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_input: Option<String>,
}

/// Human decision sent back to the agent hook.
///
/// Precedence when merging sources is deny > ask > allow > defer: an explicit veto
/// always wins, and an escalation beats a default-allow. `ask` means "pause
/// and route this to a human/another agent": the `custom_input` carries the
/// question, and the holder (native harness dialog or daemon mediator) waits
/// for the verdict instead of deciding now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptDecision {
    pub allow: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_input: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub answers: Vec<PromptAnswer>,
    /// Escalate instead of deciding. Additive field: older JSON without it
    /// parses as `false`, preserving wire compatibility.
    #[serde(default)]
    pub ask: bool,
    /// Grant beyond this one call — the host remembers the approval.
    ///
    /// Distinct from `allow`, which approves exactly this request. Several
    /// harnesses offer it natively (opencode's `reply: "always"`, pi/omp's
    /// `allow_always`) and a decider that can only say `allow` has to
    /// re-approve the same action forever, one call at a time. Additive:
    /// absent parses as `false`, and a host without the concept renders it
    /// as a plain allow.
    #[serde(default)]
    pub allow_always: bool,
    /// With `allow_always`: apply only this entry of
    /// [`PromptRequest::suggestions`]. `None` applies every suggestion the host
    /// offered (the behaviour before suggestions were selectable); an index
    /// the host no longer has degrades to a plain allow.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggestion: Option<usize>,
    /// No opinion: the harness proceeds exactly as if no hook were installed
    /// (its native permission dialog, its own policy). Distinct from `allow`,
    /// which on hosts like Claude Code's PreToolUse *bypasses* the host's
    /// permission checks. A decider that won't answer a hold defers; so does
    /// the daemon for gates nobody claimed. Additive: absent parses as `false`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub defer: bool,
    /// Text for the harness to put in the model's context, independent of the
    /// verdict. Rendered through the adapter's `inject` template for events
    /// that declare one, dropped silently elsewhere, and never sent with a
    /// deny or ask. Additive: absent parses as `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub additional_context: Option<String>,
}

impl PromptDecision {
    pub fn allow() -> Self {
        Self {
            allow: true,
            custom_input: None,
            answers: Vec::new(),
            ask: false,
            allow_always: false,
            suggestion: None,
            defer: false,
            additional_context: None,
        }
    }

    /// Approve this call and every later one like it, where the host can
    /// remember. Falls back to a plain allow where it cannot.
    pub fn allow_always() -> Self {
        Self {
            allow_always: true,
            suggestion: None,
            ..Self::allow()
        }
    }

    /// Approve this call and apply one of the host's offered suggestions.
    pub fn allow_with_suggestion(index: usize) -> Self {
        Self {
            suggestion: Some(index),
            ..Self::allow_always()
        }
    }

    pub fn deny(reason: Option<String>) -> Self {
        Self {
            allow: false,
            custom_input: reason,
            answers: Vec::new(),
            ask: false,
            allow_always: false,
            suggestion: None,
            defer: false,
            additional_context: None,
        }
    }

    /// Pause and ask: `custom_input` is the question for the decider.
    pub fn ask(question: Option<String>) -> Self {
        Self {
            allow: false,
            ask: true,
            custom_input: question,
            answers: Vec::new(),
            allow_always: false,
            suggestion: None,
            defer: false,
            additional_context: None,
        }
    }

    /// Hand the call back to the harness's own handling (see `defer`).
    pub fn defer() -> Self {
        Self {
            allow: false,
            ask: false,
            custom_input: None,
            answers: Vec::new(),
            allow_always: false,
            suggestion: None,
            defer: true,
            additional_context: None,
        }
    }

    /// Attach context for the model (see `additional_context`).
    pub fn with_additional_context(mut self, context: impl Into<String>) -> Self {
        let context = context.into();
        self.additional_context = (!context.trim().is_empty()).then_some(context);
        self
    }

    /// An explicit veto: neither allow, ask nor defer.
    pub fn is_deny(&self) -> bool {
        !self.allow && !self.ask && !self.defer
    }
}

/// Complete response object sent across RPC to resolve a prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptResponse {
    pub prompt_id: String,
    pub decision: PromptDecision,
}

/// Snapshot of one held question for `prompt.list` (backlog UI).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingPromptInfo {
    pub prompt_id: String,
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question: Option<String>,
    #[serde(default)]
    pub waiters: usize,
    #[serde(default)]
    pub parked: usize,
    #[serde(default)]
    pub age_ms: u64,
    /// Full question payload (options, multi-select, free-text) for rich UIs.
    /// None for holds registered without metadata (older shims) or plain
    /// tool gates with no questions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<PromptRequest>,
}
