//! Oh My Pi transcripts: `~/.omp/agent/sessions/<cwd-slug>/<ts>_<id>.jsonl`.
//!
//! Subagent runs live in nested directories beside their parent session and
//! are sessions of their own (own header id), so discovery is recursive. The
//! session format is shared with pi, see [`discover`] and [`import`].
//!
//! Linux: same layout under `~/.omp/agent/sessions` (not implemented yet).
//!
//! Rows: a `session` header (id, cwd) followed by tree entries. Only
//! `message` entries matter: `user` text, `assistant` text / `toolCall`
//! parts, and `toolResult` messages answering a `toolCall` by `toolCallId`.
//! Thinking, titles, model/usage bookkeeping and custom entries are ignored.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use agent_hooks_protocol::{
    Capture, CaptureEntry, HistoryEntryKind, HistoryToolStatus, LOCAL_ORIGIN,
};
use serde_json::Value;

use crate::error::AdapterError;
use crate::importer::{text_delivery_id, tool_delivery_id, ImportProgress, TranscriptImporter};

pub(crate) struct OmpImporter;

impl TranscriptImporter for OmpImporter {
    fn supported_os(&self) -> &'static [&'static str] {
        &["macos"]
    }

    fn discover(&self, home: &Path, since: Option<SystemTime>) -> Vec<PathBuf> {
        discover(&home.join(".omp").join("agent").join("sessions"), since)
    }

    fn import_file(
        &self,
        path: &Path,
        from_offset: u64,
        sink: &mut dyn FnMut(Capture),
    ) -> Result<ImportProgress, AdapterError> {
        import("omp", path, from_offset, sink)
    }
}

/// Every `.jsonl` under `root` (any depth), modified at or after `since`.
pub(super) fn discover(root: &Path, since: Option<SystemTime>) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            match entry.file_type() {
                Ok(t) if t.is_dir() => dirs.push(path),
                Ok(t) if t.is_file() && path.extension().is_some_and(|e| e == "jsonl") => {
                    let fresh = since.is_none_or(|since| {
                        entry
                            .metadata()
                            .and_then(|m| m.modified())
                            .is_ok_and(|m| m >= since)
                    });
                    if fresh {
                        files.push(path);
                    }
                }
                _ => {}
            }
        }
    }
    files.sort();
    files
}

/// The `session` header's id and cwd; it sits within the first few rows
/// (after an optional `title` row).
fn read_header(path: &Path) -> Option<(String, Option<String>)> {
    let reader = BufReader::new(File::open(path).ok()?);
    reader.split(b'\n').take(8).flatten().find_map(|line| {
        let row: Value = serde_json::from_slice(&line).ok()?;
        if row.get("type")?.as_str()? != "session" {
            return None;
        }
        let id = row.get("id")?.as_str().filter(|s| !s.is_empty())?;
        let cwd = row.get("cwd").and_then(Value::as_str);
        Some((id.to_string(), cwd.map(str::to_string)))
    })
}

/// Import one session file for the harness `harness`. The file is an
/// append-only JSONL stream, so the returned offset is resumable.
pub(super) fn import(
    harness: &str,
    path: &Path,
    from_offset: u64,
    sink: &mut dyn FnMut(Capture),
) -> Result<ImportProgress, AdapterError> {
    let mut progress = ImportProgress {
        offset: from_offset,
        skipped: 0,
    };
    let Some((session_id, cwd)) = read_header(path) else {
        progress.skipped += 1;
        return Ok(progress);
    };
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(from_offset))?;
    let mut reader = BufReader::new(file);
    let ctx = Context {
        harness,
        session_id: &session_id,
        cwd: cwd.as_deref(),
        path,
    };
    let mut tool_names = HashMap::new();
    let mut line = Vec::new();
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            return Ok(progress);
        }
        let complete = line.last() == Some(&b'\n');
        match serde_json::from_slice::<Value>(&line) {
            Ok(row) => {
                if !map_line(&ctx, &row, progress.offset, &mut tool_names, sink) {
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

struct Context<'a> {
    harness: &'a str,
    session_id: &'a str,
    cwd: Option<&'a str>,
    path: &'a Path,
}

/// Emit the captures of one row. `false` when the row is a message that could
/// not be mapped (counted as skipped); non-conversation rows report `true`.
fn map_line(
    ctx: &Context,
    row: &Value,
    line_pos: u64,
    tool_names: &mut HashMap<String, String>,
    sink: &mut dyn FnMut(Capture),
) -> bool {
    if row.get("type").and_then(Value::as_str) != Some("message") {
        return true;
    }
    let Some(message) = row.get("message") else {
        return false;
    };
    let role = message.get("role").and_then(Value::as_str);
    if !matches!(role, Some("user" | "assistant" | "toolResult")) {
        return true;
    }
    let (Some(entry_id), Some(ts)) = (
        row.get("id").and_then(Value::as_str),
        row.get("timestamp")
            .and_then(Value::as_str)
            .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok()),
    ) else {
        return false;
    };
    let timestamp_ms = u64::try_from(ts.timestamp_millis()).unwrap_or(0);

    let capture = |delivery_id: String, entry: CaptureEntry| Capture {
        origin: LOCAL_ORIGIN.to_string(),
        harness: ctx.harness.to_string(),
        session_id: ctx.session_id.to_string(),
        parent_session_id: None,
        workspace: ctx.cwd.map(str::to_string),
        title: None,
        status: "idle".to_string(),
        delivery_id,
        timestamp_ms,
        turn_id: None,
        entry: Some(entry),
    };
    let tool = |id: &str, name: Option<&str>, status, text| CaptureEntry {
        kind: HistoryEntryKind::Tool,
        text,
        native_message_id: None,
        tool_call_id: Some(id.to_string()),
        tool_name: name.map(str::to_string),
        tool_status: Some(status),
    };

    if role == Some("toolResult") {
        let Some(id) = message.get("toolCallId").and_then(Value::as_str) else {
            return false;
        };
        let status = if message.get("isError").and_then(Value::as_bool) == Some(true) {
            HistoryToolStatus::Failed
        } else {
            HistoryToolStatus::Completed
        };
        let name = message
            .get("toolName")
            .and_then(Value::as_str)
            .or_else(|| tool_names.get(id).map(String::as_str));
        let output = text_of(message.get("content"));
        sink(capture(
            tool_delivery_id(id, "result"),
            tool(id, name, status, output),
        ));
        return true;
    }

    let kind = if role == Some("user") {
        HistoryEntryKind::User
    } else {
        HistoryEntryKind::Assistant
    };
    let content = message.get("content");
    let single = [serde_json::json!({"type": "text", "text": content})];
    let parts: &[Value] = match content {
        Some(Value::String(_)) => &single,
        Some(Value::Array(parts)) => parts,
        _ => return false,
    };
    for (idx, part) in parts.iter().enumerate() {
        match part.get("type").and_then(Value::as_str) {
            Some("text") => {
                let Some(text) = part
                    .get("text")
                    .and_then(Value::as_str)
                    .filter(|t| !t.trim().is_empty())
                else {
                    continue;
                };
                let native_message_id = if idx == 0 {
                    entry_id.to_string()
                } else {
                    format!("{entry_id}:{idx}")
                };
                sink(capture(
                    text_delivery_id(ctx.path, line_pos, idx, kind.as_str()),
                    CaptureEntry {
                        kind,
                        text: Some(text.to_string()),
                        native_message_id: Some(native_message_id),
                        tool_call_id: None,
                        tool_name: None,
                        tool_status: None,
                    },
                ));
            }
            Some("toolCall") if kind == HistoryEntryKind::Assistant => {
                let (Some(id), Some(name)) = (
                    part.get("id").and_then(Value::as_str),
                    part.get("name").and_then(Value::as_str),
                ) else {
                    return false;
                };
                tool_names.insert(id.to_string(), name.to_string());
                let input = part.get("arguments").filter(|a| !a.is_null());
                sink(capture(
                    tool_delivery_id(id, "running"),
                    tool(
                        id,
                        Some(name),
                        HistoryToolStatus::Running,
                        input.map(Value::to_string),
                    ),
                ));
            }
            _ => {}
        }
    }
    true
}

/// A tool result's text blocks joined; blank output is no output.
fn text_of(content: Option<&Value>) -> Option<String> {
    let text = match content? {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => return None,
    };
    (!text.trim().is_empty()).then_some(text)
}
