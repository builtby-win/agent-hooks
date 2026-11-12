//! Muse session logs:
//! `~/.local/share/muse/sessions/YYYY/MM/DD/<session>/session.jsonl`, plus
//! `<session>/subagent/<child>/session.jsonl` for subagents (each its own
//! session). Linux uses the same path under the XDG data dir.
//!
//! The log is an append-only event stream. A line is either a record or a
//! `retained_frame` wrapping several records as JSON strings. Conversation is
//! four durable records: `runtime.user_intent.accepted` (user text),
//! `assistant_message_committed`, `assistant_tool_calls_committed` and
//! `tool_result_batch_committed` (run events inside `runtime.session`).
//! Everything else (hooks, tasks, reasoning, tracing, markers) is ignored.

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
const HARNESS: &str = "muse";
const LOG_FILE: &str = "session.jsonl";

pub struct MuseImporter;

impl TranscriptImporter for MuseImporter {
    fn supported_os(&self) -> &'static [&'static str] {
        &["macos"]
    }

    fn discover(&self, home: &Path, since: Option<SystemTime>) -> Vec<PathBuf> {
        let mut files = Vec::new();
        collect_logs(&home.join(".local/share/muse/sessions"), &mut files);
        files.retain(|p| {
            since.is_none_or(|since| {
                std::fs::metadata(p)
                    .and_then(|m| m.modified())
                    .is_ok_and(|m| m >= since)
            })
        });
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
        let mut state = Session::default();
        let mut line = Vec::new();
        loop {
            line.clear();
            if reader.read_until(b'\n', &mut line)? == 0 {
                return Ok(progress);
            }
            let complete = line.last() == Some(&b'\n');
            match serde_json::from_slice::<Value>(&line) {
                Ok(row) => {
                    progress.skipped += state.map_line(&row, path, progress.offset, sink);
                }
                // A torn final write: leave it for the next import.
                Err(_) if !complete => return Ok(progress),
                Err(_) => progress.skipped += 1,
            }
            progress.offset += line.len() as u64;
        }
    }
}

/// Every `session.jsonl` below `dir`. Dot-directories (`.msp-view-*` are
/// derived snapshots, not logs) are not entered.
fn collect_logs(dir: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if !entry.file_name().to_string_lossy().starts_with('.') {
                collect_logs(&path, files);
            }
        } else if entry.file_name() == LOG_FILE {
            files.push(path);
        }
    }
}

/// State carried across the lines of one import pass.
#[derive(Default)]
struct Session {
    workspace: Option<String>,
    tool_names: HashMap<String, String>,
}

impl Session {
    /// Emit the captures of one log line; returns how many records looked
    /// like conversation but could not be mapped.
    fn map_line(
        &mut self,
        row: &Value,
        path: &Path,
        line_pos: u64,
        sink: &mut dyn FnMut(Capture),
    ) -> u64 {
        // Index of the next observation on this line: keeps delivery ids
        // distinct across the records of a frame and the parts of a record.
        let mut part = 0;
        let mut skipped = 0;
        let frame;
        let records: &[Value] = match row.get("children").and_then(Value::as_array) {
            Some(children) => {
                frame = children
                    .iter()
                    .map(|c| {
                        c.get("record_json")
                            .and_then(Value::as_str)
                            .and_then(|s| serde_json::from_str(s).ok())
                            .unwrap_or(Value::Null)
                    })
                    .collect::<Vec<_>>();
                &frame
            }
            None => std::slice::from_ref(row),
        };
        for record in records {
            if !self.map_record(record, path, line_pos, &mut part, sink) {
                skipped += 1;
            }
        }
        skipped
    }

    /// `false` when the record looked like conversation but lacked what a
    /// capture needs; records that are not conversation report `true`.
    fn map_record(
        &mut self,
        record: &Value,
        path: &Path,
        line_pos: u64,
        part: &mut usize,
        sink: &mut dyn FnMut(Capture),
    ) -> bool {
        // Retained markers carry no payload_type and are not conversation.
        let Some(payload_type) = record.get("payload_type").and_then(Value::as_str) else {
            return record.get("retained_marker").is_some();
        };
        let payload = &record["payload"];
        if payload_type == "runtime.session.metadata" {
            if let Some(root) = payload.pointer("/record/workspace_root").and_then(Value::as_str) {
                self.workspace = Some(root.to_string());
            }
            return true;
        }
        let event = &payload["event"];
        let conversation = match payload_type {
            "runtime.user_intent.accepted" => true,
            "runtime.session" => {
                payload["kind"] == "run"
                    && matches!(
                        event["kind"].as_str(),
                        Some(
                            "assistant_message_committed"
                                | "assistant_tool_calls_committed"
                                | "tool_result_batch_committed"
                        )
                    )
            }
            _ => false,
        };
        if !conversation {
            return true;
        }
        let Some(session_id) = record.pointer("/stream/id").and_then(Value::as_str) else {
            return false;
        };
        let timestamp_ms = record
            .get("recorded_at")
            .and_then(Value::as_u64)
            .map_or(0, |micros| micros / 1000);
        let workspace = self.workspace.clone();
        let capture = |delivery_id: String, entry: CaptureEntry| Capture {
            origin: LOCAL_ORIGIN.to_string(),
            harness: HARNESS.to_string(),
            session_id: session_id.to_string(),
            parent_session_id: None,
            workspace: workspace.clone(),
            title: None,
            status: "idle".to_string(),
            delivery_id,
            timestamp_ms,
            turn_id: None,
            entry: Some(entry),
        };
        let mut text = |kind: HistoryEntryKind, native_id: String, text: &str| {
            let delivery_id = text_delivery_id(path, line_pos, *part, kind.as_str());
            *part += 1;
            sink(capture(
                delivery_id,
                CaptureEntry {
                    kind,
                    text: Some(text.to_string()),
                    native_message_id: Some(native_id),
                    tool_call_id: None,
                    tool_name: None,
                    tool_status: None,
                },
            ));
        };
        let tool = |id: &str, name: Option<&str>, status, text| CaptureEntry {
            kind: HistoryEntryKind::Tool,
            text,
            native_message_id: None,
            tool_call_id: Some(id.to_string()),
            tool_name: name.map(str::to_string),
            tool_status: Some(status),
        };

        match (payload_type, event["kind"].as_str()) {
            ("runtime.user_intent.accepted", _) => {
                if payload.pointer("/semantic_kind/kind") != Some(&Value::from("chat")) {
                    return true;
                }
                let Some(intent_id) = payload.get("intent_id").and_then(Value::as_str) else {
                    return false;
                };
                let texts = payload["model_messages"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .flat_map(|m| m["content"].as_array().into_iter().flatten())
                    .filter(|p| p["kind"] == "text")
                    .filter_map(|p| p["text"].as_str())
                    .filter(|t| !t.trim().is_empty());
                for (idx, body) in texts.enumerate() {
                    let native_id = if idx == 0 {
                        intent_id.to_string()
                    } else {
                        format!("{intent_id}:{idx}")
                    };
                    text(HistoryEntryKind::User, native_id, body);
                }
            }
            (_, Some("assistant_message_committed")) => {
                let (Some(id), Some(body)) = (
                    event.get("message_id").and_then(Value::as_str),
                    event.get("text").and_then(Value::as_str),
                ) else {
                    return false;
                };
                if !body.trim().is_empty() {
                    text(HistoryEntryKind::Assistant, id.to_string(), body);
                }
            }
            (_, Some("assistant_tool_calls_committed")) => {
                for call in event["tool_calls"].as_array().into_iter().flatten() {
                    let (Some(id), Some(name)) = (
                        call.get("call_id").and_then(Value::as_str),
                        call.get("name").and_then(Value::as_str),
                    ) else {
                        return false;
                    };
                    self.tool_names.insert(id.to_string(), name.to_string());
                    // `args` is a JSON document in a string; normalize to
                    // compact JSON like every other harness.
                    let input = call
                        .get("args")
                        .and_then(Value::as_str)
                        .filter(|a| !a.trim().is_empty())
                        .map(|a| {
                            serde_json::from_str::<Value>(a).map_or_else(|_| a.to_string(), |v| v.to_string())
                        });
                    sink(capture(
                        tool_delivery_id(id, "running"),
                        tool(id, Some(name), HistoryToolStatus::Running, input),
                    ));
                }
            }
            _ => {
                for result in event["results"].as_array().into_iter().flatten() {
                    let Some(id) = result.get("tool_call_id").and_then(Value::as_str) else {
                        return false;
                    };
                    let output = result
                        .get("text")
                        .and_then(Value::as_str)
                        .filter(|t| !t.trim().is_empty())
                        .map(str::to_string);
                    sink(capture(
                        tool_delivery_id(id, "result"),
                        tool(
                            id,
                            self.tool_names.get(id).map(String::as_str),
                            // Muse results carry no error flag.
                            HistoryToolStatus::Completed,
                            output,
                        ),
                    ));
                }
            }
        }
        true
    }
}
