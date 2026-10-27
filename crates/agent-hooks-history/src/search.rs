//! Substring search over captured user/assistant text.
//!
//! The query type and snippet windowing live here; the scan itself is
//! [`HistoryReader::search_messages`](crate::HistoryReader::search_messages).

use agent_hooks_protocol::HistorySessionKey;

/// Cap on a hit's snippet in characters (a leading `…` is extra), so one
/// minified dump cannot make a hit enormous.
const MAX_SNIPPET_CHARS: usize = 240;
/// Characters of context kept before the match when a long line is windowed.
const LEAD_CHARS: usize = 60;

#[derive(Debug, Default, Clone)]
pub struct MessageSearchQuery {
    /// Literal substring, case-insensitive (Unicode lowercase folding).
    /// Surrounding whitespace is trimmed; a blank value matches nothing.
    pub needle: String,
    /// Only these sessions are searched. Text of every other session is never
    /// decoded, so a caller can scope the search to what it is allowed to show.
    /// Empty means nothing is searched.
    pub sessions: Vec<HistorySessionKey>,
    /// Maximum hits (one per session). Defaults to 100; at most 1000.
    pub limit: Option<u32>,
}

/// The last non-blank line of `text` containing `needle` (already trimmed and
/// lowercased).
pub(crate) fn matching_snippet(text: &str, needle: &str) -> Option<String> {
    let line = text
        .lines()
        .map(str::trim)
        .rev()
        .find(|line| !line.is_empty() && line.to_lowercase().contains(needle))?;
    if line.chars().count() <= MAX_SNIPPET_CHARS {
        return Some(line.to_string());
    }
    let lowered = line.to_lowercase();
    let match_chars = lowered
        .find(needle)
        .map_or(0, |byte| lowered[..byte].chars().count());
    let start = match_chars.saturating_sub(LEAD_CHARS);
    let window: String = line.chars().skip(start).take(MAX_SNIPPET_CHARS).collect();
    Some(if start > 0 {
        format!("…{window}")
    } else {
        window
    })
}
