use thiserror::Error;

/// Errors produced by [`crate::AgentHooksClient`].
#[derive(Debug, Error)]
pub enum ClientError {
    /// Underlying I/O error (socket, read/write, connect).
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// JSON serialization / deserialization error.
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    /// Failed to establish a connection to the daemon.
    #[error("connection failed: {0}")]
    Connection(String),

    /// JSON-RPC error returned by the daemon.
    #[error("RPC error {code}: {message}")]
    Rpc { code: i32, message: String },

    /// Connection was closed unexpectedly.
    #[error("disconnected")]
    Disconnected,

    /// Operation timed out.
    #[error("timeout")]
    Timeout,

    /// Response payload could not be understood.
    #[error("invalid response: {0}")]
    InvalidResponse(String),

    /// Attempted operation without an active connection.
    #[error("not connected: {0}")]
    NotConnected(String),

    /// Protocol-level error.
    #[error("protocol error: {0}")]
    Protocol(String),
}

impl From<tokio::time::error::Elapsed> for ClientError {
    fn from(_: tokio::time::error::Elapsed) -> Self {
        Self::Timeout
    }
}
