//! `agent-hooks history ...`: initialize the opt-in store and query it
//! read-only. Every result is one JSON document on stdout; failures print a
//! `HistoryErrorResponse` and exit 1. The JSON is the consumer contract, the
//! SQLite schema is not.

use std::path::{Path, PathBuf};

use agent_hooks_history::{
    init_store, resolve_history_dir, HistoryError, HistoryReader, MessagesQuery, SessionsQuery,
};
use serde::Serialize;

use crate::cli::{HistoryArgs, HistoryCommand, HistoryKindArg};

/// Shown by `history init`: capture is prospective, indefinite, unfiltered by
/// content, and complete (tool bodies included).
const INIT_NOTICE: &str = "History stores complete user prompts, assistant summaries, and tool \
inputs and outputs. Nothing is truncated and there is no retention limit. Data may contain \
secrets and is not encrypted: large values are compressed losslessly for size only, which is not a \
security measure. An existing store from an older version is upgraded in place (schema v1 to v2) by \
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
        HistoryCommand::Status(a) => status(a.history_dir.as_deref()),
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

fn status(dir: Option<&Path>) -> Result<String, HistoryError> {
    let reader = HistoryReader::open(&resolve_history_dir(dir))?;
    to_json(&reader.status()?)
}

fn to_json<T: Serialize>(value: &T) -> Result<String, HistoryError> {
    serde_json::to_string_pretty(value).map_err(|e| HistoryError::InvalidArgument(e.to_string()))
}
