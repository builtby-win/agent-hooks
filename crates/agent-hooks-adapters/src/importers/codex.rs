//! Codex rollouts: `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`.
//!
//! Every file is an append-only stream of `{timestamp, type, payload}` rows.
//! The first row is `session_meta` (session id, cwd); `turn_context` rows
//! carry the turn id. Conversation lives in `response_item` rows: `message`
//! (user/assistant text), `function_call` / `custom_tool_call` and their
//! `*_output` rows, correlated by `call_id`. Reasoning, developer messages,
//! token counts and the context codex injects as user messages (AGENTS.md,
//! environment) are not conversation and are ignored.
//!
//! Only plain `.jsonl` rollouts are read: codex later compresses old ones to
//! `*.jsonl.zst` (and moves them to `archived_sessions`), which is not
//! importable without a zstd dependency.
//!
//! Linux: `~/.codex/sessions`, same layout (not enabled yet, unverified).

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
const HARNESS: &str = "codex";

/// User-role messages codex injects for the model; not typed by the user.
const INJECTED_USER_PREFIXES: &[&str] = &[
    "# AGENTS.md instructions",
    "<environment_context>",
    "<external_codex_apps_open_page>",
    "<user_instructions>",
    "<turn_aborted>",
];

pub(crate) struct CodexImporter;

/// Session identity read from the rollout's own rows.
#[derive(Default)]
struct Context {
    session_id: String,
    cwd: Option<String>,
    turn_id: Option<String>,
}

impl TranscriptImporter for CodexImporter {
    fn supported_os(&self) -> &'static [&'static str] {
        &["macos"]
    }

    fn discover(&self, home: &Path, since: Option<SystemTime>) -> Vec<PathBuf> {
        let mut files = Vec::new();
        collect_rollouts(&home.join(".codex").join("sessions"), 0, &mut files);
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
        let mut reader = BufReader::new(File::open(path)?);
        let mut progress = ImportProgress {
            offset: from_offset,
            skipped: 0,
        };

        // The session id lives only on the first row, even when resuming.
        let mut first = Vec::new();
        reader.read_until(b'\n', &mut first)?;
        let mut ctx = Context::default();
        match serde_json::from_slice::<Value>(&first) {
            Ok(row) if row.get("type").and_then(Value::as_str) == Some("session_meta") => {
                apply_context(&row, &mut ctx);
            }
            _ => {
                progress.skipped += 1;
                return Ok(progress);
            }
        }
        if ctx.session_id.is_empty() {
            progress.skipped += 1;
            return Ok(progress);
        }
        reader.seek(SeekFrom::Start(from_offset))?;

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
                    if !map_line(&row, path, line_pos, &mut ctx, &mut tool_names, sink) {
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

/// Rollouts sit at `sessions/YYYY/MM/DD/`; worktree-style trees do not exist
/// here, so a depth-bounded walk is enough.
fn collect_rollouts(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for path in entries.flatten().map(|e| e.path()) {
        if path.is_dir() {
            if depth < 3 {
                collect_rollouts(&path, depth + 1, out);
            }
        } else if path.extension().is_some_and(|e| e == "jsonl") {
            out.push(path);
        }
    }
}

/// Fold a `session_meta` / `turn_context` row into the running context.
fn apply_context(row: &Value, ctx: &mut Context) {
    let payload = row.get("payload");
    let field = |k: &str| {
        payload
            .and_then(|p| p.get(k))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    match row.get("type").and_then(Value::as_str) {
        Some("session_meta") => {
            if let Some(id) = field("id") {
                ctx.session_id = id;
            }
            ctx.cwd = field("cwd").or(ctx.cwd.take());
        }
        Some("turn_context") => {
            ctx.turn_id = field("turn_id");
            ctx.cwd = field("cwd").or(ctx.cwd.take());
        }
        _ => {}
    }
}

/// Emit the captures of one rollout row. `false` when the row looked like a
/// message but could not be mapped (counted as skipped); rows that are not
/// conversation are ignored and report `true`.
fn map_line(
    row: &Value,
    path: &Path,
    line_pos: u64,
    ctx: &mut Context,
    tool_names: &mut HashMap<String, String>,
    sink: &mut dyn FnMut(Capture),
) -> bool {
    match row.get("type").and_then(Value::as_str) {
        Some("session_meta" | "turn_context") => {
            apply_context(row, ctx);
            return true;
        }
        Some("response_item") => {}
        _ => return true,
    }
    let Some(payload) = row.get("payload") else {
        return false;
    };
    let Some(ts) = row
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
    else {
        return false;
    };
    let timestamp_ms = u64::try_from(ts.timestamp_millis()).unwrap_or(0);
    let payload_str = |k: &str| payload.get(k).and_then(Value::as_str);

    let capture = |delivery_id: String, entry: CaptureEntry| Capture {
        origin: LOCAL_ORIGIN.to_string(),
        harness: HARNESS.to_string(),
        session_id: ctx.session_id.clone(),
        parent_session_id: None,
        workspace: ctx.cwd.clone(),
        title: None,
        status: "idle".to_string(),
        delivery_id,
        timestamp_ms,
        turn_id: ctx.turn_id.clone(),
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

    match payload_str("type") {
        Some("message") => {
            let kind = match payload_str("role") {
                Some("user") => HistoryEntryKind::User,
                Some("assistant") => HistoryEntryKind::Assistant,
                _ => return true,
            };
            let Some(parts) = payload.get("content").and_then(Value::as_array) else {
                return false;
            };
            for (idx, part) in parts.iter().enumerate() {
                let Some(text) = part
                    .get("text")
                    .and_then(Value::as_str)
                    .filter(|t| !t.trim().is_empty())
                else {
                    continue;
                };
                if kind == HistoryEntryKind::User
                    && INJECTED_USER_PREFIXES.iter().any(|p| text.starts_with(p))
                {
                    continue;
                }
                let native_message_id = payload_str("id").map(|id| match idx {
                    0 => id.to_string(),
                    _ => format!("{id}:{idx}"),
                });
                sink(capture(
                    text_delivery_id(path, line_pos, idx, kind.as_str()),
                    CaptureEntry {
                        kind,
                        text: Some(text.to_string()),
                        native_message_id,
                        tool_call_id: None,
                        tool_name: None,
                        tool_status: None,
                    },
                ));
            }
        }
        Some("function_call" | "custom_tool_call") => {
            let (Some(id), Some(name)) = (payload_str("call_id"), payload_str("name")) else {
                return false;
            };
            tool_names.insert(id.to_string(), name.to_string());
            let input = payload_str("arguments")
                .or_else(|| payload_str("input"))
                .filter(|i| !i.trim().is_empty());
            sink(capture(
                tool_delivery_id(id, "running"),
                tool(
                    id,
                    Some(name),
                    HistoryToolStatus::Running,
                    input.map(str::to_string),
                ),
            ));
        }
        Some("function_call_output" | "custom_tool_call_output") => {
            let Some(id) = payload_str("call_id") else {
                return false;
            };
            sink(capture(
                tool_delivery_id(id, "result"),
                tool(
                    id,
                    tool_names.get(id).map(String::as_str),
                    HistoryToolStatus::Completed,
                    output_text(payload.get("output")),
                ),
            ));
        }
        _ => {}
    }
    true
}

/// Tool output is a string or an array of `input_text` blocks. Blank output
/// is no output.
fn output_text(output: Option<&Value>) -> Option<String> {
    let text = match output? {
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
