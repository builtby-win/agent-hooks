use serde::{Deserialize, Serialize};

use crate::prompt::{PromptKind, PromptRequest};

/// High-level normalized operational status of an agent session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NormalizedStatus {
    /// Session is ready and idle, waiting for user input
    Idle,
    /// Agent is running, computing, or calling tools
    Running,
    /// Agent is paused waiting for user input (permission or question)
    WaitingForInput,
    /// Agent completed its assigned task/turn. Reserved: no lifecycle
    /// currently derives it — harness `Stop` hooks are per-turn and map to
    /// `Idle`; only `SessionEnd` terminates (→ `Closed`/`Failed`).
    Completed,
    /// Agent encountered a fatal error or crashed
    Failed,
    /// Agent session terminated / closed
    Closed,
}

/// Agent lifecycle: what the agent just did (one turn step).
/// Aligned with modern agent architectures (Claude Code, OpenAI Codex, etc.).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum AgentLifecycle {
    /// Agent session was initiated or attached.
    SessionStart {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resume_command: Option<Vec<String>>,
    },

    /// User submitted a prompt / instruction to the agent.
    UserPromptSubmit {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        user_prompt: Option<String>,
    },

    /// Agent is about to invoke a tool (guardrail / inspection point).
    PreToolUse {
        tool_name: String,
        #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
        input: serde_json::Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prompt: Option<PromptRequest>,
    },

    /// Tool finished execution successfully.
    PostToolUse {
        tool_name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output_preview: Option<String>,
    },

    /// Tool execution failed.
    PostToolUseFailure { tool_name: String, error: String },

    /// Agent is blocked and requires human permission or interactive answer.
    PermissionRequest { prompt: PromptRequest },

    /// Out-of-band notification or alert from the agent.
    Notification {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        notification_type: Option<String>,
    },

    /// A subagent or child task was spawned.
    SubagentStart {
        subagent_id: String,
        subagent_type: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
    },

    /// A subagent finished its task.
    SubagentStop {
        subagent_id: String,
        success: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
    },

    /// A single agent turn began (one LLM response + tool calls). Busy signal:
    /// asserts Running, never terminal. Many turns per agent run.
    TurnStart {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_index: Option<u64>,
    },
    /// A single agent turn ended. Busy signal: stays Running (the run may
    /// continue with more turns). Only Stop/SessionEnd terminate.
    /// `assistant_message` carries the turn's printed assistant reply when
    /// the harness forwards it (e.g. pi/omp `turn_end` with
    /// `last_assistant_message`); `None` preserves the old status-only shape.
    TurnEnd {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_index: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        assistant_message: Option<String>,
    },

    /// Agent finished its execution turn and is now awaiting new user input.
    /// Per-turn signal in every harness that emits it (Claude Code `Stop`,
    /// Antigravity `Stop`, OpenCode `session.idle`): the session stays alive
    /// and routinely continues afterward, so this derives `Idle`, never a
    /// terminal status. Only `SessionEnd` terminates.
    Stop {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
        #[serde(default)]
        success: bool,
    },

    /// Agent session terminated / exited.
    SessionEnd {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
    },

    /// Agent is about to compact context (Codex PreCompact). The `trigger`
    /// carries the matcher trigger (`manual`|`auto`); `continue:false`
    /// stops before compact.
    PreCompact {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        trigger: Option<String>,
    },

    /// Agent finished compacting context (Codex PostCompact).
    PostCompact {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        trigger: Option<String>,
    },

    /// A committed batch of tool calls finished. The ordered native batch
    /// remains in `AgentEvent::raw_event`; this is not an end-of-turn signal.
    PostToolBatch,

    /// Agent is about to call the language model.
    PreLLMCall,

    /// A language-model call completed; the agent may still run tools.
    PostLLMCall,

    /// A turn ended with a final model failure, not session termination.
    StopFailure { error: String },

    /// Tool execution started after pre-tool hooks and approval completed.
    /// Native tool details remain in `AgentEvent::raw_event`.
    ToolUseStart,

    /// User interrupted the agent (Codex Interrupt, main-thread only).
    /// Cannot be prevented; carries no fields.
    Interrupt,
}

/// Tool names that mean the agent is blocked on the user. Real hooks spell
/// these inconsistently (`RequestUserInputAsync`, `request_user_input`,
/// `AskUserQuestion`), so matching collapses to a lowercase alphanumeric key.
/// Port of B2V legacy lifecycle.rs `tool_blocks_on_user`.
pub fn tool_blocks_on_user(name: &str) -> bool {
    let key: String = name
        .trim()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect();
    matches!(
        key.as_str(),
        "requestuserinput"
            | "requestuserinputasync"
            | "askquestion"
            | "askuserquestion"
            | "permissionrequest"
            | "permission"
    )
}

impl AgentLifecycle {
    /// Derive the normalized session status from this lifecycle event.
    pub fn derived_status(&self) -> NormalizedStatus {
        match self {
            Self::SessionStart { .. } => NormalizedStatus::Idle,
            Self::UserPromptSubmit { .. } => NormalizedStatus::Running,
            Self::PreToolUse {
                prompt, tool_name, ..
            } => {
                if prompt
                    .as_ref()
                    .is_some_and(|p| p.kind == PromptKind::Question)
                    || tool_blocks_on_user(tool_name)
                {
                    NormalizedStatus::WaitingForInput
                } else {
                    NormalizedStatus::Running
                }
            }
            // Legacy status for a user-input tool's PostToolUse was busy in
            // practice: the b2v CLI forwarded PostToolUse as a "cheap" summary
            // without tool fields (src-b2v hooks/agent_event.rs
            // classify_agent_event_name_for_capture), so decide_status only saw
            // the event name. Live parity e2e confirms busy.
            Self::PostToolUse { .. } => NormalizedStatus::Running,
            Self::PostToolUseFailure { .. } => NormalizedStatus::Running,
            Self::PermissionRequest { .. } => NormalizedStatus::WaitingForInput,
            // Adapters normalize `notification_type` to permission | question
            // | idle (B2V legacy notification_status_for_agent); anything else
            // is informational and leaves the session running.
            Self::Notification {
                notification_type, ..
            } => match notification_type.as_deref() {
                Some("permission" | "question") => NormalizedStatus::WaitingForInput,
                Some("idle") => NormalizedStatus::Idle,
                _ => NormalizedStatus::Running,
            },
            Self::SubagentStart { .. } => NormalizedStatus::Running,
            Self::SubagentStop { .. } => NormalizedStatus::Running,
            Self::TurnStart { .. } => NormalizedStatus::Running,
            Self::TurnEnd { .. } => NormalizedStatus::Running,
            Self::Stop { success, .. } => {
                if *success {
                    // Per-turn "model stopped talking" signal, not session
                    // end: harnesses keep the session alive and the next
                    // `UserPromptSubmit` arrives seconds later. Marking
                    // Completed here produced running→completed flapping.
                    NormalizedStatus::Idle
                } else {
                    NormalizedStatus::Failed
                }
            }
            Self::SessionEnd { exit_code } => match exit_code {
                Some(code) if *code != 0 => NormalizedStatus::Failed,
                _ => NormalizedStatus::Closed,
            },
            Self::PreCompact { .. } => NormalizedStatus::Running,
            Self::PostCompact { .. } => NormalizedStatus::Running,
            Self::Interrupt => NormalizedStatus::Running,
            Self::PostToolBatch | Self::PreLLMCall | Self::PostLLMCall => NormalizedStatus::Running,
            Self::StopFailure { .. } => NormalizedStatus::Failed,
            Self::ToolUseStart => NormalizedStatus::Running,
        }
    }

    /// String name of the lifecycle event (e.g. "UserPromptSubmit", "PreToolUse")
    pub fn name(&self) -> &'static str {
        match self {
            Self::SessionStart { .. } => "SessionStart",
            Self::UserPromptSubmit { .. } => "UserPromptSubmit",
            Self::PreToolUse { .. } => "PreToolUse",
            Self::PostToolUse { .. } => "PostToolUse",
            Self::PostToolUseFailure { .. } => "PostToolUseFailure",
            Self::PermissionRequest { .. } => "PermissionRequest",
            Self::Notification { .. } => "Notification",
            Self::SubagentStart { .. } => "SubagentStart",
            Self::SubagentStop { .. } => "SubagentStop",
            Self::TurnStart { .. } => "TurnStart",
            Self::TurnEnd { .. } => "TurnEnd",
            Self::Stop { .. } => "Stop",
            Self::SessionEnd { .. } => "SessionEnd",
            Self::PreCompact { .. } => "PreCompact",
            Self::PostCompact { .. } => "PostCompact",
            Self::Interrupt => "Interrupt",
            Self::PostToolBatch => "PostToolBatch",
            Self::PreLLMCall => "PreLLMCall",
            Self::PostLLMCall => "PostLLMCall",
            Self::StopFailure { .. } => "StopFailure",
            Self::ToolUseStart => "ToolUseStart",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notification_status_follows_normalized_type() {
        let n = |t: Option<&str>| AgentLifecycle::Notification {
            message: "m".into(),
            title: None,
            notification_type: t.map(str::to_string),
        };
        assert_eq!(
            n(Some("permission")).derived_status(),
            NormalizedStatus::WaitingForInput
        );
        assert_eq!(
            n(Some("question")).derived_status(),
            NormalizedStatus::WaitingForInput
        );
        assert_eq!(n(Some("idle")).derived_status(), NormalizedStatus::Idle);
        assert_eq!(
            n(Some("auth_success")).derived_status(),
            NormalizedStatus::Running
        );
        assert_eq!(n(None).derived_status(), NormalizedStatus::Running);
    }

    // PreToolUse of a user-input tool waits (legacy pre_tool_use_implies_waiting);
    // its PostToolUse is busy (live parity e2e: legacy CLI strips PostToolUse).
    #[test]
    fn user_input_tools_wait_on_pre_tool_use_only() {
        for name in [
            "request_user_input",
            "RequestUserInputAsync",
            "AskUserQuestion",
            "ask_question",
        ] {
            let pre = AgentLifecycle::PreToolUse {
                tool_name: name.into(),
                input: serde_json::Value::Null,
                prompt: None,
            };
            let post = AgentLifecycle::PostToolUse {
                tool_name: name.into(),
                output_preview: None,
            };
            assert_eq!(
                pre.derived_status(),
                NormalizedStatus::WaitingForInput,
                "{name}"
            );
            assert_eq!(post.derived_status(), NormalizedStatus::Running, "{name}");
        }
    }
}
