//! GitHub Copilot CLI sessions:
//! `~/.copilot/session-state/<session>/events.jsonl`
//! (Linux: same path under `~/.copilot`; not enabled yet).
//!
//! An append-only event log (`~/.copilot/pkg/universal/*/schemas/
//! session-events.schema.json`). Conversation lives in `user.message`,
//! `assistant.message` (text) and `tool.execution_start` / `_complete`
//! (tool calls keyed by `toolCallId`). Reasoning, hooks, turn and session
//! bookkeeping events are ignored. The workspace comes from the sibling
//! `workspace.yaml`.

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
const HARNESS: &str = "copilot";

pub(crate) struct CopilotImporter;

impl TranscriptImporter for CopilotImporter {
    fn supported_os(&self) -> &'static [&'static str] {
        &["macos"]
    }

    fn discover(&self, home: &Path, since: Option<SystemTime>) -> Vec<PathBuf> {
        let Ok(sessions) = std::fs::read_dir(home.join(".copilot").join("session-state")) else {
            return Vec::new();
        };
        let mut files: Vec<PathBuf> = sessions
            .flatten()
            .map(|s| s.path().join("events.jsonl"))
            .filter(|p| {
                std::fs::metadata(p)
                    .and_then(|m| m.modified())
                    .is_ok_and(|m| since.is_none_or(|since| m >= since))
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
        let mut progress = ImportProgress {
            offset: from_offset,
            skipped: 0,
        };
        let Some(session_dir) = path.parent() else {
            return Ok(progress);
        };
        let Some(session_id) = session_dir.file_name().and_then(|n| n.to_str()) else {
            return Ok(progress);
        };
        let workspace = std::fs::read_to_string(session_dir.join("workspace.yaml"))
            .ok()
            .and_then(|yaml| {
                yaml.lines()
                    .find_map(|l| l.strip_prefix("cwd:"))
                    .map(|cwd| cwd.trim().to_string())
            })
            .filter(|cwd| !cwd.is_empty());

        let mut file = File::open(path)?;
        file.seek(SeekFrom::Start(from_offset))?;
        let mut reader = BufReader::new(file);
        let mut tool_names = std::collections::HashMap::new();
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
                        workspace: workspace.as_deref(),
                        line_pos,
                    };
                    if !map_line(&row, &ctx, &mut tool_names, sink) {
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
    workspace: Option<&'a str>,
    line_pos: u64,
}

/// Emit the captures of one event. `false` when the event looked like
/// conversation but could not be mapped; other event types report `true`.
fn map_line(
    row: &Value,
    ctx: &Row,
    tool_names: &mut std::collections::HashMap<String, String>,
    sink: &mut dyn FnMut(Capture),
) -> bool {
    let Some(event_type) = row.get("type").and_then(Value::as_str) else {
        return false;
    };
    if !matches!(
        event_type,
        "user.message" | "assistant.message" | "tool.execution_start" | "tool.execution_complete"
    ) {
        return true;
    }
    let data = row.get("data").unwrap_or(&Value::Null);
    let str_field = |v: &'_ Value, k: &str| {
        v.get(k)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let Some(ts) = str_field(row, "timestamp")
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(&t).ok())
    else {
        return false;
    };
    let capture = |delivery_id: String, entry: CaptureEntry| Capture {
        origin: LOCAL_ORIGIN.to_string(),
        harness: HARNESS.to_string(),
        session_id: ctx.session_id.to_string(),
        parent_session_id: None,
        workspace: ctx.workspace.map(str::to_string),
        title: None,
        status: "idle".to_string(),
        delivery_id,
        timestamp_ms: u64::try_from(ts.timestamp_millis()).unwrap_or(0),
        turn_id: None,
        entry: Some(entry),
    };
    let text_entry = |kind, text, native_message_id| CaptureEntry {
        kind,
        text: Some(text),
        native_message_id,
        tool_call_id: None,
        tool_name: None,
        tool_status: None,
    };

    match event_type {
        "user.message" | "assistant.message" => {
            let (kind, native_message_id) = if event_type == "user.message" {
                (HistoryEntryKind::User, str_field(row, "id"))
            } else {
                (HistoryEntryKind::Assistant, str_field(data, "messageId"))
            };
            let Some(text) = data.get("content").and_then(Value::as_str) else {
                return false;
            };
            if !text.trim().is_empty() {
                sink(capture(
                    text_delivery_id(ctx.path, ctx.line_pos, 0, kind.as_str()),
                    text_entry(kind, text.to_string(), native_message_id),
                ));
            }
        }
        "tool.execution_start" => {
            let (Some(id), Some(name)) = (str_field(data, "toolCallId"), str_field(data, "toolName"))
            else {
                return false;
            };
            let input = data.get("arguments").filter(|a| !a.is_null());
            sink(capture(
                tool_delivery_id(&id, "running"),
                CaptureEntry {
                    kind: HistoryEntryKind::Tool,
                    text: input.map(Value::to_string),
                    native_message_id: None,
                    tool_call_id: Some(id.clone()),
                    tool_name: Some(name.clone()),
                    tool_status: Some(HistoryToolStatus::Running),
                },
            ));
            tool_names.insert(id, name);
        }
        _ => {
            let (Some(id), Some(success)) = (
                str_field(data, "toolCallId"),
                data.get("success").and_then(Value::as_bool),
            ) else {
                return false;
            };
            let (status, output) = if success {
                (
                    HistoryToolStatus::Completed,
                    data.pointer("/result/content").and_then(Value::as_str),
                )
            } else {
                (
                    HistoryToolStatus::Failed,
                    data.pointer("/error/message").and_then(Value::as_str),
                )
            };
            sink(capture(
                tool_delivery_id(&id, "result"),
                CaptureEntry {
                    kind: HistoryEntryKind::Tool,
                    text: output.filter(|o| !o.trim().is_empty()).map(str::to_string),
                    native_message_id: None,
                    tool_name: tool_names.get(&id).cloned(),
                    tool_call_id: Some(id),
                    tool_status: Some(status),
                },
            ));
        }
    }
    true
}
