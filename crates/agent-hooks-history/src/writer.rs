use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use agent_hooks_protocol::{HistoryEntryKind, HistoryToolStatus};
use rusqlite::types::Null;
use rusqlite::{
    params, Connection, OpenFlags, OptionalExtension, ToSql, Transaction, TransactionBehavior,
};

use crate::capture::{Capture, CaptureEntry};
use crate::payload::encode;
use crate::store::{
    check_existing, configure, harden_files, upgrade, user_version, version_mismatch,
};
use crate::{HistoryError, SCHEMA_VERSION};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordOutcome {
    Stored,
    /// The delivery receipt already existed; nothing was changed.
    Duplicate,
}

/// Single-writer handle to an initialized store.
pub struct HistoryWriter {
    conn: Connection,
}

fn to_i64(ms: u64) -> i64 {
    i64::try_from(ms).unwrap_or(i64::MAX)
}

/// Next value of the global sequence shared by sessions and entries.
fn alloc_seq(tx: &Transaction<'_>) -> Result<i64, HistoryError> {
    Ok(tx.query_row(
        "UPDATE index_meta SET last_seq = last_seq + 1 WHERE id = 1 RETURNING last_seq",
        [],
        |r| r.get(0),
    )?)
}

fn entry_key(capture: &Capture, entry: &CaptureEntry) -> String {
    let kind = entry.kind.as_str();
    if let Some(id) = &entry.native_message_id {
        format!("message:{kind}:{id}")
    } else if let Some(id) = &entry.tool_call_id {
        format!("tool:{id}")
    } else {
        format!("delivery:{}:{kind}", capture.delivery_id)
    }
}

impl HistoryWriter {
    /// Open an already initialized store. A missing store is
    /// [`HistoryError::Disabled`]; an older schema is migrated; a newer one is
    /// refused.
    pub fn open(dir: &Path) -> Result<Self, HistoryError> {
        let db = check_existing(dir)?;
        let mut conn = Connection::open_with_flags(
            &db,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        let found = user_version(&conn)?;
        if found == 0 {
            return Err(HistoryError::Disabled(db));
        }
        if found > SCHEMA_VERSION {
            return Err(version_mismatch(found));
        }
        configure(&conn)?;
        if found < SCHEMA_VERSION {
            upgrade(&mut conn)?;
        }
        harden_files(&db)?;
        Ok(Self { conn })
    }

    /// Replace the lock-wait budget (default 50ms). Short-lived writers that
    /// compete with sibling processes for the single WAL writer lock raise it.
    pub fn set_busy_timeout(&self, wait: std::time::Duration) -> Result<(), HistoryError> {
        self.conn.busy_timeout(wait)?;
        Ok(())
    }

    /// Persist one capture atomically. The schema version is re-checked
    /// inside every transaction because another binary may have migrated the
    /// store since this writer opened it. A replayed delivery returns before
    /// any row, status, metadata or sequence value is touched.
    pub fn record(&mut self, capture: &Capture) -> Result<RecordOutcome, HistoryError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let found = user_version(&tx)?;
        if found != SCHEMA_VERSION {
            return Err(version_mismatch(found));
        }

        let existing: Option<i64> = tx
            .query_row(
                "SELECT first_seq FROM sessions
                 WHERE origin = ?1 AND harness = ?2 AND session_id = ?3",
                params![capture.origin, capture.harness, capture.session_id],
                |r| r.get(0),
            )
            .optional()?;
        let session_ref = if let Some(session_ref) = existing {
            let receipt = tx.execute(
                "INSERT OR IGNORE INTO deliveries(session_ref, delivery_id) VALUES (?1, ?2)",
                params![session_ref, capture.delivery_id],
            )?;
            if receipt == 0 {
                return Ok(RecordOutcome::Duplicate);
            }
            update_session(&tx, session_ref, capture)?;
            session_ref
        } else {
            let session_ref = insert_session(&tx, capture)?;
            tx.execute(
                "INSERT INTO deliveries(session_ref, delivery_id) VALUES (?1, ?2)",
                params![session_ref, capture.delivery_id],
            )?;
            session_ref
        };

        if let Some(entry) = &capture.entry {
            upsert_entry(&tx, session_ref, capture, entry)?;
        }

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
        tx.execute(
            "UPDATE index_meta SET last_persisted_ms = ?1 WHERE id = 1",
            [to_i64(now)],
        )?;
        tx.commit()?;
        Ok(RecordOutcome::Stored)
    }
}

fn insert_session(tx: &Transaction<'_>, c: &Capture) -> Result<i64, HistoryError> {
    let ts = to_i64(c.timestamp_ms);
    let seq = alloc_seq(tx)?;
    tx.execute(
        "INSERT INTO sessions(first_seq, origin, harness, session_id, parent_session_id,
            workspace, title, first_seen_ms, last_seen_ms, status)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8, ?9)",
        params![
            seq,
            c.origin,
            c.harness,
            c.session_id,
            c.parent_session_id,
            c.workspace,
            c.title,
            ts,
            c.status
        ],
    )?;
    Ok(seq)
}

/// Metadata and status follow the newest observation (`last_seen_ms`). A
/// stale observation may only fill metadata that is still missing; it never
/// replaces a known value or the status.
fn update_session(tx: &Transaction<'_>, session_ref: i64, c: &Capture) -> Result<(), HistoryError> {
    tx.execute(
        "UPDATE sessions SET
            parent_session_id = CASE WHEN ?2 >= last_seen_ms
                THEN COALESCE(?3, parent_session_id)
                ELSE COALESCE(parent_session_id, ?3) END,
            workspace = CASE WHEN ?2 >= last_seen_ms
                THEN COALESCE(?4, workspace)
                ELSE COALESCE(workspace, ?4) END,
            title = CASE WHEN ?2 >= last_seen_ms
                THEN COALESCE(?5, title)
                ELSE COALESCE(title, ?5) END,
            status = CASE WHEN ?2 >= last_seen_ms THEN ?6 ELSE status END,
            last_seen_ms = max(last_seen_ms, ?2)
         WHERE first_seq = ?1",
        params![
            session_ref,
            to_i64(c.timestamp_ms),
            c.parent_session_id,
            c.workspace,
            c.title,
            c.status
        ],
    )?;
    Ok(())
}

/// Messages keep last-write-wins semantics: a present payload replaces the
/// stored text.
const UPDATE_MESSAGE: &str = "UPDATE entries SET
        text = COALESCE(?3, text),
        turn_id = COALESCE(?5, turn_id),
        last_seen_ms = max(last_seen_ms, ?4)
     WHERE session_ref = ?1 AND entry_key = ?2";

/// Tool input phase (a `running` observation). Independent of the output
/// phase; never regresses a terminal status. `tool_input_ms` is the
/// timestamp of the stored input payload only: it moves iff the payload is
/// replaced (newer or equal) or filled while missing.
const UPDATE_TOOL_INPUT: &str = "UPDATE entries SET
        tool_input_ms = CASE WHEN ?3 IS NOT NULL
                AND (tool_input IS NULL OR tool_input_ms IS NULL OR ?4 >= tool_input_ms)
            THEN ?4 ELSE tool_input_ms END,
        tool_input = CASE WHEN ?3 IS NOT NULL
                AND (tool_input IS NULL OR tool_input_ms IS NULL OR ?4 >= tool_input_ms)
            THEN ?3 ELSE tool_input END,
        turn_id = COALESCE(?5, turn_id),
        tool_name = COALESCE(?6, tool_name),
        tool_status = CASE WHEN tool_status IN ('completed', 'failed')
            THEN tool_status ELSE COALESCE(?7, tool_status) END,
        last_seen_ms = max(last_seen_ms, ?4)
     WHERE session_ref = ?1 AND entry_key = ?2";

/// Tool output phase (a terminal observation). `tool_output_ms` tracks the
/// stored output payload exactly as `tool_input_ms` does; `tool_status_ms`
/// independently orders terminal observations (including payload-less
/// markers) so a stale terminal event can fill a missing payload but never
/// replace a newer payload or the status.
const UPDATE_TOOL_OUTPUT: &str = "UPDATE entries SET
        tool_output_ms = CASE WHEN ?3 IS NOT NULL
                AND (tool_output IS NULL OR tool_output_ms IS NULL OR ?4 >= tool_output_ms)
            THEN ?4 ELSE tool_output_ms END,
        tool_output = CASE WHEN ?3 IS NOT NULL
                AND (tool_output IS NULL OR tool_output_ms IS NULL OR ?4 >= tool_output_ms)
            THEN ?3 ELSE tool_output END,
        tool_status = CASE WHEN tool_status_ms IS NULL OR ?4 >= tool_status_ms
            THEN COALESCE(?7, tool_status) ELSE tool_status END,
        tool_status_ms = CASE WHEN tool_status_ms IS NULL OR ?4 >= tool_status_ms
            THEN ?4 ELSE tool_status_ms END,
        turn_id = COALESCE(?5, turn_id),
        tool_name = COALESCE(?6, tool_name),
        last_seen_ms = max(last_seen_ms, ?4)
     WHERE session_ref = ?1 AND entry_key = ?2";

fn upsert_entry(
    tx: &Transaction<'_>,
    session_ref: i64,
    c: &Capture,
    entry: &CaptureEntry,
) -> Result<(), HistoryError> {
    let ts = to_i64(c.timestamp_ms);
    let key = entry_key(c, entry);
    let status = entry.tool_status.map(HistoryToolStatus::as_str);
    let payload = encode(entry.text.as_deref())?;
    let terminal = matches!(
        entry.tool_status,
        Some(HistoryToolStatus::Completed | HistoryToolStatus::Failed)
    );

    let changed = match entry.kind {
        HistoryEntryKind::User | HistoryEntryKind::Assistant => tx.execute(
            UPDATE_MESSAGE,
            params![session_ref, key, payload, ts, c.turn_id],
        )?,
        HistoryEntryKind::Tool => tx.execute(
            if terminal {
                UPDATE_TOOL_OUTPUT
            } else {
                UPDATE_TOOL_INPUT
            },
            params![
                session_ref,
                key,
                payload,
                ts,
                c.turn_id,
                entry.tool_name,
                status
            ],
        )?,
    };
    if changed > 0 {
        return Ok(());
    }

    let is_tool = entry.kind == HistoryEntryKind::Tool;
    let has_payload = entry.text.is_some();
    let null = Null;
    let text: &dyn ToSql = if is_tool { &null } else { &payload };
    let tool_input: &dyn ToSql = if is_tool && !terminal {
        &payload
    } else {
        &null
    };
    let tool_output: &dyn ToSql = if is_tool && terminal { &payload } else { &null };
    // Payload timestamps describe stored payloads only; the status
    // timestamp describes the latest terminal observation.
    let tool_input_ms = (is_tool && !terminal && has_payload).then_some(ts);
    let tool_output_ms = (is_tool && terminal && has_payload).then_some(ts);
    let tool_status_ms = (is_tool && terminal).then_some(ts);

    let seq = alloc_seq(tx)?;
    tx.execute(
        "INSERT INTO entries(seq, session_ref, entry_key, kind, text, tool_input, tool_output,
            tool_input_ms, tool_output_ms, tool_status_ms, turn_id, native_message_id,
            tool_call_id, tool_name, tool_status, first_seen_ms, last_seen_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?16)",
        params![
            seq,
            session_ref,
            key,
            entry.kind.as_str(),
            text,
            tool_input,
            tool_output,
            tool_input_ms,
            tool_output_ms,
            tool_status_ms,
            c.turn_id,
            entry.native_message_id,
            entry.tool_call_id,
            entry.tool_name,
            status,
            ts
        ],
    )?;
    Ok(())
}
