//! `agent-hooks history ...`: initialize the opt-in store and query it
//! read-only. Every result is one JSON document on stdout; failures print a
//! `HistoryErrorResponse` and exit 1. The JSON is the consumer contract, the
//! SQLite schema is not.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use agent_hooks_adapters::{parse_line, AdapterRegistry, HarnessAdapter, ParsedLine, RecordSource};
use agent_hooks_history::{
    init_store, resolve_history_dir, BackfillFile, HistoryError, HistoryReader, HistoryWriter,
    MessageSearchQuery, MessagesQuery, RecordOutcome, SessionsQuery,
};
use agent_hooks_protocol::{
    HistorySearchPage, HistorySessionKey, HISTORY_API_VERSION, LOCAL_ORIGIN,
};
use serde::Serialize;
use std::io::{BufRead, Read};
use std::ops::ControlFlow;
use std::time::SystemTime;

use crate::cli::{
    HistoryArgs, HistoryBackfillArgs, HistoryCommand, HistoryImportArgs, HistoryKindArg,
};

/// Shown by `history init`: capture is prospective, indefinite, unfiltered by
/// content, and complete (tool bodies included).
const INIT_NOTICE: &str = "History stores complete user prompts, assistant summaries, and tool \
inputs and outputs. Nothing is truncated and there is no retention limit. Data may contain \
secrets and is not encrypted: large values are compressed losslessly for size only, which is not a \
security measure. An existing store from an older version is upgraded in place to the current schema by \
this command or by the next recording hook, and may keep reusable free pages until an explicit SQLite \
VACUUM (the file size does not shrink automatically); tool text recorded before the upgrade is kept verbatim \
without a known input/output boundary. To remove the history, delete the history directory. \
Capture starts with the next agent hook; hooks find the store at the default location or at \
$AGENT_HOOKS_HISTORY_DIR (which must be set in the agent's environment).";

#[derive(Serialize)]
struct InitOutput<'a> {
    api_version: u32,
    path: &'a Path,
    schema_version: u32,
    created: bool,
    notice: &'static str,
}

pub fn run(args: HistoryArgs) -> anyhow::Result<()> {
    let result = match args.command {
        HistoryCommand::Init(a) => init(a.history_dir.as_deref()),
        HistoryCommand::Sessions(a) => sessions(
            a.history_dir.as_deref(),
            SessionsQuery {
                session: a.session,
                search: a.search,
                harness: a.harness,
                origin: a.origin,
                after: a.after,
                limit: a.limit,
            },
        ),
        HistoryCommand::Messages(a) => messages(
            a.history_dir.as_deref(),
            MessagesQuery {
                session: a.session,
                harness: a.harness,
                origin: a.origin,
                after: a.after,
                before: a.before,
                limit: a.limit,
                kinds: a.kind.into_iter().map(entry_kind).collect(),
            },
        ),
        HistoryCommand::Search(a) => search(a.history_dir.as_deref(), a.query, a.limit),
        HistoryCommand::Status(a) => status(a.history_dir.as_deref()),
        HistoryCommand::Backfill(a) => backfill(a),
        HistoryCommand::Import(a) => import(a),
    };
    match result {
        Ok(json) => {
            println!("{json}");
            Ok(())
        }
        Err(err) => {
            println!("{}", serde_json::to_string_pretty(&err.to_response())?);
            std::process::exit(1);
        }
    }
}

fn init(dir: Option<&Path>) -> Result<String, HistoryError> {
    let dir: PathBuf = resolve_history_dir(dir);
    let report = init_store(&dir)?;
    to_json(&InitOutput {
        api_version: agent_hooks_protocol::HISTORY_API_VERSION,
        path: &report.path,
        schema_version: report.schema_version,
        created: report.created,
        notice: INIT_NOTICE,
    })
}

fn entry_kind(kind: HistoryKindArg) -> agent_hooks_protocol::HistoryEntryKind {
    use agent_hooks_protocol::HistoryEntryKind;
    match kind {
        HistoryKindArg::User => HistoryEntryKind::User,
        HistoryKindArg::Assistant => HistoryEntryKind::Assistant,
        HistoryKindArg::Tool => HistoryEntryKind::Tool,
    }
}

fn sessions(dir: Option<&Path>, query: SessionsQuery) -> Result<String, HistoryError> {
    let reader = HistoryReader::open(&resolve_history_dir(dir))?;
    to_json(&reader.sessions(&query)?)
}

fn messages(dir: Option<&Path>, query: MessagesQuery) -> Result<String, HistoryError> {
    let reader = HistoryReader::open(&resolve_history_dir(dir))?;
    to_json(&reader.messages(&query)?)
}

/// The sessions to search are a JSON array of session keys on stdin, so a
/// caller can scope a search to hundreds of sessions without hitting argv
/// limits. Text of sessions outside that array is never decoded.
fn search(dir: Option<&Path>, query: String, limit: Option<u32>) -> Result<String, HistoryError> {
    let mut stdin = String::new();
    std::io::stdin().read_to_string(&mut stdin)?;
    let sessions: Vec<HistorySessionKey> = serde_json::from_str(&stdin).map_err(|_| {
        HistoryError::InvalidArgument(
            "stdin must be a JSON array of {origin, harness, session_id} session keys".into(),
        )
    })?;
    let reader = HistoryReader::open(&resolve_history_dir(dir))?;
    let items = reader.search_messages(&MessageSearchQuery {
        needle: query,
        sessions,
        limit,
    })?;
    to_json(&HistorySearchPage {
        api_version: HISTORY_API_VERSION,
        items,
    })
}

fn status(dir: Option<&Path>) -> Result<String, HistoryError> {
    let reader = HistoryReader::open(&resolve_history_dir(dir))?;
    to_json(&reader.status()?)
}

fn to_json<T: Serialize>(value: &T) -> Result<String, HistoryError> {
    serde_json::to_string_pretty(value).map_err(|e| HistoryError::InvalidArgument(e.to_string()))
}

/// Per-harness result of `history backfill`.
#[derive(Serialize, Default)]
struct BackfillReport {
    harness: String,
    /// `imported` (built-in importer), `script` (manifest command or native
    /// cursor source), `no importer` for an adapter that cannot backfill, or
    /// `unsupported os` when its importer or source does not cover this OS.
    status: &'static str,
    /// Set when a cursor source failed (non-zero exit, timeout, unreadable
    /// store). Records stored before the failure stay; its cursor was not saved.
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    files: Option<u64>,
    /// Files skipped because nothing changed since the last backfill.
    #[serde(skip_serializing_if = "Option::is_none")]
    unchanged: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    new: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    duplicate: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    skipped: Option<u64>,
    /// Sessions left alone because live hooks already captured them.
    #[serde(skip_serializing_if = "Option::is_none")]
    live_sessions_skipped: Option<u64>,
}

#[derive(Serialize)]
struct BackfillOutput {
    api_version: u32,
    harnesses: Vec<BackfillReport>,
}

fn parse_since(raw: &str) -> Result<SystemTime, HistoryError> {
    let ts = chrono::DateTime::parse_from_rfc3339(raw)
        .map(|t| t.to_utc())
        .or_else(|_| {
            chrono::NaiveDate::parse_from_str(raw, "%Y-%m-%d")
                .ok()
                .and_then(|d| d.and_hms_opt(0, 0, 0))
                .map(|t| t.and_utc())
                .ok_or(())
        })
        .map_err(|()| {
            HistoryError::InvalidArgument("--since must be RFC 3339 or YYYY-MM-DD".into())
        })?;
    Ok(SystemTime::from(ts))
}

fn backfill(args: HistoryBackfillArgs) -> Result<String, HistoryError> {
    let since = args.since.as_deref().map(parse_since).transpose()?;
    let adapters = match &args.harness {
        Some(id) => vec![AdapterRegistry::get(id)
            .ok_or_else(|| HistoryError::InvalidArgument(format!("unknown harness: {id}")))?],
        None => AdapterRegistry::all(),
    };
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .ok_or_else(|| HistoryError::InvalidArgument("no home directory".into()))?;
    let mut writer = HistoryWriter::open(&resolve_history_dir(args.history_dir.as_deref()))?;
    // Bulk import: wait for a competing hook shim rather than fail.
    writer.set_busy_timeout(std::time::Duration::from_secs(5))?;

    let harnesses = adapters
        .iter()
        .map(|adapter| backfill_adapter(&mut writer, adapter.as_ref(), &home, since, args.rescan))
        .collect::<Result<_, _>>()?;
    to_json(&BackfillOutput {
        api_version: HISTORY_API_VERSION,
        harnesses,
    })
}

fn backfill_adapter(
    writer: &mut HistoryWriter,
    adapter: &dyn HarnessAdapter,
    home: &Path,
    since: Option<SystemTime>,
    rescan: bool,
) -> Result<BackfillReport, HistoryError> {
    let harness = adapter.id();
    if let Some(source) = adapter.record_source() {
        if !source.supports_os(std::env::consts::OS) {
            return Ok(BackfillReport {
                harness: harness.to_string(),
                status: "unsupported os",
                ..Default::default()
            });
        }
        return backfill_source(writer, harness, source.as_ref(), home, since, rescan);
    }
    let Some(importer) = adapter.transcript_importer() else {
        return Ok(BackfillReport {
            harness: harness.to_string(),
            status: "no importer",
            ..Default::default()
        });
    };
    if !importer.supported_os().contains(&std::env::consts::OS) {
        return Ok(BackfillReport {
            harness: harness.to_string(),
            status: "unsupported os",
            ..Default::default()
        });
    }
    let (mut files, mut unchanged) = (0, 0);
    let mut counts = Counts::default();
    for path in importer.discover(home, since) {
        files += 1;
        let observed = std::fs::metadata(&path)?;
        let from = match writer.backfill_file(LOCAL_ORIGIN, harness, &path)? {
            Some(saved) if !rescan => match saved.resume(&path)? {
                Some(offset) => offset,
                None => {
                    unchanged += 1;
                    continue;
                }
            },
            _ => 0,
        };
        let mut failure = None;
        let progress = importer
            .import_file(&path, from, &mut |capture| {
                if failure.is_some() {
                    return;
                }
                if let Err(e) = counts.store(writer, Some(capture)) {
                    failure = Some(e);
                }
            })
            .map_err(|e| HistoryError::Io(std::io::Error::other(e.to_string())))?;
        if let Some(e) = failure {
            return Err(e);
        }
        counts.skipped += progress.skipped;
        writer.save_backfill_file(&BackfillFile::snapshot(
            LOCAL_ORIGIN,
            harness,
            &path,
            &observed,
            progress.offset,
        )?)?;
    }
    Ok(BackfillReport {
        harness: harness.to_string(),
        status: "imported",
        files: Some(files),
        unchanged: Some(unchanged),
        new: Some(counts.new),
        duplicate: Some(counts.duplicate),
        skipped: Some(counts.skipped),
        live_sessions_skipped: Some(counts.live_sessions),
        ..Default::default()
    })
}

#[derive(Default)]
struct Counts {
    new: u64,
    duplicate: u64,
    skipped: u64,
    /// Distinct live-captured sessions whose records were dropped.
    live_sessions: u64,
    /// Per run: session id -> captured live (checked once against the store).
    live: HashMap<String, bool>,
}

impl Counts {
    /// Store one observation; `None` (an unmappable record) counts as skipped.
    /// Records of a session that live hooks already captured are dropped
    /// without counting as duplicate/skipped: backfill only fills sessions
    /// missing from the store (or ones it backfilled itself).
    fn store(
        &mut self,
        writer: &mut HistoryWriter,
        capture: Option<agent_hooks_protocol::Capture>,
    ) -> Result<(), HistoryError> {
        let Some(capture) = capture else {
            self.skipped += 1;
            return Ok(());
        };
        let live = match self.live.get(&capture.session_id) {
            Some(&live) => live,
            None => {
                let live = writer.session_is_live(
                    &capture.origin,
                    &capture.harness,
                    &capture.session_id,
                )?;
                self.live.insert(capture.session_id.clone(), live);
                self.live_sessions += u64::from(live);
                live
            }
        };
        if live {
            return Ok(());
        }
        match writer.record(&capture)? {
            RecordOutcome::Stored => self.new += 1,
            RecordOutcome::Duplicate => self.duplicate += 1,
        }
        Ok(())
    }
}

/// The saved cursor is stored as `<coverage>:<cursor>`, `coverage` being the
/// `--since` (unix ms) it was built under, or `-` for everything. A cursor is
/// only valid for requests it covers; otherwise a narrower earlier run would
/// hide older sessions from a later, wider one. Values without the prefix
/// (older versions) never cover anything.
fn encode_cursor(coverage: Option<u64>, cursor: &str) -> String {
    match coverage {
        Some(ms) => format!("{ms}:{cursor}"),
        None => format!("-:{cursor}"),
    }
}

/// `Some((coverage, bare cursor))` when the saved value covers `since`.
fn decode_cursor(saved: &str, since: Option<u64>) -> Option<(Option<u64>, &str)> {
    let (coverage, cursor) = saved.split_once(':')?;
    let coverage = match coverage {
        "-" => None,
        ms => Some(ms.parse::<u64>().ok()?),
    };
    let covers = match (coverage, since) {
        (None, _) => true,
        (Some(saved), Some(requested)) => requested >= saved,
        (Some(_), None) => false,
    };
    covers.then_some((coverage, cursor))
}

/// Drive a cursor source and store what it emits. Records already stored stay
/// on failure; the cursor is saved only when the source succeeds.
fn backfill_source(
    writer: &mut HistoryWriter,
    harness: &str,
    source: &dyn RecordSource,
    home: &Path,
    since: Option<SystemTime>,
    rescan: bool,
) -> Result<BackfillReport, HistoryError> {
    let since_ms = since.map(unix_ms);
    let saved = match rescan {
        true => None,
        false => writer.backfill_cursor(LOCAL_ORIGIN, harness)?,
    };
    let resumed = saved.as_deref().and_then(|s| decode_cursor(s, since_ms));
    let coverage = resumed.map_or(since_ms, |(coverage, _)| coverage);
    let cursor = resumed.map(|(_, cursor)| cursor);
    let mut counts = Counts::default();
    let mut failure = None;
    let result = source.run(home, since_ms, cursor, &mut |capture| match counts
        .store(writer, capture)
    {
        Ok(()) => ControlFlow::Continue(()),
        Err(e) => {
            failure = Some(e);
            ControlFlow::Break(())
        }
    });
    if let Some(e) = failure {
        return Err(e);
    }
    let error = match result {
        Ok(Some(cursor)) => {
            let saved = encode_cursor(coverage, &cursor);
            writer.save_backfill_cursor(
                LOCAL_ORIGIN,
                harness,
                &saved,
                unix_ms(SystemTime::now()),
            )?;
            None
        }
        Ok(None) => None,
        Err(e) => {
            eprintln!("history backfill ({harness}): {e}");
            Some(e.to_string())
        }
    };
    Ok(BackfillReport {
        harness: harness.to_string(),
        status: "script",
        error,
        new: Some(counts.new),
        duplicate: Some(counts.duplicate),
        skipped: Some(counts.skipped),
        live_sessions_skipped: Some(counts.live_sessions),
        ..Default::default()
    })
}

fn unix_ms(t: SystemTime) -> u64 {
    t.duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

#[derive(Serialize)]
struct ImportOutput {
    api_version: u32,
    harness: String,
    new: u64,
    duplicate: u64,
    skipped: u64,
    live_sessions_skipped: u64,
}

/// `history import`: the stdin half of a script backfill, for piping by hand.
fn import(args: HistoryImportArgs) -> Result<String, HistoryError> {
    let adapter = AdapterRegistry::get(&args.harness).ok_or_else(|| {
        HistoryError::InvalidArgument(format!("unknown harness: {}", args.harness))
    })?;
    let mut writer = HistoryWriter::open(&resolve_history_dir(args.history_dir.as_deref()))?;
    writer.set_busy_timeout(std::time::Duration::from_secs(5))?;
    let mut counts = Counts::default();
    for line in std::io::stdin().lock().split(b'\n') {
        match parse_line(adapter.id(), &line?) {
            ParsedLine::Capture(capture) => counts.store(&mut writer, Some(capture))?,
            ParsedLine::Cursor(_) => {}
            ParsedLine::Skipped => counts.store(&mut writer, None)?,
        }
    }
    to_json(&ImportOutput {
        api_version: HISTORY_API_VERSION,
        harness: adapter.id().to_string(),
        new: counts.new,
        duplicate: counts.duplicate,
        skipped: counts.skipped,
        live_sessions_skipped: counts.live_sessions,
    })
}
