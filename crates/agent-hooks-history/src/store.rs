//! Store location security, schema creation/migration, and initialization.

use std::fs::{self, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use rusqlite::types::{ToSqlOutput, ValueRef};
use rusqlite::{Connection, OpenFlags, Transaction, TransactionBehavior};

use crate::payload;
use crate::{HistoryError, DB_FILE, SCHEMA_VERSION};

/// Schema version 2. `sessions.first_seq` is the integer session reference
/// and the public session ordering; `index_meta.last_seq` is the singleton
/// sequence counter. Payload columns hold NULL, exact TEXT, or a zstd BLOB
/// (see `payload`).
const SCHEMA_V2: &str = "
CREATE TABLE index_meta (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    index_id TEXT NOT NULL,
    last_persisted_ms INTEGER,
    last_seq INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE sessions (
    first_seq INTEGER PRIMARY KEY,
    origin TEXT NOT NULL,
    harness TEXT NOT NULL,
    session_id TEXT NOT NULL,
    parent_session_id TEXT,
    workspace TEXT,
    title TEXT,
    first_seen_ms INTEGER NOT NULL,
    last_seen_ms INTEGER NOT NULL,
    status TEXT NOT NULL,
    UNIQUE (origin, harness, session_id)
);
CREATE INDEX sessions_session_id ON sessions(session_id);
CREATE TABLE deliveries (
    session_ref INTEGER NOT NULL REFERENCES sessions(first_seq),
    delivery_id TEXT NOT NULL,
    PRIMARY KEY (session_ref, delivery_id)
) WITHOUT ROWID;
CREATE TABLE entries (
    seq INTEGER PRIMARY KEY,
    session_ref INTEGER NOT NULL REFERENCES sessions(first_seq),
    entry_key TEXT NOT NULL,
    kind TEXT NOT NULL,
    text BLOB,
    tool_input BLOB,
    tool_output BLOB,
    tool_input_ms INTEGER,
    tool_output_ms INTEGER,
    tool_status_ms INTEGER,
    turn_id TEXT,
    native_message_id TEXT,
    tool_call_id TEXT,
    tool_name TEXT,
    tool_status TEXT,
    first_seen_ms INTEGER NOT NULL,
    last_seen_ms INTEGER NOT NULL,
    UNIQUE (session_ref, entry_key)
);
CREATE INDEX entries_session_seq ON entries(session_ref, seq);
";

/// Park the version 1 tables under `v1_*` names so [`SCHEMA_V2`] can be
/// created under the final names inside the same transaction. The v1
/// secondary indexes are dropped first: their names are reused by v2.
const PARK_V1: &str = "
DROP INDEX sessions_first_seq;
DROP INDEX sessions_session_id;
ALTER TABLE index_meta RENAME TO v1_index_meta;
ALTER TABLE sequence RENAME TO v1_sequence;
ALTER TABLE sessions RENAME TO v1_sessions;
ALTER TABLE deliveries RENAME TO v1_deliveries;
ALTER TABLE entries RENAME TO v1_entries;
";

/// Remove the parked v1 tables, children before parents.
const DROP_V1: &str = "
DROP TABLE v1_deliveries;
DROP TABLE v1_entries;
DROP TABLE v1_sessions;
DROP TABLE v1_sequence;
DROP TABLE v1_index_meta;
";

const V1_VERSION: u32 = 1;

pub(crate) fn db_path(dir: &Path) -> PathBuf {
    dir.join(DB_FILE)
}

fn sidecar_paths(db: &Path) -> [PathBuf; 2] {
    let name = db.file_name().unwrap_or_default();
    ["-wal", "-shm"].map(|suffix| {
        let mut n = name.to_os_string();
        n.push(suffix);
        db.with_file_name(n)
    })
}

pub(crate) fn user_version(conn: &Connection) -> Result<u32, HistoryError> {
    let v: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    Ok(u32::try_from(v).unwrap_or(u32::MAX))
}

/// Error for a store whose `user_version` is not [`SCHEMA_VERSION`].
pub(crate) fn version_mismatch(found: u32) -> HistoryError {
    if found > SCHEMA_VERSION {
        HistoryError::UnsupportedNewerSchema {
            found,
            expected: SCHEMA_VERSION,
        }
    } else {
        HistoryError::SchemaUpgradeRequired {
            found,
            expected: SCHEMA_VERSION,
        }
    }
}

#[cfg(unix)]
fn too_open(md: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    md.permissions().mode() & 0o077 != 0
}

#[cfg(not(unix))]
fn too_open(_md: &fs::Metadata) -> bool {
    // ACL hardening is not implemented on this platform; nothing to check.
    false
}

/// Validate an existing store location without creating or changing anything:
/// no symlinks, correct file types, no group/other access (unix).
pub(crate) fn check_existing(dir: &Path) -> Result<PathBuf, HistoryError> {
    let db = db_path(dir);
    let dir_md = match fs::symlink_metadata(dir) {
        Ok(md) => md,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(HistoryError::Disabled(db)),
        Err(e) => return Err(e.into()),
    };
    if dir_md.file_type().is_symlink() {
        return Err(HistoryError::Insecure(
            "history directory is a symlink".into(),
        ));
    }
    if !dir_md.is_dir() {
        return Err(HistoryError::Insecure(
            "history path is not a directory".into(),
        ));
    }
    if too_open(&dir_md) {
        return Err(HistoryError::Insecure(
            "history directory is accessible to other users (expected mode 0700)".into(),
        ));
    }
    let db_md = match fs::symlink_metadata(&db) {
        Ok(md) => md,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(HistoryError::Disabled(db)),
        Err(e) => return Err(e.into()),
    };
    if db_md.file_type().is_symlink() {
        return Err(HistoryError::Insecure(
            "history database is a symlink".into(),
        ));
    }
    if !db_md.is_file() {
        return Err(HistoryError::Insecure(
            "history database is not a regular file".into(),
        ));
    }
    if too_open(&db_md) {
        return Err(HistoryError::Insecure(
            "history database is accessible to other users (expected mode 0600)".into(),
        ));
    }
    for side in sidecar_paths(&db) {
        if let Ok(md) = fs::symlink_metadata(&side) {
            if md.file_type().is_symlink() {
                return Err(HistoryError::Insecure(
                    "history WAL/SHM file is a symlink".into(),
                ));
            }
        }
    }
    Ok(db)
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

/// Restrict the database and any WAL/SHM files to the owner (unix). On
/// Windows this is a no-op: ACL hardening is not implemented.
pub(crate) fn harden_files(db: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        set_mode(db, 0o600)?;
        for side in sidecar_paths(db) {
            match set_mode(&side, 0o600) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
    }
    #[cfg(not(unix))]
    let _ = db;
    Ok(())
}

pub(crate) fn configure(conn: &Connection) -> Result<(), HistoryError> {
    conn.busy_timeout(std::time::Duration::from_millis(50))?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get::<_, String>(0))?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    Ok(())
}

/// Create the current schema on an empty database or migrate an older one,
/// atomically: any failure rolls back every change, including the schema
/// version. Returns true when the store was freshly created.
pub(crate) fn upgrade(conn: &mut Connection) -> Result<bool, HistoryError> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let found = user_version(&tx)?;
    if found > SCHEMA_VERSION {
        return Err(version_mismatch(found));
    }
    let created = found == 0;
    if created {
        tx.execute_batch(SCHEMA_V2)?;
        tx.execute(
            "INSERT INTO index_meta(id, index_id, last_seq) VALUES (1, ?1, 0)",
            [uuid::Uuid::new_v4().to_string()],
        )?;
    } else if found == V1_VERSION {
        migrate_v1(&tx)?;
    }
    if found != SCHEMA_VERSION {
        tx.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))?;
    }
    tx.commit()?;
    Ok(created)
}

/// Fail unless every row of a v1 table reached its v2 counterpart.
fn ensure_copied(tx: &Transaction<'_>, v1_table: &str, copied: usize) -> Result<(), HistoryError> {
    let total: i64 = tx.query_row(&format!("SELECT COUNT(*) FROM {v1_table}"), [], |r| {
        r.get(0)
    })?;
    if i64::try_from(copied).ok() != Some(total) {
        return Err(HistoryError::Corrupt(
            "v1 rows reference a missing session and cannot be migrated",
        ));
    }
    Ok(())
}

/// Convert a v1 store to v2 inside `tx`. Every session, receipt and entry
/// keeps its sequence, key, timestamps and metadata; the index id and the
/// highest allocated sequence carry over so existing cursors stay valid.
/// Entry text is copied byte for byte into `text` (compressed by the shared
/// codec when worthwhile): v1 stored tool input and output as one string
/// whose boundary was never recorded, so it stays opaque rather than being
/// split by guesswork. Phase timestamps stay NULL for the same reason; a
/// terminal tool row's `tool_status_ms` starts at its v1 `last_seen_ms`.
fn migrate_v1(tx: &Transaction<'_>) -> Result<(), HistoryError> {
    let index_rows: i64 = tx.query_row("SELECT COUNT(*) FROM index_meta", [], |r| r.get(0))?;
    if index_rows != 1 {
        return Err(HistoryError::Corrupt(
            "v1 index metadata does not have exactly one row",
        ));
    }
    let last_seq: i64 = tx.query_row(
        "SELECT MAX(
            COALESCE((SELECT MAX(seq) FROM sequence), 0),
            COALESCE((SELECT seq FROM sqlite_sequence WHERE name = 'sequence'), 0),
            COALESCE((SELECT MAX(first_seq) FROM sessions), 0),
            COALESCE((SELECT MAX(seq) FROM entries), 0)
        )",
        [],
        |r| r.get(0),
    )?;

    tx.execute_batch(PARK_V1)?;
    tx.execute_batch(SCHEMA_V2)?;
    tx.execute(
        "INSERT INTO index_meta(id, index_id, last_persisted_ms, last_seq)
         SELECT 1, index_id, last_persisted_ms, ?1 FROM v1_index_meta",
        [last_seq],
    )?;
    let sessions = tx.execute(
        "INSERT INTO sessions(first_seq, origin, harness, session_id, parent_session_id,
                              workspace, title, first_seen_ms, last_seen_ms, status)
         SELECT first_seq, origin, harness, session_id, parent_session_id,
                workspace, title, first_seen_ms, last_seen_ms, status
         FROM v1_sessions ORDER BY first_seq",
        [],
    )?;
    ensure_copied(tx, "v1_sessions", sessions)?;
    let deliveries = tx.execute(
        "INSERT INTO deliveries(session_ref, delivery_id)
         SELECT s.first_seq, d.delivery_id
         FROM v1_deliveries d JOIN sessions s
           ON s.origin = d.origin AND s.harness = d.harness AND s.session_id = d.session_id",
        [],
    )?;
    ensure_copied(tx, "v1_deliveries", deliveries)?;
    let entries = copy_v1_entries(tx)?;
    ensure_copied(tx, "v1_entries", entries)?;
    tx.execute_batch(DROP_V1)?;
    Ok(())
}

/// Copy v1 entries into v2, encoding `text` with the shared payload codec.
/// Returns the number of rows copied.
fn copy_v1_entries(tx: &Transaction<'_>) -> Result<usize, HistoryError> {
    // Column order matches the INSERT below; only `text` (index 4) is
    // re-encoded, everything else is bound straight from the source row.
    let mut read = tx.prepare(
        "SELECT e.seq, s.first_seq, e.entry_key, e.kind, e.text, e.turn_id,
                e.native_message_id, e.tool_call_id, e.tool_name, e.tool_status,
                e.first_seen_ms, e.last_seen_ms,
                CASE WHEN e.kind = 'tool' AND e.tool_status IN ('completed', 'failed')
                     THEN e.last_seen_ms END
         FROM v1_entries e JOIN sessions s
           ON s.origin = e.origin AND s.harness = e.harness AND s.session_id = e.session_id
         ORDER BY e.seq",
    )?;
    let mut write = tx.prepare(
        "INSERT INTO entries(seq, session_ref, entry_key, kind, text, turn_id,
                             native_message_id, tool_call_id, tool_name, tool_status,
                             first_seen_ms, last_seen_ms, tool_status_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
    )?;
    let mut rows = read.query([])?;
    let mut copied = 0usize;
    while let Some(row) = rows.next()? {
        for index in 0..13 {
            let value = row.get_ref(index)?;
            if index == 4 {
                let text = match value {
                    ValueRef::Text(_) | ValueRef::Null => value.as_str_or_null(),
                    _ => return Err(HistoryError::Corrupt("v1 entry text is not text")),
                }
                .map_err(|_| HistoryError::Corrupt("v1 entry text is not valid UTF-8"))?;
                write.raw_bind_parameter(index + 1, payload::encode(text)?)?;
            } else {
                write.raw_bind_parameter(index + 1, ToSqlOutput::Borrowed(value))?;
            }
        }
        write.raw_execute()?;
        copied += 1;
    }
    Ok(copied)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitReport {
    pub path: PathBuf,
    pub schema_version: u32,
    pub created: bool,
}

fn create_dir_private(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        set_mode(dir, 0o700)
    }
    #[cfg(not(unix))]
    {
        // ACL hardening is not implemented on this platform: the directory
        // keeps the default permissions of its parent.
        fs::create_dir_all(dir)
    }
}

fn create_db_private(db: &Path) -> io::Result<()> {
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // Applied at creation so no window exists where umask exposes the file.
        opts.mode(0o600);
    }
    match opts.open(db) {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e),
    }
}

/// Create (or verify and migrate) the store. Idempotent. Creates the
/// directory 0700 and the database with its WAL/SHM files 0600 on unix;
/// rejects symlinked directories and databases and stores from a newer
/// release.
pub fn init_store(dir: &Path) -> Result<InitReport, HistoryError> {
    match fs::symlink_metadata(dir) {
        Ok(md) if md.file_type().is_symlink() => {
            return Err(HistoryError::Insecure(
                "history directory is a symlink".into(),
            ))
        }
        Ok(md) if !md.is_dir() => {
            return Err(HistoryError::Insecure(
                "history path is not a directory".into(),
            ))
        }
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    create_dir_private(dir)?;

    let db = db_path(dir);
    match fs::symlink_metadata(&db) {
        Ok(md) if md.file_type().is_symlink() => {
            return Err(HistoryError::Insecure(
                "history database is a symlink".into(),
            ))
        }
        Ok(md) if !md.is_file() => {
            return Err(HistoryError::Insecure(
                "history database is not a regular file".into(),
            ))
        }
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => create_db_private(&db)?,
        Err(e) => return Err(e.into()),
    }
    harden_files(&db)?;
    // Re-validate: also rejects a symlinked sidecar planted before we opened.
    check_existing(dir)?;

    let mut conn = Connection::open_with_flags(
        &db,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let found = user_version(&conn)?;
    if found > SCHEMA_VERSION {
        return Err(version_mismatch(found));
    }
    configure(&conn)?;
    let created = upgrade(&mut conn)?;
    drop(conn);
    harden_files(&db)?;
    Ok(InitReport {
        path: db,
        schema_version: SCHEMA_VERSION,
        created,
    })
}
