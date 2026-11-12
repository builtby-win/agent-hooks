//! Kiro CLI sessions on macOS: `~/.kiro/sessions/cli/<session>.jsonl` with a
//! `<session>.json` sidecar (cwd, created/updated times). Linux uses the same
//! `~/.kiro/sessions/cli` layout (unverified).
//!
//! The jsonl is an append-only event log: `Prompt`, `AssistantMessage` and
//! `ToolResults` rows whose `content` parts are `text`, `toolUse` or
//! `toolResult`. Only prompts carry a timestamp (`meta.timestamp`, seconds);
//! every other row inherits the latest one seen, starting from the sidecar.
//! `Clear` and any other row kind are not conversation and are ignored.

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
const HARNESS: &str = "kiro";

pub(crate) struct KiroImporter;

impl TranscriptImporter for KiroImporter {
    fn supported_os(&self) -> &'static [&'static str] {
        &["macos"]
    }

    fn discover(&self, home: &Path, since: Option<SystemTime>) -> Vec<PathBuf> {
        let Ok(dir) = std::fs::read_dir(home.join(".kiro").join("sessions").join("cli")) else {
            return Vec::new();
        };
        let mut files: Vec<PathBuf> = dir
            .flatten()
            .map(|e| e.path())
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
        let Some(session_id) = path.file_stem().and_then(|s| s.to_str()) else {
            return Ok(ImportProgress {
                offset: from_offset,
                skipped: 0,
            });
        };
        let sidecar: Value = std::fs::read(path.with_extension("json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or(Value::Null);
        let sidecar_ms = |key: &str| {
            sidecar
                .get(key)
                .and_then(Value::as_str)
                .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
                .map_or(0, |t| u64::try_from(t.timestamp_millis()).unwrap_or(0))
        };
        let mut session = Session {
            id: session_id,
            workspace: sidecar.get("cwd").and_then(Value::as_str),
            title: sidecar.get("title").and_then(Value::as_str),
            // A resumed import starts mid-log, past the prompt that set the clock.
            timestamp_ms: sidecar_ms(if from_offset == 0 { "created_at" } else { "updated_at" }),
            tool_names: HashMap::new(),
        };

        let mut file = File::open(path)?;
        file.seek(SeekFrom::Start(from_offset))?;
        let mut reader = BufReader::new(file);
        let mut progress = ImportProgress {
            offset: from_offset,
            skipped: 0,
        };
        let mut line = Vec::new();
        loop {
            line.clear();
            if reader.read_until(b'\n', &mut line)? == 0 {
                return Ok(progress);
            }
            let complete = line.last() == Some(&b'\n');
            match serde_json::from_slice::<Value>(&line) {
                Ok(row) => {
                    if !session.map_line(&row, path, progress.offset, sink) {
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

struct Session<'a> {
    id: &'a str,
    workspace: Option<&'a str>,
    title: Option<&'a str>,
    timestamp_ms: u64,
    tool_names: HashMap<String, String>,
}

impl Session<'_> {
    /// Emit the captures of one row. `false` when the row looked like a
    /// message but could not be mapped (counted as skipped).
    fn map_line(
        &mut self,
        row: &Value,
        path: &Path,
        line_pos: u64,
        sink: &mut dyn FnMut(Capture),
    ) -> bool {
        let kind = match row.get("kind").and_then(Value::as_str) {
            Some("Prompt") => HistoryEntryKind::User,
            Some("AssistantMessage") => HistoryEntryKind::Assistant,
            Some("ToolResults") => HistoryEntryKind::Tool,
            _ => return true,
        };
        let data = row.get("data");
        let (Some(message_id), Some(parts)) = (
            data.and_then(|d| d.get("message_id")).and_then(Value::as_str),
            data.and_then(|d| d.get("content")).and_then(Value::as_array),
        ) else {
            return false;
        };
        if let Some(secs) = data
            .and_then(|d| d.pointer("/meta/timestamp"))
            .and_then(Value::as_u64)
        {
            self.timestamp_ms = secs * 1000;
        }

        let capture = |delivery_id: String, timestamp_ms: u64, entry: CaptureEntry| Capture {
            origin: LOCAL_ORIGIN.to_string(),
            harness: HARNESS.to_string(),
            session_id: self.id.to_string(),
            parent_session_id: None,
            workspace: self.workspace.map(str::to_string),
            title: self.title.map(str::to_string),
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

        let mut ok = true;
        for (idx, part) in parts.iter().enumerate() {
            let data = part.get("data");
            match part.get("kind").and_then(Value::as_str) {
                Some("text") if kind != HistoryEntryKind::Tool => {
                    let Some(text) = data
                        .and_then(Value::as_str)
                        .filter(|t| !t.trim().is_empty())
                    else {
                        continue;
                    };
                    let native_message_id = if idx == 0 {
                        message_id.to_string()
                    } else {
                        format!("{message_id}:{idx}")
                    };
                    sink(capture(
                        text_delivery_id(path, line_pos, idx, kind.as_str()),
                        self.timestamp_ms,
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
                Some("toolUse") => {
                    let (Some(id), Some(name)) = (
                        data.and_then(|d| d.get("toolUseId")).and_then(Value::as_str),
                        data.and_then(|d| d.get("name")).and_then(Value::as_str),
                    ) else {
                        ok = false;
                        continue;
                    };
                    self.tool_names.insert(id.to_string(), name.to_string());
                    let input = data.and_then(|d| d.get("input")).filter(|i| !i.is_null());
                    sink(capture(
                        tool_delivery_id(id, "running"),
                        self.timestamp_ms,
                        tool(
                            id,
                            Some(name),
                            HistoryToolStatus::Running,
                            input.map(Value::to_string),
                        ),
                    ));
                }
                Some("toolResult") => {
                    let Some(id) = data.and_then(|d| d.get("toolUseId")).and_then(Value::as_str)
                    else {
                        ok = false;
                        continue;
                    };
                    let status = if data.and_then(|d| d.get("status")).and_then(Value::as_str)
                        == Some("error")
                    {
                        HistoryToolStatus::Failed
                    } else {
                        HistoryToolStatus::Completed
                    };
                    sink(capture(
                        tool_delivery_id(id, "result"),
                        self.timestamp_ms,
                        tool(
                            id,
                            self.tool_names.get(id).map(String::as_str),
                            status,
                            tool_result_text(data.and_then(|d| d.get("content"))),
                        ),
                    ));
                }
                _ => {}
            }
        }
        ok
    }
}

/// Result content is a list of `text` / `json` parts; text is verbatim, json
/// compact. Blank output is no output.
fn tool_result_text(content: Option<&Value>) -> Option<String> {
    let text = content?
        .as_array()?
        .iter()
        .filter_map(|p| match (p.get("kind")?.as_str()?, p.get("data")?) {
            ("text", Value::String(s)) => Some(s.clone()),
            ("json", v) => Some(v.to_string()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    (!text.trim().is_empty()).then_some(text)
}
