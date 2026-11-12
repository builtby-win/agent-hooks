//! Antigravity CLI (`agy`) transcripts:
//! `~/.gemini/antigravity-cli/brain/<conversation>/.system_generated/logs/`.
//! Linux: `~/.gemini/antigravity-cli/brain/...` (same layout, not implemented).
//!
//! Each conversation has `transcript_full.jsonl` (untruncated tool output,
//! structured tool args), `transcript.jsonl` (the same steps with truncated
//! output and JSON-string-encoded tool args) and `chunks/` (shards of those
//! two, redundant and mostly absent). Neither whole file is a superset of the
//! other (steps are occasionally dropped from one of them), so a conversation
//! is the union of its steps with `transcript_full.jsonl` winning per step.
//! Both files are rewritten in place and steps are not always written in
//! order, so every import reads the conversation whole (offset 0) and
//! processes steps sorted by `step_index`; all ids derive from the step
//! index, which makes re-imports idempotent.
//!
//! Steps carry no tool call ids and tool results carry no link to their
//! call. A `PLANNER_RESPONSE` lists its `tool_calls` and the following
//! `MODEL` result steps answer them in order, so calls get the synthetic id
//! `<conversation>:<step>:<n>` (live `toolCall.id` values are not recorded,
//! these rows will not merge with live tool rows). No workspace is recorded.
//! The IDE (`~/.gemini/antigravity`) keeps no readable transcript.

use std::collections::{BTreeMap, VecDeque};
use std::fs::File;
use std::io::{BufRead, BufReader, ErrorKind};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use agent_hooks_protocol::{
    Capture, CaptureEntry, HistoryEntryKind, HistoryToolStatus, LOCAL_ORIGIN,
};
use serde_json::Value;

use crate::error::AdapterError;
use crate::importer::{text_delivery_id, tool_delivery_id, ImportProgress, TranscriptImporter};

/// Same id the live hook events use.
const HARNESS: &str = "antigravity";
const FULL: &str = "transcript_full.jsonl";
const TRUNCATED: &str = "transcript.jsonl";

pub struct AntigravityImporter;

impl TranscriptImporter for AntigravityImporter {
    fn supported_os(&self) -> &'static [&'static str] {
        &["macos"]
    }

    fn discover(&self, home: &Path, since: Option<SystemTime>) -> Vec<PathBuf> {
        let Ok(brain) = std::fs::read_dir(home.join(".gemini/antigravity-cli/brain")) else {
            return Vec::new();
        };
        let mut files: Vec<PathBuf> = brain
            .flatten()
            .map(|c| c.path().join(".system_generated/logs"))
            .filter_map(|logs| {
                [FULL, TRUNCATED]
                    .map(|name| logs.join(name))
                    .into_iter()
                    .find(|p| p.is_file())
            })
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
        let Some(session_id) = path
            .ancestors()
            .nth(3)
            .and_then(Path::file_name)
            .and_then(|n| n.to_str())
        else {
            progress.skipped += 1;
            return Ok(progress);
        };
        let logs = path.parent().unwrap_or(path);
        let mut steps = BTreeMap::new();
        progress.skipped += load_steps(&logs.join(TRUNCATED), true, &mut steps)?;
        progress.skipped += load_steps(&logs.join(FULL), false, &mut steps)?;

        let mut pending: VecDeque<(String, String)> = VecDeque::new();
        for (index, step) in &steps {
            if !map_step(session_id, path, *index, step, &mut pending, sink) {
                progress.skipped += 1;
            }
        }
        Ok(progress)
    }
}

/// Insert the steps of one transcript file, replacing earlier ones. A missing
/// file is an empty transcript. `decode_args` undoes the JSON-string
/// encoding of tool args in `transcript.jsonl`. Returns unparsable lines.
fn load_steps(
    path: &Path,
    decode_args: bool,
    steps: &mut BTreeMap<u64, Value>,
) -> Result<u64, AdapterError> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e.into()),
    };
    let mut skipped = 0;
    for line in BufReader::new(file).split(b'\n') {
        let line = line?;
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let Ok(mut step) = serde_json::from_slice::<Value>(&line) else {
            skipped += 1;
            continue;
        };
        let Some(index) = step.get("step_index").and_then(Value::as_u64) else {
            skipped += 1;
            continue;
        };
        if decode_args {
            decode_tool_args(&mut step);
        }
        steps.insert(index, step);
    }
    Ok(skipped)
}

fn decode_tool_args(step: &mut Value) {
    let Some(calls) = step.get_mut("tool_calls").and_then(Value::as_array_mut) else {
        return;
    };
    for args in calls.iter_mut().filter_map(|c| c.get_mut("args")) {
        let Some(args) = args.as_object_mut() else {
            continue;
        };
        for value in args.values_mut() {
            if let Some(decoded) = value
                .as_str()
                .and_then(|s| serde_json::from_str::<Value>(s).ok())
            {
                *value = decoded;
            }
        }
    }
}

/// Emit the captures of one step. `false` when it looked like conversation
/// but could not be mapped; bookkeeping steps report `true`.
fn map_step(
    session_id: &str,
    path: &Path,
    index: u64,
    step: &Value,
    pending: &mut VecDeque<(String, String)>,
    sink: &mut dyn FnMut(Capture),
) -> bool {
    let str_field = |k: &str| step.get(k).and_then(Value::as_str);
    let (Some(kind), Some(source)) = (str_field("type"), str_field("source")) else {
        return false;
    };
    // Cleared steps had their payload wiped.
    if str_field("status") == Some("CLEARED") {
        return true;
    }
    let Some(timestamp_ms) = str_field("created_at")
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .map(|t| u64::try_from(t.timestamp_millis()).unwrap_or(0))
    else {
        return false;
    };
    let content = str_field("content").unwrap_or_default();

    let capture = |delivery_id: String, entry: CaptureEntry| Capture {
        origin: LOCAL_ORIGIN.to_string(),
        harness: HARNESS.to_string(),
        session_id: session_id.to_string(),
        parent_session_id: None,
        workspace: None,
        title: None,
        status: "idle".to_string(),
        delivery_id,
        timestamp_ms,
        turn_id: None,
        entry: Some(entry),
    };
    let message = |kind: HistoryEntryKind, text: &str| {
        capture(
            text_delivery_id(path, index, 0, kind.as_str()),
            CaptureEntry {
                kind,
                text: Some(text.to_string()),
                native_message_id: Some(format!("step-{index}")),
                tool_call_id: None,
                tool_name: None,
                tool_status: None,
            },
        )
    };
    let tool = |id: &str, name: &str, status, text, phase| {
        capture(
            tool_delivery_id(id, phase),
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

    match (source, kind) {
        ("USER_EXPLICIT", "USER_INPUT") => {
            let request = user_request(content);
            if !request.trim().is_empty() {
                sink(message(HistoryEntryKind::User, request));
            }
        }
        ("MODEL", "PLANNER_RESPONSE") => {
            if !content.trim().is_empty() {
                sink(message(HistoryEntryKind::Assistant, content));
            }
            pending.clear();
            let calls = step
                .get("tool_calls")
                .and_then(Value::as_array)
                .map_or(&[][..], Vec::as_slice);
            for (n, call) in calls.iter().enumerate() {
                let Some(name) = call.get("name").and_then(Value::as_str) else {
                    return false;
                };
                let id = format!("{session_id}:{index}:{n}");
                let input = call.get("args").filter(|a| !a.is_null());
                sink(tool(
                    &id,
                    name,
                    HistoryToolStatus::Running,
                    input.map(Value::to_string),
                    "running",
                ));
                pending.push_back((id, name.to_string()));
            }
        }
        // A tool result answers the oldest unanswered call of the last
        // planner step. A still-running background task counts as answered.
        ("MODEL", _) => {
            let Some((id, name)) = pending.pop_front() else {
                return false;
            };
            let (status, text) = match str_field("status") {
                Some("ERROR" | "INVALID") => (
                    HistoryToolStatus::Failed,
                    str_field("error").unwrap_or(content),
                ),
                _ => (HistoryToolStatus::Completed, content),
            };
            let output = strip_result_header(text);
            sink(tool(
                &id,
                &name,
                status,
                (!output.is_empty()).then(|| output.to_string()),
                "result",
            ));
        }
        // Checkpoints, system messages, errors, directory rules, history.
        _ => {}
    }
    true
}

/// The text between `<USER_REQUEST>` tags; the rest is harness metadata
/// (clock, settings changes). Untagged input is returned whole.
fn user_request(content: &str) -> &str {
    content
        .split_once("<USER_REQUEST>")
        .and_then(|(_, rest)| rest.split_once("</USER_REQUEST>"))
        .map_or(content, |(request, _)| request.trim())
}

/// Result steps open with `Created At:` / `Completed At:` timing lines.
fn strip_result_header(content: &str) -> &str {
    let mut rest = content;
    for prefix in ["Created At:", "Completed At:"] {
        if rest.starts_with(prefix) {
            rest = rest.split_once('\n').map_or("", |(_, tail)| tail);
        }
    }
    rest.trim()
}
