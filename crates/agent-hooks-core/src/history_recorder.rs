//! Daemon-side persistence of *forwarded* hook observations.
//!
//! Local hooks are recorded by the `hook` shim itself, with no daemon. This
//! exists for events that reach the daemon from elsewhere (a remote machine's
//! shim over a forwarded socket) and so never touch this machine's store.
//! A failed write is logged without any captured content, counted, and never
//! changes a hook verdict.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use agent_hooks_history::{Capture, HistoryError, HistoryWriter};
use agent_hooks_protocol::AgentEvent;
use parking_lot::Mutex;
use tracing::{debug, error};

/// Lock-wait budget: local shims write to the same store concurrently.
const HISTORY_LOCK_WAIT: Duration = Duration::from_millis(200);

/// Handle to the history store. Cheap to share behind an `Arc`.
pub struct HistoryRecorder {
    writer: Arc<Mutex<HistoryWriter>>,
    failures: Arc<AtomicU64>,
}

impl HistoryRecorder {
    /// Open an already-initialized store. Fails before the daemon serves
    /// anything when the store is missing or unusable.
    pub fn open(dir: &Path) -> Result<Self, HistoryError> {
        let writer = HistoryWriter::open(dir)?;
        writer.set_busy_timeout(HISTORY_LOCK_WAIT)?;
        Ok(Self {
            writer: Arc::new(Mutex::new(writer)),
            failures: Arc::new(AtomicU64::new(0)),
        })
    }

    /// Persist `event` under `origin`, resolving when the commit lands so a
    /// subscriber notified after this returns always finds it in the store.
    /// SQLite runs on a blocking thread, never an executor thread, and the
    /// verdict never depends on the database. Events without a harness-native
    /// session id are skipped.
    pub async fn record(&self, event: &AgentEvent, origin: &str) {
        let Some(capture) = Capture::from_event(event, origin) else {
            debug!(
                harness = %event.harness,
                lifecycle = %event.lifecycle.name(),
                "history capture skipped: no native session id"
            );
            return;
        };
        let writer = self.writer.clone();
        let failures = self.failures.clone();
        // Await the blocking commit: callers broadcast only after this
        // returns, so remote-event visibility never races persistence.
        // Store errors carry SQLite/IO/schema facts only, never captured text.
        match tokio::task::spawn_blocking(move || writer.lock().record(&capture)).await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => {
                failures.fetch_add(1, Ordering::Relaxed);
                error!(error = %e, "history write failed");
            }
            Err(e) => {
                failures.fetch_add(1, Ordering::Relaxed);
                error!(error = %e, "history write task failed");
            }
        }
    }

    /// Writes that failed since startup (in-memory only).
    pub fn failure_count(&self) -> u64 {
        self.failures.load(Ordering::Relaxed)
    }
}
