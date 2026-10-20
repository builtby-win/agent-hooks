//! Schema v1 -> v2 migration: preservation, atomicity, version handling,
//! and continuity of cursors and delivery de-duplication across the upgrade.

use std::path::{Path, PathBuf};

use agent_hooks_history::{
    init_store, Capture, CaptureEntry, HistoryError, HistoryReader, HistoryWriter, MessagesQuery,
    RecordOutcome, SessionsQuery, DB_FILE, SCHEMA_VERSION,
};
use agent_hooks_protocol::{HistoryEntryKind, HistoryToolStatus, LOCAL_ORIGIN};
use rusqlite::Connection;
use tempfile::TempDir;

/// The version 1 DDL exactly as the pre-v2 release created it.
const V1_SCHEMA: &str = "
CREATE TABLE index_meta (
    index_id TEXT NOT NULL,
    last_persisted_ms INTEGER
);
CREATE TABLE sequence (
    seq INTEGER PRIMARY KEY AUTOINCREMENT
);
CREATE TABLE sessions (
    origin TEXT NOT NULL,
    harness TEXT NOT NULL,
    session_id TEXT NOT NULL,
    parent_session_id TEXT,
    workspace TEXT,
    title TEXT,
    first_seq INTEGER NOT NULL,
    first_seen_ms INTEGER NOT NULL,
    last_seen_ms INTEGER NOT NULL,
    status TEXT NOT NULL,
    PRIMARY KEY (origin, harness, session_id)
);
CREATE INDEX sessions_first_seq ON sessions(first_seq);
CREATE INDEX sessions_session_id ON sessions(session_id);
CREATE TABLE deliveries (
    origin TEXT NOT NULL,
    harness TEXT NOT NULL,
    session_id TEXT NOT NULL,
    delivery_id TEXT NOT NULL,
    PRIMARY KEY (origin, harness, session_id, delivery_id),
    FOREIGN KEY (origin, harness, session_id)
        REFERENCES sessions(origin, harness, session_id)
        DEFERRABLE INITIALLY DEFERRED
);
CREATE TABLE entries (
    seq INTEGER PRIMARY KEY,
    origin TEXT NOT NULL,
    harness TEXT NOT NULL,
    session_id TEXT NOT NULL,
    entry_key TEXT NOT NULL,
    kind TEXT NOT NULL,
    text TEXT,
    turn_id TEXT,
    native_message_id TEXT,
    tool_call_id TEXT,
    tool_name TEXT,
    tool_status TEXT,
    first_seen_ms INTEGER NOT NULL,
    last_seen_ms INTEGER NOT NULL,
    UNIQUE (origin, harness, session_id, entry_key),
    FOREIGN KEY (origin, harness, session_id)
        REFERENCES sessions(origin, harness, session_id)
);
";

const V1_INDEX_ID: &str = "idx-v1-fixture";
const V1_PERSISTED_MS: i64 = 5_000;
/// Highest sequence the v1 store ever allocated: rows 11 and 12 were deleted
/// from `sequence`, so only `sqlite_sequence` remembers it.
const V1_LAST_SEQ: i64 = 12;

fn big_unicode() -> String {
    "héllo 世界 🚀 \"quoted\" \\ \n\t\u{0}end ".repeat(300)
}

/// Opaque legacy tool text: input and output joined, boundary unrecorded.
fn legacy_tool_text() -> String {
    format!("{{\"cmd\":\"ls -la 世界\"}}\n{}", big_unicode())
}

fn private_dir(tmp: &TempDir) -> PathBuf {
    let dir = tmp.path().join("history");
    std::fs::create_dir(&dir).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    dir
}

fn open_raw(dir: &Path) -> Connection {
    Connection::open(dir.join(DB_FILE)).unwrap()
}

fn restrict_db(dir: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.join(DB_FILE), std::fs::Permissions::from_mode(0o600))
            .unwrap();
    }
    #[cfg(not(unix))]
    let _ = dir;
}

/// A realistic v1 store:
/// - sessions: `sess-a` (seq 1), `sess-b` (seq 2), plus `sess-a` under another
///   harness (seq 3) to exercise the composite external key;
/// - entries: user 4, long assistant 5, completed legacy tool 6, running
///   legacy tool 7, `sess-b` user 8, long legacy failed tool 9;
/// - deliveries d1..d3 for `sess-a`, d4 for `sess-b`.
fn v1_store(tmp: &TempDir) -> PathBuf {
    let dir = private_dir(tmp);
    let conn = open_raw(&dir);
    conn.execute_batch(V1_SCHEMA).unwrap();
    conn.pragma_update(None, "user_version", 1).unwrap();
    conn.execute(
        "INSERT INTO index_meta(index_id, last_persisted_ms) VALUES (?1, ?2)",
        (V1_INDEX_ID, V1_PERSISTED_MS),
    )
    .unwrap();
    for _ in 0..V1_LAST_SEQ {
        conn.execute("INSERT INTO sequence DEFAULT VALUES", [])
            .unwrap();
    }
    conn.execute("DELETE FROM sequence WHERE seq > 10", [])
        .unwrap();

    let session = |first_seq: i64, harness: &str, id: &str, parent: Option<&str>, status: &str| {
        conn.execute(
            "INSERT INTO sessions(origin, harness, session_id, parent_session_id, workspace,
                                  title, first_seq, first_seen_ms, last_seen_ms, status)
             VALUES (?1, ?2, ?3, ?4, '/work/世界', 'Title 🚀', ?5, ?6, ?7, ?8)",
            (
                LOCAL_ORIGIN,
                harness,
                id,
                parent,
                first_seq,
                1_000 + first_seq,
                2_000 + first_seq,
                status,
            ),
        )
        .unwrap();
    };
    session(1, "claude-code", "sess-a", Some("parent-1"), "idle");
    session(2, "opencode", "sess-b", None, "running");
    session(3, "opencode", "sess-a", None, "completed");

    let entry = |seq: i64,
                 harness: &str,
                 session_id: &str,
                 key: &str,
                 kind: &str,
                 text: Option<&str>,
                 tool: Option<(&str, &str, &str)>| {
        let (call_id, name, status) = match tool {
            Some((c, n, s)) => (Some(c), Some(n), Some(s)),
            None => (None, None, None),
        };
        conn.execute(
            "INSERT INTO entries(seq, origin, harness, session_id, entry_key, kind, text,
                                 turn_id, native_message_id, tool_call_id, tool_name,
                                 tool_status, first_seen_ms, last_seen_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'turn-1', ?8, ?9, ?10, ?11, ?12, ?13)",
            (
                seq,
                LOCAL_ORIGIN,
                harness,
                session_id,
                key,
                kind,
                text,
                (kind != "tool").then(|| format!("native-{seq}")),
                call_id,
                name,
                status,
                3_000 + seq,
                4_000 + seq,
            ),
        )
        .unwrap();
    };
    let assistant = big_unicode();
    let legacy = legacy_tool_text();
    entry(
        4,
        "claude-code",
        "sess-a",
        "message:user:native-4",
        "user",
        Some("hello"),
        None,
    );
    entry(
        5,
        "claude-code",
        "sess-a",
        "message:assistant:native-5",
        "assistant",
        Some(&assistant),
        None,
    );
    entry(
        6,
        "claude-code",
        "sess-a",
        "tool:t1",
        "tool",
        Some("ls\nok output"),
        Some(("t1", "Bash", "completed")),
    );
    entry(
        7,
        "claude-code",
        "sess-a",
        "tool:t2",
        "tool",
        None,
        Some(("t2", "Read", "running")),
    );
    entry(
        8,
        "opencode",
        "sess-b",
        "message:user:native-8",
        "user",
        Some("héllo 世界"),
        None,
    );
    entry(
        9,
        "opencode",
        "sess-a",
        "tool:t9",
        "tool",
        Some(&legacy),
        Some(("t9", "Bash", "failed")),
    );

    for (harness, session_id, delivery) in [
        ("claude-code", "sess-a", "d1"),
        ("claude-code", "sess-a", "d2"),
        ("claude-code", "sess-a", "d3"),
        ("opencode", "sess-b", "d4"),
    ] {
        conn.execute(
            "INSERT INTO deliveries(origin, harness, session_id, delivery_id)
             VALUES (?1, ?2, ?3, ?4)",
            (LOCAL_ORIGIN, harness, session_id, delivery),
        )
        .unwrap();
    }
    drop(conn);
    restrict_db(&dir);
    dir
}

type Rows = Vec<Vec<rusqlite::types::Value>>;

fn rows(conn: &Connection, sql: &str) -> Rows {
    let mut stmt = conn.prepare(sql).unwrap();
    let cols = stmt.column_count();
    stmt.query_map([], |r| {
        (0..cols)
            .map(|i| r.get::<_, rusqlite::types::Value>(i))
            .collect()
    })
    .unwrap()
    .map(Result::unwrap)
    .collect()
}

/// Schema objects (type, name, table, SQL) other than SQLite internals.
fn schema_objects(dir: &Path) -> Rows {
    rows(
        &open_raw(dir),
        "SELECT type, name, tbl_name, sql FROM sqlite_master
         WHERE name NOT LIKE 'sqlite_%' ORDER BY type, name",
    )
}

fn user_version(dir: &Path) -> i64 {
    open_raw(dir)
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap()
}

fn capture(session: &str, delivery: &str, ts: u64, text: &str) -> Capture {
    Capture {
        origin: LOCAL_ORIGIN.into(),
        harness: "claude-code".into(),
        session_id: session.into(),
        parent_session_id: None,
        workspace: None,
        title: None,
        status: "running".into(),
        delivery_id: delivery.into(),
        timestamp_ms: ts,
        turn_id: None,
        entry: Some(CaptureEntry {
            kind: HistoryEntryKind::User,
            text: Some(text.into()),
            native_message_id: None,
            tool_call_id: None,
            tool_name: None,
            tool_status: None,
        }),
    }
}

fn messages(dir: &Path, session: &str, harness: &str) -> agent_hooks_protocol::HistoryMessagesPage {
    HistoryReader::open(dir)
        .unwrap()
        .messages(&MessagesQuery {
            session: session.into(),
            harness: Some(harness.into()),
            origin: None,
            after: None,
            before: None,
            limit: None,
            kinds: Vec::new(),
        })
        .unwrap()
}

#[test]
fn migration_carries_version_counter_and_index_id_and_leaves_no_v1_tables() {
    let fresh_tmp = tempfile::tempdir().unwrap();
    let fresh = fresh_tmp.path().join("history");
    assert!(init_store(&fresh).unwrap().created);

    let tmp = tempfile::tempdir().unwrap();
    let dir = v1_store(&tmp);
    let report = init_store(&dir).unwrap();
    assert!(!report.created);
    assert_eq!(report.schema_version, SCHEMA_VERSION);
    assert_eq!(user_version(&dir), i64::from(SCHEMA_VERSION));
    assert_eq!(user_version(&fresh), i64::from(SCHEMA_VERSION));

    // Only the v2 tables remain: nothing parked or retired by the upgrade.
    let tables: Vec<String> = rows(
        &open_raw(&dir),
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'
         ORDER BY name",
    )
    .into_iter()
    .map(|r| match &r[0] {
        rusqlite::types::Value::Text(name) => name.clone(),
        other => panic!("unexpected table name {other:?}"),
    })
    .collect();
    assert_eq!(tables, ["deliveries", "entries", "index_meta", "sessions"]);

    // Fresh counter starts at zero; migrated counter and id carry over.
    let fresh_meta = rows(
        &open_raw(&fresh),
        "SELECT id, last_persisted_ms, last_seq FROM index_meta",
    );
    assert_eq!(
        fresh_meta,
        vec![vec![1.into(), rusqlite::types::Value::Null, 0.into()]]
    );
    let meta = rows(
        &open_raw(&dir),
        "SELECT id, index_id, last_persisted_ms, last_seq FROM index_meta",
    );
    assert_eq!(
        meta,
        vec![vec![
            1.into(),
            V1_INDEX_ID.to_owned().into(),
            V1_PERSISTED_MS.into(),
            V1_LAST_SEQ.into()
        ]]
    );
}

#[test]
fn migration_preserves_every_row_key_timestamp_and_receipt() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = v1_store(&tmp);
    init_store(&dir).unwrap();
    let conn = open_raw(&dir);

    let sessions = rows(
        &conn,
        "SELECT first_seq, origin, harness, session_id, parent_session_id, workspace, title,
                first_seen_ms, last_seen_ms, status FROM sessions ORDER BY first_seq",
    );
    assert_eq!(sessions.len(), 3);
    let keys: Vec<(i64, String, String)> = sessions
        .iter()
        .map(|r| match (&r[0], &r[2], &r[3]) {
            (
                rusqlite::types::Value::Integer(s),
                rusqlite::types::Value::Text(h),
                rusqlite::types::Value::Text(id),
            ) => (*s, h.clone(), id.clone()),
            other => panic!("unexpected session row {other:?}"),
        })
        .collect();
    assert_eq!(
        keys,
        vec![
            (1, "claude-code".into(), "sess-a".into()),
            (2, "opencode".into(), "sess-b".into()),
            (3, "opencode".into(), "sess-a".into()),
        ]
    );
    assert_eq!(sessions[0][4], "parent-1".to_owned().into());
    assert_eq!(sessions[0][5], "/work/世界".to_owned().into());
    assert_eq!(sessions[0][6], "Title 🚀".to_owned().into());
    assert_eq!(sessions[0][7], 1_001.into());
    assert_eq!(sessions[0][8], 2_001.into());
    assert_eq!(sessions[0][9], "idle".to_owned().into());

    let receipts = rows(
        &conn,
        "SELECT session_ref, delivery_id FROM deliveries ORDER BY session_ref, delivery_id",
    );
    assert_eq!(
        receipts,
        vec![
            vec![1.into(), "d1".to_owned().into()],
            vec![1.into(), "d2".to_owned().into()],
            vec![1.into(), "d3".to_owned().into()],
            vec![2.into(), "d4".to_owned().into()],
        ]
    );

    let entries = rows(
        &conn,
        "SELECT seq, session_ref, entry_key, kind, turn_id, native_message_id, tool_call_id,
                tool_name, tool_status, first_seen_ms, last_seen_ms,
                tool_input, tool_output, tool_input_ms, tool_output_ms, tool_status_ms
         FROM entries ORDER BY seq",
    );
    let seqs: Vec<_> = entries.iter().map(|r| r[0].clone()).collect();
    assert_eq!(
        seqs,
        [4, 5, 6, 7, 8, 9]
            .map(rusqlite::types::Value::Integer)
            .to_vec()
    );
    let session_refs: Vec<_> = entries.iter().map(|r| r[1].clone()).collect();
    assert_eq!(
        session_refs,
        [1, 1, 1, 1, 2, 3]
            .map(rusqlite::types::Value::Integer)
            .to_vec()
    );
    assert_eq!(entries[0][2], "message:user:native-4".to_owned().into());
    assert_eq!(entries[0][5], "native-4".to_owned().into());
    assert_eq!(entries[2][6], "t1".to_owned().into());
    assert_eq!(entries[2][7], "Bash".to_owned().into());
    assert_eq!(entries[2][8], "completed".to_owned().into());
    assert_eq!(entries[2][9], 3_006.into());
    assert_eq!(entries[2][10], 4_006.into());
    // Historical tool text is opaque: phase payloads and phase timestamps
    // stay empty, and only terminal rows get a status timestamp.
    for tool in [&entries[2], &entries[3], &entries[5]] {
        assert_eq!(tool[11], rusqlite::types::Value::Null);
        assert_eq!(tool[12], rusqlite::types::Value::Null);
        assert_eq!(tool[13], rusqlite::types::Value::Null);
        assert_eq!(tool[14], rusqlite::types::Value::Null);
    }
    assert_eq!(entries[2][15], 4_006.into());
    assert_eq!(entries[3][15], rusqlite::types::Value::Null);
    assert_eq!(entries[5][15], 4_009.into());
    for non_tool in [&entries[0], &entries[1], &entries[4]] {
        assert_eq!(non_tool[15], rusqlite::types::Value::Null);
    }
}

#[test]
fn migrated_text_is_byte_exact_and_compressed_when_large() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = v1_store(&tmp);
    init_store(&dir).unwrap();

    let conn = open_raw(&dir);
    let storage: Vec<(i64, String, Option<i64>)> = {
        let mut stmt = conn
            .prepare("SELECT seq, typeof(text), length(text) FROM entries ORDER BY seq")
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    let big = big_unicode();
    let legacy = legacy_tool_text();
    assert_eq!(storage[0], (4, "text".into(), Some(5)));
    assert_eq!(storage[1].1, "blob");
    assert!(storage[1].2.unwrap() < i64::try_from(big.len()).unwrap());
    assert_eq!(storage[2], (6, "text".into(), Some(12)));
    assert_eq!(storage[3], (7, "null".into(), None));
    assert_eq!(storage[5].1, "blob");
    assert!(storage[5].2.unwrap() < i64::try_from(legacy.len()).unwrap());

    let a = messages(&dir, "sess-a", "claude-code");
    let texts: Vec<_> = a.items.iter().map(|e| e.text.clone()).collect();
    assert_eq!(
        texts,
        vec![
            Some("hello".to_owned()),
            Some(big.clone()),
            Some("ls\nok output".to_owned()),
            None,
        ]
    );
    for tool in &a.items[2..] {
        assert_eq!(tool.kind, HistoryEntryKind::Tool);
        assert_eq!((&tool.tool_input, &tool.tool_output), (&None, &None));
    }
    assert_eq!(a.items[2].tool_status, Some(HistoryToolStatus::Completed));
    assert_eq!(a.items[3].tool_status, Some(HistoryToolStatus::Running));

    let opencode_a = messages(&dir, "sess-a", "opencode");
    assert_eq!(opencode_a.items.len(), 1);
    assert_eq!(opencode_a.items[0].text.as_deref(), Some(legacy.as_str()));
    assert_eq!(
        opencode_a.items[0].tool_status,
        Some(HistoryToolStatus::Failed)
    );
    assert_eq!(
        messages(&dir, "sess-b", "opencode").items[0]
            .text
            .as_deref(),
        Some("héllo 世界")
    );
}

#[test]
fn migrated_sessions_keep_sequence_order_and_metadata() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = v1_store(&tmp);
    init_store(&dir).unwrap();

    let page = HistoryReader::open(&dir)
        .unwrap()
        .sessions(&SessionsQuery::default())
        .unwrap();
    let listed: Vec<_> = page
        .items
        .iter()
        .map(|s| {
            (
                s.seq,
                s.key.harness.as_str(),
                s.key.session_id.as_str(),
                s.status.as_str(),
            )
        })
        .collect();
    assert_eq!(
        listed,
        vec![
            (1, "claude-code", "sess-a", "idle"),
            (2, "opencode", "sess-b", "running"),
            (3, "opencode", "sess-a", "completed"),
        ]
    );
    assert_eq!(page.items[0].parent_session_id.as_deref(), Some("parent-1"));
    assert_eq!(page.items[0].first_seen_ms, 1_001);
    assert_eq!(page.items[0].last_seen_ms, 2_001);
    let status = HistoryReader::open(&dir).unwrap().status().unwrap();
    assert_eq!((status.sessions, status.entries), (3, 6));
    assert_eq!(status.last_persisted_ms, Some(5_000));
}

#[test]
fn reader_refuses_v1_until_explicitly_migrated_and_stays_read_only() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = v1_store(&tmp);
    let before = schema_objects(&dir);

    match HistoryReader::open(&dir) {
        Err(HistoryError::SchemaUpgradeRequired { found: 1, expected }) => {
            assert_eq!(expected, SCHEMA_VERSION);
        }
        other => panic!("expected upgrade required, got {:?}", other.map(|_| ())),
    }
    assert_eq!(user_version(&dir), 1);
    assert_eq!(schema_objects(&dir), before);

    // The writer is the explicit upgrade path besides init.
    drop(HistoryWriter::open(&dir).unwrap());
    assert_eq!(user_version(&dir), i64::from(SCHEMA_VERSION));
    assert_eq!(
        HistoryReader::open(&dir).unwrap().status().unwrap().entries,
        6
    );
}

#[test]
fn migration_is_atomic_when_a_late_row_cannot_be_converted() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = v1_store(&tmp);
    // Entry 9 is copied last, after sessions, receipts and earlier entries.
    open_raw(&dir)
        .execute(
            "UPDATE entries SET text = CAST('opaque' AS BLOB) WHERE seq = 9",
            [],
        )
        .unwrap();
    let before_objects = schema_objects(&dir);
    let before_entries = rows(&open_raw(&dir), "SELECT * FROM entries ORDER BY seq");

    let err = init_store(&dir).unwrap_err();
    assert!(matches!(err, HistoryError::Corrupt(_)), "{err}");
    assert!(!err.to_string().contains("opaque"));
    assert_eq!(user_version(&dir), 1);
    assert_eq!(schema_objects(&dir), before_objects);
    assert_eq!(
        rows(&open_raw(&dir), "SELECT * FROM entries ORDER BY seq"),
        before_entries
    );
    // The writer path fails the same way and leaves nothing behind.
    assert!(HistoryWriter::open(&dir).is_err());
    assert_eq!(user_version(&dir), 1);

    // Repairing the row lets the same store migrate cleanly afterwards.
    open_raw(&dir)
        .execute("UPDATE entries SET text = 'opaque' WHERE seq = 9", [])
        .unwrap();
    init_store(&dir).unwrap();
    assert_eq!(user_version(&dir), i64::from(SCHEMA_VERSION));
    assert_eq!(
        messages(&dir, "sess-a", "opencode").items[0]
            .text
            .as_deref(),
        Some("opaque")
    );
}

#[test]
fn migration_is_atomic_for_orphaned_receipts_and_ambiguous_index_metadata() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = v1_store(&tmp);
    // Bundled SQLite enforces foreign keys by default; plant the orphan the
    // way a damaged v1 file would contain it.
    let damaged = open_raw(&dir);
    damaged.pragma_update(None, "foreign_keys", "OFF").unwrap();
    damaged
        .execute(
            "INSERT INTO deliveries(origin, harness, session_id, delivery_id)
             VALUES (?1, 'claude-code', 'missing-session', 'dx')",
            [LOCAL_ORIGIN],
        )
        .unwrap();
    drop(damaged);
    let before = schema_objects(&dir);
    assert!(matches!(init_store(&dir), Err(HistoryError::Corrupt(_))));
    assert_eq!(user_version(&dir), 1);
    assert_eq!(schema_objects(&dir), before);

    open_raw(&dir)
        .execute("DELETE FROM deliveries WHERE delivery_id = 'dx'", [])
        .unwrap();
    open_raw(&dir)
        .execute("INSERT INTO index_meta(index_id) VALUES ('second')", [])
        .unwrap();
    assert!(matches!(init_store(&dir), Err(HistoryError::Corrupt(_))));
    assert_eq!(user_version(&dir), 1);
    assert_eq!(schema_objects(&dir), before);
}

#[test]
fn newer_schema_is_refused_without_touching_the_store() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = v1_store(&tmp);
    open_raw(&dir)
        .pragma_update(None, "user_version", 99)
        .unwrap();
    let before = schema_objects(&dir);

    assert!(matches!(
        init_store(&dir),
        Err(HistoryError::UnsupportedNewerSchema { found: 99, .. })
    ));
    assert!(matches!(
        HistoryWriter::open(&dir),
        Err(HistoryError::UnsupportedNewerSchema { found: 99, .. })
    ));
    assert!(matches!(
        HistoryReader::open(&dir),
        Err(HistoryError::UnsupportedNewerSchema { found: 99, .. })
    ));
    assert_eq!(user_version(&dir), 99);
    assert_eq!(schema_objects(&dir), before);
}

#[test]
fn init_after_migration_is_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = v1_store(&tmp);
    init_store(&dir).unwrap();
    let snapshot = rows(&open_raw(&dir), "SELECT * FROM entries ORDER BY seq");
    let objects = schema_objects(&dir);

    let again = init_store(&dir).unwrap();
    assert!(!again.created);
    assert_eq!(schema_objects(&dir), objects);
    assert_eq!(
        rows(&open_raw(&dir), "SELECT * FROM entries ORDER BY seq"),
        snapshot
    );
}

fn hex_json(raw: &str) -> serde_json::Value {
    let bytes: Vec<u8> = (0..raw.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&raw[i..i + 2], 16).unwrap())
        .collect();
    serde_json::from_slice(&bytes).unwrap()
}

#[test]
fn receipts_and_cursors_continue_across_the_migration() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = v1_store(&tmp);
    init_store(&dir).unwrap();

    // First page: one of four entries, cursor pinned to the preserved index.
    let first = HistoryReader::open(&dir)
        .unwrap()
        .messages(&MessagesQuery {
            session: "sess-a".into(),
            harness: Some("claude-code".into()),
            origin: None,
            after: None,
            before: None,
            limit: Some(1),
            kinds: Vec::new(),
        })
        .unwrap();
    assert_eq!(first.items.len(), 1);
    let cursor = first.next_cursor.clone().expect("more pages");
    let state = hex_json(&cursor);
    assert_eq!(state["index_id"], V1_INDEX_ID);
    assert_eq!(state["high_water"], V1_LAST_SEQ);

    let mut writer = HistoryWriter::open(&dir).unwrap();
    // A receipt written before the upgrade still de-duplicates.
    assert_eq!(
        writer
            .record(&capture("sess-a", "d1", 9_000, "replayed"))
            .unwrap(),
        RecordOutcome::Duplicate
    );
    assert_eq!(
        writer
            .record(&capture("sess-a", "d-new", 9_001, "after upgrade"))
            .unwrap(),
        RecordOutcome::Stored
    );
    // Same external key, same session: no second session row appears.
    assert_eq!(
        HistoryReader::open(&dir)
            .unwrap()
            .status()
            .unwrap()
            .sessions,
        3
    );

    // The new entry continues the global sequence beyond the v1 maximum and
    // sits above the old cursor's high-water mark.
    let max_entry: i64 = open_raw(&dir)
        .query_row("SELECT MAX(seq) FROM entries", [], |r| r.get(0))
        .unwrap();
    assert!(max_entry > V1_LAST_SEQ);
    let counter: i64 = open_raw(&dir)
        .query_row("SELECT last_seq FROM index_meta", [], |r| r.get(0))
        .unwrap();
    assert!(counter >= max_entry);

    let rest = HistoryReader::open(&dir)
        .unwrap()
        .messages(&MessagesQuery {
            session: "sess-a".into(),
            harness: Some("claude-code".into()),
            origin: None,
            after: Some(cursor),
            before: None,
            limit: None,
            kinds: Vec::new(),
        })
        .unwrap();
    let seqs: Vec<i64> = first
        .items
        .iter()
        .chain(&rest.items)
        .map(|e| e.seq)
        .collect();
    assert_eq!(seqs, vec![4, 5, 6, 7]);
    assert!(rest
        .items
        .iter()
        .all(|e| e.text.as_deref() != Some("after upgrade")));

    let fresh = messages(&dir, "sess-a", "claude-code");
    assert_eq!(fresh.items.len(), 5);
    assert_eq!(fresh.items[4].text.as_deref(), Some("after upgrade"));
    assert_eq!(fresh.items[4].seq, max_entry);
}
