use std::path::PathBuf;

use agent_hooks_protocol::{
    HistoryErrorBody, HistoryErrorCode, HistoryErrorResponse, HistorySessionKey,
    HISTORY_API_VERSION,
};

/// Failure of a history store operation. Display text never includes captured
/// message text.
#[derive(Debug, thiserror::Error)]
pub enum HistoryError {
    /// No initialized store at this location.
    #[error("history is not enabled: no store at {}", .0.display())]
    Disabled(PathBuf),
    #[error("history store schema v{found} must be upgraded to v{expected}")]
    SchemaUpgradeRequired { found: u32, expected: u32 },
    #[error("history store schema v{found} is newer than supported v{expected}")]
    UnsupportedNewerSchema { found: u32, expected: u32 },
    /// Symlinked path, wrong file type, or permissions open to other users.
    #[error("history store path is insecure: {0}")]
    Insecure(String),
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
    #[error("session id matches {} sessions", .0.len())]
    AmbiguousSession(Vec<HistorySessionKey>),
    #[error("session not found")]
    NotFound,
    /// A payload could not be encoded or decoded. The message is fixed text,
    /// never captured content.
    #[error("history payload error: {0}")]
    Payload(&'static str),
    /// The store holds data that cannot be migrated or read losslessly.
    #[error("history store is corrupt: {0}")]
    Corrupt(&'static str),
    #[error("history store sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("history store io error: {0}")]
    Io(#[from] std::io::Error),
}

impl HistoryError {
    /// The wire error envelope for `agent-hooks history ...`.
    pub fn to_response(&self) -> HistoryErrorResponse {
        let (code, candidates) = match self {
            Self::Disabled(_) => (HistoryErrorCode::HistoryDisabled, Vec::new()),
            Self::SchemaUpgradeRequired { .. } => {
                (HistoryErrorCode::SchemaUpgradeRequired, Vec::new())
            }
            Self::AmbiguousSession(keys) => (HistoryErrorCode::AmbiguousSession, keys.clone()),
            Self::NotFound => (HistoryErrorCode::NotFound, Vec::new()),
            Self::InvalidArgument(_) => (HistoryErrorCode::InvalidArgument, Vec::new()),
            Self::UnsupportedNewerSchema { .. }
            | Self::Insecure(_)
            | Self::Payload(_)
            | Self::Corrupt(_)
            | Self::Sqlite(_)
            | Self::Io(_) => (HistoryErrorCode::StoreUnavailable, Vec::new()),
        };
        HistoryErrorResponse {
            api_version: HISTORY_API_VERSION,
            error: HistoryErrorBody {
                code,
                message: self.to_string(),
                candidates,
            },
        }
    }
}
