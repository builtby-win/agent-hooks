//! # agent-hooks-history
//!
//! Opt-in SQLite store of hook observations (user prompts, assistant `Stop`
//! summaries, and tool inputs/outputs). Payload compression is lossless:
//! nothing in this crate trims, caps, or truncates captured strings.

mod capture;
mod error;
mod payload;
mod reader;
mod search;
mod store;
mod writer;

use std::path::{Path, PathBuf};

pub use capture::{Capture, CaptureEntry};
pub use error::HistoryError;
pub use reader::{HistoryReader, MessagesQuery, SessionsQuery};
pub use search::MessageSearchQuery;
pub use store::{init_store, InitReport};
pub use writer::{HistoryWriter, RecordOutcome};

/// `PRAGMA user_version` of the schema this crate reads and writes.
pub const SCHEMA_VERSION: u32 = 2;
/// Database file name inside the history directory.
pub const DB_FILE: &str = "history.sqlite3";
/// Environment variable overriding the history directory.
pub const ENV_HISTORY_DIR: &str = "AGENT_HOOKS_HISTORY_DIR";

/// Resolve the history directory: explicit override, then
/// [`ENV_HISTORY_DIR`], then `~/.agent-hooks/history` (`HOME`, falling back
/// to `USERPROFILE`). When no home directory is known the result is the
/// relative path `.agent-hooks/history`.
pub fn resolve_history_dir(override_dir: Option<&Path>) -> PathBuf {
    if let Some(dir) = override_dir {
        return dir.to_path_buf();
    }
    if let Some(dir) = std::env::var_os(ENV_HISTORY_DIR).filter(|v| !v.is_empty()) {
        return PathBuf::from(dir);
    }
    let home = std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .or_else(|| std::env::var_os("USERPROFILE").filter(|v| !v.is_empty()));
    match home {
        Some(home) => PathBuf::from(home).join(".agent-hooks").join("history"),
        None => PathBuf::from(".agent-hooks").join("history"),
    }
}
