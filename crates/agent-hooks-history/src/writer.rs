use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use agent_hooks_protocol::{stable_hash, Capture, CaptureEntry, HistoryEntryKind, HistoryToolStatus};
use rusqlite::types::Null;
use rusqlite::{
    params, Connection, OpenFlags, OptionalExtension, ToSql, Transaction, TransactionBehavior,
};

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

/// Progress of one backfilled transcript file; see [`HistoryWriter::backfill_file`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackfillFile {
    pub origin: String,
    pub harness: String,
    pub path: String,
    pub size: u64,
    pub mtime_ms: u64,
    /// End of the last fully imported line.
    pub byte_offset: u64,
    /// Fingerprint of the first `min(byte_offset, HEAD_BYTES)` bytes.
    pub head_hash: String,
}

/// Bytes of the file head covered by [`BackfillFile::head_hash`].
const HEAD_BYTES: u64 = 4096;

fn head_hash(path: &Path, len: u64) -> std::io::Result<String> {
    let mut buf = Vec::new();
    File::open(path)?.take(len.min(HEAD_BYTES)).read_to_end(&mut buf)?;
    Ok(format!("{:016x}", stable_hash(&buf)))
}

impl BackfillFile {
    /// Record `path` as imported up to `byte_offset`. `observed` must be the
    /// metadata read *before* importing, so bytes appended during the import
    /// still make the file look changed next time.
    pub fn snapshot(
        origin: &str,
        harness: &str,
        path: &Path,
        observed: &std::fs::Metadata,
        byte_offset: u64,
    ) -> Result<Self, HistoryError> {
        let md = observed;
        Ok(Self {
            origin: origin.to_string(),
            harness: harness.to_string(),
            path: path.to_string_lossy().into_owned(),
            size: md.len(),
            mtime_ms: mtime_ms(md),
            byte_offset,
            head_hash: head_hash(path, byte_offset)?,
        })
    }

    /// Where to resume reading `path`, or `None` when it is unchanged since
    /// this record. Append-only growth resumes at `byte_offset`; a shrunken
    /// file or a changed head is a rewrite and restarts at 0.
    pub fn resume(&self, path: &Path) -> Result<Option<u64>, HistoryError> {
        let md = std::fs::metadata(path)?;
        if md.len() == self.size && mtime_ms(&md) == self.mtime_ms {
            return Ok(None);
        }
        let intact = md.len() >= self.byte_offset
            && head_hash(path, self.byte_offset)? == self.head_hash;
        Ok(Some(if intact { self.byte_offset } else { 0 }))
    }
}

fn mtime_ms(md: &std::fs::Metadata) -> u64 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// Per-file progress of transcript backfills, keyed by
/// `(origin, harness, path)`. `byte_offset` is the end of the last fully
/// imported line; `head_hash` fingerprints the first `min(byte_offset, 4096)`
/// bytes so a rewritten file is detected and re-imported from 0.
const BACKFILL_FILES_DDL: &str = "
CREATE TABLE IF NOT EXISTS backfill_files (
    origin TEXT NOT NULL,
    harness TEXT NOT NULL,
    path TEXT NOT NULL,
    size INTEGER NOT NULL,
    mtime_ms INTEGER NOT NULL,
    byte_offset INTEGER NOT NULL,
    head_hash TEXT NOT NULL,
    PRIMARY KEY (origin, harness, path)
) WITHOUT ROWID;
";

/// Opaque resume cursor of script-based backfills, keyed by `(origin, harness)`.
const BACKFILL_CURSORS_DDL: &str = "
CREATE TABLE IF NOT EXISTS backfill_cursors (
    origin TEXT NOT NULL,
    harness TEXT NOT NULL,
    cursor TEXT NOT NULL,
    updated_ms INTEGER NOT NULL,
    PRIMARY KEY (origin, harness)
) WITHOUT ROWID;
";

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

    /// Whether `table` exists. Backfill tables are created lazily by the
    /// first write that needs them and never bump `user_version`, so older
    /// binaries keep using the same store.
    fn table_exists(&self, table: &str) -> Result<bool, HistoryError> {
        Ok(self
            .conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
                [table],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    /// Backfill progress recorded for a transcript file, if any.
    pub fn backfill_file(
        &self,
        origin: &str,
        harness: &str,
        path: &Path,
    ) -> Result<Option<BackfillFile>, HistoryError> {
        if !self.table_exists("backfill_files")? {
            return Ok(None);
        }
        let path = path.to_string_lossy();
        Ok(self
            .conn
            .query_row(
                "SELECT size, mtime_ms, byte_offset, head_hash FROM backfill_files
                 WHERE origin = ?1 AND harness = ?2 AND path = ?3",
                params![origin, harness, path],
                |r| {
                    Ok(BackfillFile {
                        origin: origin.to_string(),
                        harness: harness.to_string(),
                        path: path.to_string(),
                        size: r.get::<_, i64>(0)?.try_into().unwrap_or(0),
                        mtime_ms: r.get::<_, i64>(1)?.try_into().unwrap_or(0),
                        byte_offset: r.get::<_, i64>(2)?.try_into().unwrap_or(0),
                        head_hash: r.get(3)?,
                    })
                },
            )
            .optional()?)
    }

    /// Insert or replace a file's backfill progress.
    pub fn save_backfill_file(&self, f: &BackfillFile) -> Result<(), HistoryError> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute_batch(BACKFILL_FILES_DDL)?;
        tx.execute(
            "INSERT OR REPLACE INTO backfill_files
                 (origin, harness, path, size, mtime_ms, byte_offset, head_hash)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                f.origin,
                f.harness,
                f.path,
                to_i64(f.size),
                to_i64(f.mtime_ms),
                to_i64(f.byte_offset),
                f.head_hash
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Saved resume cursor of a script backfill, if any.
    pub fn backfill_cursor(
        &self,
        origin: &str,
        harness: &str,
    ) -> Result<Option<String>, HistoryError> {
        if !self.table_exists("backfill_cursors")? {
            return Ok(None);
        }
        Ok(self
            .conn
            .query_row(
                "SELECT cursor FROM backfill_cursors WHERE origin = ?1 AND harness = ?2",
                params![origin, harness],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Insert or replace a script backfill's resume cursor.
    pub fn save_backfill_cursor(
        &self,
        origin: &str,
        harness: &str,
        cursor: &str,
        updated_ms: u64,
    ) -> Result<(), HistoryError> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute_batch(BACKFILL_CURSORS_DDL)?;
        tx.execute(
            "INSERT OR REPLACE INTO backfill_cursors (origin, harness, cursor, updated_ms)
             VALUES (?1, ?2, ?3, ?4)",
            params![origin, harness, cursor, to_i64(updated_ms)],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// True when the session already has a delivery that did not come from a
    /// backfill (`backfill:` prefix), i.e. live hooks captured it. A session
    /// that is absent from the store is not live.
    pub fn session_is_live(
        &self,
        origin: &str,
        harness: &str,
        session_id: &str,
    ) -> Result<bool, HistoryError> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM deliveries d
                 WHERE d.session_ref = (SELECT first_seq FROM sessions
                                        WHERE origin = ?1 AND harness = ?2 AND session_id = ?3)
                   AND substr(d.delivery_id, 1, 9) <> 'backfill:')",
            params![origin, harness, session_id],
            |r| r.get(0),
        )?)
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
