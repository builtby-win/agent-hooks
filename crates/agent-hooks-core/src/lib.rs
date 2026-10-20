//! # agent-hooks-core
//!
//! Core runtime for the `agent-hooks` daemon: session tracking, prompt mediation,
//! and the IPC server.

pub mod daemon;
pub mod enricher;
pub mod error;
pub mod history_recorder;
pub mod plugin;
pub mod prompt_mediator;
pub mod session_tracker;

pub use daemon::{
    default_socket_path, AgentHooksDaemon, DEFAULT_UNIX_SOCKET_PATH, DEFAULT_WINDOWS_PIPE_PATH,
    ENV_AGENT_HOOKS_SOCKET,
};
pub use enricher::{EventEnricher, NoopEnricher};
pub use error::CoreError;
pub use history_recorder::HistoryRecorder;
pub use plugin::{Plugin, PluginManager};
pub use prompt_mediator::{ClaimOutcome, ParkedQuestion, PromptMediator, PromptMeta, QueueEntry};
pub use session_tracker::{SessionInfo, SessionTracker};

pub use agent_hooks_adapters;
pub use agent_hooks_protocol;
