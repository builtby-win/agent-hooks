use thiserror::Error;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Adapter error: {0}")]
    Adapter(#[from] agent_hooks_adapters::error::AdapterError),

    #[error("Protocol error: {0}")]
    Protocol(#[from] agent_hooks_protocol::error::ProtocolError),

    #[error("IPC error: {0}")]
    Ipc(String),

    #[error("Config parse error: {0}")]
    ConfigParse(String),

    #[error("Config IO error at {path}: {source}")]
    ConfigIo {
        path: String,
        source: std::io::Error,
    },

    #[error("Prompt not found: {0}")]
    PromptNotFound(String),

    #[error("Prompt timed out: {0}")]
    PromptTimeout(String),

    #[error("Prompt already pending: {0}")]
    PromptAlreadyPending(String),

    #[error("Channel closed for prompt: {0}")]
    ChannelClosed(String),

    #[error("Daemon error: {0}")]
    Daemon(String),

    #[error("Serialization error: {0}")]
    Serialization(String),
}

impl From<serde_json::Error> for CoreError {
    fn from(e: serde_json::Error) -> Self {
        CoreError::Serialization(e.to_string())
    }
}
