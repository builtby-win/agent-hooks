use parking_lot::Mutex;
use std::collections::BTreeMap;

use agent_hooks_protocol::{
    AgentEvent, AgentLifecycle, HistoryIdentity, PromptKind, PromptOption, PromptQuestion,
    PromptRequest, PromptSuggestion, RawHookInput, SuggestionKind,
};

use crate::error::AdapterError;
use crate::manifest::{
    AdapterManifest, Capability, ExtractionSource, LifecycleRef, QuestionPrompt, ResponseMode,
    SessionLinkSpec, ShapedAskTool, TranscriptExtract, VersionSpec,
};
use crate::traits::{
    extract_session_id, extract_string_field, normalize_event, parse_payload, payload_value,
    resolve_event_name, HarnessAdapter, HookDefinition, HookEventTarget, HookLocation,
};

// ---------------------------------------------------------------------------
// helpers: dotted walk and extraction
// ---------------------------------------------------------------------------

fn walk_dotted<'a>(value: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
    // Mid-path arrays/nulls/numbers are not traversable: fall through to the
    // next extraction candidate (return None) rather than aborting the event.
    let mut cur = value;
    for seg in path.split('.') {
        match cur {
            serde_json::Value::Object(map) => cur = map.get(seg)?,
            _ => return None,
        }
    }
    Some(cur)
}

/// Read human text blocks without dumping tool arguments or arbitrary JSON.
fn flavor_from_value(value: &serde_json::Value) -> Option<String> {
    fn append(value: &serde_json::Value, out: &mut String, remaining: &mut usize, depth: usize) {
        if *remaining == 0 || depth > 8 {
            return;
        }
        match value {
            serde_json::Value::String(text) => {
                let text = text.trim();
                if text.is_empty() {
                    return;
                }
                if !out.is_empty() {
                    out.push('\n');
                    *remaining -= 1;
                }
                for ch in text.chars().take(*remaining) {
                    out.push(ch);
                    *remaining -= 1;
                }
            }
            serde_json::Value::Array(parts) => {
                for part in parts {
                    append(part, out, remaining, depth + 1);
                    if *remaining == 0 {
                        break;
                    }
                }
            }
            serde_json::Value::Object(object) => {
                if object
                    .get("type")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|kind| !matches!(kind, "text" | "input_text" | "output_text"))
                {
                    return;
                }
                for key in ["text", "content", "message"] {
                    if let Some(text) = object.get(key) {
                        let before = out.len();
                        append(text, out, remaining, depth + 1);
                        if out.len() != before {
                            break;
                        }
                    }
                }
            }
            _ => {}
        }
    }
    let mut out = String::new();
    append(value, &mut out, &mut 300, 0);
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

fn walk_dotted_str(payload: &serde_json::Value, path: &str) -> Option<String> {
    let v = walk_dotted(payload, path)?;
    if let Some(s) = v.as_str() {
        if !s.is_empty() {
            return Some(s.to_string());
        }
    }
    if v.is_number() || v.is_boolean() {
        return Some(v.to_string());
    }
    None
}
fn extract_transcript_path(payload: &serde_json::Value) -> Option<String> {
    extract_string_field(payload, &["transcriptPath", "transcript_path"])
}

fn extract_first_timestamp(transcript_path: &str) -> Option<u64> {
    // Head-bounded: scan at most the first ~64KB so a huge transcript cannot
    // stall a gate hook. Missing/permission-error/unreadable files fall
    // through (None) instead of erroring.
    use std::io::Read;
    let path = crate::traits::expand_tilde_path(transcript_path);
    let file = std::fs::File::open(&path).ok()?;
    const MAX_HEAD: u64 = 65536;
    let mut buf = Vec::new();
    file.take(MAX_HEAD).read_to_end(&mut buf).ok()?;
    let head = String::from_utf8_lossy(&buf);
    let first_line = head.lines().next()?;
    let v: serde_json::Value = serde_json::from_str(first_line.trim()).ok()?;
    let created_at_str = v.get("created_at").and_then(|s| s.as_str())?;
    let dt = chrono::DateTime::parse_from_rfc3339(created_at_str).ok()?;
    Some(dt.timestamp_millis() as u64)
}

fn extract_first_prompt(transcript_path: &str) -> Option<String> {
    // Head-bounded: scan at most the first ~1MB, stopping at the first user
    // prompt match, so a huge transcript cannot stall a gate hook.
    // Missing/permission-error/unreadable/oversize files fall through (None).
    use std::io::Read;
    let path = crate::traits::expand_tilde_path(transcript_path);
    let file = std::fs::File::open(&path).ok()?;
    const MAX_HEAD: u64 = 1024 * 1024;
    let mut buf = Vec::new();
    file.take(MAX_HEAD).read_to_end(&mut buf).ok()?;
    if buf.is_empty() {
        return None;
    }
    let chunk_str = String::from_utf8_lossy(&buf);
    for line in chunk_str.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
            let is_user = v.get("source").and_then(|s| s.as_str()) == Some("USER_EXPLICIT")
                || v.get("type").and_then(|s| s.as_str()) == Some("USER_INPUT");
            if is_user {
                if let Some(content_str) = v.get("content").and_then(|c| c.as_str()) {
                    if let Some(start) = content_str.find("<USER_REQUEST>") {
                        let after = &content_str[start + "<USER_REQUEST>".len()..];
                        if let Some(end) = after.find("</USER_REQUEST>") {
                            let extracted = after[..end].trim().to_string();
                            if !extracted.is_empty() {
                                return Some(extracted);
                            }
                        }
                    }
                    let cleaned = content_str.trim().to_string();
                    if !cleaned.is_empty() {
                        return Some(cleaned);
                    }
                }
            }
        }
        // Stop scanning once the head budget is consumed: `lines()` above
        // already only covers the bounded head buffer.
    }
    None
}
fn extract_latest_prompt(transcript_path: &str) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    const MAX_TAIL: u64 = 1024 * 1024;

    let path = crate::traits::expand_tilde_path(transcript_path);
    let mut file = std::fs::File::open(&path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(MAX_TAIL);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf);
    let mut lines = text.lines();
    if start > 0 {
        lines.next();
    }
    for line in lines.rev() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            continue;
        };
        let is_user = value.get("source").and_then(|s| s.as_str()) == Some("USER_EXPLICIT")
            || value.get("type").and_then(|s| s.as_str()) == Some("USER_INPUT");
        if !is_user {
            continue;
        }
        let Some(content) = value.get("content").and_then(|c| c.as_str()) else {
            continue;
        };
        if let Some(start) = content.find("<USER_REQUEST>") {
            let after = &content[start + "<USER_REQUEST>".len()..];
            if let Some(end) = after.find("</USER_REQUEST>") {
                let prompt = after[..end].trim();
                if !prompt.is_empty() {
                    return Some(prompt.to_string());
                }
            }
        }
        let content = content.trim();
        if !content.is_empty() {
            return Some(content.to_string());
        }
    }
    None
}

fn extract_latest_assistant(transcript_path: &str) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    const MAX_TAIL: u64 = 1024 * 1024;

    let path = crate::traits::expand_tilde_path(transcript_path);
    let mut file = std::fs::File::open(&path).ok()?;
    let start = file.metadata().ok()?.len().saturating_sub(MAX_TAIL);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::new();
    file.take(MAX_TAIL).read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf);
    let mut lines = text.lines();
    if start > 0 {
        lines.next();
    }
    for line in lines.rev() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            continue;
        };
        // Never reuse the preceding turn's reply when this turn failed or
        // stopped before producing a completed response.
        if value.get("source").and_then(|v| v.as_str()) == Some("USER_EXPLICIT")
            || value.get("type").and_then(|v| v.as_str()) == Some("USER_INPUT")
        {
            return None;
        }
        if value.get("source").and_then(|v| v.as_str()) != Some("MODEL")
            || value.get("type").and_then(|v| v.as_str()) != Some("PLANNER_RESPONSE")
            || value.get("status").and_then(|v| v.as_str()) != Some("DONE")
        {
            continue;
        }
        if let Some(content) = value.get("content").and_then(|v| v.as_str()) {
            if !content.trim().is_empty() {
                return Some(content.to_owned());
            }
        }
    }
    None
}

/// Parent of a child session, read from the harness's session store (see
/// [`SessionLinkSpec`]). Bounded: only the newest day directories, the newest
/// logs in them, and the head of each log are read, so a gate hook can never
/// stall on a large store. Any miss or I/O error is `None`.
fn extract_session_link(payload: &serde_json::Value, spec: &SessionLinkSpec) -> Option<String> {
    use std::io::{BufRead, BufReader, Read};
    const MAX_DAYS: usize = 3;
    const MAX_LOGS: usize = 40;
    const MAX_HEAD: u64 = 8 * 1024 * 1024;
    let child = extract_string_field(payload, &[spec.child_from.as_str()])?;
    // The id becomes a path segment and a substring needle: reject anything
    // that is not a plain id.
    if child.is_empty() || !child.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return None;
    }
    let root = crate::traits::expand_tilde_path(&spec.root);
    let newest = |dir: &std::path::Path| -> Vec<std::path::PathBuf> {
        let mut v: Vec<_> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        v.sort();
        v.reverse();
        v
    };
    let days: Vec<_> = newest(&root)
        .into_iter()
        .flat_map(|y| newest(&y))
        .flat_map(|m| newest(&m))
        .take(MAX_DAYS)
        .collect();
    // A session with its own directory is top-level.
    if days.iter().any(|d| d.join(&child).is_dir()) {
        return None;
    }
    let mut logs: Vec<(std::time::SystemTime, std::path::PathBuf)> = days
        .iter()
        .flat_map(|d| newest(d))
        .filter_map(|s| {
            let log = s.join("session.jsonl");
            Some((std::fs::metadata(&log).ok()?.modified().ok()?, log))
        })
        .collect();
    logs.sort_by(|a, b| b.0.cmp(&a.0));
    let needle = format!("\"{}\":\"{}\"", spec.link_key, child);
    let needle_spaced = format!("\"{}\": \"{}\"", spec.link_key, child);
    for (_, log) in logs.into_iter().take(MAX_LOGS) {
        let Ok(file) = std::fs::File::open(&log) else {
            continue;
        };
        for line in BufReader::new(file.take(MAX_HEAD))
            .lines()
            .map_while(Result::ok)
        {
            if !line.contains(&needle) && !line.contains(&needle_spaced) {
                continue;
            }
            let Ok(rec) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            if let Some(id) = walk_dotted_str(&rec, "stream.id") {
                if id != child {
                    return Some(id);
                }
            }
        }
    }
    None
}

/// Id extraction reads payload paths only: transcript/session-link sources
/// perform I/O and are meaningless for message or tool call identity.
fn path_sources_only(sources: Vec<ExtractionSource>) -> Vec<ExtractionSource> {
    sources
        .into_iter()
        .filter(|s| matches!(s, ExtractionSource::Path(_)))
        .collect()
}

fn extract_string_via_sources(
    payload: &serde_json::Value,
    sources: &[ExtractionSource],
) -> Option<String> {
    for src in sources {
        match src {
            ExtractionSource::Path(p) => {
                if p.contains('.') {
                    if let Some(s) = walk_dotted_str(payload, p) {
                        return Some(s);
                    }
                    // also try walk_dotted for nested value that is stringified?
                    // strict walk already done
                } else {
                    // bare key: recursive fallback semantics
                    if let Some(s) = extract_string_field(payload, &[p.as_str()]) {
                        return Some(s);
                    }
                }
            }
            ExtractionSource::SessionLink(link) => {
                if let Some(id) = extract_session_link(payload, &link.session_link) {
                    return Some(id);
                }
            }
            ExtractionSource::Transcript(tr) => {
                // A missing/unreadable transcript path falls through to the
                // next source — never aborts extraction of this field.
                let Some(tp) = extract_transcript_path(payload) else {
                    continue;
                };
                match tr.transcript {
                    TranscriptExtract::FirstPrompt => {
                        if let Some(s) = extract_first_prompt(&tp) {
                            return Some(s);
                        }
                    }
                    TranscriptExtract::LatestPrompt => {
                        if let Some(s) = extract_latest_prompt(&tp) {
                            return Some(s);
                        }
                    }
                    TranscriptExtract::LatestAssistant => {
                        if let Some(s) = extract_latest_assistant(&tp) {
                            return Some(s);
                        }
                    }
                    TranscriptExtract::FirstTimestamp => {
                        if let Some(ts) = extract_first_timestamp(&tp) {
                            return Some(ts.to_string());
                        }
                    }
                }
            }
        }
    }
    None
}

fn extract_value_via_sources(
    payload: &serde_json::Value,
    sources: &[ExtractionSource],
) -> Option<serde_json::Value> {
    for src in sources {
        match src {
            ExtractionSource::Path(p) => {
                if p.contains('.') {
                    if let Some(v) = walk_dotted(payload, p) {
                        if !v.is_null() {
                            return Some(v.clone());
                        }
                    }
                } else {
                    if let Some(v) = payload_value(payload, &[p.as_str()]) {
                        return Some(v);
                    }
                    // also try direct dotted walk fallback? already covered
                    if let Some(v) = walk_dotted(payload, p) {
                        if !v.is_null() {
                            return Some(v.clone());
                        }
                    }
                }
            }
            ExtractionSource::SessionLink(link) => {
                if let Some(id) = extract_session_link(payload, &link.session_link) {
                    return Some(serde_json::Value::String(id));
                }
            }
            ExtractionSource::Transcript(tr) => {
                // A missing/unreadable transcript path falls through to the
                // next source — never aborts extraction of this field.
                let Some(tp) = extract_transcript_path(payload) else {
                    continue;
                };
                match tr.transcript {
                    TranscriptExtract::FirstPrompt => {
                        if let Some(s) = extract_first_prompt(&tp) {
                            return Some(serde_json::Value::String(s));
                        }
                    }
                    TranscriptExtract::LatestPrompt => {
                        if let Some(s) = extract_latest_prompt(&tp) {
                            return Some(serde_json::Value::String(s));
                        }
                    }
                    TranscriptExtract::LatestAssistant => {
                        if let Some(s) = extract_latest_assistant(&tp) {
                            return Some(serde_json::Value::String(s));
                        }
                    }
                    TranscriptExtract::FirstTimestamp => {
                        if let Some(ts) = extract_first_timestamp(&tp) {
                            return Some(serde_json::Value::Number(serde_json::Number::from(ts)));
                        }
                    }
                }
            }
        }
    }
    None
}
/// Clean one raw cwd candidate: trim, strip `file://`, expand `~`, reject
/// empties and harness-internal config dirs declared in `ignore_patterns`
/// (e.g. `[".gemini"]` for Antigravity).
fn clean_cwd_candidate(raw: &str, ignore_patterns: &[String]) -> Option<std::path::PathBuf> {
    let s = raw.trim().strip_prefix("file://").unwrap_or(raw.trim());
    if s.is_empty() || ignore_patterns.iter().any(|pat| s.contains(pat.as_str())) {
        return None;
    }
    let expanded = crate::traits::expand_tilde_path(s);
    if expanded.as_os_str().is_empty() {
        return None;
    }
    Some(expanded)
}

/// Workspace cwd from payload: string values used directly, arrays take the
/// first string element (Antigravity `workspacePaths`). Dotted paths walk
/// nested objects (`toolCall.args.Cwd`).
fn extract_cwd_via_sources(
    payload: &serde_json::Value,
    sources: &[ExtractionSource],
    ignore_patterns: &[String],
) -> Option<std::path::PathBuf> {
    for src in sources {
        let ExtractionSource::Path(p) = src else {
            continue;
        };
        let value: Option<serde_json::Value> = if p.contains('.') {
            walk_dotted(payload, p).cloned()
        } else {
            payload_value(payload, &[p.as_str()]).or_else(|| walk_dotted(payload, p).cloned())
        };
        match value.as_ref() {
            Some(v) => {
                if let Some(s) = v.as_str() {
                    if let Some(cleaned) = clean_cwd_candidate(s, ignore_patterns) {
                        return Some(cleaned);
                    }
                } else if let Some(arr) = v.as_array() {
                    for item in arr {
                        if let Some(s) = item.as_str() {
                            if let Some(cleaned) = clean_cwd_candidate(s, ignore_patterns) {
                                return Some(cleaned);
                            }
                        }
                    }
                }
            }
            None => continue,
        }
    }
    None
}

/// Coerce a JSON value to bool, preserving unknown as None.
///
/// Bools as-is; numerics (0 = false, else true); strings: empty or
/// unrecognized spellings yield None (unknown), case-insensitive
/// `"true"`/`"1"`/`"yes"`/`"y"` = true and `"false"`/`"0"`/`"no"`/`"n"` =
/// false. Null and empty collections yield None; non-empty collections
/// count as true. Callers apply their own default for None: success
/// extraction leans allow (`unwrap_or(true)`), flag checks lean false.
fn coerce_bool(value: &serde_json::Value) -> Option<bool> {
    match value {
        serde_json::Value::Bool(b) => Some(*b),
        serde_json::Value::Number(n) => n
            .as_f64()
            .map(|f| f != 0.0)
            .or_else(|| n.as_i64().map(|i| i != 0))
            .or_else(|| n.as_u64().map(|u| u != 0)),
        serde_json::Value::String(s) => {
            let t = s.trim().to_ascii_lowercase();
            if t.is_empty() {
                None
            } else if t == "true" || t == "1" || t == "yes" || t == "y" {
                Some(true)
            } else if t == "false" || t == "0" || t == "no" || t == "n" {
                Some(false)
            } else {
                None
            }
        }
        serde_json::Value::Null => None,
        serde_json::Value::Array(a) => {
            if a.is_empty() {
                None
            } else {
                Some(true)
            }
        }
        serde_json::Value::Object(m) => {
            if m.is_empty() {
                None
            } else {
                Some(true)
            }
        }
    }
}

/// First `Some` across payload candidates, in argument order.
///
/// `parse_event` works with up to three views of the same input
/// (JSON-RPC params, envelope, full payload); per-site precedence is set by
/// the order of the slice passed in.
fn first_across<T>(
    payloads: &[&serde_json::Value],
    f: impl Fn(&serde_json::Value) -> Option<T>,
) -> Option<T> {
    payloads.iter().find_map(|p| f(p))
}

/// Cap a rendered reason/answers string at 2000 chars (char-unified) with a
/// `...[truncated]` suffix so oversized verdict text cannot bloat responses.
/// Largest context payload one hook response will carry to the model.
const MAX_CONTEXT_CHARS: usize = 8000;

fn cap_context(s: &str) -> String {
    if s.chars().count() <= MAX_CONTEXT_CHARS {
        return s.to_string();
    }
    let truncated: String = s.chars().take(MAX_CONTEXT_CHARS).collect();
    format!("{truncated}...[truncated]")
}

fn render_inject(value: &serde_json::Value, event: &str, context: &str) -> serde_json::Value {
    match value {
        serde_json::Value::String(s) => serde_json::Value::String(
            s.replace("{additional_context}", context)
                .replace("{event}", event),
        ),
        serde_json::Value::Array(arr) => serde_json::Value::Array(
            arr.iter()
                .map(|v| render_inject(v, event, context))
                .collect(),
        ),
        serde_json::Value::Object(map) => serde_json::Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), render_inject(v, event, context)))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn cap_reason(s: &str) -> String {
    const MAX_REASON_CHARS: usize = 2000;
    if s.chars().count() <= MAX_REASON_CHARS {
        return s.to_string();
    }
    let truncated: String = s.chars().take(MAX_REASON_CHARS).collect();
    format!("{truncated}...[truncated]")
}

/// Structured answer groups: one array per answered question, its selected
/// options with any custom input appended.
///
/// Distinct from `{answers}`, which flattens every pick into one
/// comma-joined string. That is fine for a host that wants prose context
/// (Claude Code's `additionalContext`), but lossy for one that wants the
/// picks back as data — opencode's `question.reply` takes `string[][]` and
/// has to know which answer belongs to which question. A comma-joined string
/// cannot say that, so hosts that answer structurally get this placeholder.
fn answer_groups(decision: &agent_hooks_protocol::PromptDecision) -> serde_json::Value {
    serde_json::Value::Array(
        decision
            .answers
            .iter()
            .map(|a| {
                let mut values: Vec<serde_json::Value> = a
                    .selected_options
                    .iter()
                    .map(|s| serde_json::Value::String(s.clone()))
                    .collect();
                if let Some(custom) = a.custom_input.as_ref().filter(|c| !c.trim().is_empty()) {
                    values.push(serde_json::Value::String(custom.clone()));
                }
                serde_json::Value::Array(values)
            })
            .collect(),
    )
}

/// Normalized reply verb for hosts that distinguish approving *this* call
/// from approving every later one like it. Harness-agnostic on purpose —
/// each plugin maps these to its own vocabulary (opencode says
/// once/always/reject, pi says allow/allow_always/deny).
fn reply_verb(decision: &agent_hooks_protocol::PromptDecision) -> &'static str {
    if decision.ask {
        "ask"
    } else if decision.allow_always {
        "allow_always"
    } else if decision.allow {
        "allow"
    } else {
        "deny"
    }
}

fn render_template_value(
    value: &serde_json::Value,
    reason: &str,
    event: &str,
    answers: &str,
    groups: &serde_json::Value,
    reply: &str,
) -> serde_json::Value {
    // Cap user-controlled substitutions so overlong reasons/answers cannot
    // bloat host responses; truncation marker preserves debuggability.
    let reason = cap_reason(reason);
    let answers = cap_reason(answers);
    match value {
        // Structured placeholder: replaces the whole value, not a substring,
        // because the payload is an array rather than text.
        serde_json::Value::String(s) if s.trim() == "{answer_groups}" => groups.clone(),
        serde_json::Value::String(s) => {
            let r = s
                .replace("{reason}", reason.as_str())
                .replace("{reason?}", reason.as_str())
                .replace("{event}", event)
                .replace("{answers}", answers.as_str())
                .replace("{reply}", reply);
            serde_json::Value::String(r)
        }
        serde_json::Value::Array(arr) => serde_json::Value::Array(
            arr.iter()
                .map(|v| render_template_value(v, &reason, event, &answers, groups, reply))
                .collect(),
        ),
        serde_json::Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, v) in map {
                // Omit answer-carrying keys when nothing was picked: absent
                // beats empty (an empty context string is noise the agent
                // still reads). Scoped to `{answers}` only — `{reason}`
                // keeps its exact-render behavior.
                if let serde_json::Value::String(s) = v {
                    let t = s.trim();
                    if answers.is_empty() && t == "{answers}" {
                        continue;
                    }
                    // `{reason?}`: optional reason — absent, never `""`
                    // (B2V legacy Claude deny carries no empty message).
                    if reason.is_empty() && t == "{reason?}" {
                        continue;
                    }
                    // Same rule for structured picks: no answers, no key.
                    if t == "{answer_groups}" && groups.as_array().is_some_and(|a| a.is_empty()) {
                        continue;
                    }
                }
                out.insert(
                    k.clone(),
                    render_template_value(v, &reason, event, &answers, groups, reply),
                );
            }
            serde_json::Value::Object(out)
        }
        _ => value.clone(),
    }
}

// ---------------------------------------------------------------------------
// question lifting: normalize agent-question (ask) payloads into PromptQuestion
// ---------------------------------------------------------------------------

fn obj_get<'a>(
    map: &'a serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Option<&'a serde_json::Value> {
    map.iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .map(|(_, v)| v)
}

fn find_questions_array<'a>(
    value: &'a serde_json::Value,
    keys: &[&str],
) -> Option<&'a Vec<serde_json::Value>> {
    match value {
        serde_json::Value::Object(map) => {
            for key in keys {
                if let Some(v) = obj_get(map, key) {
                    if let Some(arr) = v.as_array() {
                        return Some(arr);
                    }
                }
            }
            for (_, v) in map.iter() {
                if let Some(found) = find_questions_array(v, keys) {
                    return Some(found);
                }
            }
            None
        }
        serde_json::Value::Array(arr) => {
            for v in arr {
                if let Some(found) = find_questions_array(v, keys) {
                    return Some(found);
                }
            }
            None
        }
        _ => None,
    }
}

fn first_string(map: &serde_json::Map<String, serde_json::Value>, keys: &[&str]) -> Option<String> {
    for k in keys {
        if let Some(v) = obj_get(map, k) {
            if let Some(s) = v.as_str() {
                if !s.is_empty() {
                    return Some(s.to_string());
                }
            }
        }
    }
    None
}

fn scalar_to_string(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Null => None,
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn first_scalar(map: &serde_json::Map<String, serde_json::Value>, keys: &[&str]) -> Option<String> {
    for k in keys {
        if let Some(v) = obj_get(map, k) {
            if let Some(s) = scalar_to_string(v) {
                if !s.is_empty() {
                    return Some(s);
                }
            }
        }
    }
    None
}

fn first_usize(map: &serde_json::Map<String, serde_json::Value>, keys: &[&str]) -> Option<usize> {
    for k in keys {
        if let Some(v) = obj_get(map, k) {
            match v {
                serde_json::Value::Number(n) => {
                    if let Some(u) = n.as_u64() {
                        return Some(u as usize);
                    }
                    if let Some(i) = n.as_i64() {
                        if i >= 0 {
                            return Some(i as usize);
                        }
                    }
                }
                serde_json::Value::String(s) => {
                    if let Ok(u) = s.trim().parse::<usize>() {
                        return Some(u);
                    }
                }
                _ => continue,
            }
        }
    }
    None
}

struct LiftKeys<'a> {
    arrays: Vec<&'a str>,
    text: Vec<&'a str>,
    id: Vec<&'a str>,
    options: Vec<&'a str>,
    label: Vec<&'a str>,
}

fn with_override<'a>(ov: Option<&'a String>, defaults: &[&'static str]) -> Vec<&'a str> {
    let mut out = Vec::with_capacity(defaults.len() + 1);
    if let Some(o) = ov {
        if !o.trim().is_empty() {
            out.push(o.as_str());
        }
    }
    out.extend(defaults.iter().copied());
    out
}

fn lift_keys(shaped: Option<&ShapedAskTool>) -> LiftKeys<'_> {
    LiftKeys {
        arrays: with_override(
            shaped.and_then(|s| s.questions.as_ref()),
            &["questions", "prompts"],
        ),
        text: with_override(
            shaped.and_then(|s| s.text.as_ref()),
            &["question", "text", "prompt", "label", "title"],
        ),
        id: with_override(shaped.and_then(|s| s.id.as_ref()), &["id", "name", "key"]),
        options: with_override(
            shaped.and_then(|s| s.options.as_ref()),
            &["options", "choices", "alternatives"],
        ),
        label: with_override(
            shaped.and_then(|s| s.label.as_ref()),
            &["label", "name", "title", "value"],
        ),
    }
}

/// Best-effort question text from a flat (non-array) ask-tool payload, e.g.
/// `{"question": "Deploy?"}`. Only used for the human-readable description;
/// absence never changes whether the event counts as a question.
fn first_string_deep(value: &serde_json::Value, keys: &[&str]) -> Option<String> {
    let obj = value.as_object()?;
    for k in keys {
        if let Some(s) = obj.get(*k).and_then(|v| v.as_str()) {
            if !s.trim().is_empty() {
                return Some(s.to_string());
            }
        }
    }
    None
}

fn lift_questions(value: &serde_json::Value, keys: &LiftKeys<'_>) -> Vec<PromptQuestion> {
    const MAX_QUESTIONS: usize = 20;
    const MAX_OPTIONS: usize = 20;
    let Some(arr) = find_questions_array(value, &keys.arrays) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (i, item) in arr.iter().enumerate() {
        if out.len() >= MAX_QUESTIONS {
            break;
        }
        let serde_json::Value::Object(map) = item else {
            continue;
        };
        let Some(text) = first_string(map, &keys.text) else {
            continue;
        };
        if text.trim().is_empty() {
            continue;
        }
        let id = first_string(map, &keys.id)
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| format!("q{i}"));
        let header = first_string(map, &["header", "label", "title"])
            .filter(|s| !s.trim().is_empty() && s.trim() != text.trim());
        let mut options = Vec::new();
        if let Some(opt_arr) = keys
            .options
            .iter()
            .find_map(|k| obj_get(map, k))
            .and_then(|v| v.as_array())
        {
            for opt in opt_arr.iter().take(MAX_OPTIONS) {
                if options.len() >= MAX_OPTIONS {
                    break;
                }
                // Codex `request_user_input_async` sends bare strings
                // (`"options": ["Red", "Blue"]`); others send `{label, ...}`.
                let (label, description) = match opt {
                    serde_json::Value::String(label) => (label.clone(), None),
                    serde_json::Value::Object(omap) => {
                        let Some(label) = first_scalar(omap, &keys.label) else {
                            continue;
                        };
                        let description = first_scalar(omap, &["description", "desc", "hint"])
                            .filter(|s| !s.trim().is_empty());
                        (label, description)
                    }
                    _ => continue,
                };
                if label.trim().is_empty() {
                    continue;
                }
                options.push(PromptOption {
                    label,
                    description,
                    preview: None,
                });
            }
        }
        let multi = ["multi", "multiple", "multiSelect"]
            .iter()
            .any(|k| obj_get(map, k).and_then(coerce_bool).unwrap_or(false));
        let recommended = first_usize(map, &["recommended", "suggested"]);
        out.push(PromptQuestion {
            id,
            header,
            question: text,
            options,
            multiple: multi,
            // Every answer path (claude updatedInput, codex reason, answer
            // groups, pi/opencode plugins) carries `custom_input` through.
            custom: true,
            recommended,
        });
    }
    out
}

fn is_question_tool_name(ask_tools: &[crate::manifest::AskToolSpec], tool_name: &str) -> bool {
    let normalized = tool_name.trim().to_ascii_lowercase();
    if matches!(
        normalized.as_str(),
        "ask_question" | "askuserquestion" | "ask_user_question" | "request_user_input"
    ) || normalized.starts_with("request_user_input")
        || normalized.starts_with("requestuserinput")
    {
        return true;
    }
    ask_tools
        .iter()
        .any(|t| t.name().eq_ignore_ascii_case(tool_name))
}

fn has_non_empty_questions_array(value: &serde_json::Value) -> bool {
    let has_non_empty = |v: Option<&serde_json::Value>| matches!(v, Some(serde_json::Value::Array(items)) if !items.is_empty());
    if let Some(obj) = value.as_object() {
        if has_non_empty(obj.get("questions")) || has_non_empty(obj.get("prompts")) {
            return true;
        }
        for sub in &["tool_input", "toolInput", "input", "params"] {
            if let Some(sub_val) = obj.get(*sub) {
                if has_non_empty_questions_array(sub_val) {
                    return true;
                }
            }
        }
    }
    false
}

fn has_question_id_path(value: &serde_json::Value) -> bool {
    let has_id = |v: Option<&serde_json::Value>| {
        v.and_then(|x| x.get("id"))
            .and_then(|id| id.as_str())
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false)
    };
    if let Some(obj) = value.as_object() {
        if has_id(obj.get("question"))
            || has_id(obj.get("properties").and_then(|p| p.get("question")))
        {
            return true;
        }
        for sub in &["tool_input", "toolInput", "input", "params"] {
            if let Some(sub_val) = obj.get(*sub) {
                if has_question_id_path(sub_val) {
                    return true;
                }
            }
        }
    }
    false
}

fn pre_tool_use_implies_question(
    question_payload_heuristic: bool,
    ask_tools: &[crate::manifest::AskToolSpec],
    tool_name: &str,
    tool_input: &serde_json::Value,
    payloads: &[&serde_json::Value],
) -> bool {
    if is_question_tool_name(ask_tools, tool_name) {
        return true;
    }
    if question_payload_heuristic && !tool_name.eq_ignore_ascii_case("Bash") {
        if has_non_empty_questions_array(tool_input) || has_question_id_path(tool_input) {
            return true;
        }
        for p in payloads {
            if has_non_empty_questions_array(p) || has_question_id_path(p) {
                return true;
            }
        }
    }
    false
}

fn extract_call_id_from_payloads(payloads: &[&serde_json::Value]) -> Option<String> {
    for p in payloads {
        if let Some(obj) = p.as_object() {
            if let Some(id) = first_string(
                obj,
                &[
                    "tool_use_id",
                    "toolUseId",
                    "tool_call_id",
                    "toolCallId",
                    "call_id",
                    "callId",
                ],
            ) {
                if !id.trim().is_empty() {
                    return Some(id);
                }
            }
            if let Some(id) = obj
                .get("toolCall")
                .or_else(|| obj.get("tool_call"))
                .and_then(|tc| tc.get("id"))
                .and_then(|x| x.as_str())
            {
                if !id.trim().is_empty() {
                    return Some(id.to_string());
                }
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// legacy prompt fields + Claude answer echo (ports of B2V legacy
// src-tauri/src/agent_event/{payload.rs,prompt.rs})
// ---------------------------------------------------------------------------

/// Dotted lookup that also indexes arrays (`properties.questions.0.question`),
/// returning a non-blank string. Mirrors legacy `lookup_string_path`.
fn lookup_str(value: &serde_json::Value, path: &str) -> Option<String> {
    let mut cur = value;
    for seg in path.split('.') {
        cur = match cur {
            serde_json::Value::Object(map) => map.get(seg)?,
            serde_json::Value::Array(items) => items.get(seg.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    cur.as_str()
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
}

fn first_lookup(value: &serde_json::Value, paths: &[&str]) -> Option<String> {
    paths.iter().find_map(|p| lookup_str(value, p))
}

/// `(command, file_path, description)` in legacy `extract_prompt_fields`
/// precedence.
fn legacy_prompt_fields(
    value: &serde_json::Value,
    questions: &[PromptQuestion],
) -> (Option<String>, Option<String>, Option<String>) {
    let command = first_lookup(
        value,
        &[
            "command",
            "input.command",
            "tool_input.command",
            "toolInput.command",
            "tool.input.command",
            "toolCall.input.command",
            "tool_call.input.command",
            "properties.metadata.command",
            "metadata.command",
            "properties.command",
        ],
    );
    let file_path = first_lookup(
        value,
        &[
            "file_path",
            "filePath",
            "file",
            "path",
            "tool_input.file_path",
            "toolInput.filePath",
            "properties.file_path",
            "properties.filePath",
            "properties.path",
            "properties.patterns.0",
        ],
    );
    let description = first_lookup(
        value,
        &[
            "permissionDecisionReason",
            "hookSpecificOutput.decision.permissionDecisionReason",
            "description",
            "message",
            "text",
            "title",
            "question",
            "summary",
            "tool_input.description",
            "properties.questions.0.question",
            "properties.always.0",
        ],
    )
    .or_else(|| questions.first().map(|q| q.question.clone()));
    (command, file_path, description)
}

/// Legacy prompt.rs `build_claude_question_updated_input`: the questions
/// echoed back (header / options / `multiSelect` only when true) plus an
/// `answers` map keyed by question text; multi-picks and custom input join
/// with ", ".
fn claude_updated_input(
    questions: &[PromptQuestion],
    decision: &agent_hooks_protocol::PromptDecision,
) -> serde_json::Value {
    let mut answer_map = serde_json::Map::new();
    let items: Vec<serde_json::Value> = questions
        .iter()
        .enumerate()
        .map(|(index, q)| {
            let answer = decision
                .answers
                .iter()
                .find(|a| a.question_id == q.id)
                .or_else(|| decision.answers.get(index));
            let mut values: Vec<String> = answer
                .map(|a| a.selected_options.clone())
                .unwrap_or_default();
            if let Some(custom) = answer.and_then(|a| a.custom_input.clone()) {
                values.push(custom);
            }
            answer_map.insert(
                q.question.clone(),
                serde_json::Value::String(values.join(", ")),
            );

            let mut item = serde_json::Map::new();
            item.insert("question".into(), serde_json::json!(q.question));
            if let Some(header) = &q.header {
                item.insert("header".into(), serde_json::json!(header));
            }
            let options: Vec<serde_json::Value> = q
                .options
                .iter()
                .map(|o| {
                    let mut opt = serde_json::Map::new();
                    opt.insert("label".into(), serde_json::json!(o.label));
                    if let Some(d) = &o.description {
                        opt.insert("description".into(), serde_json::json!(d));
                    }
                    if let Some(p) = &o.preview {
                        opt.insert("preview".into(), serde_json::json!(p));
                    }
                    serde_json::Value::Object(opt)
                })
                .collect();
            item.insert("options".into(), serde_json::Value::Array(options));
            if q.multiple {
                item.insert("multiSelect".into(), serde_json::json!(true));
            }
            serde_json::Value::Object(item)
        })
        .collect();
    serde_json::json!({ "questions": items, "answers": answer_map })
}

/// Codex answer-by-block text. Mirrors Codex's own answered-question fragment
/// (`> question\n\nanswer`) so the model reads it like a native reply.
fn codex_answer_reason(
    questions: &[PromptQuestion],
    decision: &agent_hooks_protocol::PromptDecision,
) -> String {
    let replies: Vec<String> = decision
        .answers
        .iter()
        .enumerate()
        .map(|(index, answer)| {
            let question = questions
                .iter()
                .find(|q| q.id == answer.question_id)
                .or_else(|| questions.get(index))
                .map(|q| q.question.as_str())
                .unwrap_or("Question");
            let mut values = answer.selected_options.clone();
            if let Some(custom) = answer
                .custom_input
                .as_ref()
                .filter(|c| !c.trim().is_empty())
            {
                values.push(custom.clone());
            }
            format!(
                "> {}\n\n{}",
                question.replace(['\n', '\r'], " "),
                values.join(", ")
            )
        })
        .collect();
    format!(
        "The user already answered this question outside Codex. Use these answers and continue; do not ask again.\n\n{}",
        replies.join("\n\n")
    )
}

/// Legacy prompt.rs `extract_claude_permission_suggestions`: the first
/// non-empty suggestions array, passed back verbatim.
fn permission_suggestions(payload: &serde_json::Value) -> Option<serde_json::Value> {
    [
        "permission_suggestions",
        "permissionSuggestions",
        "properties.permission_suggestions",
        "properties.permissionSuggestions",
    ]
    .iter()
    .find_map(|p| walk_dotted(payload, p))
    .filter(|v| v.as_array().is_some_and(|a| !a.is_empty()))
    .cloned()
}

/// Display-ready summary of Claude's `permission_suggestions`, one entry per
/// element and in the same order, so a [`PromptDecision::suggestion`] index
/// addresses the same element that is echoed back as `updatedPermissions`.
fn describe_permission_suggestions(raw: &serde_json::Value) -> Vec<PromptSuggestion> {
    raw.as_array()
        .map(|items| items.iter().map(describe_permission_suggestion).collect())
        .unwrap_or_default()
}

fn describe_permission_suggestion(item: &serde_json::Value) -> PromptSuggestion {
    let text = |key: &str| item.get(key).and_then(|v| v.as_str());
    let session = text("destination") == Some("session");
    match text("type") {
        Some("addRules") | Some("replaceRules") => {
            let verb = if text("behavior") == Some("deny") {
                "Always deny"
            } else {
                "Always allow"
            };
            let rules: Vec<String> = item
                .get("rules")
                .and_then(|v| v.as_array())
                .map(|rules| {
                    rules
                        .iter()
                        .filter_map(|rule| {
                            let tool = rule.get("toolName").and_then(|v| v.as_str())?;
                            Some(match rule.get("ruleContent").and_then(|v| v.as_str()) {
                                Some(content) if !content.is_empty() => {
                                    format!("{tool}({content})")
                                }
                                _ => tool.to_string(),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            let target = if rules.is_empty() {
                "this".to_string()
            } else {
                rules.join(", ")
            };
            PromptSuggestion {
                label: format!("{verb} {target}"),
                kind: SuggestionKind::Rule,
            }
        }
        Some("setMode") => {
            let mode = match text("mode") {
                Some("auto") => "Auto mode".to_string(),
                Some("acceptEdits") => "Auto-accept edits".to_string(),
                Some("plan") => "Plan mode".to_string(),
                Some("bypassPermissions") => "Bypass permissions".to_string(),
                Some("default") => "Default mode".to_string(),
                Some(other) => other.to_string(),
                None => "Change mode".to_string(),
            };
            PromptSuggestion {
                label: if session {
                    format!("{mode} for this session")
                } else {
                    mode
                },
                kind: SuggestionKind::Mode,
            }
        }
        Some("addDirectories") => {
            let dirs = item
                .get("directories")
                .and_then(|v| v.as_array())
                .map(|dirs| {
                    dirs.iter()
                        .filter_map(|d| d.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .filter(|d| !d.is_empty())
                .unwrap_or_else(|| "this folder".to_string());
            PromptSuggestion {
                label: format!("Allow access to {dirs}"),
                kind: SuggestionKind::Directory,
            }
        }
        // Older shape: {"type":"tool","tool_name":..,"patterns":[..]}.
        Some("tool") => PromptSuggestion {
            label: format!("Always allow {}", text("tool_name").unwrap_or("this")),
            kind: SuggestionKind::Rule,
        },
        _ => PromptSuggestion {
            label: "Remember this".to_string(),
            kind: SuggestionKind::Other,
        },
    }
}

/// `updatedPermissions` for an allow-always: the one chosen suggestion, or all
/// of them when the decider did not choose. A stale index sends none, so the
/// call is approved once rather than remembering something unintended.
fn select_permission_suggestion(
    all: serde_json::Value,
    chosen: Option<usize>,
) -> serde_json::Value {
    match chosen {
        None => all,
        Some(index) => match all.get(index) {
            Some(entry) => serde_json::Value::Array(vec![entry.clone()]),
            None => serde_json::Value::Array(Vec::new()),
        },
    }
}

/// Structured placeholder (`{updated_input}`, `{updated_permissions}`): a
/// string value exactly equal to `name` is replaced by `with`, or its key is
/// omitted when there is nothing to send.
fn fill_placeholder(
    value: serde_json::Value,
    name: &str,
    with: Option<&serde_json::Value>,
) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => serde_json::Value::Object(
            map.into_iter()
                .filter_map(|(k, v)| match &v {
                    serde_json::Value::String(s) if s == name => with.map(|w| (k, w.clone())),
                    _ => Some((k, fill_placeholder(v, name, with))),
                })
                .collect(),
        ),
        serde_json::Value::Array(items) => serde_json::Value::Array(
            items
                .into_iter()
                .map(|v| fill_placeholder(v, name, with))
                .collect(),
        ),
        other => other,
    }
}

// ---------------------------------------------------------------------------
// notification + subagent-scope classification (ports of B2V legacy
// src-tauri/src/agent_event/lifecycle.rs)
// ---------------------------------------------------------------------------

/// Canonical `notification_type`: `permission` | `question` | `idle`, else
/// the raw string. Keys and substring buckets follow legacy
/// `notification_type_for_value` / `notification_status_for_agent`; Codex's
/// `wait` bucket (legacy `codex_notification_status`) counts as a question.
/// A typeless Gemini notification with `promptResponse: true` is idle.
fn notification_type_from_payload(payload: &serde_json::Value) -> Option<String> {
    let raw = [
        "notification_type",
        "notificationType",
        "subtype",
        "properties.notification_type",
        "properties.notificationType",
    ]
    .iter()
    .find_map(|p| walk_dotted_str(payload, p))
    .map(|s| s.trim().to_string())
    .filter(|s| !s.is_empty());
    let canonical = raw.as_deref().and_then(|t| {
        let t = t.to_ascii_lowercase();
        if t.contains("permission") {
            Some("permission")
        } else if t.contains("question") || t.contains("elicitation") || t.contains("wait") {
            Some("question")
        } else if ["completion", "complete", "idle", "finished", "done"]
            .iter()
            .any(|k| t.contains(k))
        {
            Some("idle")
        } else {
            None
        }
    });
    if let Some(c) = canonical {
        return Some(c.to_string());
    }
    let prompt_response = ["prompt_response", "promptResponse"]
        .iter()
        .find_map(|k| payload.get(*k))
        .and_then(coerce_bool)
        .unwrap_or(false);
    if prompt_response {
        return Some("idle".to_string());
    }
    raw
}

/// Legacy `event_has_parent_marker` || `event_has_subagent_mode`: the event
/// belongs to a child agent, not the session's own turn.
fn payload_is_subagent_scoped(payload: &serde_json::Value) -> bool {
    const PARENT_KEYS: &[&str] = &[
        "parentID",
        "parentId",
        "parent_id",
        "parentSessionID",
        "parentSessionId",
        "parent_session_id",
        "parentSessionHash",
        "parent_session_hash",
        "parentMessageID",
        "parentMessageId",
        "parent_message_id",
    ];
    const LOCATIONS: &[&str] = &[
        "",
        "properties",
        "session",
        "properties.session",
        "body",
        "properties.body",
        "message",
    ];
    let non_empty = |path: &str| {
        walk_dotted(payload, path)
            .and_then(|v| v.as_str())
            .is_some_and(|s| !s.trim().is_empty())
    };
    let has_parent = LOCATIONS.iter().any(|loc| {
        PARENT_KEYS.iter().any(|k| {
            if loc.is_empty() {
                non_empty(k)
            } else {
                non_empty(&format!("{loc}.{k}"))
            }
        })
    });
    // properties.message.* carries OpenCode's per-message parentID on every
    // turn, so the bare parentID/parentId/parent_id keys don't count there.
    let has_message_parent = PARENT_KEYS[3..]
        .iter()
        .any(|k| non_empty(&format!("properties.message.{k}")));
    let subagent_mode = [
        "mode",
        "properties.mode",
        "agent.mode",
        "properties.agent.mode",
        "config.mode",
        "agentConfig.mode",
        "agent_config.mode",
        "properties.config.mode",
        "properties.agentConfig.mode",
        "properties.agent_config.mode",
    ]
    .iter()
    .any(|p| {
        walk_dotted(payload, p)
            .and_then(|v| v.as_str())
            .is_some_and(|m| {
                matches!(
                    m.trim().to_ascii_lowercase().as_str(),
                    "subagent" | "sub_agent"
                )
            })
    });
    has_parent || has_message_parent || subagent_mode
}

// ---------------------------------------------------------------------------
// DeclarativeAdapter
#[derive(Default, Clone)]
struct EventResponseState {
    host_event: Option<String>,
    capability: Option<Capability>,
    verdicts: Option<Vec<crate::manifest::Verdict>>,
    lifecycle: Option<LifecycleRef>,
    tool_name: Option<String>,
    tool_input: Option<serde_json::Value>,
    questions: Vec<PromptQuestion>,
    is_question: bool,
    permission_suggestions: Option<serde_json::Value>,
}

pub struct DeclarativeAdapter {
    manifest: AdapterManifest,
    version: String,
    state: Mutex<EventResponseState>,
}

impl DeclarativeAdapter {
    pub fn new(manifest: AdapterManifest, version: String) -> Result<Self, AdapterError> {
        if !manifest.versions.contains_key(&version) {
            return Err(AdapterError::InvalidConfig(format!(
                "version '{}' not found for adapter '{}'",
                version, manifest.id
            )));
        }
        Ok(Self {
            manifest,
            version,
            state: Mutex::new(EventResponseState::default()),
        })
    }

    fn version_spec(&self) -> &VersionSpec {
        self.manifest
            .get_version(&self.version)
            .expect("version must exist; validated in new()")
    }

    fn find_event_spec(&self, host_event: &str) -> Option<(&String, &crate::manifest::EventSpec)> {
        let norm_target = normalize_event(host_event);
        for (k, v) in &self.version_spec().events {
            if normalize_event(k) == norm_target {
                return Some((k, v));
            }
        }
        None
    }

    fn resolve_host_event(
        &self,
        input: &RawHookInput,
        payload: &serde_json::Value,
    ) -> Option<String> {
        // 1. event_hint if it matches events map
        if let Some(hint) = &input.event_hint {
            let t = hint.trim();
            if !t.is_empty() {
                if self.find_event_spec(t).is_some() {
                    return Some(t.to_string());
                }
                // normalized match
                let norm = normalize_event(t);
                for k in self.version_spec().events.keys() {
                    if normalize_event(k) == norm {
                        return Some(k.clone());
                    }
                }
                // return hint as candidate even if not in map, for error reporting
                return Some(t.to_string());
            }
        }
        // 2. resolve_event_name fallback
        if let Some(name) = resolve_event_name(input, payload) {
            // check if name matches events map normalized; if so return canonical key
            if self.find_event_spec(&name).is_some() {
                // return canonical key to preserve manifest spelling?
                let norm = normalize_event(&name);
                for k in self.version_spec().events.keys() {
                    if normalize_event(k) == norm {
                        return Some(k.clone());
                    }
                }
                return Some(name);
            }
            return Some(name);
        }
        None
    }

    fn effective_sources<'a>(
        &'a self,
        field: &str,
        host_event_key: &str,
        lifecycle: &LifecycleRef,
    ) -> Option<Vec<ExtractionSource>> {
        // check extraction_overrides for host_event_key or lifecycle string
        let overrides = &self.version_spec().extraction_overrides;
        let mut inner: Option<&BTreeMap<String, Vec<ExtractionSource>>> = None;
        let norm_host = normalize_event(host_event_key);
        let lifecycle_str = lifecycle.as_str();
        let norm_lifecycle = normalize_event(lifecycle_str);
        for (k, v) in overrides {
            if normalize_event(k) == norm_host
                || normalize_event(k) == norm_lifecycle
                || k == host_event_key
                || k == lifecycle_str
            {
                inner = Some(v);
                break;
            }
        }
        if let Some(map) = inner {
            if let Some(sources) = map.get(field) {
                return Some(sources.clone());
            }
        }
        None
    }

    fn get_field_sources(
        &self,
        field: &str,
        host_event_key: &str,
        lifecycle: &LifecycleRef,
    ) -> Vec<ExtractionSource> {
        if let Some(ov) = self.effective_sources(field, host_event_key, lifecycle) {
            return ov;
        }
        self.version_spec().extraction.sources_with_defaults(field)
    }

    fn build_lifecycle(
        &self,
        spec: &crate::manifest::EventSpec,
        host_event_key: &str,
        payload: &serde_json::Value,
        input: &RawHookInput,
    ) -> Result<AgentLifecycle, AdapterError> {
        let lifecycle = &spec.lifecycle;
        // helper to extract string fields
        let session_sources = self.get_field_sources("session_id", host_event_key, lifecycle);
        let _session_dbg = extract_string_via_sources(payload, &session_sources);

        let prompt_sources = self.get_field_sources("prompt", host_event_key, lifecycle);
        let tool_name_sources = self.get_field_sources("tool_name", host_event_key, lifecycle);
        let tool_input_sources = self.get_field_sources("tool_input", host_event_key, lifecycle);
        let tool_output_sources = self.get_field_sources("tool_output", host_event_key, lifecycle);
        let error_sources = self.get_field_sources("error", host_event_key, lifecycle);
        let success_sources = self.get_field_sources("success", host_event_key, lifecycle);
        let summary_sources = self.get_field_sources("summary", host_event_key, lifecycle);

        match lifecycle {
            LifecycleRef::SessionStart => Ok(AgentLifecycle::SessionStart {
                resume_command: None,
            }),
            LifecycleRef::SessionEnd => Ok(AgentLifecycle::SessionEnd { exit_code: None }),
            LifecycleRef::UserPromptSubmit => {
                let prompt = extract_string_via_sources(payload, &prompt_sources);
                // also try transcript fallback if not found?
                Ok(AgentLifecycle::UserPromptSubmit {
                    user_prompt: prompt,
                })
            }
            LifecycleRef::PreToolUse => {
                // Tool names are preserved VERBATIM from the payload (no
                // canonical alias map): verbatim-as-contract so consumers
                // branch per harness.
                let tool_name = extract_string_via_sources(payload, &tool_name_sources)
                    .unwrap_or_else(|| "unknown".to_string());
                let input_val = extract_value_via_sources(payload, &tool_input_sources)
                    .unwrap_or(serde_json::Value::Null);
                // Ensure input is object-ish; keep as is
                Ok(AgentLifecycle::PreToolUse {
                    tool_name,
                    input: input_val,
                    prompt: None,
                })
            }
            LifecycleRef::PostToolUse => {
                // Tool names are preserved VERBATIM from the payload (no
                // canonical alias map): verbatim-as-contract so consumers
                // branch per harness.
                let tool_name = extract_string_via_sources(payload, &tool_name_sources)
                    .unwrap_or_else(|| "unknown".to_string());
                let output = extract_string_via_sources(payload, &tool_output_sources);
                let output_preview = output.or_else(|| {
                    extract_value_via_sources(payload, &tool_output_sources).map(|v| {
                        if let Some(s) = v.as_str() {
                            s.to_string()
                        } else {
                            v.to_string()
                        }
                    })
                });
                Ok(AgentLifecycle::PostToolUse {
                    tool_name,
                    output_preview: output_preview,
                })
            }
            LifecycleRef::PostToolUseFailure => {
                // Tool names are preserved VERBATIM from the payload (no
                // canonical alias map): verbatim-as-contract so consumers
                // branch per harness.
                let tool_name = extract_string_via_sources(payload, &tool_name_sources)
                    .unwrap_or_else(|| "unknown".to_string());
                let error = extract_string_via_sources(payload, &error_sources)
                    .unwrap_or_else(|| "tool failure".to_string());
                Ok(AgentLifecycle::PostToolUseFailure { tool_name, error })
            }
            LifecycleRef::PermissionRequest => {
                let prompt_id = input
                    .request_id
                    .clone()
                    .or_else(|| extract_string_via_sources(payload, &prompt_sources))
                    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                let tool_name = extract_string_via_sources(payload, &tool_name_sources);
                let description = extract_string_via_sources(payload, &prompt_sources)
                    .or_else(|| extract_string_via_sources(payload, &summary_sources));
                let pr = PromptRequest {
                    prompt_id,
                    kind: PromptKind::Permission,
                    tool_name,
                    command: None,
                    file_path: None,
                    description,
                    questions: Vec::new(),
                    suggestions: Vec::new(),
                };
                Ok(AgentLifecycle::PermissionRequest { prompt: pr })
            }
            LifecycleRef::Notification => {
                let message = extract_string_via_sources(payload, &summary_sources)
                    .or_else(|| extract_string_via_sources(payload, &prompt_sources))
                    .unwrap_or_else(|| "notification".to_string());
                Ok(AgentLifecycle::Notification {
                    message,
                    title: extract_string_field(payload, &["title"]),
                    notification_type: notification_type_from_payload(payload),
                })
            }
            LifecycleRef::SubagentStart => {
                let subagent_id = extract_string_via_sources(payload, &prompt_sources)
                    .or_else(|| extract_string_via_sources(payload, &tool_name_sources))
                    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                Ok(AgentLifecycle::SubagentStart {
                    subagent_id,
                    subagent_type: "general-purpose".to_string(),
                    description: extract_string_via_sources(payload, &summary_sources),
                })
            }
            LifecycleRef::SubagentStop => {
                let subagent_id = extract_string_via_sources(payload, &tool_name_sources)
                    .unwrap_or_else(|| "unknown".to_string());
                // Bool-required site: unknown success preserves the prior
                // allow-leaning default; the coercion itself yields None for
                // missing/unrecognized values (never default-true there).
                let success = extract_value_via_sources(payload, &success_sources)
                    .as_ref()
                    .and_then(coerce_bool)
                    .or_else(|| {
                        extract_string_via_sources(payload, &success_sources)
                            .and_then(|s| coerce_bool(&serde_json::Value::String(s)))
                    })
                    .unwrap_or(true);
                let summary = extract_string_via_sources(payload, &summary_sources);
                Ok(AgentLifecycle::SubagentStop {
                    subagent_id,
                    success,
                    summary,
                })
            }
            LifecycleRef::TurnStart => Ok(AgentLifecycle::TurnStart { turn_index: None }),
            LifecycleRef::TurnEnd => Ok(AgentLifecycle::TurnEnd { turn_index: None }),
            LifecycleRef::Stop => {
                // A manifest may demote a weak Stop to TurnEnd (derived
                // status stays Running) on a payload condition — e.g.
                // Antigravity declares `demote_when: {"fullyIdle": false}`:
                // background tasks still running, the agent is NOT done.
                if spec
                    .demote_when
                    .as_ref()
                    .is_some_and(|when| when.iter().all(|(k, v)| payload.get(k) == Some(v)))
                {
                    return Ok(AgentLifecycle::TurnEnd { turn_index: None });
                }
                let summary = extract_string_via_sources(payload, &summary_sources)
                    .or_else(|| extract_string_via_sources(payload, &prompt_sources));
                let mut success = extract_value_via_sources(payload, &success_sources)
                    .as_ref()
                    .and_then(coerce_bool)
                    .or_else(|| {
                        extract_string_via_sources(payload, &success_sources)
                            .and_then(|s| coerce_bool(&serde_json::Value::String(s)))
                    })
                    .unwrap_or(true);
                // A non-empty canonical error (e.g. Antigravity `error:
                // "..."`) fails the stop even when no explicit success
                // source says so. Scoped to `error`/`error_message` only:
                // broader error-source lists (e.g. `message`) double as
                // summary text on other harnesses.
                if success {
                    if let Some(err) = extract_string_field(payload, &["error", "error_message"]) {
                        if !err.trim().is_empty() {
                            success = false;
                        }
                    }
                }
                Ok(AgentLifecycle::Stop { summary, success })
            }
            LifecycleRef::PreCompact => {
                let trigger = extract_string_via_sources(payload, &summary_sources)
                    .or_else(|| extract_string_via_sources(payload, &prompt_sources));
                Ok(AgentLifecycle::PreCompact { trigger })
            }
            LifecycleRef::PostCompact => {
                let trigger = extract_string_via_sources(payload, &summary_sources)
                    .or_else(|| extract_string_via_sources(payload, &prompt_sources));
                Ok(AgentLifecycle::PostCompact { trigger })
            }
            LifecycleRef::Interrupt => Ok(AgentLifecycle::Interrupt),
            LifecycleRef::PostToolBatch => Ok(AgentLifecycle::PostToolBatch),
            LifecycleRef::PreLLMCall => Ok(AgentLifecycle::PreLLMCall),
            LifecycleRef::PostLLMCall => Ok(AgentLifecycle::PostLLMCall),
            LifecycleRef::StopFailure => Ok(AgentLifecycle::StopFailure {
                error: extract_string_via_sources(payload, &error_sources)
                    .unwrap_or_else(|| "model call failed".to_string()),
            }),
            LifecycleRef::ToolUseStart => Ok(AgentLifecycle::ToolUseStart),
        }
    }
}

impl std::fmt::Debug for DeclarativeAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeclarativeAdapter")
            .field("id", &self.manifest.id)
            .field("version", &self.version)
            .finish()
    }
}

impl HarnessAdapter for DeclarativeAdapter {
    fn id(&self) -> &str {
        &self.manifest.id
    }

    fn display_name(&self) -> &str {
        &self.manifest.display_name
    }

    fn aliases(&self) -> &[&str] {
        // Trick: need &'static? Manifest stores owned Strings, but trait expects &[&str] with self lifetime.
        // We leak aliases as static? Instead, we return empty and handle alias matching via registry override.
        // For correctness, we can store aliases in manifest and registry will check manifest's aliases via load_embedded scoring.
        // For direct adapter, return empty. Registry-level alias handling will use manifest aliases.
        &[]
    }

    fn hook_definitions(&self) -> Vec<HookDefinition> {
        let vs = self.version_spec();
        let mut defs = Vec::new();
        // Collect host event names from events map keys
        let host_events: Vec<String> = vs.events.keys().cloned().collect();
        // Find JsonHooks/TomlHooks ops
        for op in &vs.install {
            match op {
                crate::manifest::InstallOp::JsonHooks {
                    paths,
                    root: _,
                    entry_shape: _,
                    name: _,
                    fixed_fields: _,
                    entry,
                    dedupe: _,
                    entry_overrides: _,
                    ..
                }
                | crate::manifest::InstallOp::TomlHooks {
                    paths,
                    create: _,
                    root: _,
                    entry_shape: _,
                    entry,
                }
                | crate::manifest::InstallOp::TomlListHooks {
                    paths,
                    create: _,
                    list_key: _,
                    entry,
                    entry_overrides: _,
                } => {
                    // Determine command template from entry
                    let cmd_template = entry
                        .get("command")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| {
                            format!("agent-hooks hook {} --event {{event}}", self.id())
                        });
                    for host_event in &host_events {
                        let rendered_cmd = cmd_template.replace("{event}", host_event);
                        // {command} left as is for installer; hook_definitions keeps it literal
                        for p in paths {
                            let location = if p.starts_with("~/") {
                                HookLocation::GlobalFile(p.clone())
                            } else if p.starts_with('.')
                                || p.starts_with('/') && !p.starts_with("~/")
                            {
                                // heuristic: project files start with '.' or are relative with '.'
                                if p.starts_with('.') {
                                    HookLocation::ProjectFile(p.clone())
                                } else {
                                    HookLocation::Custom(p.clone())
                                }
                            } else {
                                HookLocation::Custom(p.clone())
                            };
                            defs.push(HookDefinition::new(
                                location,
                                HookEventTarget::Lifecycle(host_event.clone()),
                                rendered_cmd.clone(),
                            ));
                        }
                    }
                }
                _ => continue,
            }
        }
        defs
    }

    fn parse_event(&self, input: &RawHookInput) -> Result<AgentEvent, AdapterError> {
        let payload = parse_payload(&input.raw_payload)?;
        // Manifest-declared noise (Codex realtime voice) never becomes an
        // event: checked before any state capture so format_response falls
        // back to the observe no-op.
        if let Some(rule) = self
            .version_spec()
            .ignore_when
            .iter()
            .find(|r| r.matches(&payload))
        {
            return Err(AdapterError::Ignored(format!(
                "{} payload matched ignore_when {:?}",
                self.manifest.id, rule.fields
            )));
        }
        // Unwrap JSON-RPC envelope params as secondary payload but keep envelope for event extraction
        let (event_payload, payload_for_fields) = if payload.get("jsonrpc").is_some() {
            if let Some(params) = payload.get("params") {
                (payload.clone(), params.clone())
            } else {
                (payload.clone(), payload.clone())
            }
        } else {
            (payload.clone(), payload.clone())
        };

        let host_event = first_across(&[&event_payload, &payload_for_fields], |p| {
            self.resolve_host_event(input, p)
        })
        .ok_or_else(|| AdapterError::UnsupportedEvent("unknown event".to_string()))?;

        let mut spec = self
            .find_event_spec(&host_event)
            .map(|(_, s)| s.clone())
            .ok_or_else(|| AdapterError::UnsupportedEvent(host_event.clone()))?;
        if self.manifest.id == "antigravity" && host_event == "PreInvocation" {
            spec.lifecycle = if payload_for_fields
                .get("invocationNum")
                .and_then(serde_json::Value::as_u64)
                == Some(0)
            {
                LifecycleRef::UserPromptSubmit
            } else {
                LifecycleRef::PreLLMCall
            };
        }

        // capture for format_response
        {
            let mut st = self.state.lock();
            st.host_event = Some(host_event.clone());
            st.capability = Some(spec.capability);
            st.verdicts = spec.verdicts.clone();
            st.lifecycle = Some(spec.lifecycle.clone());
            st.tool_name = None;
            st.tool_input = None;
            st.questions.clear();
            st.is_question = false;
            st.permission_suggestions = None;
        }

        // Session-less harnesses get a documented ephemeral session: try the
        // payload, event payload, and full payload, then context/input
        // extractors (same order as before), and fall back to a fresh UUID
        // instead of erroring. No downstream invariant here requires a real
        // id (session grouping is best-effort), so MissingSessionId is never
        // raised from this path.
        let sources = self.get_field_sources("session_id", &host_event, &spec.lifecycle);
        let payloads = [&payload_for_fields, &event_payload, &payload];
        let extracted_session =
            first_across(&payloads, |p| extract_string_via_sources(p, &sources))
                .or_else(|| first_across(&payloads, |p| extract_session_id(p, input)));
        // History keys only on a session id the harness really sent.
        let native_session = extracted_session.is_some();
        let session_id = extracted_session.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

        let mut lifecycle = self
            .build_lifecycle(&spec, &host_event, &payload_for_fields, input)
            .or_else(|_| self.build_lifecycle(&spec, &host_event, &event_payload, input))?;

        match &mut lifecycle {
            AgentLifecycle::PreToolUse {
                tool_name,
                input: tool_input,
                prompt,
            } => {
                let ask_tools = &self.version_spec().ask_tools;
                let is_question = spec.question_prompt != Some(QuestionPrompt::None)
                    && pre_tool_use_implies_question(
                        spec.question_payload_heuristic,
                        ask_tools,
                        tool_name,
                        tool_input,
                        &payloads,
                    );
                if is_question {
                    let shaped = self.version_spec().question_keys_for(tool_name);
                    let keys = lift_keys(shaped);
                    let mut questions = lift_questions(tool_input, &keys);
                    if questions.is_empty() {
                        for p in &payloads {
                            questions = lift_questions(p, &keys);
                            if !questions.is_empty() {
                                break;
                            }
                        }
                    }
                    let prompt_id = extract_call_id_from_payloads(&payloads)
                        .or_else(|| extract_call_id_from_payloads(&[tool_input]))
                        .or_else(|| input.request_id.clone())
                        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                    let description = questions
                        .first()
                        .map(|q| q.question.clone())
                        .or_else(|| first_string_deep(tool_input, &keys.text));
                    *prompt = Some(PromptRequest {
                        prompt_id,
                        kind: PromptKind::Question,
                        tool_name: Some(tool_name.clone()),
                        command: None,
                        file_path: None,
                        description,
                        questions: questions.clone(),
                        suggestions: Vec::new(),
                    });
                    let mut st = self.state.lock();
                    st.tool_name = Some(tool_name.clone());
                    st.tool_input = Some(tool_input.clone());
                    st.questions = questions;
                    st.is_question = true;
                } else {
                    let mut st = self.state.lock();
                    st.tool_name = Some(tool_name.clone());
                    st.tool_input = Some(tool_input.clone());
                }
            }
            AgentLifecycle::PermissionRequest { prompt: pr } => {
                let suggestions =
                    first_across(&[&payload_for_fields, &event_payload, &payload], |p| {
                        permission_suggestions(p)
                    });
                pr.suggestions = suggestions
                    .as_ref()
                    .map(describe_permission_suggestions)
                    .unwrap_or_default();
                self.state.lock().permission_suggestions = suggestions;
                let keys = lift_keys(None);
                let questions = first_across(&[&payload, &payload_for_fields], |p| {
                    let q = lift_questions(p, &keys);
                    if q.is_empty() {
                        None
                    } else {
                        Some(q)
                    }
                })
                .unwrap_or_default();
                if !questions.is_empty() {
                    if spec.question_prompt != Some(QuestionPrompt::Permission) {
                        pr.kind = PromptKind::Question;
                    }
                    // Kept for `{updated_input}` when the answer is rendered.
                    self.state.lock().questions = questions.clone();
                    pr.questions = questions;
                }
            }
            _ => {}
        }

        // Legacy payload.rs extract_prompt_fields: every prompt carries the
        // command / file path / description the human needs to decide.
        let prompt_ref = match &mut lifecycle {
            AgentLifecycle::PermissionRequest { prompt } => Some((prompt, true)),
            AgentLifecycle::PreToolUse {
                prompt: Some(prompt),
                ..
            } => Some((prompt, false)),
            _ => None,
        };
        if let Some((prompt, is_permission)) = prompt_ref {
            let fields = first_across(&[&payload_for_fields, &event_payload, &payload], |p| {
                let f = legacy_prompt_fields(p, &prompt.questions);
                (f.0.is_some() || f.1.is_some() || f.2.is_some()).then_some(f)
            });
            if let Some((command, file_path, description)) = fields {
                prompt.command = prompt.command.take().or(command);
                prompt.file_path = prompt.file_path.take().or(file_path);
                if is_permission {
                    prompt.description = description.or(prompt.description.take());
                }
            }
        }

        let mut context = input.context.clone();
        // Payload cwd is authoritative: several harnesses spawn hooks with
        // the process cwd pointed at their own config dir (Antigravity:
        // `~/.gemini/config`). Prefer `workspacePaths[0]`-style sources.
        // Missing/unusable payload cwd falls back to the shim context (never
        // emits garbage paths): `context` keeps `input.context` unless a
        // cleaned payload candidate above replaces it.
        let cwd_sources = self.get_field_sources("cwd", &host_event, &spec.lifecycle);
        let ignore_cwd = &self.version_spec().ignore_cwd_patterns;
        let payload_cwd = first_across(&[&payload_for_fields, &event_payload, &payload], |p| {
            extract_cwd_via_sources(p, &cwd_sources, ignore_cwd)
        });
        if let Some(cwd) = payload_cwd {
            // The shim-computed git worktree derives from the (possibly
            // wrong) process cwd — drop it when it looks config-derived so
            // consumers don't pin the session to the harness config dir.
            let worktree_is_config = context
                .git_worktree
                .as_ref()
                .map(|p| {
                    let s = p.to_string_lossy();
                    ignore_cwd.iter().any(|pat| s.contains(pat.as_str()))
                })
                .unwrap_or(true);
            if worktree_is_config {
                context.git_worktree = Some(cwd.clone());
            }
            context.cwd = Some(cwd);
        }
        // Payload-authoritative tmux: an event whose override declares a
        // `tmux_pane_id` source says the shim's tmux env is stale (jcode's
        // shared server spawns hooks from its own pane). Clear it, then take
        // the client's pane from the payload when present (B2V legacy
        // agent_event/mod.rs enrich_jcode_client_attachment_context).
        if let Some(pane_sources) =
            self.effective_sources("tmux_pane_id", &host_event, &spec.lifecycle)
        {
            let from = |field: &str| {
                self.effective_sources(field, &host_event, &spec.lifecycle)
                    .and_then(|src| {
                        first_across(&[&payload_for_fields, &event_payload, &payload], |p| {
                            extract_string_via_sources(p, &src)
                        })
                    })
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
            };
            context.tmux_session_name = None;
            context.tmux_window_index = None;
            context.tmux_pane_index = None;
            context.tmux_client_pid = None;
            context.tmux_pane_id =
                first_across(&[&payload_for_fields, &event_payload, &payload], |p| {
                    extract_string_via_sources(p, &pane_sources)
                })
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            if context.tmux_pane_id.is_some() {
                context.tmux_session_name = from("tmux_session_name");
                context.tmux_window_index = from("tmux_window_index").and_then(|s| s.parse().ok());
            }
        }
        // Parent passthrough: set only when the harness payload carries it.
        // No fallback, no UUID — absent means "no known parent", never an error.
        // A session is never its own parent (Codex SubagentStart falls back to
        // the thread id for both; legacy identity.rs codex_subagent_identity).
        let parent_sources =
            self.get_field_sources("parent_session_id", &host_event, &spec.lifecycle);
        let parent_session_id =
            first_across(&[&payload_for_fields, &event_payload, &payload], |p| {
                extract_string_via_sources(p, &parent_sources)
            })
            .filter(|parent| *parent != session_id);
        // A child's Stop is not the session's turn ending (legacy
        // decide_status: stop + subagent_scoped → busy).
        if spec.demote_when_subagent
            && matches!(lifecycle, AgentLifecycle::Stop { .. })
            && (parent_session_id.is_some()
                || [&payload_for_fields, &event_payload, &payload]
                    .iter()
                    .any(|p| payload_is_subagent_scoped(p)))
        {
            lifecycle = AgentLifecycle::TurnEnd { turn_index: None };
        }
        let mut event = AgentEvent::new(self.id(), session_id, lifecycle, context);
        event.passthrough_env = input.passthrough_env.clone();
        event.parent_session_id = parent_session_id;
        // turn_id fallback: input turn_id if present else the event_id, so
        // every emitted event carries a non-empty turn_id. (AgentEvent::new
        // leaves turn_id None; any other AgentEvent construction site must
        // apply the same fallback.)
        let native_turn_id = first_across(&[&payload_for_fields, &event_payload, &payload], |p| {
            extract_string_field(p, &["turn_id", "turnId"]).filter(|s| !s.trim().is_empty())
        });
        event.turn_id = Some(
            native_turn_id
                .clone()
                .unwrap_or_else(|| event.event_id.clone()),
        );
        // Native identity for the history store, captured before any fallback.
        // Only declared/conventional key names are read — no recursive id hunt.
        let message_id_sources =
            path_sources_only(self.get_field_sources("message_id", &host_event, &spec.lifecycle));
        let tool_call_id_sources =
            path_sources_only(self.get_field_sources("tool_call_id", &host_event, &spec.lifecycle));
        let id_payloads = [&payload_for_fields, &event_payload, &payload];
        event.history_identity = Some(HistoryIdentity {
            native_session,
            native_turn_id,
            native_message_id: first_across(&id_payloads, |p| {
                extract_string_via_sources(p, &message_id_sources)
            }),
            native_tool_call_id: first_across(&id_payloads, |p| {
                extract_string_via_sources(p, &tool_call_id_sources)
            }),
            delivery_id: None,
        });
        // Harness-pushed title: passthrough only, blank means "unknown".
        let title_sources = self.get_field_sources("session_title", &host_event, &spec.lifecycle);
        event.session_title = first_across(&[&payload_for_fields, &event_payload, &payload], |p| {
            extract_string_via_sources(p, &title_sources)
        })
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty());
        let mut flavor_sources =
            self.get_field_sources("flavor_text", &host_event, &spec.lifecycle);
        for source in self
            .version_spec()
            .extraction
            .sources_with_defaults("flavor_text")
        {
            if !flavor_sources.contains(&source) {
                flavor_sources.push(source);
            }
        }
        event.flavor_text = flavor_sources.iter().find_map(|source| {
            let ExtractionSource::Path(path) = source else {
                return None;
            };
            [&payload_for_fields, &event_payload, &payload]
                .iter()
                .find_map(|payload| walk_dotted(payload, path).and_then(flavor_from_value))
        });
        event.raw_event = Some(input.raw_payload.clone());
        Ok(event)
    }
    fn normalize_response(
        &self,
        decision: &agent_hooks_protocol::PromptDecision,
    ) -> agent_hooks_protocol::NormalizedResponse {
        use agent_hooks_protocol::NormalizedResponse;
        let cap = self.state.lock().capability;
        match cap {
            Some(Capability::Continue) => {
                if decision.allow {
                    NormalizedResponse::Proceed
                } else {
                    NormalizedResponse::Continue {
                        reason: decision.custom_input.clone().unwrap_or_default(),
                    }
                }
            }
            Some(Capability::Gate) => {
                let st = self.state.lock();
                let host_event = st.host_event.clone();
                let lifecycle_key = st.lifecycle.as_ref().map(|l| l.as_str().to_string());
                drop(st);
                let ask_allowed = host_event
                    .as_deref()
                    .map(|key| self.version_spec().ask_allowed(key))
                    .or_else(|| {
                        lifecycle_key
                            .as_deref()
                            .map(|key| self.version_spec().ask_allowed(key))
                    })
                    .unwrap_or(true);
                NormalizedResponse::from_gate_decision(decision, ask_allowed)
            }
            Some(Capability::Observe) | None => NormalizedResponse::Proceed,
        }
    }

    fn format_response(
        &self,
        decision: &agent_hooks_protocol::PromptDecision,
    ) -> serde_json::Value {
        let st = self.state.lock().clone();
        let host_event = st.host_event.unwrap_or_default();
        let capability = st.capability;
        let vs = self.version_spec();
        // Context for the model rides only on a non-blocking verdict, through
        // the event's own `inject` template. No template = dropped silently.
        if decision.allow || decision.defer {
            if let Some(ctx) = decision
                .additional_context
                .as_deref()
                .filter(|c| !c.trim().is_empty())
            {
                let template = vs.response.inject.get(&host_event).or_else(|| {
                    st.lifecycle
                        .as_ref()
                        .and_then(|l| vs.response.inject.get(l.as_str()))
                });
                if let Some(template) = template {
                    return render_inject(template, &host_event, &cap_context(ctx));
                }
            }
        }
        // Defer = no opinion: emit the host's "no hook output" shape (the
        // observe value) for every capability, so the host runs its native
        // dialog/policy. Never render it through a gate template — defer
        // carries allow=false and would come out as a deny.
        if decision.defer {
            let deferred = st
                .lifecycle
                .as_ref()
                .and_then(|l| vs.response.defer_overrides.get(l.as_str()).cloned())
                .unwrap_or_else(|| vs.response.observe_value());
            return render_template_value(
                &deferred,
                "",
                &host_event,
                "",
                &serde_json::Value::Array(Vec::new()),
                "",
            );
        }
        let reason = decision.custom_input.clone().unwrap_or_default();
        let event_name = host_event.clone();
        let answers = decision
            .answers
            .iter()
            .flat_map(|a| a.selected_options.iter().cloned())
            .collect::<Vec<_>>()
            .join(",");
        let groups = answer_groups(decision);
        let reply = reply_verb(decision);

        match capability {
            Some(Capability::Gate) => {
                let lifecycle_key = st.lifecycle.map(|l| l.as_str().to_string());
                let normalized = self.normalize_response(decision);
                fn pick<'a>(
                    g: &'a crate::manifest::GateResponse,
                    n: &agent_hooks_protocol::NormalizedResponse,
                ) -> &'a serde_json::Value {
                    use agent_hooks_protocol::NormalizedResponse as N;
                    match n {
                        // Escalate where the host has an ask dialect;
                        // otherwise fail safe to deny (Cursor treats ask as
                        // deny, Codex ignores unknown shapes). Ask reaches
                        // here only when the event's verdicts allow it —
                        // `normalize_response` already degraded the rest.
                        N::Ask { .. } => g.ask.as_ref().unwrap_or(&g.deny),
                        N::Proceed => &g.allow,
                        N::Deny { .. } | N::Continue { .. } => &g.deny,
                    }
                }
                let override_gate = vs.response.response_overrides.get(&host_event).or_else(|| {
                    lifecycle_key
                        .as_deref()
                        .and_then(|key| vs.response.response_overrides.get(key))
                });
                let template = override_gate
                    .map(|g| pick(g, &normalized))
                    .or_else(|| vs.response.gate.as_ref().map(|g| pick(g, &normalized)));
                let is_deny_block_reason = vs.response.question_answer_strategy
                    == Some(crate::manifest::QuestionAnswerStrategy::DenyBlockReason);
                if is_deny_block_reason
                    && (event_name.eq_ignore_ascii_case("PreToolUse")
                        || lifecycle_key.as_deref() == Some("PreToolUse"))
                    && (st.is_question || !st.questions.is_empty() || !decision.answers.is_empty())
                {
                    // Harness has no in-band answer channel: `updatedInput` only
                    // re-parses the tool's own args. A block reason is handed to the model as the
                    // tool result, so answers travel there. Allow without
                    // answers = let host's own question UI run untouched.
                    return if !decision.answers.is_empty() {
                        serde_json::json!({
                            "hookSpecificOutput": {
                                "hookEventName": "PreToolUse",
                                "permissionDecision": "deny",
                                "permissionDecisionReason": codex_answer_reason(
                                    &st.questions,
                                    decision,
                                ),
                            }
                        })
                    } else if decision.allow {
                        serde_json::json!({})
                    } else {
                        let mut out = serde_json::json!({
                            "hookSpecificOutput": {
                                "hookEventName": "PreToolUse",
                                "permissionDecision": "deny",
                            }
                        });
                        if !reason.is_empty() {
                            out["hookSpecificOutput"]["permissionDecisionReason"] =
                                serde_json::Value::String(reason);
                        }
                        out
                    };
                }
                // Structured answers ride along whenever the decider gave
                // them: the template dialect owns allow/deny, while the ask
                // bridge in the TS shims parses `answers` to resolve the
                // tool call. Additive — older readers ignore unknown keys.
                if let Some(template) = template {
                    let rendered = render_template_value(
                        template,
                        &reason,
                        &event_name,
                        &answers,
                        &groups,
                        reply,
                    );
                    let updated = (!decision.answers.is_empty())
                        .then(|| claude_updated_input(&st.questions, decision));
                    let original_input = (decision.allow && vs.response.allow_requires_tool_input)
                        .then(|| st.tool_input.clone());
                    let updated = updated
                        .as_ref()
                        .or_else(|| original_input.as_ref().and_then(|input| input.as_ref()));
                    let rendered = fill_placeholder(rendered, "{updated_input}", updated);
                    // Legacy prompt.rs build_claude_permission_response: only
                    // AllowAlways carries the host's own suggestions back.
                    let permissions = decision
                        .allow_always
                        .then(|| st.permission_suggestions.clone())
                        .flatten()
                        .map(|all| select_permission_suggestion(all, decision.suggestion));
                    fill_placeholder(rendered, "{updated_permissions}", permissions.as_ref())
                } else if decision.allow {
                    serde_json::json!({"decision":"allow"})
                } else {
                    let mut v = serde_json::json!({"decision":"deny"});
                    if !reason.is_empty() {
                        v["reason"] = serde_json::Value::String(reason);
                    }
                    v
                }
            }
            Some(Capability::Continue) => {
                if let Some(cont) = &vs.response.continue_response {
                    // normalize_response maps continue-allow to Proceed;
                    // every other outcome (deny, ask) keeps the agent going.
                    let template = match self.normalize_response(decision) {
                        agent_hooks_protocol::NormalizedResponse::Proceed => &cont.stop,
                        _ => &cont.continue_,
                    };
                    render_template_value(template, &reason, &event_name, &answers, &groups, reply)
                } else if decision.allow {
                    serde_json::json!({})
                } else {
                    serde_json::json!({"reason": reason})
                }
            }
            Some(Capability::Observe) | None => {
                // empty_on_undecided handling: if true and decision is allow with no reason, return observe empty?
                // For now, always return observe value
                let obs = vs.response.observe_value();
                render_template_value(&obs, &reason, &event_name, &answers, &groups, reply)
            }
        }
    }

    fn response_mode(&self) -> ResponseMode {
        self.version_spec().response.mode
    }

    fn capability_for(&self, _event: &AgentEvent) -> Option<Capability> {
        self.state.lock().capability
    }
    fn verdicts_for(&self, _event: &AgentEvent) -> Option<Vec<crate::manifest::Verdict>> {
        self.state.lock().verdicts.clone()
    }
    fn entry_timeout_for(&self, _event: &AgentEvent) -> Option<std::time::Duration> {
        let host_event = self.state.lock().host_event.clone()?;
        let secs = self.version_spec().install.iter().find_map(|op| match op {
            crate::manifest::InstallOp::JsonHooks {
                entry,
                entry_overrides,
                ..
            } => entry_overrides
                .get(&host_event)
                .and_then(|o| o.get("timeout"))
                .and_then(|t| t.as_u64())
                .or_else(|| entry.get("timeout").and_then(|t| t.as_u64())),
            crate::manifest::InstallOp::TomlHooks { entry, .. } => {
                entry.get("timeout").and_then(|t| t.as_u64())
            }
            crate::manifest::InstallOp::TomlListHooks {
                entry,
                entry_overrides,
                ..
            } => entry_overrides
                .get(&host_event)
                .and_then(|o| o.get("timeout"))
                .and_then(|t| t.as_u64())
                .or_else(|| entry.get("timeout").and_then(|t| t.as_u64())),
            _ => None,
        });
        // Command-only host entries (Kiro) carry no timeout: fall back to the
        // host's documented default from the manifest.
        let secs = secs.or(self.version_spec().host_timeout_secs)?;
        Some(std::time::Duration::from_secs(secs))
    }
    fn matches_input(&self, input: &RawHookInput) -> bool {
        // delegate to DetectSpec scoring via registry; but for direct adapter, check hint
        let hint = crate::traits::normalize_hint(&input.harness_hint);
        let id_norm = crate::traits::normalize_hint(self.id());
        if hint == id_norm {
            return true;
        }
        for alias in &self.manifest.aliases {
            if hint == crate::traits::normalize_hint(alias) {
                return true;
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::AdapterManifest;
    use agent_hooks_protocol::PromptDecision;

    fn ask_test_manifest(with_ask: bool) -> AdapterManifest {
        let mut gate = serde_json::json!({
            "allow": {"permission": "allow"},
            "deny": {"permission": "deny", "reason": "{reason}"}
        });
        if with_ask {
            gate["ask"] = serde_json::json!({"permission": "ask", "question": "{reason}"});
        }
        let text = serde_json::json!({
            "id": "asktest",
            "display_name": "Ask Test",
            "default_version": "v1",
            "detect": {"harness_hint": ["asktest"]},
            "versions": {"v1": {
                "install": [],
                "events": {"preToolUse": {"lifecycle": "PreToolUse", "capability": "gate"}},
                "extraction": {
                    "session_id": ["session_id"],
                    "prompt": ["prompt"],
                    "tool_name": ["tool_name"],
                    "tool_input": ["tool_input"],
                    "tool_output": ["tool_output"],
                    "error": ["error"],
                    "success": ["success"]
                },
                "response": {"mode": "json", "gate": gate, "observe": {}}
            }}
        })
        .to_string();
        AdapterManifest::parse(&text).unwrap()
    }

    fn ask_test_input() -> RawHookInput {
        RawHookInput {
            harness_hint: "asktest".to_string(),
            event_hint: Some("preToolUse".to_string()),
            raw_payload: r#"{"session_id":"s","tool_name":"Bash","tool_input":{}}"#.to_string(),
            context: agent_hooks_protocol::EventContext::default(),
            request_id: None,
            passthrough_env: Default::default(),
        }
    }

    #[test]
    fn ask_renders_template_where_supported() {
        let adapter = DeclarativeAdapter::new(ask_test_manifest(true), "v1".to_string()).unwrap();
        adapter.parse_event(&ask_test_input()).unwrap();
        let out = adapter.format_response(&PromptDecision::ask(Some("deploy?".to_string())));
        assert_eq!(out["permission"], "ask");
        assert_eq!(out["question"], "deploy?");
    }

    #[test]
    fn ask_falls_back_to_deny_without_template() {
        let adapter = DeclarativeAdapter::new(ask_test_manifest(false), "v1".to_string()).unwrap();
        adapter.parse_event(&ask_test_input()).unwrap();
        let out = adapter.format_response(&PromptDecision::ask(Some("deploy?".to_string())));
        assert_eq!(out["permission"], "deny");
    }
    fn question_manifest(ask_tools: &[&str], events: serde_json::Value) -> AdapterManifest {
        let text = serde_json::json!({
            "id": "qtest",
            "display_name": "Question Test",
            "default_version": "v1",
            "detect": {"harness_hint": ["qtest"]},
            "versions": {"v1": {
                "install": [],
                "events": events,
                "extraction": {
                    "session_id": ["session_id"],
                    "prompt": ["prompt"],
                    "tool_name": ["tool_name"],
                    "tool_input": ["tool_input"],
                    "tool_output": ["tool_output"],
                    "error": ["error"],
                    "success": ["success"]
                },
                "ask_tools": ask_tools,
                "response": {"mode": "json", "gate": {
                    "allow": {"permission": "allow"},
                    "deny": {"permission": "deny", "reason": "{reason}", "picked": "{answers}"}
                }, "observe": {}}
            }}
        })
        .to_string();
        AdapterManifest::parse(&text).unwrap()
    }
    fn question_manifest_json(
        ask_tools: serde_json::Value,
        events: serde_json::Value,
    ) -> AdapterManifest {
        let text = serde_json::json!({
            "id": "qtest",
            "display_name": "Question Test",
            "default_version": "v1",
            "detect": {"harness_hint": ["qtest"]},
            "versions": {"v1": {
                "install": [],
                "events": events,
                "extraction": {
                    "session_id": ["session_id"],
                    "prompt": ["prompt"],
                    "tool_name": ["tool_name"],
                    "tool_input": ["tool_input"],
                    "tool_output": ["tool_output"],
                    "error": ["error"],
                    "success": ["success"]
                },
                "ask_tools": ask_tools,
                "response": {"mode": "json", "gate": {
                    "allow": {"permission": "allow"},
                    "deny": {"permission": "deny", "reason": "{reason}", "picked": "{answers}"}
                }, "observe": {}}
            }}
        })
        .to_string();
        AdapterManifest::parse(&text).unwrap()
    }

    fn pre_tool_use_events() -> serde_json::Value {
        serde_json::json!({"preToolUse": {"lifecycle": "PreToolUse", "capability": "gate"}})
    }

    fn permission_events() -> serde_json::Value {
        serde_json::json!({"permission": {"lifecycle": "PermissionRequest", "capability": "gate"}})
    }

    fn qtest_input(event: &str, payload: &str) -> RawHookInput {
        RawHookInput {
            harness_hint: "qtest".to_string(),
            event_hint: Some(event.to_string()),
            raw_payload: payload.to_string(),
            context: agent_hooks_protocol::EventContext::default(),
            request_id: None,
            passthrough_env: Default::default(),
        }
    }

    #[test]
    fn omp_ask_shape_lifts_with_options_intact() {
        let adapter = DeclarativeAdapter::new(
            question_manifest(&["ask"], pre_tool_use_events()),
            "v1".to_string(),
        )
        .unwrap();
        let input = qtest_input(
            "preToolUse",
            r#"{"session_id":"s","tool_name":"ASK","tool_input":{"questions":[{"id":"color","header":"Pick one","question":"Favorite color?","options":[{"label":"Blue","description":"Ocean"},{"label":"Red"}],"multi":true}]}}"#,
        );
        let event = adapter.parse_event(&input).unwrap();
        match event.lifecycle {
            AgentLifecycle::PreToolUse {
                tool_name, prompt, ..
            } => {
                assert_eq!(tool_name, "ASK");
                let pr = prompt.expect("prompt should lift");
                assert_eq!(pr.kind, PromptKind::Question);
                assert_eq!(pr.tool_name.as_deref(), Some("ASK"));
                assert_eq!(pr.description.as_deref(), Some("Favorite color?"));
                assert_eq!(pr.questions.len(), 1);
                let q = &pr.questions[0];
                assert_eq!(q.id, "color");
                assert_eq!(q.header.as_deref(), Some("Pick one"));
                assert_eq!(q.question, "Favorite color?");
                assert!(q.multiple);
                assert_eq!(q.options.len(), 2);
                assert_eq!(q.options[0].label, "Blue");
                assert_eq!(q.options[0].description.as_deref(), Some("Ocean"));
                assert_eq!(q.options[1].label, "Red");
                assert_eq!(q.options[1].description, None);
            }
            ref other => panic!("expected PreToolUse, got {other:?}"),
        }
    }

    #[test]
    fn claude_ask_user_question_shape_lifts_multi_select() {
        let adapter = DeclarativeAdapter::new(
            question_manifest(&["AskUserQuestion"], pre_tool_use_events()),
            "v1".to_string(),
        )
        .unwrap();
        let input = qtest_input(
            "preToolUse",
            r#"{"session_id":"s","tool_name":"AskUserQuestion","tool_input":{"questions":[{"question":"Which files?","header":"Files","multiSelect":true,"options":[{"label":"a.ts","description":"first"},{"label":"b.ts"}]}]}}"#,
        );
        let event = adapter.parse_event(&input).unwrap();
        match event.lifecycle {
            AgentLifecycle::PreToolUse { prompt, .. } => {
                let pr = prompt.expect("prompt should lift");
                assert_eq!(pr.kind, PromptKind::Question);
                assert_eq!(pr.questions.len(), 1);
                let q = &pr.questions[0];
                assert_eq!(q.id, "q0");
                assert_eq!(q.header.as_deref(), Some("Files"));
                assert!(q.multiple);
                assert_eq!(q.options.len(), 2);
                assert_eq!(q.options[0].label, "a.ts");
                assert_eq!(q.options[0].description.as_deref(), Some("first"));
            }
            ref other => panic!("expected PreToolUse, got {other:?}"),
        }
    }

    #[test]
    fn permission_request_with_questions_flips_to_question() {
        let adapter = DeclarativeAdapter::new(
            question_manifest(&[], permission_events()),
            "v1".to_string(),
        )
        .unwrap();
        let input = qtest_input(
            "permission",
            r#"{"session_id":"s","tool_name":"Bash","prompt":"Run tests?","questions":[{"question":"Allow?","options":[{"label":"Yes"},{"label":"No"}]}]}"#,
        );
        let event = adapter.parse_event(&input).unwrap();
        match event.lifecycle {
            AgentLifecycle::PermissionRequest { prompt } => {
                assert_eq!(prompt.kind, PromptKind::Question);
                assert_eq!(prompt.questions.len(), 1);
                assert_eq!(prompt.questions[0].question, "Allow?");
                assert_eq!(prompt.questions[0].options.len(), 2);
            }
            ref other => panic!("expected PermissionRequest, got {other:?}"),
        }
    }

    #[test]
    fn ask_tool_lifts_a_question_even_when_the_shape_is_unparseable() {
        // Membership in `ask_tools` says the agent stopped to ask a person.
        // That must hold whether or not we recognise the argument shape:
        // harnesses word their ask payloads differently (a `questions` array,
        // a flat `question`, or something we have not seen), and a parse miss
        // must not silently reclassify a waiting agent as mid-flight — that
        // is what makes a decider skip the one event it exists to answer.
        let adapter = DeclarativeAdapter::new(
            question_manifest(&["ask"], pre_tool_use_events()),
            "v1".to_string(),
        )
        .unwrap();

        // Shape we cannot parse at all.
        let event = adapter
            .parse_event(&qtest_input(
                "preToolUse",
                r#"{"session_id":"s","tool_name":"ask","tool_input":{"totally":"unknown"}}"#,
            ))
            .unwrap();
        let prompt = event
            .prompt_request()
            .expect("ask tool lifts a question regardless of shape");
        assert_eq!(prompt.kind, agent_hooks_protocol::PromptKind::Question);
        assert!(prompt.questions.is_empty());
        assert!(
            event.is_interactive(),
            "a waiting agent must read as waiting"
        );

        // Flat single-question shape still fills the description.
        let event = adapter
            .parse_event(&qtest_input(
                "preToolUse",
                r#"{"session_id":"s","tool_name":"ask","tool_input":{"question":"Deploy?"}}"#,
            ))
            .unwrap();
        let prompt = event.prompt_request().expect("flat shape lifts too");
        assert_eq!(prompt.description.as_deref(), Some("Deploy?"));

        // A tool that is not an ask tool stays mid-flight.
        let event = adapter
            .parse_event(&qtest_input(
                "preToolUse",
                r#"{"session_id":"s","tool_name":"Bash","tool_input":{"command":"ls"}}"#,
            ))
            .unwrap();
        assert!(!event.is_interactive());
    }

    #[test]
    fn answers_placeholder_renders_selected_options() {
        let adapter = DeclarativeAdapter::new(
            question_manifest(&["ask"], pre_tool_use_events()),
            "v1".to_string(),
        )
        .unwrap();
        adapter
            .parse_event(&qtest_input(
                "preToolUse",
                r#"{"session_id":"s","tool_name":"Bash","tool_input":{}}"#,
            ))
            .unwrap();
        let decision = PromptDecision {
            allow: false,
            custom_input: None,
            answers: vec![agent_hooks_protocol::PromptAnswer {
                question_id: "favorite_color".to_string(),
                selected_options: vec!["Blue".to_string()],
                custom_input: None,
            }],
            ask: false,
            allow_always: false,
            suggestion: None,
            defer: false,
            additional_context: None,
        };
        let out = adapter.format_response(&decision);
        assert_eq!(out["picked"], "Blue");
        let empty = PromptDecision::deny(None);
        let out_empty = adapter.format_response(&empty);
        assert!(
            out_empty.get("picked").is_none(),
            "empty picks omit the key instead of rendering \"\", got {out_empty}"
        );
    }

    fn verdict_manifest(verdicts: Option<serde_json::Value>) -> AdapterManifest {
        let mut event = serde_json::json!({"lifecycle": "PreToolUse", "capability": "gate"});
        if let Some(v) = verdicts {
            event["verdicts"] = v;
        }
        let text = serde_json::json!({
            "id": "verdictest",
            "display_name": "Verdict Test",
            "default_version": "v1",
            "detect": {"harness_hint": ["verdictest"]},
            "versions": {"v1": {
                "install": [],
                "events": {"PreToolUse": event},
                "extraction": {
                    "session_id": ["session_id"],
                    "prompt": ["prompt"],
                    "tool_name": ["tool_name"],
                    "tool_input": ["tool_input"],
                    "tool_output": ["tool_output"],
                    "error": ["error"],
                    "success": ["success"]
                },
                "response": {"mode": "json", "gate": {
                    "allow": {"permission": "allow"},
                    "deny": {"permission": "deny", "reason": "{reason}"},
                    "ask": {"permission": "ask", "question": "{reason}"}
                }, "observe": {}}
            }}
        })
        .to_string();
        AdapterManifest::parse(&text).unwrap()
    }

    fn verdict_test_input() -> RawHookInput {
        RawHookInput {
            harness_hint: "verdictest".to_string(),
            event_hint: Some("PreToolUse".to_string()),
            raw_payload: r#"{"session_id":"s","tool_name":"Bash","tool_input":{}}"#.to_string(),
            context: agent_hooks_protocol::EventContext::default(),
            request_id: None,
            passthrough_env: Default::default(),
        }
    }

    #[test]
    fn ask_disallowed_by_verdicts_renders_deny_despite_ask_template() {
        let adapter = DeclarativeAdapter::new(
            verdict_manifest(Some(serde_json::json!(["allow", "deny"]))),
            "v1".to_string(),
        )
        .unwrap();
        adapter.parse_event(&verdict_test_input()).unwrap();
        let out = adapter.format_response(&PromptDecision::ask(Some("deploy?".to_string())));
        assert_eq!(out["permission"], "deny");
        assert_eq!(out["reason"], "deploy?");
    }

    #[test]
    fn verdicts_absent_preserves_ask_template() {
        let adapter = DeclarativeAdapter::new(verdict_manifest(None), "v1".to_string()).unwrap();
        adapter.parse_event(&verdict_test_input()).unwrap();
        let out = adapter.format_response(&PromptDecision::ask(Some("deploy?".to_string())));
        assert_eq!(out["permission"], "ask");
        assert_eq!(out["question"], "deploy?");
    }

    #[test]
    fn ask_allowed_defaults_true_for_unknown_key() {
        let manifest = verdict_manifest(Some(serde_json::json!(["allow", "deny"])));
        let vs = manifest.get_version("v1").unwrap();
        assert!(vs.ask_allowed("NoSuchEvent"));
        assert!(!vs.ask_allowed("PreToolUse"));
    }

    #[test]
    fn shaped_entry_with_exotic_keys_lifts_questions_and_options() {
        let ask_tools = serde_json::json!([{
            "tool": "FancyAsk",
            "questions": "prompts",
            "text": "caption",
            "options": "items",
            "label": "title"
        }]);
        let adapter = DeclarativeAdapter::new(
            question_manifest_json(ask_tools, pre_tool_use_events()),
            "v1".to_string(),
        )
        .unwrap();
        let input = qtest_input(
            "preToolUse",
            r#"{"session_id":"s","tool_name":"FancyAsk","tool_input":{"prompts":[{"caption":"Pick a color?","items":[{"title":"Blue","description":"Ocean"},{"title":"Red"}]}]}}"#,
        );
        let event = adapter.parse_event(&input).unwrap();
        match event.lifecycle {
            AgentLifecycle::PreToolUse { prompt, .. } => {
                let pr = prompt.expect("prompt should lift");
                assert_eq!(pr.kind, PromptKind::Question);
                assert_eq!(pr.questions.len(), 1);
                let q = &pr.questions[0];
                assert_eq!(q.question, "Pick a color?");
                assert_eq!(q.options.len(), 2);
                assert_eq!(q.options[0].label, "Blue");
                assert_eq!(q.options[0].description.as_deref(), Some("Ocean"));
                assert_eq!(q.options[1].label, "Red");
                assert!(q.custom, "lifted questions accept free text");
            }
            ref other => panic!("expected PreToolUse, got {other:?}"),
        }
    }

    #[test]
    fn shaped_entry_with_partial_overrides_falls_back_to_defaults() {
        let ask_tools = serde_json::json!([{"tool": "FancyAsk", "questions": "prompts"}]);
        let adapter = DeclarativeAdapter::new(
            question_manifest_json(ask_tools, pre_tool_use_events()),
            "v1".to_string(),
        )
        .unwrap();
        let input = qtest_input(
            "preToolUse",
            r#"{"session_id":"s","tool_name":"FancyAsk","tool_input":{"prompts":[{"question":"Allow?","options":[{"label":"Yes"},{"label":"No"}]}]}}"#,
        );
        let event = adapter.parse_event(&input).unwrap();
        match event.lifecycle {
            AgentLifecycle::PreToolUse { prompt, .. } => {
                let pr = prompt.expect("prompt should lift");
                assert_eq!(pr.questions.len(), 1);
                assert_eq!(pr.questions[0].question, "Allow?");
                assert_eq!(pr.questions[0].options.len(), 2);
                assert_eq!(pr.questions[0].options[0].label, "Yes");
            }
            ref other => panic!("expected PreToolUse, got {other:?}"),
        }
    }

    #[test]
    fn plain_string_entry_behaves_as_before() {
        let adapter = DeclarativeAdapter::new(
            question_manifest(&["ask"], pre_tool_use_events()),
            "v1".to_string(),
        )
        .unwrap();
        let input = qtest_input(
            "preToolUse",
            r#"{"session_id":"s","tool_name":"ASK","tool_input":{"questions":[{"question":"Continue?","options":[{"label":"Yes"},{"label":"No"}]}]}}"#,
        );
        let event = adapter.parse_event(&input).unwrap();
        match event.lifecycle {
            AgentLifecycle::PreToolUse { prompt, .. } => {
                let pr = prompt.expect("prompt should lift");
                assert_eq!(pr.kind, PromptKind::Question);
                assert_eq!(pr.questions.len(), 1);
                assert_eq!(pr.questions[0].question, "Continue?");
                assert_eq!(pr.questions[0].options.len(), 2);
            }
            ref other => panic!("expected PreToolUse, got {other:?}"),
        }
        let other = qtest_input(
            "preToolUse",
            r#"{"session_id":"s","tool_name":"Bash","tool_input":{"questions":[{"question":"Continue?","options":[{"label":"Yes"}]}]}}"#,
        );
        let event = adapter.parse_event(&other).unwrap();
        match event.lifecycle {
            AgentLifecycle::PreToolUse { prompt, .. } => {
                assert!(prompt.is_none(), "unlisted tool must not lift");
            }
            ref other => panic!("expected PreToolUse, got {other:?}"),
        }
    }

    #[test]
    fn legacy_string_ask_tools_parse_into_name_variants() {
        let text = serde_json::json!({
            "id": "legacy",
            "display_name": "Legacy",
            "default_version": "v1",
            "versions": {"v1": {
                "install": [],
                "events": {"preToolUse": {"lifecycle": "PreToolUse", "capability": "gate"}},
                "ask_tools": ["ask"]
            }}
        })
        .to_string();
        let manifest = AdapterManifest::parse(&text).unwrap();
        let vs = manifest.get_version("v1").unwrap();
        assert_eq!(
            vs.ask_tools,
            vec![crate::manifest::AskToolSpec::Name("ask".to_string())]
        );
        assert!(vs.question_keys_for("ask").is_none());
        assert!(vs.question_keys_for("other").is_none());
    }

    #[test]
    fn shaped_tool_name_matching_is_case_insensitive() {
        let ask_tools = serde_json::json!([{
            "tool": "FancyAsk",
            "questions": "prompts",
            "text": "caption",
            "options": "items",
            "label": "title"
        }]);
        let manifest = question_manifest_json(ask_tools, pre_tool_use_events());
        let vs = manifest.get_version("v1").unwrap();
        assert!(vs.question_keys_for("fancyask").is_some());
        assert!(vs.question_keys_for("FANCYASK").is_some());
        assert!(vs.question_keys_for("other").is_none());
        let adapter = DeclarativeAdapter::new(manifest, "v1".to_string()).unwrap();
        let input = qtest_input(
            "preToolUse",
            r#"{"session_id":"s","tool_name":"fancyask","tool_input":{"prompts":[{"caption":"Pick?","items":[{"title":"One"}]}]}}"#,
        );
        let event = adapter.parse_event(&input).unwrap();
        match event.lifecycle {
            AgentLifecycle::PreToolUse { prompt, .. } => {
                let pr = prompt.expect("prompt should lift despite case mismatch");
                assert_eq!(pr.questions.len(), 1);
                assert_eq!(pr.questions[0].question, "Pick?");
                assert_eq!(pr.questions[0].options[0].label, "One");
            }
            ref other => panic!("expected PreToolUse, got {other:?}"),
        }
    }
}
