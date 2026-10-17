//! # agent-hooks-client
//!
//! Resilient async client for the `agent-hooks` daemon's IPC socket.
//! Speaks JSON-RPC 2.0 / NDJSON over a Unix Domain Socket
//! (`/tmp/agent-hooks.sock`) on Unix/macOS and a Named Pipe
//! (`\\.\pipe\agent-hooks`) on Windows.

pub mod client;
pub mod error;

pub use client::{
    default_socket_path, AgentHooksClient, SessionSummary, DEFAULT_UNIX_SOCKET_PATH,
    DEFAULT_WINDOWS_PIPE_PATH, ENV_AGENT_HOOKS_SOCKET,
};
pub use error::ClientError;

// Re-export protocol types so consumers need only depend on the client crate.
pub use agent_hooks_protocol::{
    AgentEvent, AgentLifecycle, EnvRequestedResponse, EventContext, NormalizedStatus, PromptAnswer,
    PromptDecision, PromptKind, PromptOption, PromptQuestion, PromptRequest, PromptResponse,
    RawHookInput, SubscriptionFilter, ENV_REQUESTED_METHOD,
};
pub use agent_hooks_protocol::{JsonRpcNotification, JsonRpcRequest, JsonRpcResponse};
