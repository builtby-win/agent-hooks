use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::AdapterError;

// ---------------------------------------------------------------------------
// ResponseMode
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ResponseMode {
    #[default]
    Json,
    #[serde(rename = "exit2")]
    Exit2,
}

// ---------------------------------------------------------------------------
// Capability
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    Gate,
    Observe,
    Continue,
}

// ---------------------------------------------------------------------------
// LifecycleRef — case-insensitive, separator-insensitive mapping
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum LifecycleRef {
    SessionStart,
    SessionEnd,
    UserPromptSubmit,
    PreToolUse,
    PostToolUse,
    PostToolUseFailure,
    PermissionRequest,
    Notification,
    SubagentStart,
    SubagentStop,
    TurnStart,
    TurnEnd,
    Stop,
    PreCompact,
    PostCompact,
    Interrupt,
    PostToolBatch,
    PreLLMCall,
    PostLLMCall,
    StopFailure,
    ToolUseStart,
}

impl LifecycleRef {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::SessionStart => "SessionStart",
            Self::SessionEnd => "SessionEnd",
            Self::UserPromptSubmit => "UserPromptSubmit",
            Self::PreToolUse => "PreToolUse",
            Self::PostToolUse => "PostToolUse",
            Self::PostToolUseFailure => "PostToolUseFailure",
            Self::PermissionRequest => "PermissionRequest",
            Self::Notification => "Notification",
            Self::SubagentStart => "SubagentStart",
            Self::SubagentStop => "SubagentStop",
            Self::TurnStart => "TurnStart",
            Self::TurnEnd => "TurnEnd",
            Self::Stop => "Stop",
            Self::PreCompact => "PreCompact",
            Self::PostCompact => "PostCompact",
            Self::Interrupt => "Interrupt",
            Self::PostToolBatch => "PostToolBatch",
            Self::PreLLMCall => "PreLLMCall",
            Self::PostLLMCall => "PostLLMCall",
            Self::StopFailure => "StopFailure",
            Self::ToolUseStart => "ToolUseStart",
        }
    }
    fn from_normalized(norm: &str) -> Option<Self> {
        match norm {
            "sessionstart" => Some(Self::SessionStart),
            "sessionend" => Some(Self::SessionEnd),
            "userpromptsubmit" => Some(Self::UserPromptSubmit),
            "pretooluse" => Some(Self::PreToolUse),
            "posttooluse" => Some(Self::PostToolUse),
            "posttoolusefailure" => Some(Self::PostToolUseFailure),
            "permissionrequest" => Some(Self::PermissionRequest),
            "notification" => Some(Self::Notification),
            "subagentstart" => Some(Self::SubagentStart),
            "subagentstop" => Some(Self::SubagentStop),
            "turnstart" => Some(Self::TurnStart),
            "turnend" => Some(Self::TurnEnd),
            "stop" => Some(Self::Stop),
            "precompact" => Some(Self::PreCompact),
            "postcompact" => Some(Self::PostCompact),
            "interrupt" => Some(Self::Interrupt),
            "posttoolbatch" => Some(Self::PostToolBatch),
            "prellmcall" => Some(Self::PreLLMCall),
            "postllmcall" => Some(Self::PostLLMCall),
            "stopfailure" => Some(Self::StopFailure),
            "toolusestart" => Some(Self::ToolUseStart),
            _ => None,
        }
    }
}

impl<'de> Deserialize<'de> for LifecycleRef {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        let norm = s.to_ascii_lowercase().replace(['_', '-', '.'], "");
        LifecycleRef::from_normalized(&norm)
            .ok_or_else(|| serde::de::Error::custom(format!("unknown lifecycle: {}", s)))
    }
}

// ---------------------------------------------------------------------------
// EntryShape / EditMode
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryShape {
    Flat,
    MatcherGroup,
    Named,
    /// The event key's value is the bare hook command string
    /// (`hooks.<event> = "<cmd>"`): one command per event, e.g. jcode.
    /// `toml_hooks` only.
    Command,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EditMode {
    AppendUnique,
    Set,
    Remove,
}

// ---------------------------------------------------------------------------
// Transcript extraction
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptExtract {
    FirstPrompt,
    LatestPrompt,
    LatestAssistant,
    FirstTimestamp,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct TranscriptRef {
    pub transcript: TranscriptExtract,
}

impl Default for TranscriptExtract {
    fn default() -> Self {
        Self::FirstPrompt
    }
}

/// Resolve a child session's parent from the harness's own on-disk session
/// store, for harnesses whose hook payloads carry no parent marker (Muse).
/// `root` holds `YYYY/MM/DD/<session-id>/session.jsonl`; a session with its
/// own directory is top-level (no parent), otherwise the parent is the
/// session whose log records `child_key` = this session's id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionLinkSpec {
    pub root: String,
    /// Payload key holding the (child) session id to look up.
    pub child_from: String,
    /// JSON key in the parent's log whose value equals the child session id.
    pub link_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionLinkRef {
    pub session_link: SessionLinkSpec,
}

// ---------------------------------------------------------------------------
// ExtractionSource
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ExtractionSource {
    Path(String),
    Transcript(TranscriptRef),
    SessionLink(SessionLinkRef),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ExtractionSpec {
    #[serde(default)]
    pub session_id: Vec<ExtractionSource>,
    /// Parent session id for subagent/child sessions. Passthrough only:
    /// set when the harness payload carries it, else None (never UUID).
    #[serde(default)]
    pub parent_session_id: Vec<ExtractionSource>,
    #[serde(default)]
    pub prompt: Vec<ExtractionSource>,
    #[serde(default)]
    pub tool_name: Vec<ExtractionSource>,
    #[serde(default)]
    pub tool_input: Vec<ExtractionSource>,
    #[serde(default)]
    pub tool_output: Vec<ExtractionSource>,
    #[serde(default)]
    pub error: Vec<ExtractionSource>,
    #[serde(default)]
    pub success: Vec<ExtractionSource>,
    /// Workspace cwd sources. Payload wins over shim process cwd (harnesses
    /// like Antigravity spawn hooks with cwd pointed at their config dir).
    /// Array values take the first string element (e.g. `workspacePaths`).
    #[serde(default)]
    pub cwd: Vec<ExtractionSource>,
    #[serde(default)]
    pub summary: Option<Vec<ExtractionSource>>,
    /// Session title pushed by plugin harnesses (pi/omp/opencode). Passthrough
    /// only: set when the payload carries a non-empty title, else None.
    #[serde(default)]
    pub session_title: Vec<ExtractionSource>,
    /// Optional display text; only payload paths are read, never session stores.
    #[serde(default)]
    pub flavor_text: Vec<ExtractionSource>,
    /// Harness-native message id for the opt-in history store. Only declared
    /// or conventional key names are read; absent means "no native id".
    #[serde(default)]
    pub message_id: Vec<ExtractionSource>,
    /// Harness-native tool call id for the opt-in history store, used to pair
    /// a tool's start and end markers. Absent means "no native id".
    #[serde(default)]
    pub tool_call_id: Vec<ExtractionSource>,
}

impl ExtractionSpec {
    /// Return the field's explicit sources combined with universal default conventions.
    /// If explicit sources are provided, they are prioritized first, followed by
    /// standard ecosystem field name variations (e.g. camelCase, snake_case).
    pub fn sources_with_defaults(&self, field: &str) -> Vec<ExtractionSource> {
        let explicit = match field {
            "session_id" => &self.session_id,
            "parent_session_id" => &self.parent_session_id,
            "prompt" => &self.prompt,
            "tool_name" => &self.tool_name,
            "tool_input" => &self.tool_input,
            "tool_output" => &self.tool_output,
            "error" => &self.error,
            "success" => &self.success,
            "cwd" => &self.cwd,
            "session_title" => &self.session_title,
            "flavor_text" => &self.flavor_text,
            "message_id" => &self.message_id,
            "tool_call_id" => &self.tool_call_id,
            "summary" => {
                return self
                    .summary
                    .clone()
                    .unwrap_or_else(|| Self::standard_defaults("summary"))
            }
            _ => return Vec::new(),
        };

        let mut out = explicit.clone();
        for default_src in Self::standard_defaults(field) {
            if !out.contains(&default_src) {
                out.push(default_src);
            }
        }
        out
    }

    fn standard_defaults(field: &str) -> Vec<ExtractionSource> {
        let paths: &[&str] = match field {
            "session_id" => &[
                "session_id",
                "sessionId",
                "sessionID",
                "conversation_id",
                "conversationId",
                "thread_id",
                "threadId",
            ],
            "parent_session_id" => &[
                "parent_session_id",
                "parentSessionId",
                "parentSessionID",
                "parent_id",
                "parentId",
                "parentID",
                "parent_session_hash",
                "parentSessionHash",
                "session.parent_id",
                "session.parent_session_id",
                "session.parentSessionId",
                "session.parent_session_hash",
                "properties.session.parent_id",
                "properties.session.parent_session_id",
            ],
            "prompt" => &[
                "prompt",
                "user_prompt",
                "userPrompt",
                "message",
                "input",
                "content",
            ],
            "tool_name" => &["tool_name", "toolName", "tool", "name"],
            "tool_input" => &[
                "tool_input",
                "toolInput",
                "input",
                "params",
                "arguments",
                "args",
            ],
            "tool_output" => &[
                "tool_output",
                "toolOutput",
                "tool_response",
                "output",
                "result",
                "content",
            ],
            "error" => &["error", "error_message", "tool_error", "failure"],
            "success" => &["success"],
            "cwd" => &["cwd", "workspacePaths", "workspace_roots"],
            "session_title" => &["session_title", "sessionTitle"],
            "flavor_text" => &["flavor_text", "flavorText", "display_text", "displayText"],
            "message_id" => &["message_id", "messageId"],
            // Dotted `toolCall.id`/`tool_call.id` name the harness's own tool container
            // (Antigravity, Cursor); they are exact paths, never a recursive id search.
            "tool_call_id" => &[
                "tool_use_id",
                "toolUseId",
                "toolUseID",
                "tool_call_id",
                "toolCallId",
                "toolCallID",
                "call_id",
                "callId",
                "callID",
                "toolCall.id",
                "tool_call.id",
            ],
            "summary" => &[
                "summary",
                "message",
                "reason",
                "result",
                "notification",
                "text",
            ],
            _ => &[],
        };
        paths
            .iter()
            .map(|p| ExtractionSource::Path(p.to_string()))
            .collect()
    }
}

impl Default for ExtractionSpec {
    fn default() -> Self {
        Self {
            session_id: Vec::new(),
            parent_session_id: Vec::new(),
            prompt: Vec::new(),
            tool_name: Vec::new(),
            tool_input: Vec::new(),
            tool_output: Vec::new(),
            error: Vec::new(),
            success: Vec::new(),
            cwd: Vec::new(),
            summary: None,
            session_title: Vec::new(),
            flavor_text: Vec::new(),
            message_id: Vec::new(),
            tool_call_id: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// ResponseSpec
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GateResponse {
    pub allow: serde_json::Value,
    pub deny: serde_json::Value,
    /// Escalation template; `{reason}` is the question. Absent → deny
    /// fallback (fail-safe: cannot escalate where the host has no ask
    /// dialect — e.g. Cursor treats ask as deny, Codex ignores it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ask: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContinueResponse {
    #[serde(rename = "continue")]
    pub continue_: serde_json::Value,
    pub stop: serde_json::Value,
}

fn default_response_mode() -> ResponseMode {
    ResponseMode::Json
}
fn default_empty_on_undecided() -> bool {
    false
}

/// How answered questions are rendered back to the host for PreToolUse gates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuestionAnswerStrategy {
    /// Render formatted answers as blockquote reason in a deny response
    /// (Codex native format: `> question\n\nanswer`).
    DenyBlockReason,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ResponseSpec {
    #[serde(default = "default_response_mode")]
    pub mode: ResponseMode,
    #[serde(default)]
    pub gate: Option<GateResponse>,
    #[serde(default)]
    pub observe: Option<serde_json::Value>,
    #[serde(default, rename = "continue")]
    pub continue_response: Option<ContinueResponse>,
    #[serde(default)]
    pub response_overrides: std::collections::BTreeMap<String, GateResponse>,
    #[serde(default = "default_empty_on_undecided")]
    pub empty_on_undecided: bool,
    /// Hosts such as Muse require `updatedInput` even for an unchanged
    /// PreToolUse approval. Use the original tool input for `{updated_input}`
    /// when the decision supplies no structured question answers.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub allow_requires_tool_input: bool,
    /// Strategy for rendering answered questions back to the agent
    /// (e.g. Codex requires answers to be injected into `permissionDecisionReason`
    /// of a deny verdict because it has no in-band updatedInput channel).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question_answer_strategy: Option<QuestionAnswerStrategy>,
    /// Per-lifecycle output for a `defer` decision; absent keys fall back to
    /// `observe`. Lets a host whose status hooks expect an explicit allow
    /// (legacy B2V gemini/cursor `{"decision":"allow"}`) keep tool gates at
    /// "no decision" so the host's own confirmation still runs.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub defer_overrides: BTreeMap<String, serde_json::Value>,
    /// Per-event (host event name, else lifecycle) template that carries
    /// `PromptDecision::additional_context` to the model. `{additional_context}`
    /// and `{event}` are the only placeholders. It replaces the allow/defer
    /// output for that event, so declare it only where that output is empty
    /// or the template restates it. Events without an entry drop the context.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub inject: BTreeMap<String, serde_json::Value>,
}

impl Default for ResponseSpec {
    fn default() -> Self {
        Self {
            mode: ResponseMode::Json,
            gate: None,
            observe: None,
            continue_response: None,
            response_overrides: std::collections::BTreeMap::new(),
            empty_on_undecided: false,
            allow_requires_tool_input: false,
            question_answer_strategy: None,
            defer_overrides: BTreeMap::new(),
            inject: BTreeMap::new(),
        }
    }
}

impl ResponseSpec {
    pub fn observe_value(&self) -> serde_json::Value {
        self.observe
            .clone()
            .unwrap_or_else(|| serde_json::json!({}))
    }
}

// ---------------------------------------------------------------------------
// DetectSpec
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
pub struct DetectSpec {
    #[serde(default)]
    pub harness_hint: Vec<String>,
    #[serde(default)]
    pub payload_keys_any: Vec<Vec<String>>,
    #[serde(default)]
    pub event_names: Vec<String>,
}

// ---------------------------------------------------------------------------
// Asset / Install ops
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssetSpec {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedded: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template: Option<serde_json::Value>,
}

fn default_create_true() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum InstallOp {
    JsonHooks {
        paths: Vec<String>,
        #[serde(default = "default_create_true")]
        create: bool,
        root: String,
        entry_shape: EntryShape,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fixed_fields: Option<serde_json::Map<String, serde_json::Value>>,
        entry: serde_json::Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        dedupe: Option<String>,
        #[serde(default)]
        entry_overrides: BTreeMap<String, serde_json::Value>,
    },
    TomlHooks {
        paths: Vec<String>,
        #[serde(default = "default_create_true")]
        create: bool,
        root: String,
        entry_shape: EntryShape,
        entry: serde_json::Value,
    },
    /// Flat array-of-tables TOML hooks (`[[hooks]]` rows, one per host
    /// event) for harnesses like Kimi Code (`~/.kimi-code/config.toml`)
    /// and Mistral Vibe (`hooks.toml`) whose config format has no
    /// per-event key. The installer appends one `[[<list_key>]]` table per
    /// installed event under a `# Added by agent-hooks` marker; `entry`
    /// is the per-row template with `{event}`/`{command}` substitution
    /// (same helper as `json_hooks`), `entry_overrides` merge per key,
    /// and the row `command` is always the canonical installer command.
    /// Uninstall removes only rows whose command matches the installer
    /// contract, preserving unrelated rows and comments.
    TomlListHooks {
        paths: Vec<String>,
        #[serde(default = "default_create_true")]
        create: bool,
        list_key: String,
        entry: serde_json::Value,
        #[serde(default)]
        entry_overrides: BTreeMap<String, serde_json::Value>,
    },
    StageAssets {
        stage_dir: String,
        assets: Vec<AssetSpec>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        managed_marker: Option<String>,
    },
    RunCommand {
        command: String,
        #[serde(default)]
        optional: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        undo: Option<String>,
    },
    JsonEdit {
        paths: Vec<String>,
        pointer: String,
        mode: EditMode,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value: Option<serde_json::Value>,
        /// `set` only: record the value it replaced (or its absence) so
        /// uninstall can restore it — or drop the key when we added it and it
        /// still holds our value. Without this, json_edit is non-inverting.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        restore_on_uninstall: bool,
        /// Skip the edit while the directory of this file holds any other
        /// `*.json` — e.g. only claim Kiro's default agent when ours is the
        /// sole custom agent, so a user's own agent choice is never taken.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        only_if_sole_json: Option<String>,
    },
}

// ---------------------------------------------------------------------------
// Verdict — per-event verdicts the host honors
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Allow,
    Deny,
    Ask,
}

// ---------------------------------------------------------------------------
// EventSpec
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventSpec {
    pub lifecycle: LifecycleRef,
    pub capability: Capability,
    /// Payload condition under which a `Stop`-lifecycle event demotes to
    /// `TurnEnd` (derived status stays Running). Every listed payload key
    /// must equal its value. Antigravity's weak Stop declares
    /// `{"fullyIdle": false}` — background tasks still churning, agent not
    /// actually done.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub demote_when: Option<BTreeMap<String, serde_json::Value>>,
    /// Demote a `Stop`-lifecycle event to `TurnEnd` when the payload is
    /// subagent-scoped (a parent marker or `mode: "subagent"`): a child
    /// finishing is not the session's turn ending. Mirrors B2V legacy
    /// `decide_status` (stop + subagent_scoped → busy).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub demote_when_subagent: bool,
    /// How questions lifted from this event's payload surface as a prompt.
    /// Absent = `question` (a held question prompt).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question_prompt: Option<QuestionPrompt>,
    /// Detect questions in PreToolUse events by payload structure
    /// (`questions`/`prompts` array or `question.id` path) for tools other than
    /// bash, even when the tool name is not in `ask_tools`. Used by Codex.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub question_payload_heuristic: bool,
    /// Verdicts the host honors for this event. None/absent = all
    /// verdicts supported (back-compat).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verdicts: Option<Vec<Verdict>>,
    /// Per-event override of the install op's `entry_shape`. Antigravity's
    /// named bundle takes matcher groups only for tool events; its lifecycle
    /// events (PreInvocation, PostInvocation, Stop) must be flat command
    /// lists — the host parser drops the whole file otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry_shape: Option<EntryShape>,
    /// `false` = normalize-only: the adapter parses this event name (an
    /// alias the host may also emit) but `install` never registers a hook
    /// for it. Hosts with a fixed hook-key set (jcode) reject extra keys.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub install: bool,
}

fn default_true() -> bool {
    true
}

fn is_true(v: &bool) -> bool {
    *v
}

fn is_false(v: &bool) -> bool {
    !*v
}

/// Per-event question surfacing. Claude Code asks through PermissionRequest
/// (a permission prompt that carries the questions) and never through
/// PreToolUse(AskUserQuestion) — holding both would ask the user twice
/// (B2V legacy payload.rs parse_claude_value / prompt.rs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuestionPrompt {
    /// Never attach a question prompt (status still derives from the tool).
    None,
    /// Keep `PromptKind::Permission`, with the lifted questions attached.
    Permission,
    /// `PromptKind::Question` (default behaviour).
    Question,
}

// ---------------------------------------------------------------------------
// Notes
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
pub struct Notes {
    #[serde(default)]
    pub post_install: Vec<String>,
}

// ---------------------------------------------------------------------------
// AskToolSpec — plain tool name or shaped entry carrying inline key overrides
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShapedAskTool {
    pub tool: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub questions: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AskToolSpec {
    Name(String),
    Shaped(ShapedAskTool),
}

impl AskToolSpec {
    pub fn name(&self) -> &str {
        match self {
            AskToolSpec::Name(n) => n,
            AskToolSpec::Shaped(s) => &s.tool,
        }
    }
}

impl PartialEq<str> for AskToolSpec {
    fn eq(&self, other: &str) -> bool {
        self.name() == other
    }
}

// ---------------------------------------------------------------------------
// Agent CLI metadata
// ---------------------------------------------------------------------------

/// Installed agent CLI details and safe, tokenized command recipes for consumers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentCliSpec {
    pub executables: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub documentation: Vec<String>,
    pub recipe_source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume: Option<ResumeCommandSpec>,
    /// Interactive launch with an initial prompt (e.g. `claude "<prompt>"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<PromptCommandSpec>,
    /// Non-interactive one-shot run (e.g. `claude -p "<prompt>"`, `codex exec "<prompt>"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exec: Option<PromptCommandSpec>,
    /// Command that prints the models the installed CLI can use. Output is
    /// passed through verbatim; models are never tracked in the manifest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models: Option<ModelsCommandSpec>,
    /// Stable aliases for CLIs with no listing command (e.g. Claude Code's
    /// `sonnet`/`opus`); printed by `models` when `models` is absent.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub model_aliases: Vec<String>,
    /// Args selecting a model (e.g. `["--model", "{model}"]`), spliced right
    /// after the executable of `start`/`exec`. `{model}` is one argv value.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub model_args: Vec<String>,
}

/// A model-listing argv; runs as-is, no placeholders. Stdout is read to
/// validate `--model`: JSON output yields every string under `id_fields`;
/// otherwise each line's `id_column`th (default 0) whitespace-separated token
/// is a model id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelsCommandSpec {
    pub argv: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub id_fields: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id_column: Option<usize>,
    /// Text output only: ids are `<provider>/<id>`, with the provider read
    /// from this column (for CLIs where a bare id is ambiguous across providers).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_column: Option<usize>,
    /// Text output only: leading lines to ignore (a header row).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub skip_lines: usize,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

/// A prompt-carrying argv template. `{prompt}` is substituted as one argv
/// value, never as shell syntax.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptCommandSpec {
    pub argv: Vec<String>,
    /// This mode has no model option (e.g. OpenCode's TUI), so `--model` is
    /// refused rather than passed to a flag the CLI does not know.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub model_unsupported: bool,
}

/// A resume argv template. `{session_ref}` is substituted as one argv value,
/// never as shell syntax.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResumeCommandSpec {
    pub argv: Vec<String>,
    pub session_ref: SessionRefKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionRefKind {
    Id,
    IdOrPath,
}

// ---------------------------------------------------------------------------
// VersionSpec
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VersionSpec {
    #[serde(default)]
    pub install: Vec<InstallOp>,
    #[serde(default)]
    pub events: BTreeMap<String, EventSpec>,
    #[serde(default)]
    pub extraction: ExtractionSpec,
    #[serde(default)]
    pub extraction_overrides: BTreeMap<String, BTreeMap<String, Vec<ExtractionSource>>>,
    #[serde(default)]
    pub response: ResponseSpec,
    #[serde(default)]
    pub unavailable: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<Notes>,
    #[serde(default)]
    pub ask_tools: Vec<AskToolSpec>,
    /// Substrings in paths to treat as harness-internal config directories
    /// (e.g. `[".gemini"]` for Antigravity), rejecting them as cwd candidates.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ignore_cwd_patterns: Vec<String>,
    /// Payloads the harness fires for work the user never started (Codex
    /// realtime-voice threads). Any matching rule drops the event entirely:
    /// `parse_event` returns [`crate::AdapterError::Ignored`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ignore_when: Vec<IgnoreRule>,
    /// Seconds the host waits for one hook when its written entries carry no
    /// `timeout` (e.g. Kiro: command-only entries, host default 30s). The
    /// shim's budget only; never written into host config.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_timeout_secs: Option<u64>,
    /// Harness version this spec was last confirmed against a live run
    /// (e.g. `"1.4.6"`). Informational only: nothing enforces it. Shown in
    /// the generated README table and `list-adapters --json`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verified_against: Option<String>,
    /// Agent CLI launch metadata. This is versioned with the adapter behavior,
    /// but its command recipes are informational until a consumer executes them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_cli: Option<AgentCliSpec>,
}

/// One `ignore_when` rule, matched against every object nested anywhere in
/// the payload. An object matches when each `fields` entry is a string equal
/// (ASCII case-insensitive) to its value AND, if set, one of the object's own
/// string values contains `any_value_contains`. A rule with neither
/// condition never matches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
pub struct IgnoreRule {
    pub fields: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub any_value_contains: Option<String>,
}

impl IgnoreRule {
    fn matches_object(&self, obj: &serde_json::Map<String, serde_json::Value>) -> bool {
        if self.fields.is_empty() && self.any_value_contains.is_none() {
            return false;
        }
        let fields_ok = self.fields.iter().all(|(k, want)| {
            obj.get(k)
                .and_then(|v| v.as_str())
                .is_some_and(|s| s.eq_ignore_ascii_case(want))
        });
        fields_ok
            && self.any_value_contains.as_ref().is_none_or(|needle| {
                obj.values()
                    .any(|v| v.as_str().is_some_and(|s| s.contains(needle.as_str())))
            })
    }

    /// Depth-bounded walk: hook payloads are shallow, and a pathological
    /// payload must never stall a gate hook.
    pub fn matches(&self, value: &serde_json::Value) -> bool {
        fn walk(rule: &IgnoreRule, v: &serde_json::Value, depth: usize) -> bool {
            if depth > 32 {
                return false;
            }
            match v {
                serde_json::Value::Object(obj) => {
                    rule.matches_object(obj) || obj.values().any(|c| walk(rule, c, depth + 1))
                }
                serde_json::Value::Array(items) => items.iter().any(|c| walk(rule, c, depth + 1)),
                _ => false,
            }
        }
        walk(self, value, 0)
    }
}

impl VersionSpec {
    pub fn ask_allowed(&self, lifecycle_key: &str) -> bool {
        self.events
            .get(lifecycle_key)
            .and_then(|e| e.verdicts.as_ref())
            .map(|v| v.contains(&Verdict::Ask))
            .unwrap_or(true)
    }
    pub fn question_keys_for(&self, tool_name: &str) -> Option<&ShapedAskTool> {
        self.ask_tools.iter().find_map(|t| match t {
            AskToolSpec::Shaped(s) if s.tool.eq_ignore_ascii_case(tool_name) => Some(s),
            _ => None,
        })
    }
}

// ---------------------------------------------------------------------------
// AdapterManifest
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdapterManifest {
    pub id: String,
    pub display_name: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    pub default_version: String,
    /// Base manifest id to inherit from (builtin id or another loaded
    /// manifest's id). Resolved after all tiers load; the resolved output
    /// is fully materialized with `extends: None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extends: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_agent_hooks: Option<String>,
    /// Docs-only adapter: built from the harness's published hook contract
    /// but never exercised against the live CLI. Unit tests cover parsing
    /// and response rendering from documentation-derived payloads; native
    /// event emission, decision enforcement, and installer paths are
    /// unverified. Flip to false once a live run confirms the contract.
    #[serde(default, skip_serializing_if = "is_false")]
    pub unverified: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detect: Option<DetectSpec>,
    #[serde(default)]
    pub versions: BTreeMap<String, VersionSpec>,
}
impl AdapterManifest {
    pub fn parse(s: &str) -> Result<Self, AdapterError> {
        let manifest: Self =
            serde_json::from_str(s).map_err(|e| AdapterError::Parse(e.to_string()))?;
        if manifest.id.trim().is_empty() {
            return Err(AdapterError::InvalidConfig(
                "id must not be empty".to_string(),
            ));
        }
        // basic validation: default_version must exist. Manifests with
        if manifest.extends.is_none() && !manifest.versions.contains_key(&manifest.default_version)
        {
            return Err(AdapterError::InvalidConfig(format!(
                "default_version '{}' not found in versions map",
                manifest.default_version
            )));
        }
        manifest.validate()?;
        Ok(manifest)
    }

    /// Semantic validation of manifest structure.
    pub fn validate(&self) -> Result<(), AdapterError> {
        for (ver_name, vs) in &self.versions {
            if let Some(agent_cli) = &vs.agent_cli {
                self.validate_agent_cli(ver_name, agent_cli)?;
            }
            for (ev_name, ev_spec) in &vs.events {
                if ev_spec.capability == Capability::Gate
                    && ev_spec
                        .verdicts
                        .as_ref()
                        .map(|v| v.is_empty())
                        .unwrap_or(false)
                {
                    tracing::warn!(
                        "Manifest '{}' ({}) event '{}' is a gate but specifies empty verdicts",
                        self.id,
                        ver_name,
                        ev_name
                    );
                }
            }
        }
        Ok(())
    }

    /// `agent_cli` argv templates run unshelled, so each must start with one
    /// of the declared executables and carry its placeholder exactly once.
    fn validate_agent_cli(&self, ver_name: &str, cli: &AgentCliSpec) -> Result<(), AdapterError> {
        let err = |msg: String| {
            Err(AdapterError::InvalidConfig(format!(
                "adapter '{}' version '{}' {msg}",
                self.id, ver_name
            )))
        };
        if cli.executables.is_empty()
            || cli.executables.iter().any(String::is_empty)
            || cli.recipe_source.is_empty()
            || cli.documentation.iter().any(String::is_empty)
            || cli.model_aliases.iter().any(String::is_empty)
        {
            return err("has incomplete agent_cli metadata".into());
        }
        let valid_argv = |argv: &[String], placeholder: &str| {
            let count: usize = argv.iter().map(|a| a.matches(placeholder).count()).sum();
            argv.first()
                .is_some_and(|exe| cli.executables.contains(exe))
                && (placeholder.is_empty() || count == 1)
        };
        if let Some(resume) = &cli.resume {
            if !valid_argv(&resume.argv, "{session_ref}") {
                return err("resume argv must start with a configured executable and contain exactly one {session_ref}".into());
            }
        }
        for (kind, spec) in [("start", &cli.start), ("exec", &cli.exec)] {
            if let Some(spec) = spec {
                if !valid_argv(&spec.argv, "{prompt}") {
                    return err(format!("{kind} argv must start with a configured executable and contain exactly one {{prompt}}"));
                }
            }
        }
        if let Some(models) = &cli.models {
            if !valid_argv(&models.argv, "")
                || (!models.id_fields.is_empty() && models.id_column.is_some())
            {
                return err("models argv must start with a configured executable and must not mix id_fields with id_column".into());
            }
        }
        if !cli.model_args.is_empty() && !valid_model_args(&cli.model_args) {
            return err("model_args must contain exactly one {model}".into());
        }
        Ok(())
    }

    pub fn get_version(&self, version: &str) -> Option<&VersionSpec> {
        self.versions.get(version)
    }

    pub fn default_version(&self) -> &str {
        &self.default_version
    }

    pub fn load_embedded() -> Vec<AdapterManifest> {
        let mut out = Vec::new();
        for (_dir, raw) in super::builtin_manifests::BUILTIN_MANIFESTS.iter() {
            if let Ok(m) = Self::parse(raw) {
                out.push(m);
            } else {
                tracing::warn!("failed to parse embedded manifest");
            }
        }
        out
    }

    /// Discovery directories for external manifest tiers, in precedence
    /// order: project (`.agent-hooks/adapters`) first, then user
    /// (`~/.config/agent-hooks/adapters`); embedded builtins rank last.
    /// Design doc §5.
    pub fn tier_dirs() -> Vec<PathBuf> {
        let mut dirs = Vec::new();
        if let Ok(cwd) = std::env::current_dir() {
            dirs.push(cwd.join(".agent-hooks/adapters"));
        }
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .ok();
        if let Some(home) = home {
            dirs.push(PathBuf::from(home).join(".config/agent-hooks/adapters"));
        }
        dirs
    }

    /// Tier dirs trusted to define commands that agent-hooks itself spawns
    /// (`start`/`exec`/`models`): user config only. The project tier lives in
    /// the cwd, so a cloned repo must not pick the executable that runs.
    pub fn trusted_tier_dirs() -> Vec<PathBuf> {
        let project = std::env::current_dir()
            .ok()
            .map(|cwd| cwd.join(".agent-hooks/adapters"));
        Self::tier_dirs()
            .into_iter()
            .filter(|d| Some(d) != project.as_ref())
            .collect()
    }

    /// Scan one tier directory. Two layouts are accepted, path-sorted:
    ///
    /// - flat: `<id>.json` next to the tier dir (`.meta.json` provenance
    ///   sidecars skipped; this is what `adapter add` writes)
    /// - folder: `<id>/adapter.json` (or `<id>/<id>.json`) with the
    ///   adapter's `fixtures/` as a sibling — what `adapter new` scaffolds,
    ///   so a manifest and its captured payloads travel together
    ///
    /// Unparseable manifests are skipped with a warning — a broken user
    /// manifest must never take the daemon down.
    fn scan_tier(dir: &Path) -> Vec<AdapterManifest> {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut entries: Vec<PathBuf> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
        entries.sort();
        let mut manifests = Vec::new();
        for path in entries {
            let candidates: Vec<PathBuf> = if path.is_dir() {
                let adapter_json = path.join("adapter.json");
                if adapter_json.is_file() {
                    vec![adapter_json]
                } else if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
                    let named = path.join(format!("{name}.json"));
                    if named.is_file() {
                        vec![named]
                    } else {
                        Vec::new()
                    }
                } else {
                    Vec::new()
                }
            } else {
                let skip_meta = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .map(|s| s.ends_with(".meta"))
                    .unwrap_or(false);
                let is_json = path.extension().and_then(|e| e.to_str()) == Some("json");
                if is_json && !skip_meta {
                    vec![path]
                } else {
                    Vec::new()
                }
            };
            for manifest_path in candidates {
                match std::fs::read_to_string(&manifest_path)
                    .map_err(AdapterError::Io)
                    .and_then(|text| Self::parse(&text))
                {
                    Ok(m) => manifests.push(m),
                    Err(e) => {
                        tracing::warn!("skipping manifest {}: {e}", manifest_path.display())
                    }
                }
            }
        }
        manifests
    }

    /// Load manifests across all tiers: embedded < user < project.
    ///
    /// Higher tiers come FIRST so the first-match semantics shared by the
    /// registry lookup and [`Self::resolve_all`]'s id map yield
    /// project > user > embedded. A same-id manifest from a lower tier is
    /// shadowed, not merged — inherit with `extends` instead of overriding.
    pub fn load_tiered() -> Vec<AdapterManifest> {
        Self::load_tiered_from(&Self::tier_dirs())
    }

    /// [`Self::load_tiered`] with explicit tier dirs (same precedence order
    /// as [`Self::tier_dirs`]: higher tiers first) — the env-free core,
    /// unit-testable.
    pub fn load_tiered_from(dirs: &[PathBuf]) -> Vec<AdapterManifest> {
        let mut out = Vec::new();
        for dir in dirs {
            out.extend(Self::scan_tier(dir));
        }
        out.extend(Self::load_embedded());
        let mut seen = BTreeSet::new();
        out.retain(|m| seen.insert(m.id.clone()));
        out
    }

    /// Resolve `extends` chains across a fully loaded manifest set.
    ///
    /// Call after all tiers (builtin + user + project) load so a base may
    /// come from any tier. Each base resolves recursively first, then merges
    /// with the local manifest (local wins when present). Output order
    /// matches input order; every resolved manifest has `extends: None`.
    ///
    /// Errors: unknown base id names the base and its referrer; a cycle
    /// (including self-extends) names the full chain (`a -> b -> a`).
    pub fn resolve_all(
        manifests: &[AdapterManifest],
    ) -> Result<Vec<AdapterManifest>, AdapterError> {
        let mut by_id: BTreeMap<&str, &AdapterManifest> = BTreeMap::new();
        for m in manifests {
            // First occurrence wins, matching registry first-match lookup.
            by_id.entry(m.id.as_str()).or_insert(m);
        }
        let mut resolved: BTreeMap<String, AdapterManifest> = BTreeMap::new();
        let mut out = Vec::with_capacity(manifests.len());
        for m in manifests {
            let mut stack: Vec<String> = Vec::new();
            out.push(Self::resolve_one(
                m.id.as_str(),
                &by_id,
                &mut resolved,
                &mut stack,
            )?);
        }
        Ok(out)
    }

    fn resolve_one(
        id: &str,
        by_id: &BTreeMap<&str, &AdapterManifest>,
        resolved: &mut BTreeMap<String, AdapterManifest>,
        stack: &mut Vec<String>,
    ) -> Result<AdapterManifest, AdapterError> {
        if let Some(done) = resolved.get(id) {
            return Ok(done.clone());
        }
        if let Some(pos) = stack.iter().position(|s| s == id) {
            let mut chain: Vec<String> = stack[pos..].to_vec();
            chain.push(id.to_string());
            return Err(AdapterError::InvalidConfig(format!(
                "extends cycle detected: {}",
                chain.join(" -> ")
            )));
        }
        // Every id resolved here is either a top-level manifest or a base
        // named by some manifest's `extends`; the parent frame checks base
        // existence first, so `None` is unreachable — but stay total anyway.
        let local = match by_id.get(id) {
            Some(m) => *m,
            None => {
                return Err(AdapterError::InvalidConfig(format!(
                    "unknown extends base '{id}'"
                )))
            }
        };
        let merged = match local.extends.as_deref() {
            None => {
                let mut m = local.clone();
                m.extends = None;
                m
            }
            Some(base_id) => {
                if !by_id.contains_key(base_id) {
                    return Err(AdapterError::InvalidConfig(format!(
                        "adapter '{}' extends unknown base '{base_id}'",
                        local.id
                    )));
                }
                stack.push(id.to_string());
                let base = Self::resolve_one(base_id, by_id, resolved, stack)?;
                stack.pop();
                Self::merge_manifest(&base, local)
            }
        };
        if !merged.versions.contains_key(&merged.default_version) {
            return Err(AdapterError::InvalidConfig(format!(
                "adapter '{}' default_version '{}' not found in versions map",
                merged.id, merged.default_version
            )));
        }
        resolved.insert(id.to_string(), merged.clone());
        Ok(merged)
    }

    /// Merge base `B` with local `L`; `L` wins when present.
    ///
    /// Top level: `id` is always local; `display_name` / `default_version`
    /// take local when non-empty; `aliases` takes local when non-empty;
    /// `min_agent_hooks` / `detect` take local when `Some` (non-empty for
    /// the string); `versions` merge per version key. The output is fully
    /// materialized with `extends: None`.
    fn merge_manifest(base: &AdapterManifest, local: &AdapterManifest) -> AdapterManifest {
        let mut versions = base.versions.clone();
        for (key, local_ver) in &local.versions {
            match versions.get(key) {
                Some(base_ver) => {
                    versions.insert(key.clone(), Self::merge_version(base_ver, local_ver));
                }
                None => {
                    versions.insert(key.clone(), local_ver.clone());
                }
            }
        }
        AdapterManifest {
            id: local.id.clone(),
            display_name: if local.display_name.is_empty() {
                base.display_name.clone()
            } else {
                local.display_name.clone()
            },
            aliases: if local.aliases.is_empty() {
                base.aliases.clone()
            } else {
                local.aliases.clone()
            },
            default_version: if local.default_version.is_empty() {
                base.default_version.clone()
            } else {
                local.default_version.clone()
            },
            extends: None,
            min_agent_hooks: match &local.min_agent_hooks {
                Some(s) if !s.is_empty() => local.min_agent_hooks.clone(),
                _ => base.min_agent_hooks.clone(),
            },
            // Sticky: a docs-only base stays docs-only through extends;
            // graduating to verified means rewriting as an explicit
            // manifest without the flag.
            unverified: local.unverified || base.unverified,
            detect: local.detect.clone().or_else(|| base.detect.clone()),
            versions,
        }
    }

    /// Merge base `B` with local `L` for one shared version key.
    ///
    /// `events` / `extraction_overrides` merge per key (`L` wins);
    /// `extraction` merges per field (`L` replaces iff non-empty, `summary`
    /// iff `Some`); `install` / `unavailable` / `ask_tools` take `L` iff
    /// non-empty; `notes` and `agent_cli` take `L` iff `Some`; `response` merges per field.
    fn merge_version(base: &VersionSpec, local: &VersionSpec) -> VersionSpec {
        let mut events = base.events.clone();
        for (key, spec) in &local.events {
            events.insert(key.clone(), spec.clone());
        }
        let mut extraction_overrides = base.extraction_overrides.clone();
        for (key, spec) in &local.extraction_overrides {
            extraction_overrides.insert(key.clone(), spec.clone());
        }
        VersionSpec {
            install: if local.install.is_empty() {
                base.install.clone()
            } else {
                local.install.clone()
            },
            events,
            extraction: Self::merge_extraction(&base.extraction, &local.extraction),
            extraction_overrides,
            response: Self::merge_response(&base.response, &local.response),
            unavailable: if local.unavailable.is_empty() {
                base.unavailable.clone()
            } else {
                local.unavailable.clone()
            },
            notes: local.notes.clone().or_else(|| base.notes.clone()),
            ask_tools: if local.ask_tools.is_empty() {
                base.ask_tools.clone()
            } else {
                local.ask_tools.clone()
            },
            ignore_cwd_patterns: if local.ignore_cwd_patterns.is_empty() {
                base.ignore_cwd_patterns.clone()
            } else {
                local.ignore_cwd_patterns.clone()
            },
            host_timeout_secs: local.host_timeout_secs.or(base.host_timeout_secs),
            verified_against: local
                .verified_against
                .clone()
                .or_else(|| base.verified_against.clone()),
            agent_cli: local.agent_cli.clone().or_else(|| base.agent_cli.clone()),
            ignore_when: if local.ignore_when.is_empty() {
                base.ignore_when.clone()
            } else {
                local.ignore_when.clone()
            },
        }
    }

    fn merge_extraction(base: &ExtractionSpec, local: &ExtractionSpec) -> ExtractionSpec {
        ExtractionSpec {
            message_id: if local.message_id.is_empty() {
                base.message_id.clone()
            } else {
                local.message_id.clone()
            },
            tool_call_id: if local.tool_call_id.is_empty() {
                base.tool_call_id.clone()
            } else {
                local.tool_call_id.clone()
            },
            flavor_text: if local.flavor_text.is_empty() {
                base.flavor_text.clone()
            } else {
                local.flavor_text.clone()
            },
            session_title: if local.session_title.is_empty() {
                base.session_title.clone()
            } else {
                local.session_title.clone()
            },
            session_id: if local.session_id.is_empty() {
                base.session_id.clone()
            } else {
                local.session_id.clone()
            },
            parent_session_id: if local.parent_session_id.is_empty() {
                base.parent_session_id.clone()
            } else {
                local.parent_session_id.clone()
            },
            prompt: if local.prompt.is_empty() {
                base.prompt.clone()
            } else {
                local.prompt.clone()
            },
            tool_name: if local.tool_name.is_empty() {
                base.tool_name.clone()
            } else {
                local.tool_name.clone()
            },
            tool_input: if local.tool_input.is_empty() {
                base.tool_input.clone()
            } else {
                local.tool_input.clone()
            },
            tool_output: if local.tool_output.is_empty() {
                base.tool_output.clone()
            } else {
                local.tool_output.clone()
            },
            error: if local.error.is_empty() {
                base.error.clone()
            } else {
                local.error.clone()
            },
            success: if local.success.is_empty() {
                base.success.clone()
            } else {
                local.success.clone()
            },
            cwd: if local.cwd.is_empty() {
                base.cwd.clone()
            } else {
                local.cwd.clone()
            },
            summary: local.summary.clone().or_else(|| base.summary.clone()),
        }
    }

    /// Merge base `B` with local `L`: `gate` / `observe` / `continue` take
    /// `L` iff `Some`; `response_overrides` merge per key (`L` wins);
    /// `mode` takes `L` only when non-default (`Json` carries no signal, so
    /// an explicit `json` never clears a base mode); boolean response flags
    /// are OR-combined (`true` wins).
    fn merge_response(base: &ResponseSpec, local: &ResponseSpec) -> ResponseSpec {
        let mut response_overrides = base.response_overrides.clone();
        for (key, spec) in &local.response_overrides {
            response_overrides.insert(key.clone(), spec.clone());
        }
        ResponseSpec {
            mode: if local.mode != ResponseMode::Json {
                local.mode
            } else {
                base.mode
            },
            gate: local.gate.clone().or_else(|| base.gate.clone()),
            observe: local.observe.clone().or_else(|| base.observe.clone()),
            continue_response: local
                .continue_response
                .clone()
                .or_else(|| base.continue_response.clone()),
            response_overrides,
            empty_on_undecided: local.empty_on_undecided || base.empty_on_undecided,
            allow_requires_tool_input: local.allow_requires_tool_input
                || base.allow_requires_tool_input,
            question_answer_strategy: local
                .question_answer_strategy
                .or(base.question_answer_strategy),
            defer_overrides: {
                let mut merged = base.defer_overrides.clone();
                merged.extend(local.defer_overrides.clone());
                merged
            },
            inject: {
                let mut merged = base.inject.clone();
                merged.extend(local.inject.clone());
                merged
            },
        }
    }
}

fn valid_model_args(args: &[String]) -> bool {
    args.iter()
        .map(|a| a.matches("{model}").count())
        .sum::<usize>()
        == 1
}
