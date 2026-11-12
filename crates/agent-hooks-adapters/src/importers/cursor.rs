//! Cursor agent transcripts:
//! `~/.cursor/projects/<project>/agent-transcripts/<session>/<session>.jsonl`
//! (Linux: same path under `~/.cursor`; not enabled yet).
//!
//! The session id is the directory name (the `conversation_id` live hooks
//! report). Rows are `{"role", "message": {"content": [parts]}}` with `text`
//! and `tool_use` parts; transcripts carry no tool ids, no tool results and
//! no timestamps. Tool calls therefore get a synthetic id from their position
//! and are imported as running only. The only clock is the
//! `<timestamp>…(UTC-7)</timestamp>` prefix of user prompts; later rows reuse
//! it and the file mtime stands in before the first one. The project folder
//! name is a lossy slug of the workspace path, so no workspace is recorded.

use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use agent_hooks_protocol::{
    Capture, CaptureEntry, HistoryEntryKind, HistoryToolStatus, LOCAL_ORIGIN,
};
use chrono::{FixedOffset, NaiveDateTime, TimeZone};
use serde_json::Value;

use crate::error::AdapterError;
use crate::importer::{text_delivery_id, tool_delivery_id, ImportProgress, TranscriptImporter};

/// Same id the live hook events use, so rows merge.
const HARNESS: &str = "cursor";

pub(crate) struct CursorImporter;

impl TranscriptImporter for CursorImporter {
    fn supported_os(&self) -> &'static [&'static str] {
        &["macos"]
    }

    fn discover(&self, home: &Path, since: Option<SystemTime>) -> Vec<PathBuf> {
        let Ok(projects) = std::fs::read_dir(home.join(".cursor").join("projects")) else {
            return Vec::new();
        };
        let mut files: Vec<PathBuf> = projects
            .flatten()
            .filter_map(|p| std::fs::read_dir(p.path().join("agent-transcripts")).ok())
            .flat_map(|dir| dir.flatten())
            .filter_map(|session| std::fs::read_dir(session.path()).ok())
            .flat_map(|dir| dir.flatten().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
            .filter(|p| {
                since.is_none_or(|since| {
                    std::fs::metadata(p)
                        .and_then(|m| m.modified())
                        .is_ok_and(|m| m >= since)
                })
            })
            .collect();
        files.sort();
        files
    }

    fn import_file(
        &self,
        path: &Path,
        from_offset: u64,
        sink: &mut dyn FnMut(Capture),
    ) -> Result<ImportProgress, AdapterError> {
        let Some(session_id) = path
            .parent()
            .and_then(Path::file_name)
            .and_then(|n| n.to_str())
        else {
            return Ok(ImportProgress {
                offset: from_offset,
                skipped: 0,
            });
        };
        let mut file = File::open(path)?;
        let mtime_ms = file
            .metadata()?
            .modified()?
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(0));
        file.seek(SeekFrom::Start(from_offset))?;
        let mut reader = BufReader::new(file);
        let mut progress = ImportProgress {
            offset: from_offset,
            skipped: 0,
        };
        let mut timestamp_ms = mtime_ms;
        let mut line = Vec::new();
        loop {
            line.clear();
            if reader.read_until(b'\n', &mut line)? == 0 {
                return Ok(progress);
            }
            let line_pos = progress.offset;
            let complete = line.last() == Some(&b'\n');
            match serde_json::from_slice::<Value>(&line) {
                Ok(row) => {
                    let ctx = Row {
                        path,
                        session_id,
                        line_pos,
                    };
                    if !map_line(&row, &ctx, &mut timestamp_ms, sink) {
                        progress.skipped += 1;
                    }
                }
                // A torn final write: leave it for the next import.
                Err(_) if !complete => return Ok(progress),
                Err(_) => progress.skipped += 1,
            }
            progress.offset += line.len() as u64;
        }
    }
}

struct Row<'a> {
    path: &'a Path,
    session_id: &'a str,
    line_pos: u64,
}

/// Emit the captures of one transcript row. `false` when the row looked like
/// a message but could not be mapped; rows without a role (status/error
/// markers) are not conversation and report `true`.
fn map_line(
    row: &Value,
    ctx: &Row,
    timestamp_ms: &mut u64,
    sink: &mut dyn FnMut(Capture),
) -> bool {
    let (kind, assistant) = match row.get("role").and_then(Value::as_str) {
        Some("user") => (HistoryEntryKind::User, false),
        Some("assistant") => (HistoryEntryKind::Assistant, true),
        _ => return true,
    };
    let Some(parts) = row.pointer("/message/content").and_then(Value::as_array) else {
        return false;
    };

    for (idx, part) in parts.iter().enumerate() {
        if let Some(ms) = prompt_timestamp(part) {
            *timestamp_ms = ms;
        }
        let at = *timestamp_ms;
        let capture = |delivery_id: String, entry: CaptureEntry| Capture {
            origin: LOCAL_ORIGIN.to_string(),
            harness: HARNESS.to_string(),
            session_id: ctx.session_id.to_string(),
            parent_session_id: None,
            workspace: None,
            title: None,
            status: "idle".to_string(),
            delivery_id,
            timestamp_ms: at,
            turn_id: None,
            entry: Some(entry),
        };
        match part.get("type").and_then(Value::as_str) {
            Some("text") => {
                let Some(text) = part.get("text").and_then(Value::as_str) else {
                    return false;
                };
                let text = if assistant { text } else { user_query(text) };
                if text.trim().is_empty() {
                    continue;
                }
                sink(capture(
                    text_delivery_id(ctx.path, ctx.line_pos, idx, kind.as_str()),
                    CaptureEntry {
                        kind,
                        text: Some(text.to_string()),
                        native_message_id: None,
                        tool_call_id: None,
                        tool_name: None,
                        tool_status: None,
                    },
                ));
            }
            Some("tool_use") if assistant => {
                let Some(name) = part.get("name").and_then(Value::as_str) else {
                    return false;
                };
                let id = format!("cursor:{}:{}:{idx}", ctx.session_id, ctx.line_pos);
                let input = part.get("input").filter(|i| !i.is_null());
                sink(capture(
                    tool_delivery_id(&id, "running"),
                    CaptureEntry {
                        kind: HistoryEntryKind::Tool,
                        text: input.map(Value::to_string),
                        native_message_id: None,
                        tool_call_id: Some(id),
                        tool_name: Some(name.to_string()),
                        tool_status: Some(HistoryToolStatus::Running),
                    },
                ));
            }
            _ => {}
        }
    }
    true
}

/// The prompt typed by the user: the body of the `<user_query>` wrapper
/// Cursor adds, or the text itself when unwrapped.
fn user_query(text: &str) -> &str {
    text.split_once("<user_query>")
        .and_then(|(_, rest)| rest.rsplit_once("</user_query>"))
        .map_or(text, |(body, _)| body.trim())
}

/// Parse the `<timestamp>Monday, Sep 7, 2026, 8:38 PM (UTC-7)</timestamp>`
/// prefix of a user text part.
fn prompt_timestamp(part: &Value) -> Option<u64> {
    let inner = part
        .get("text")?
        .as_str()?
        .strip_prefix("<timestamp>")?
        .split_once("</timestamp>")?
        .0;
    let (local, offset) = inner.split_once(" (UTC")?;
    let hours: i32 = offset.strip_suffix(')')?.parse().ok()?;
    let local = NaiveDateTime::parse_from_str(local, "%A, %b %d, %Y, %I:%M %p").ok()?;
    let zone = FixedOffset::east_opt(hours * 3600)?;
    let utc = zone.from_local_datetime(&local).single()?;
    u64::try_from(utc.timestamp_millis()).ok()
}
