//! Amp threads on macOS: `~/.local/share/amp/threads/T-<id>.json`. Linux uses
//! `$XDG_DATA_HOME/amp/threads` (default `~/.local/share/amp/threads`;
//! unverified).
//!
//! A thread is one JSON document that Amp rewrites as it grows, so there is
//! no resumable offset: every import returns offset 0 and re-reads the whole
//! thread; deterministic delivery ids make that idempotent. `messages` holds
//! `user` / `assistant` turns whose content parts are `text`, `thinking`
//! (skipped), `tool_use`, `tool_result` and `image` (skipped). Only user
//! messages carry a timestamp (`meta.sentAt`, ms); later messages inherit it,
//! starting from the thread's `created`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use agent_hooks_protocol::{
    Capture, CaptureEntry, HistoryEntryKind, HistoryToolStatus, LOCAL_ORIGIN,
};
use serde_json::Value;

use crate::error::AdapterError;
use crate::importer::{text_delivery_id, tool_delivery_id, ImportProgress, TranscriptImporter};

/// Same id the live hook events use, so rows merge (`thread.id` is the
/// session id there).
const HARNESS: &str = "amp";

pub(crate) struct AmpImporter;

impl TranscriptImporter for AmpImporter {
    fn supported_os(&self) -> &'static [&'static str] {
        &["macos"]
    }

    fn discover(&self, home: &Path, since: Option<SystemTime>) -> Vec<PathBuf> {
        let Ok(dir) = std::fs::read_dir(home.join(".local/share/amp/threads")) else {
            return Vec::new();
        };
        let mut files: Vec<PathBuf> = dir
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "json"))
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
        _from_offset: u64,
        sink: &mut dyn FnMut(Capture),
    ) -> Result<ImportProgress, AdapterError> {
        let thread: Value = match serde_json::from_slice(&std::fs::read(path)?) {
            Ok(thread) => thread,
            Err(_) => return Ok(ImportProgress { offset: 0, skipped: 1 }),
        };
        let (Some(session_id), Some(messages)) = (
            thread.get("id").and_then(Value::as_str),
            thread.get("messages").and_then(Value::as_array),
        ) else {
            return Ok(ImportProgress { offset: 0, skipped: 1 });
        };
        let mut thread_ctx = Thread {
            id: session_id,
            workspace: thread
                .pointer("/env/initial/trees/0/uri")
                .and_then(Value::as_str)
                .and_then(|uri| uri.strip_prefix("file://")),
            title: thread.get("title").and_then(Value::as_str),
            timestamp_ms: thread.get("created").and_then(Value::as_u64).unwrap_or(0),
            tool_names: HashMap::new(),
        };
        let skipped = messages
            .iter()
            .enumerate()
            .filter(|(pos, message)| !thread_ctx.map_message(message, path, *pos as u64, sink))
            .count() as u64;
        Ok(ImportProgress { offset: 0, skipped })
    }
}

struct Thread<'a> {
    id: &'a str,
    workspace: Option<&'a str>,
    title: Option<&'a str>,
    timestamp_ms: u64,
    tool_names: HashMap<String, String>,
}

impl Thread<'_> {
    /// Emit the captures of one message. `false` when it could not be mapped
    /// (counted as skipped).
    fn map_message(
        &mut self,
        message: &Value,
        path: &Path,
        pos: u64,
        sink: &mut dyn FnMut(Capture),
    ) -> bool {
        let (kind, assistant) = match message.get("role").and_then(Value::as_str) {
            Some("user") => (HistoryEntryKind::User, false),
            Some("assistant") => (HistoryEntryKind::Assistant, true),
            _ => return false,
        };
        let (Some(message_id), Some(parts)) = (
            message.get("messageId").and_then(Value::as_u64),
            message.get("content").and_then(Value::as_array),
        ) else {
            return false;
        };
        if let Some(sent_at) = message.pointer("/meta/sentAt").and_then(Value::as_u64) {
            self.timestamp_ms = sent_at;
        }

        let capture = |delivery_id: String, entry: CaptureEntry| Capture {
            origin: LOCAL_ORIGIN.to_string(),
            harness: HARNESS.to_string(),
            session_id: self.id.to_string(),
            parent_session_id: None,
            workspace: self.workspace.map(str::to_string),
            title: self.title.map(str::to_string),
            status: "idle".to_string(),
            delivery_id,
            timestamp_ms: self.timestamp_ms,
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
                        message_id.to_string()
                    } else {
                        format!("{message_id}:{idx}")
                    };
                    sink(capture(
                        text_delivery_id(path, pos, idx, kind.as_str()),
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
                        ok = false;
                        continue;
                    };
                    self.tool_names.insert(id.to_string(), name.to_string());
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
                    let (Some(id), Some(run)) = (
                        part.get("toolUseID").and_then(Value::as_str),
                        part.get("run"),
                    ) else {
                        ok = false;
                        continue;
                    };
                    let (status, output) = match run.get("status").and_then(Value::as_str) {
                        Some("done") => (
                            HistoryToolStatus::Completed,
                            run.get("result").and_then(result_text),
                        ),
                        // Still running when the thread was saved: no outcome yet.
                        Some("in-progress") => continue,
                        _ => (HistoryToolStatus::Failed, failure_text(run)),
                    };
                    sink(capture(
                        tool_delivery_id(id, "result"),
                        tool(id, self.tool_names.get(id).map(String::as_str), status, output),
                    ));
                }
                _ => {}
            }
        }
        ok
    }
}

/// Tool output: a string verbatim, anything else compact JSON. Blank output
/// is no output.
fn result_text(result: &Value) -> Option<String> {
    let text = match result {
        Value::String(s) => s.clone(),
        Value::Null => return None,
        other => other.to_string(),
    };
    (!text.trim().is_empty()).then_some(text)
}

/// Why a run did not complete: its error message, else its reason, else the
/// bare status (`cancelled`, `rejected-by-user`).
fn failure_text(run: &Value) -> Option<String> {
    ["/error/message", "/reason", "/status"]
        .iter()
        .find_map(|p| run.pointer(p).and_then(Value::as_str))
        .map(str::to_string)
}
