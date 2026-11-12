//! Optional transcript backfill: an adapter that can read its harness's
//! on-disk transcripts exposes a [`TranscriptImporter`] through
//! [`crate::HarnessAdapter::transcript_importer`]. Adapters without one are
//! simply not backfilled.
//!
//! # Contract for importers
//!
//! * Stream: [`TranscriptImporter::import_file`] reads line by line and hands
//!   each observation to the sink as an [`agent_hooks_protocol::Capture`].
//! * One *observation* per delivery id, never one per line (a line may carry
//!   text plus several tool parts). Ids must be deterministic so re-imports
//!   are idempotent, see [`text_delivery_id`] and [`tool_delivery_id`].
//! * `native_message_id` / `tool_call_id` come from the transcript's own ids
//!   so backfilled rows merge with live-captured rows.
//! * Only consume complete lines: the returned offset is where the next
//!   import resumes.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use agent_hooks_protocol::{stable_hash, Capture};

use crate::error::AdapterError;

/// Result of importing one transcript file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ImportProgress {
    /// Byte offset just past the last fully consumed line.
    pub offset: u64,
    /// Lines that looked like messages but could not be mapped.
    pub skipped: u64,
}

pub trait TranscriptImporter: Send + Sync {
    /// `std::env::consts::OS` values (`macos`, `linux`, `windows`) this
    /// importer knows the transcript locations for. `history backfill`
    /// reports `unsupported os` on any other OS instead of scanning.
    fn supported_os(&self) -> &'static [&'static str];

    /// Transcript files under `home`, optionally only those modified at or
    /// after `since`. Order is deterministic.
    fn discover(&self, home: &Path, since: Option<SystemTime>) -> Vec<PathBuf>;

    /// Stream the observations of `path` starting at byte `from_offset`
    /// (always a line boundary previously returned as `offset`) into `sink`.
    fn import_file(
        &self,
        path: &Path,
        from_offset: u64,
        sink: &mut dyn FnMut(Capture),
    ) -> Result<ImportProgress, AdapterError>;
}

/// Importer for a builtin adapter id, if one ships. Add new importers here.
pub(crate) fn builtin(adapter_id: &str) -> Option<Box<dyn TranscriptImporter>> {
    match adapter_id {
        "claude-code" => Some(Box::new(crate::importers::claude_code::ClaudeCodeImporter)),
        "codex" => Some(Box::new(crate::importers::codex::CodexImporter)),
        "omp" => Some(Box::new(crate::importers::omp::OmpImporter)),
        "pi" => Some(Box::new(crate::importers::pi::PiImporter)),
        "antigravity" => Some(Box::new(crate::importers::antigravity::AntigravityImporter)),
        "gemini-cli" => Some(Box::new(crate::importers::gemini_cli::GeminiCliImporter)),
        "cursor" => Some(Box::new(crate::importers::cursor::CursorImporter)),
        "copilot" => Some(Box::new(crate::importers::copilot::CopilotImporter)),
        "kiro" => Some(Box::new(crate::importers::kiro::KiroImporter)),
        "amp" => Some(Box::new(crate::importers::amp::AmpImporter)),
        "muse" => Some(Box::new(crate::importers::muse::MuseImporter)),
        _ => None,
    }
}

/// Delivery id of a text observation: `part_idx` distinguishes parts of one
/// line, `line_pos` (byte offset of the line start) is stable under resume.
pub fn text_delivery_id(path: &Path, line_pos: u64, part_idx: usize, kind: &str) -> String {
    let path_hash = stable_hash(path.to_string_lossy().as_bytes());
    format!("backfill:{path_hash:016x}:{line_pos}:{part_idx}:{kind}")
}

/// Delivery id of a tool observation; `phase` is `running` or `result`.
pub fn tool_delivery_id(tool_call_id: &str, phase: &str) -> String {
    format!("backfill:{tool_call_id}:{phase}")
}
