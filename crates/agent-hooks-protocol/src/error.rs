use thiserror::Error;

#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("Invalid JSON-RPC message: {0}")]
    InvalidRpc(String),

    #[error("Missing required field: {0}")]
    MissingField(&'static str),

    #[error("Unknown lifecycle kind: {0}")]
    UnknownLifecycle(String),
}
