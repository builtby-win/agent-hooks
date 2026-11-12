//! OpenCode history from its SQLite store, opened read-only.
//!
//! Sessions newer than `max(cursor, since)` are replayed whole, one at a time;
//! the cursor advances past each fully emitted session. Parts mutate in place
//! (streamed text, tool state), so every delivery id carries a hash of the
//! content it delivers: an updated part lands as a new observation while an
//! unchanged one re-emits the id it already has.

use std::ops::ControlFlow;
use std::path::{Path, PathBuf};

use agent_hooks_protocol::{stable_hash, Capture, CaptureEntry, HistoryEntryKind, HistoryToolStatus};
use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;

use crate::backfill::{BackfillSink, RecordSource};
use crate::error::AdapterError;

pub(crate) struct OpencodeSource;

/// Columns of a `session` row that identify its records.
struct Session {
    id: String,
    parent_id: Option<String>,
    directory: String,
    title: String,
}

/// OpenCode resolves its data dir via xdg-basedir: `$XDG_DATA_HOME`, else
/// `~/.local/share`.
fn db_path(home: &Path) -> PathBuf {
    let data = std::env::var_os("XDG_DATA_HOME")
        .filter(|d| !d.is_empty())
        .map_or_else(|| home.join(".local/share"), PathBuf::from);
    data.join("opencode/opencode.db")
}

fn compact(value: &Value) -> String {
    value.to_string()
}

/// A JSON value that carries content (Python-style truthiness).
fn present(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
        _ => true,
    }
}

fn millis(value: Option<&Value>) -> Option<u64> {
    value.and_then(Value::as_u64).filter(|ms| *ms > 0)
}

impl RecordSource for OpencodeSource {
    fn supports_os(&self, os: &str) -> bool {
        matches!(os, "macos" | "linux")
    }

    fn run(
        &self,
        home: &Path,
        since_ms: Option<u64>,
        cursor: Option<&str>,
        sink: &mut BackfillSink,
    ) -> Result<Option<String>, AdapterError> {
        let path = db_path(home);
        if !path.exists() {
            return Ok(None);
        }
        let cursor: Option<i64> = cursor
            .map(str::parse)
            .transpose()
            .map_err(|e| AdapterError::Backfill(format!("invalid opencode cursor: {e}")))?;
        let floor = cursor
            .unwrap_or(0)
            .max(since_ms.map_or(0, |ms| i64::try_from(ms).unwrap_or(i64::MAX)));
        let db = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let sessions = db
            .prepare(
                "SELECT id, parent_id, directory, title, time_updated FROM session \
                 WHERE time_updated > ?1 ORDER BY time_updated",
            )?
            .query_map([floor], |row| {
                Ok((
                    Session {
                        id: row.get(0)?,
                        parent_id: row.get(1)?,
                        directory: row.get(2)?,
                        title: row.get(3)?,
                    },
                    row.get::<_, i64>(4)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        let mut new_cursor = None;
        for (session, updated) in sessions {
            if session_records(&db, &session, sink)?.is_break() {
                return Ok(None);
            }
            new_cursor = Some(updated.to_string());
        }
        Ok(new_cursor)
    }
}

/// Emit every observation of one session. Messages come through
/// `message_session_time_created_id_idx` and each message's parts through
/// `part_message_id_id_idx`, so memory is bounded by one message.
fn session_records(
    db: &Connection,
    session: &Session,
    sink: &mut BackfillSink,
) -> Result<ControlFlow<()>, AdapterError> {
    // Roles come from message rows; compaction summaries are assistant
    // messages flagged `summary: true`.
    let mut messages = db.prepare(
        "SELECT id, json_extract(data, '$.role'), json_extract(data, '$.summary') \
         FROM message WHERE session_id = ?1 ORDER BY time_created, id",
    )?;
    let mut parts = db.prepare(
        "SELECT id, time_created, time_updated, data FROM part \
         WHERE message_id = ?1 ORDER BY id",
    )?;
    let mut rows = messages.query([&session.id])?;
    while let Some(row) = rows.next()? {
        let message_id: String = row.get(0)?;
        let role = match (row.get::<_, Option<String>>(1)?.as_deref(), row.get::<_, SqlValue>(2)?) {
            (Some("user"), _) => HistoryEntryKind::User,
            (Some("assistant"), summary) if summary != SqlValue::Integer(1) => {
                HistoryEntryKind::Assistant
            }
            _ => continue,
        };
        let mut part_rows = parts.query([&message_id])?;
        while let Some(part) = part_rows.next()? {
            let part_id: String = part.get(0)?;
            let data: Value = serde_json::from_str(&part.get::<_, String>(3)?)
                .map_err(|e| AdapterError::Backfill(format!("part {part_id}: {e}")))?;
            let (created, updated) = (part.get::<_, u64>(1)?, part.get::<_, u64>(2)?);
            for capture in part_captures(session, role, &part_id, created, updated, &data) {
                if sink(Some(capture)).is_break() {
                    return Ok(ControlFlow::Break(()));
                }
            }
        }
    }
    Ok(ControlFlow::Continue(()))
}

fn part_captures(
    session: &Session,
    role: HistoryEntryKind,
    part_id: &str,
    created: u64,
    updated: u64,
    part: &Value,
) -> Vec<Capture> {
    let capture = |kind: &str, content: &str, timestamp_ms: u64, entry: CaptureEntry| Capture {
        origin: agent_hooks_protocol::LOCAL_ORIGIN.to_string(),
        harness: "opencode".to_string(),
        session_id: session.id.clone(),
        parent_session_id: session.parent_id.clone().filter(|p| !p.is_empty()),
        workspace: Some(session.directory.clone()).filter(|d| !d.is_empty()),
        title: Some(session.title.clone()).filter(|t| !t.is_empty()),
        status: "idle".to_string(),
        delivery_id: format!(
            "backfill:opencode:{part_id}:{kind}:{:016x}",
            stable_hash(content.as_bytes())
        ),
        timestamp_ms,
        turn_id: None,
        entry: Some(entry),
    };

    match part["type"].as_str() {
        Some("text") => {
            let text = part["text"].as_str().unwrap_or_default();
            if text.trim().is_empty()
                || part["synthetic"].as_bool() == Some(true)
                || part["ignored"].as_bool() == Some(true)
            {
                return Vec::new();
            }
            let entry = CaptureEntry {
                kind: role,
                text: Some(text.to_string()),
                // The part id, not the message id: one message holds many text
                // parts and each is its own row.
                native_message_id: Some(part_id.to_string()),
                tool_call_id: None,
                tool_name: None,
                tool_status: None,
            };
            let ts = millis(part["time"].get("start")).unwrap_or(created);
            vec![capture("text", text, ts, entry)]
        }
        Some("tool") if role == HistoryEntryKind::Assistant => {
            let state = &part["state"];
            let status = state["status"].as_str().unwrap_or_default();
            let Some(call_id) = part["callID"].as_str().filter(|c| !c.is_empty()) else {
                return Vec::new();
            };
            if !matches!(status, "pending" | "running" | "completed" | "error") {
                return Vec::new();
            }
            // No native_message_id: the writer keys rows by it before the
            // tool call id and would collapse a message's tools into one.
            let entry = |status, text: Option<String>| CaptureEntry {
                kind: HistoryEntryKind::Tool,
                text,
                native_message_id: None,
                tool_call_id: Some(call_id.to_string()),
                tool_name: part["tool"].as_str().map(str::to_string),
                tool_status: Some(status),
            };
            let times = &state["time"];
            let input = Some(&state["input"]).filter(|i| present(i)).map(compact);
            let mut out = vec![capture(
                "tool-input",
                input.as_deref().unwrap_or_default(),
                millis(times.get("start")).unwrap_or(created),
                entry(HistoryToolStatus::Running, input.clone()),
            )];
            if matches!(status, "completed" | "error") {
                let (result, tool_status) = match status {
                    "completed" => (&state["output"], HistoryToolStatus::Completed),
                    _ => (&state["error"], HistoryToolStatus::Failed),
                };
                let text = match result {
                    Value::Null => None,
                    Value::String(s) => Some(s.clone()),
                    other => Some(compact(other)),
                }
                .filter(|t| !t.is_empty());
                out.push(capture(
                    "tool-output",
                    text.as_deref().unwrap_or_default(),
                    millis(times.get("end")).unwrap_or(updated),
                    entry(tool_status, text.clone()),
                ));
            }
            out
        }
        _ => Vec::new(),
    }
}
