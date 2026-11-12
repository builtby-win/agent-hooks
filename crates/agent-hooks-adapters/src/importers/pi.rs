//! Pi transcripts: `~/.pi/agent/sessions/<cwd-slug>/<ts>_<id>.jsonl`.
//!
//! Same session JSONL format as omp, parsed by the shared helper in
//! [`super::omp`].
//!
//! Linux: same layout under `~/.pi/agent/sessions` (not implemented yet).

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use agent_hooks_protocol::Capture;

use super::omp;
use crate::error::AdapterError;
use crate::importer::{ImportProgress, TranscriptImporter};

pub(crate) struct PiImporter;

impl TranscriptImporter for PiImporter {
    fn supported_os(&self) -> &'static [&'static str] {
        &["macos"]
    }

    fn discover(&self, home: &Path, since: Option<SystemTime>) -> Vec<PathBuf> {
        omp::discover(&home.join(".pi").join("agent").join("sessions"), since)
    }

    fn import_file(
        &self,
        path: &Path,
        from_offset: u64,
        sink: &mut dyn FnMut(Capture),
    ) -> Result<ImportProgress, AdapterError> {
        omp::import("pi", path, from_offset, sink)
    }
}
