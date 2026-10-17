use thiserror::Error;

#[derive(Debug, Error)]
pub enum AdapterError {
    #[error("Parse error: {0}")]
    Parse(String),

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("Missing session ID in payload")]
    MissingSessionId,

    #[error("Unsupported event: {0}")]
    UnsupportedEvent(String),

    /// The manifest's `ignore_when` matched: the harness fired a hook for
    /// work the user never started (e.g. Codex realtime-voice threads). No
    /// event is emitted anywhere; the shim answers with the observe no-op.
    #[error("Ignored event: {0}")]
    Ignored(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Invalid config: {0}")]
    InvalidConfig(String),

    #[error("Hook install failed: {0}")]
    HookInstall(String),

    #[error("Missing field: {0}")]
    MissingField(&'static str),
}
