//! History backfill sources: anything that can replay a harness's past
//! conversations as [`Capture`]s with a resumable cursor.
//!
//! [`RecordSource`] is the one interface `agent-hooks history backfill`
//! drives. Two implementations exist: [`ScriptBackfill`] runs a manifest's
//! `history_backfill` command (NDJSON on stdout), and builtin native sources
//! (see [`builtin`]) read the harness's store directly.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::ops::ControlFlow;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use agent_hooks_protocol::{BackfillRecord, Capture};
use serde::Deserialize;

use crate::error::AdapterError;
use crate::manifest::HistoryBackfillSpec;

const DEFAULT_TIMEOUT_SECS: u64 = 3600;

/// Receives each observation of a run. `None` is a record that could not be
/// mapped (counted as skipped). `Break` stops the run early: the caller has
/// failed and discards the run's cursor.
pub type BackfillSink<'a> = dyn FnMut(Option<Capture>) -> ControlFlow<()> + 'a;

pub trait RecordSource: Send + Sync {
    /// Whether this source knows how to read the harness on `os`
    /// (`std::env::consts::OS` values). `history backfill` reports
    /// `unsupported os` instead of running otherwise.
    fn supports_os(&self, os: &str) -> bool;

    /// Replay observations newer than `since_ms` / `cursor` into `sink`.
    /// Returns the cursor to save for the next run, `None` to keep the old
    /// one. Observations already sunk stay stored when this fails.
    fn run(
        &self,
        home: &Path,
        since_ms: Option<u64>,
        cursor: Option<&str>,
        sink: &mut BackfillSink,
    ) -> Result<Option<String>, AdapterError>;
}

/// Native source of a builtin adapter id, if one ships. Add new ones here.
pub(crate) fn builtin(adapter_id: &str) -> Option<Box<dyn RecordSource>> {
    match adapter_id {
        "opencode" => Some(Box::new(crate::sources::opencode::OpencodeSource)),
        _ => None,
    }
}

/// A ready-to-spawn backfill command. `argv[0]` is the executable.
#[derive(Debug, Clone, PartialEq, Eq)]
struct BackfillCommand {
    argv: Vec<String>,
    timeout: Duration,
}

impl BackfillCommand {
    /// `{dir}` in any argv element becomes the manifest's folder and a
    /// relative executable path (`./run.sh`) is joined to it.
    fn resolve(spec: &HistoryBackfillSpec) -> Self {
        let mut argv = spec.command.clone();
        if let Some(dir) = &spec.dir {
            for arg in &mut argv {
                *arg = arg.replace("{dir}", &dir.to_string_lossy());
            }
            let exe = Path::new(&argv[0]);
            if exe.is_relative() && exe.components().count() > 1 {
                argv[0] = dir.join(exe).to_string_lossy().into_owned();
            }
        }
        Self {
            argv,
            timeout: Duration::from_secs(spec.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS)),
        }
    }
}

/// A manifest's `history_backfill` commands, one per OS, run for `harness`.
pub struct ScriptBackfill {
    harness: String,
    commands: BTreeMap<String, BackfillCommand>,
}

impl ScriptBackfill {
    pub(crate) fn new(harness: &str, specs: &BTreeMap<String, HistoryBackfillSpec>) -> Self {
        Self {
            harness: harness.to_string(),
            commands: specs
                .iter()
                .map(|(os, spec)| (os.clone(), BackfillCommand::resolve(spec)))
                .collect(),
        }
    }
}

/// One stdout line of a backfill command, or one input line of `history import`.
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum ScriptLine {
    Record(BackfillRecord),
    Cursor(String),
}

/// What a script line turned into.
pub enum ParsedLine {
    Capture(Capture),
    Cursor(String),
    /// Unparseable, or a record without a session or delivery id.
    Skipped,
}

/// Parse one NDJSON line of the script protocol for `harness`.
pub fn parse_line(harness: &str, line: &[u8]) -> ParsedLine {
    match serde_json::from_slice(line) {
        Ok(ScriptLine::Cursor(cursor)) => ParsedLine::Cursor(cursor),
        Ok(ScriptLine::Record(record)) => record
            .into_capture(harness)
            .map_or(ParsedLine::Skipped, ParsedLine::Capture),
        Err(_) => ParsedLine::Skipped,
    }
}

impl RecordSource for ScriptBackfill {
    fn supports_os(&self, os: &str) -> bool {
        self.commands.contains_key(os)
    }

    /// Run the command with the request on stdin and sink its NDJSON stdout.
    /// The last `cursor` line is returned only when the command exits 0.
    fn run(
        &self,
        home: &Path,
        since_ms: Option<u64>,
        cursor: Option<&str>,
        sink: &mut BackfillSink,
    ) -> Result<Option<String>, AdapterError> {
        let command = &self.commands[std::env::consts::OS];
        let request = serde_json::json!({"home": home, "since_ms": since_ms, "cursor": cursor});
        let mut child = Command::new(&command.argv[0])
            .args(&command.argv[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| AdapterError::Backfill(format!("cannot run {}: {e}", command.argv[0])))?;
        // A command that exits without reading its request is judged by its exit code.
        let _ = writeln!(child.stdin.take().expect("stdin is piped"), "{request}");
        let stdout = child.stdout.take().expect("stdout is piped");
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).split(b'\n').map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });

        let deadline = Instant::now() + command.timeout;
        let mut new_cursor = None;
        let mut stopped = None;
        loop {
            match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(line) => match parse_line(&self.harness, &line) {
                    ParsedLine::Cursor(c) => new_cursor = Some(c),
                    ParsedLine::Capture(c) => {
                        if sink(Some(c)).is_break() {
                            stopped = Some(None);
                            break;
                        }
                    }
                    ParsedLine::Skipped => {
                        if sink(None).is_break() {
                            stopped = Some(None);
                            break;
                        }
                    }
                },
                Err(RecvTimeoutError::Timeout) => {
                    stopped = Some(Some(format!("timed out after {:?}", command.timeout)));
                    break;
                }
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        if stopped.is_some() {
            let _ = child.kill();
        }
        let status = child.wait()?;
        match stopped {
            Some(None) => Ok(None),
            Some(Some(why)) => Err(AdapterError::Backfill(why)),
            None if status.success() => Ok(new_cursor),
            None => Err(AdapterError::Backfill(format!("command failed: {status}"))),
        }
    }
}
