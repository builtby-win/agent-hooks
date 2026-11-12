//! Gemini CLI sessions: `~/.gemini/tmp/<project>/chats/session-*.{json,jsonl}`
//! and subagent sessions in `chats/<parent-session>/*.jsonl`.
//! Linux: `~/.gemini/tmp/...` (same layout, not implemented). The
//! `~/.gemini/antigravity-cli` tree belongs to the antigravity adapter.
//!
//! Two on-disk formats share the message shape: `.json` is one object
//! (`sessionId`, `messages[]`) rewritten on every change, `.jsonl` is a
//! header line, message lines and `$set` metadata lines. In `.jsonl` a
//! message is appended again under the same `id` once its tool calls exist or
//! finish (the last record wins), and resumed sessions copy earlier messages
//! into a new file. So neither format is append-only: every import reads the
//! file whole (offset 0), collapses messages by `id`, and derives all ids
//! from session id + message/tool-call id instead of file position, which
//! also dedupes across files of one session.
//!
//! Message and tool call ids are Gemini's own; live hooks do not report them,
//! so merging with live rows is by session id only.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use agent_hooks_protocol::{
    stable_hash, Capture, CaptureEntry, HistoryEntryKind, HistoryToolStatus, LOCAL_ORIGIN,
};
use serde_json::Value;

use crate::error::AdapterError;
use crate::importer::{text_delivery_id, tool_delivery_id, ImportProgress, TranscriptImporter};

/// Same id the live hook events use.
const HARNESS: &str = "gemini-cli";

pub struct GeminiCliImporter;

impl TranscriptImporter for GeminiCliImporter {
    fn supported_os(&self) -> &'static [&'static str] {
        &["macos"]
    }

    fn discover(&self, home: &Path, since: Option<SystemTime>) -> Vec<PathBuf> {
        let Ok(projects) = std::fs::read_dir(home.join(".gemini/tmp")) else {
            return Vec::new();
        };
        let mut files: Vec<PathBuf> = projects
            .flatten()
            .filter_map(|p| std::fs::read_dir(p.path().join("chats")).ok())
            .flat_map(|chats| chats.flatten().map(|e| e.path()))
            .flat_map(|entry| {
                if entry.is_dir() {
                    std::fs::read_dir(&entry)
                        .map(|d| d.flatten().map(|e| e.path()).collect())
                        .unwrap_or_default()
                } else {
                    vec![entry]
                }
            })
            .filter(|p| p.extension().is_some_and(|e| e == "json" || e == "jsonl"))
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
        let mut progress = ImportProgress::default();
        let (header, messages, bad_lines) = if path.extension().is_some_and(|e| e == "json") {
            read_json(path)
        } else {
            read_jsonl(path)?
        };
        progress.skipped += bad_lines;
        let Some(session_id) = header.get("sessionId").and_then(Value::as_str) else {
            progress.skipped += 1;
            return Ok(progress);
        };
        let session = Session {
            id: session_id,
            parent_id: path
                .parent()
                .filter(|dir| {
                    dir.parent()
                        .and_then(Path::file_name)
                        .is_some_and(|n| n == "chats")
                })
                .and_then(Path::file_name)
                .and_then(|n| n.to_str()),
            workspace: workspace(&header, path),
        };
        for message in collapse_by_id(messages) {
            if !session.map_message(&message, sink) {
                progress.skipped += 1;
            }
        }
        Ok(progress)
    }
}

/// A `.json` session. An unreadable or malformed file is a session without a
/// header, which the caller counts as one skip.
fn read_json(path: &Path) -> (Value, Vec<Value>, u64) {
    let mut doc = std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .unwrap_or_default();
    let messages = match doc.get_mut("messages").map(Value::take) {
        Some(Value::Array(messages)) => messages,
        _ => Vec::new(),
    };
    (doc, messages, 0)
}

/// A `.jsonl` session: the header is the line with `sessionId`, messages the
/// lines with an `id`; `$set` and other metadata lines are ignored.
fn read_jsonl(path: &Path) -> Result<(Value, Vec<Value>, u64), AdapterError> {
    let mut header = Value::Null;
    let mut messages = Vec::new();
    let mut bad_lines = 0;
    for line in BufReader::new(File::open(path)?).split(b'\n') {
        let line = line?;
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        match serde_json::from_slice::<Value>(&line) {
            Ok(row) if row.get("sessionId").is_some() => header = row,
            Ok(row) if row.get("id").is_some() => messages.push(row),
            Ok(_) => {}
            Err(_) => bad_lines += 1,
        }
    }
    Ok((header, messages, bad_lines))
}

/// Keep the last record of every message id, in order of first appearance.
fn collapse_by_id(messages: Vec<Value>) -> Vec<Value> {
    let mut slot: HashMap<String, usize> = HashMap::new();
    let mut collapsed: Vec<Value> = Vec::new();
    for message in messages {
        let id = message
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match slot.get(id) {
            Some(&at) => collapsed[at] = message,
            None => {
                slot.insert(id.to_string(), collapsed.len());
                collapsed.push(message);
            }
        }
    }
    collapsed
}

/// The project directory: the first `directories` entry of a `.jsonl`
/// header, else the `.project_root` file next to `chats/`.
fn workspace(header: &Value, path: &Path) -> Option<String> {
    if let Some(dir) = header.pointer("/directories/0").and_then(Value::as_str) {
        return Some(dir.to_string());
    }
    let project = path.ancestors().find(|a| a.join("chats").is_dir())?;
    let root = std::fs::read_to_string(project.join(".project_root")).ok()?;
    let root = root.trim();
    (!root.is_empty()).then(|| root.to_string())
}

struct Session<'a> {
    id: &'a str,
    parent_id: Option<&'a str>,
    workspace: Option<String>,
}

impl Session<'_> {
    /// Emit the captures of one message. `false` when it looked like
    /// conversation but could not be mapped; info/warning/error notices are
    /// not conversation and report `true`.
    fn map_message(&self, message: &Value, sink: &mut dyn FnMut(Capture)) -> bool {
        let kind = match message.get("type").and_then(Value::as_str) {
            Some("user") => HistoryEntryKind::User,
            Some("gemini") => HistoryEntryKind::Assistant,
            _ => return true,
        };
        let (Some(id), Some(ts)) = (
            message.get("id").and_then(Value::as_str),
            message.get("timestamp").and_then(Value::as_str),
        ) else {
            return false;
        };
        let timestamp_ms = timestamp_ms(ts);

        let text = message_text(message.get("content"));
        if !text.trim().is_empty() {
            // Keyed on the session and message id, not the file, so copies of
            // one message across files of a session collapse to one row.
            let delivery_id = text_delivery_id(
                Path::new(self.id),
                stable_hash(id.as_bytes()),
                0,
                kind.as_str(),
            );
            sink(self.capture(
                delivery_id,
                timestamp_ms,
                CaptureEntry {
                    kind,
                    text: Some(text),
                    native_message_id: Some(id.to_string()),
                    tool_call_id: None,
                    tool_name: None,
                    tool_status: None,
                },
            ));
        }

        let calls = message
            .get("toolCalls")
            .and_then(Value::as_array)
            .map_or(&[][..], Vec::as_slice);
        for call in calls {
            if !self.map_tool_call(call, timestamp_ms, sink) {
                return false;
            }
        }
        true
    }

    fn map_tool_call(&self, call: &Value, message_ms: u64, sink: &mut dyn FnMut(Capture)) -> bool {
        let (Some(id), Some(name)) = (
            call.get("id").and_then(Value::as_str),
            call.get("name").and_then(Value::as_str),
        ) else {
            return false;
        };
        let tool = |status, text, phase: &str, timestamp_ms| {
            self.capture(
                tool_delivery_id(id, phase),
                timestamp_ms,
                CaptureEntry {
                    kind: HistoryEntryKind::Tool,
                    text,
                    native_message_id: None,
                    tool_call_id: Some(id.to_string()),
                    tool_name: Some(name.to_string()),
                    tool_status: Some(status),
                },
            )
        };
        let input = call.get("args").filter(|a| !a.is_null());
        sink(tool(
            HistoryToolStatus::Running,
            input.map(Value::to_string),
            "running",
            message_ms,
        ));

        let status = match call.get("status").and_then(Value::as_str) {
            Some("success") => HistoryToolStatus::Completed,
            Some("error" | "cancelled") => HistoryToolStatus::Failed,
            // Still executing when the file was last written.
            _ => return true,
        };
        let result_ms = call
            .get("timestamp")
            .and_then(Value::as_str)
            .map_or(message_ms, timestamp_ms);
        sink(tool(
            status,
            tool_output(call.get("result")),
            "result",
            result_ms,
        ));
        true
    }

    fn capture(&self, delivery_id: String, timestamp_ms: u64, entry: CaptureEntry) -> Capture {
        Capture {
            origin: LOCAL_ORIGIN.to_string(),
            harness: HARNESS.to_string(),
            session_id: self.id.to_string(),
            parent_session_id: self.parent_id.map(str::to_string),
            workspace: self.workspace.clone(),
            title: None,
            status: "idle".to_string(),
            delivery_id,
            timestamp_ms,
            turn_id: None,
            entry: Some(entry),
        }
    }
}

fn timestamp_ms(rfc3339: &str) -> u64 {
    chrono::DateTime::parse_from_rfc3339(rfc3339)
        .map_or(0, |t| u64::try_from(t.timestamp_millis()).unwrap_or(0))
}

/// Message content: a string, or user parts `[{"text": ...}]`.
fn message_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// A tool result is `[{"functionResponse": {"response": {"output"|"error"}}}]`.
fn tool_output(result: Option<&Value>) -> Option<String> {
    let text = result?
        .as_array()?
        .iter()
        .filter_map(|r| r.pointer("/functionResponse/response"))
        .filter_map(|r| r.get("output").or_else(|| r.get("error")))
        .filter_map(Value::as_str)
        .collect::<Vec<_>>()
        .join("\n");
    (!text.trim().is_empty()).then_some(text)
}
