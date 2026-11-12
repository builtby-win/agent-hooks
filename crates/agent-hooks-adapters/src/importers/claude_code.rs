//! Claude Code transcripts: `~/.claude/projects/<project>/<session>.jsonl`.
//!
//! Each assistant content block is its own line; user lines carry either a
//! prompt (string or text blocks) or `tool_result` blocks answering an
//! earlier `tool_use`. Everything else (attachments, snapshots, titles,
//! sidechains, meta rows, thinking) is not conversation and is ignored.

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

/// Same id the live hook events use, so rows merge.
const HARNESS: &str = "claude-code";

pub struct ClaudeCodeImporter;

impl TranscriptImporter for ClaudeCodeImporter {
    fn supported_os(&self) -> &'static [&'static str] {
        &["macos", "linux"]
    }

    fn discover(&self, home: &Path, since: Option<SystemTime>) -> Vec<PathBuf> {
        let Ok(projects) = std::fs::read_dir(home.join(".claude").join("projects")) else {
            return Vec::new();
        };
        let mut files: Vec<PathBuf> = projects
            .flatten()
            .filter_map(|p| std::fs::read_dir(p.path()).ok())
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
        let mut file = File::open(path)?;
        file.seek(SeekFrom::Start(from_offset))?;
        let mut reader = BufReader::new(file);
        let mut progress = ImportProgress {
            offset: from_offset,
            skipped: 0,
        };
        let mut tool_names = HashMap::new();
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
                    if !map_line(&row, path, line_pos, &mut tool_names, sink) {
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

/// Emit the captures of one transcript row. `false` when the row looked like
/// a message but could not be mapped (counted as skipped); rows that are not
/// conversation are ignored and report `true`.
fn map_line(
    row: &Value,
    path: &Path,
    line_pos: u64,
    tool_names: &mut HashMap<String, String>,
    sink: &mut dyn FnMut(Capture),
) -> bool {
    let (kind, assistant) = match row.get("type").and_then(Value::as_str) {
        Some("user") => (HistoryEntryKind::User, false),
        Some("assistant") => (HistoryEntryKind::Assistant, true),
        _ => return true,
    };
    let flag = |k: &str| row.get(k).and_then(Value::as_bool).unwrap_or(false);
    if flag("isSidechain") || flag("isMeta") {
        return true;
    }
    let str_field = |k: &str| row.get(k).and_then(Value::as_str).filter(|s| !s.is_empty());
    let (Some(session_id), Some(uuid), Some(ts)) = (
        str_field("sessionId"),
        str_field("uuid"),
        str_field("timestamp").and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok()),
    ) else {
        return false;
    };
    let timestamp_ms = u64::try_from(ts.timestamp_millis()).unwrap_or(0);
    let Some(content) = row.pointer("/message/content") else {
        return false;
    };
    let single = [serde_json::json!({"type": "text", "text": content})];
    let parts: &[Value] = match content {
        Value::String(_) => &single,
        Value::Array(parts) => parts,
        _ => return false,
    };

    let capture = |delivery_id: String, entry: CaptureEntry| Capture {
        origin: LOCAL_ORIGIN.to_string(),
        harness: HARNESS.to_string(),
        session_id: session_id.to_string(),
        parent_session_id: None,
        workspace: str_field("cwd").map(str::to_string),
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
                    uuid.to_string()
                } else {
                    format!("{uuid}:{idx}")
                };
                sink(capture(
                    text_delivery_id(path, line_pos, idx, kind.as_str()),
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
            Some("tool_use") if assistant => {
                let (Some(id), Some(name)) = (
                    part.get("id").and_then(Value::as_str),
                    part.get("name").and_then(Value::as_str),
                ) else {
                    return false;
                };
                tool_names.insert(id.to_string(), name.to_string());
                let input = part.get("input").filter(|i| !i.is_null());
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
            Some("tool_result") if !assistant => {
                let Some(id) = part.get("tool_use_id").and_then(Value::as_str) else {
                    return false;
                };
                let status = if part.get("is_error").and_then(Value::as_bool) == Some(true) {
                    HistoryToolStatus::Failed
                } else {
                    HistoryToolStatus::Completed
                };
                let output = tool_result_text(part.get("content"));
                sink(capture(
                    tool_delivery_id(id, "result"),
                    tool(id, tool_names.get(id).map(String::as_str), status, output),
                ));
            }
            _ => {}
        }
    }
    true
}

/// A tool result is a string or an array of blocks; only text blocks carry
/// output. Blank output is no output.
fn tool_result_text(content: Option<&Value>) -> Option<String> {
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
