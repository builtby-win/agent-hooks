use std::collections::{HashMap, HashSet};
use std::path::Path;

use agent_hooks_protocol::{
    HistoryCaptureInfo, HistoryEntry, HistoryEntryKind, HistoryMessagesPage, HistorySession,
    HistorySessionKey, HistorySessionsPage, HistoryStatus, HistoryToolStatus, HISTORY_API_VERSION,
};
use rusqlite::types::Type;
use rusqlite::{named_params, Connection, OpenFlags, OptionalExtension, Row};
use serde::{Deserialize, Serialize};

use crate::payload;
use crate::search::{matching_snippet, MessageHit, MessageSearchQuery};
use crate::store::{check_existing, user_version, version_mismatch};
use crate::{HistoryError, SCHEMA_VERSION};

const DEFAULT_LIMIT: u32 = 100;
const MAX_LIMIT: u32 = 1000;

#[derive(Debug, Default, Clone)]
pub struct SessionsQuery {
    pub session: Option<String>,
    pub harness: Option<String>,
    pub origin: Option<String>,
    /// Literal, ASCII case-insensitive substring matched against the session
    /// id, title and workspace (any one may match); ANDed with the other
    /// filters. Surrounding whitespace is trimmed and a blank value means no
    /// search. `%`, `_` and other punctuation match themselves. Non-ASCII
    /// letters match exactly: SQLite's `lower()` only folds ASCII.
    pub search: Option<String>,
    pub after: Option<String>,
    pub limit: Option<u32>,
}

#[derive(Debug, Default, Clone)]
pub struct MessagesQuery {
    pub session: String,
    pub harness: Option<String>,
    pub origin: Option<String>,
    pub after: Option<String>,
    /// Exclusive upper seq bound for scroll-up paging: return the newest
    /// `limit` rows below this sequence, oldest first, in a single page.
    /// Mutually exclusive with `after`.
    pub before: Option<i64>,
    pub limit: Option<u32>,
    /// Only these entry kinds; empty means every kind. Lets a chat view skip
    /// tool bodies, which are complete and can be large.
    pub kinds: Vec<HistoryEntryKind>,
}

/// Read-only handle: never creates, migrates, or writes the store.
pub struct HistoryReader {
    conn: Connection,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Filters {
    scope: String,
    session: Option<String>,
    harness: Option<String>,
    origin: Option<String>,
    /// Trimmed search text, `None` when absent or blank. Cursors from before
    /// this field existed decode as "no search".
    #[serde(default)]
    search: Option<String>,
    /// Entry kinds, sorted; empty means all. Cursors from before this field
    /// existed decode as "all".
    #[serde(default)]
    kinds: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Cursor {
    index_id: String,
    filters: Filters,
    /// Highest sequence at the first page; later rows are excluded so paging
    /// is stable while the writer appends.
    high_water: i64,
    /// `first_seq` (sessions) / `seq` (messages) of the last returned row.
    last: i64,
    /// Messages only: the session resolved on the first page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session_key: Option<HistorySessionKey>,
}

fn hex_encode(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(DIGITS[usize::from(b >> 4)] as char);
        out.push(DIGITS[usize::from(b & 0xf)] as char);
    }
    out
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if s.len() & 1 == 1 || !s.is_ascii() {
        return None;
    }
    s.as_bytes()
        .chunks(2)
        .map(|pair| {
            let hi = char::from(pair[0]).to_digit(16)?;
            let lo = char::from(pair[1]).to_digit(16)?;
            u8::try_from(hi * 16 + lo).ok()
        })
        .collect()
}

fn encode_cursor(cursor: &Cursor) -> Result<String, HistoryError> {
    let json = serde_json::to_vec(cursor)
        .map_err(|_| HistoryError::InvalidArgument("cursor could not be encoded".into()))?;
    Ok(hex_encode(&json))
}

fn decode_cursor(raw: &str, index_id: &str, filters: &Filters) -> Result<Cursor, HistoryError> {
    let malformed = || HistoryError::InvalidArgument("malformed cursor".into());
    let bytes = hex_decode(raw).ok_or_else(malformed)?;
    let cursor: Cursor = serde_json::from_slice(&bytes).map_err(|_| malformed())?;
    if cursor.index_id != index_id {
        return Err(HistoryError::InvalidArgument(
            "cursor belongs to a different history index".into(),
        ));
    }
    if cursor.filters != *filters {
        return Err(HistoryError::InvalidArgument(
            "cursor does not match the query filters".into(),
        ));
    }
    if cursor.last < 0 || cursor.high_water < 0 {
        return Err(malformed());
    }
    Ok(cursor)
}

fn parse_limit(limit: Option<u32>) -> Result<usize, HistoryError> {
    let limit = limit.unwrap_or(DEFAULT_LIMIT);
    if !(1..=MAX_LIMIT).contains(&limit) {
        return Err(HistoryError::InvalidArgument(format!(
            "limit must be between 1 and {MAX_LIMIT}"
        )));
    }
    Ok(limit as usize)
}

fn conv_err(idx: usize, what: &str) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        idx,
        Type::Text,
        format!("unrecognized {what}").into(),
    )
}

fn ms(row: &Row<'_>, idx: usize) -> rusqlite::Result<u64> {
    Ok(u64::try_from(row.get::<_, i64>(idx)?).unwrap_or(0))
}

fn session_from_row(row: &Row<'_>) -> rusqlite::Result<HistorySession> {
    Ok(HistorySession {
        key: HistorySessionKey {
            origin: row.get(0)?,
            harness: row.get(1)?,
            session_id: row.get(2)?,
        },
        seq: row.get(3)?,
        parent_session_id: row.get(4)?,
        workspace: row.get(5)?,
        title: row.get(6)?,
        status: row.get(7)?,
        first_seen_ms: ms(row, 8)?,
        last_seen_ms: ms(row, 9)?,
    })
}

/// Column order: seq, kind, text, tool_input, tool_output, turn_id,
/// native_message_id, tool_call_id, tool_name, tool_status, first_seen_ms,
/// last_seen_ms. The three payload columns go through the storage codec; a
/// corrupt value fails the whole query with a content-free error.
fn entry_from_row(row: &Row<'_>) -> rusqlite::Result<HistoryEntry> {
    let kind = match row.get_ref(1)?.as_str()? {
        "user" => HistoryEntryKind::User,
        "assistant" => HistoryEntryKind::Assistant,
        "tool" => HistoryEntryKind::Tool,
        _ => return Err(conv_err(1, "entry kind")),
    };
    let tool_status = match row.get::<_, Option<String>>(9)?.as_deref() {
        None => None,
        Some("running") => Some(HistoryToolStatus::Running),
        Some("completed") => Some(HistoryToolStatus::Completed),
        Some("failed") => Some(HistoryToolStatus::Failed),
        Some("unknown") => Some(HistoryToolStatus::Unknown),
        Some(_) => return Err(conv_err(9, "tool status")),
    };
    Ok(HistoryEntry {
        seq: row.get(0)?,
        kind,
        text: payload::decode(row.get_ref(2)?)?,
        tool_input: payload::decode(row.get_ref(3)?)?,
        tool_output: payload::decode(row.get_ref(4)?)?,
        turn_id: row.get(5)?,
        native_message_id: row.get(6)?,
        tool_call_id: row.get(7)?,
        tool_name: row.get(8)?,
        tool_status,
        first_seen_ms: ms(row, 10)?,
        last_seen_ms: ms(row, 11)?,
    })
}

/// Newest entry first, so a session's first match is its latest one. Scope is
/// checked before the payload is decoded.
fn scan_messages(
    conn: &Connection,
    wanted: &HashMap<(&str, &str, &str), &HistorySessionKey>,
    needle: &str,
    limit: usize,
) -> rusqlite::Result<Vec<MessageHit>> {
    let mut stmt = conn.prepare(
        "SELECT s.origin, s.harness, s.session_id, e.kind, e.text
         FROM entries e JOIN sessions s ON s.first_seq = e.session_ref
         WHERE e.kind IN ('user', 'assistant') AND e.text IS NOT NULL
         ORDER BY e.seq DESC",
    )?;
    let mut rows = stmt.query([])?;
    let mut found: HashSet<&HistorySessionKey> = HashSet::new();
    let mut hits = Vec::new();
    while let Some(row) = rows.next()? {
        let key = (
            row.get_ref(0)?.as_str()?,
            row.get_ref(1)?.as_str()?,
            row.get_ref(2)?.as_str()?,
        );
        let Some(&session) = wanted.get(&key) else {
            continue;
        };
        if found.contains(session) {
            continue;
        }
        let kind = match row.get_ref(3)?.as_str()? {
            "user" => HistoryEntryKind::User,
            "assistant" => HistoryEntryKind::Assistant,
            _ => return Err(conv_err(3, "entry kind")),
        };
        let Some(text) = payload::decode(row.get_ref(4)?)? else {
            continue;
        };
        let Some(snippet) = matching_snippet(&text, needle) else {
            continue;
        };
        found.insert(session);
        hits.push(MessageHit {
            session: session.clone(),
            kind,
            snippet,
        });
        if hits.len() == limit || found.len() == wanted.len() {
            break;
        }
    }
    Ok(hits)
}

impl HistoryReader {
    /// Open an existing, initialized store read-only.
    pub fn open(dir: &Path) -> Result<Self, HistoryError> {
        let db = check_existing(dir)?;
        let conn = Connection::open_with_flags(
            &db,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.busy_timeout(std::time::Duration::from_millis(250))?;
        conn.pragma_update(None, "query_only", "ON")?;
        match user_version(&conn)? {
            0 => Err(HistoryError::Disabled(db)),
            SCHEMA_VERSION => Ok(Self { conn }),
            other => Err(version_mismatch(other)),
        }
    }

    fn check_version(conn: &Connection) -> Result<(), HistoryError> {
        match user_version(conn)? {
            SCHEMA_VERSION => Ok(()),
            other => Err(version_mismatch(other)),
        }
    }

    fn index_id(conn: &Connection) -> Result<String, HistoryError> {
        Ok(
            conn.query_row("SELECT index_id FROM index_meta WHERE id = 1", [], |r| {
                r.get(0)
            })?,
        )
    }

    pub fn sessions(&self, q: &SessionsQuery) -> Result<HistorySessionsPage, HistoryError> {
        let limit = parse_limit(q.limit)?;
        let session = q
            .session
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let search = q.search.as_deref().map(str::trim).filter(|s| !s.is_empty());
        let tx = self.conn.unchecked_transaction()?;
        Self::check_version(&tx)?;
        let index_id = Self::index_id(&tx)?;
        let filters = Filters {
            scope: "sessions".into(),
            session: session.map(str::to_owned),
            harness: q.harness.clone(),
            origin: q.origin.clone(),
            search: search.map(str::to_owned),
            kinds: Vec::new(),
        };
        let (high_water, last) = match &q.after {
            Some(raw) => {
                let c = decode_cursor(raw, &index_id, &filters)?;
                (c.high_water, c.last)
            }
            None => (Self::high_water(&tx)?, 0),
        };

        let mut stmt = tx.prepare(
            "SELECT origin, harness, session_id, first_seq, parent_session_id, workspace,
                    title, status, first_seen_ms, last_seen_ms
             FROM sessions
             WHERE first_seq > :last AND first_seq <= :hw
               AND (:session IS NULL OR session_id = :session)
               AND (:harness IS NULL OR harness = :harness)
               AND (:origin IS NULL OR origin = :origin)
               AND (:search IS NULL
                    OR instr(lower(session_id), lower(:search)) > 0
                    OR instr(lower(title), lower(:search)) > 0
                    OR instr(lower(workspace), lower(:search)) > 0)
             ORDER BY first_seq ASC
             LIMIT :limit",
        )?;
        let mut items = stmt
            .query_map(
                named_params! {
                    ":last": last,
                    ":hw": high_water,
                    ":session": session,
                    ":harness": q.harness,
                    ":origin": q.origin,
                    ":search": search,
                    ":limit": (limit + 1) as i64,
                },
                session_from_row,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let mut next_cursor = None;
        if items.len() > limit {
            items.truncate(limit);
            let last = items.last().map_or(last, |s| s.seq);
            next_cursor = Some(encode_cursor(&Cursor {
                index_id,
                filters,
                high_water,
                last,
                session_key: None,
            })?);
        }
        Ok(HistorySessionsPage {
            api_version: HISTORY_API_VERSION,
            items,
            next_cursor,
        })
    }

    pub fn messages(&self, q: &MessagesQuery) -> Result<HistoryMessagesPage, HistoryError> {
        let limit = parse_limit(q.limit)?;
        if q.session.is_empty() {
            return Err(HistoryError::InvalidArgument(
                "session must not be empty".into(),
            ));
        }
        if q.after.is_some() && q.before.is_some() {
            return Err(HistoryError::InvalidArgument(
                "after and before are mutually exclusive".into(),
            ));
        }
        if let Some(before) = q.before {
            if before < 1 {
                return Err(HistoryError::InvalidArgument(
                    "before must be a positive sequence number".into(),
                ));
            }
        }
        let tx = self.conn.unchecked_transaction()?;
        Self::check_version(&tx)?;
        let index_id = Self::index_id(&tx)?;
        let filters = Filters {
            scope: "messages".into(),
            session: Some(q.session.clone()),
            harness: q.harness.clone(),
            origin: q.origin.clone(),
            search: None,
            kinds: {
                let mut kinds: Vec<String> =
                    q.kinds.iter().map(|k| k.as_str().to_string()).collect();
                kinds.sort();
                kinds.dedup();
                kinds
            },
        };
        let cursor = q
            .after
            .as_deref()
            .map(|raw| decode_cursor(raw, &index_id, &filters))
            .transpose()?;

        let (key, session_ref, high_water, last) = match cursor {
            Some(Cursor {
                session_key: Some(key),
                high_water,
                last,
                ..
            }) => {
                // Later pages pin the session resolved on page one so sessions
                // appearing meanwhile cannot make it ambiguous.
                let session_ref = tx
                    .query_row(
                        "SELECT first_seq FROM sessions
                         WHERE origin = ?1 AND harness = ?2 AND session_id = ?3",
                        [&key.origin, &key.harness, &key.session_id],
                        |r| r.get::<_, i64>(0),
                    )
                    .optional()?
                    .ok_or(HistoryError::NotFound)?;
                (key, session_ref, high_water, last)
            }
            Some(_) => {
                return Err(HistoryError::InvalidArgument("malformed cursor".into()));
            }
            None => {
                let (key, session_ref) = Self::resolve_session(&tx, q)?;
                (key, session_ref, Self::high_water(&tx)?, 0)
            }
        };

        // Equality on session_ref plus a seq range, ordered by seq, is served
        // by the (session_ref, seq) index `entries_session_seq`.
        // Kind names are the fixed `HistoryEntryKind::as_str` set, never input.
        let kind_clause = if filters.kinds.is_empty() {
            String::new()
        } else {
            let list = filters
                .kinds
                .iter()
                .map(|k| format!("'{k}'"))
                .collect::<Vec<_>>()
                .join(",");
            format!("AND kind IN ({list})")
        };
        // Scroll-up paging: the newest `limit` rows below `before`, oldest
        // first, in one indexed query. Older rows are immutable and new
        // appends land above the bound, so the window is stable without a
        // cursor; the caller continues with the first returned seq.
        if let Some(before) = q.before {
            let mut stmt = tx.prepare(&format!(
                "SELECT seq, kind, text, tool_input, tool_output, turn_id, native_message_id,
                        tool_call_id, tool_name, tool_status, first_seen_ms, last_seen_ms
                 FROM entries
                 WHERE session_ref = :session_ref
                   AND seq < :before
                   {kind_clause}
                 ORDER BY seq DESC
                 LIMIT :limit"
            ))?;
            let mut items = stmt
                .query_map(
                    named_params! {
                        ":session_ref": session_ref,
                        ":before": before,
                        ":limit": limit as i64,
                    },
                    entry_from_row,
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            items.reverse();
            return Ok(HistoryMessagesPage {
                api_version: HISTORY_API_VERSION,
                session: key,
                capture: HistoryCaptureInfo::default(),
                items,
                next_cursor: None,
            });
        }
        let mut stmt = tx.prepare(&format!(
            "SELECT seq, kind, text, tool_input, tool_output, turn_id, native_message_id,
                    tool_call_id, tool_name, tool_status, first_seen_ms, last_seen_ms
             FROM entries
             WHERE session_ref = :session_ref
               AND seq > :last AND seq <= :hw
               {kind_clause}
             ORDER BY seq ASC
             LIMIT :limit"
        ))?;
        let mut items = stmt
            .query_map(
                named_params! {
                    ":session_ref": session_ref,
                    ":last": last,
                    ":hw": high_water,
                    ":limit": (limit + 1) as i64,
                },
                entry_from_row,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let mut next_cursor = None;
        if items.len() > limit {
            items.truncate(limit);
            let last = items.last().map_or(last, |e| e.seq);
            next_cursor = Some(encode_cursor(&Cursor {
                index_id,
                filters,
                high_water,
                last,
                session_key: Some(key.clone()),
            })?);
        }
        Ok(HistoryMessagesPage {
            api_version: HISTORY_API_VERSION,
            session: key,
            capture: HistoryCaptureInfo::default(),
            items,
            next_cursor,
        })
    }

    /// Newest matching user/assistant message per session in `q.sessions`,
    /// newest first. Entries of sessions outside the scope are skipped
    /// before their payload is decoded, and the scan stops once every
    /// requested session has a hit or `q.limit` is reached.
    ///
    /// A corrupt payload in a scoped session fails the whole search, like
    /// [`HistoryReader::messages`].
    pub fn search_messages(&self, q: &MessageSearchQuery) -> Result<Vec<MessageHit>, HistoryError> {
        let limit = parse_limit(q.limit)?;
        let needle = q.needle.trim().to_lowercase();
        if needle.is_empty() || q.sessions.is_empty() {
            return Ok(Vec::new());
        }
        let tx = self.conn.unchecked_transaction()?;
        Self::check_version(&tx)?;
        let wanted: HashMap<(&str, &str, &str), &HistorySessionKey> = q
            .sessions
            .iter()
            .map(|key| {
                (
                    (
                        key.origin.as_str(),
                        key.harness.as_str(),
                        key.session_id.as_str(),
                    ),
                    key,
                )
            })
            .collect();
        Ok(scan_messages(&tx, &wanted, &needle, limit)?)
    }

    pub fn status(&self) -> Result<HistoryStatus, HistoryError> {
        let tx = self.conn.unchecked_transaction()?;
        Self::check_version(&tx)?;
        let sessions: i64 = tx.query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))?;
        let entries: i64 = tx.query_row("SELECT COUNT(*) FROM entries", [], |r| r.get(0))?;
        let last: Option<i64> = tx.query_row(
            "SELECT last_persisted_ms FROM index_meta WHERE id = 1",
            [],
            |r| r.get(0),
        )?;
        Ok(HistoryStatus {
            api_version: HISTORY_API_VERSION,
            schema_version: SCHEMA_VERSION,
            sessions: u64::try_from(sessions).unwrap_or(0),
            entries: u64::try_from(entries).unwrap_or(0),
            last_persisted_ms: last.and_then(|v| u64::try_from(v).ok()),
        })
    }

    /// Highest allocated sequence: the singleton counter, not a table scan.
    fn high_water(conn: &Connection) -> Result<i64, HistoryError> {
        Ok(
            conn.query_row("SELECT last_seq FROM index_meta WHERE id = 1", [], |r| {
                r.get(0)
            })?,
        )
    }

    fn resolve_session(
        conn: &Connection,
        q: &MessagesQuery,
    ) -> Result<(HistorySessionKey, i64), HistoryError> {
        let mut stmt = conn.prepare(
            "SELECT origin, harness, session_id, first_seq FROM sessions
             WHERE session_id = :session
               AND (:harness IS NULL OR harness = :harness)
               AND (:origin IS NULL OR origin = :origin)
             ORDER BY first_seq ASC",
        )?;
        let mut found = stmt
            .query_map(
                named_params! {
                    ":session": q.session,
                    ":harness": q.harness,
                    ":origin": q.origin,
                },
                |row| {
                    Ok((
                        HistorySessionKey {
                            origin: row.get(0)?,
                            harness: row.get(1)?,
                            session_id: row.get(2)?,
                        },
                        row.get::<_, i64>(3)?,
                    ))
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        match found.len() {
            0 => Err(HistoryError::NotFound),
            1 => Ok(found.remove(0)),
            _ => Err(HistoryError::AmbiguousSession(
                found.into_iter().map(|(key, _)| key).collect(),
            )),
        }
    }
}
