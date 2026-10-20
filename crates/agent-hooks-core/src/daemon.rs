use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_hooks_adapters::{
    adapter_for_hint, all_adapters,
    manifest::{Capability, Verdict},
};
use agent_hooks_protocol::event::cap_chars;
use agent_hooks_protocol::rpc::event_passes_filter;
use agent_hooks_protocol::{
    AgentEvent, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, PromptDecision, RawHookInput,
    SubscriptionFilter,
};
use serde_json::Value;
#[cfg(unix)]
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::sync::{broadcast, RwLock};
use tracing::{debug, error, info, warn};

use crate::error::CoreError;
use crate::history_recorder::HistoryRecorder;
use crate::prompt_mediator::PromptMediator;
use crate::session_tracker::SessionTracker;

pub fn default_socket_path() -> PathBuf {
    PathBuf::from(agent_hooks_protocol::default_socket_path())
}

pub use agent_hooks_protocol::{
    DEFAULT_UNIX_SOCKET_PATH, DEFAULT_WINDOWS_PIPE_PATH, ENV_AGENT_HOOKS_SOCKET,
};
/// How long a hook that requires human input will block waiting for a decision.
/// After this the daemon replies with an `allow: false` timeout decision (or the
/// caller times out and falls back to allow). Kept generous for human latency.
const PROMPT_TIMEOUT: Duration = Duration::from_secs(300);

/// Capacity for the broadcast channel that fans events to subscribers.
const BROADCAST_CAPACITY: usize = 512;

/// Central daemon that owns the IPC listener, session registry, prompt mediator,
/// and event broadcast.
///
/// Handles both:
/// - one-shot hook connections (`RawHookInput` -> adapter parse -> track -> broadcast -> decision)
/// - persistent client subscriptions (JSON-RPC `subscribe` + `prompt.respond`)
pub struct AgentHooksDaemon {
    tracker: Arc<RwLock<SessionTracker>>,
    mediator: Arc<PromptMediator>,
    broadcaster: broadcast::Sender<AgentEvent>,
    enricher: Arc<dyn crate::enricher::EventEnricher>,
    /// Opt-in durable history of *forwarded* hook observations (events that
    /// carry an origin label); `None` unless the CLI enabled it explicitly.
    history: Option<Arc<HistoryRecorder>>,
}

impl Default for AgentHooksDaemon {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentHooksDaemon {
    pub fn new() -> Self {
        let (tx, _rx) = broadcast::channel(BROADCAST_CAPACITY);
        Self {
            tracker: Arc::new(RwLock::new(SessionTracker::new())),
            mediator: Arc::new(PromptMediator::new()),
            broadcaster: tx,
            enricher: Arc::new(crate::enricher::NoopEnricher),
            history: None,
        }
    }

    /// Create a daemon that enriches every event with the given callback
    /// before tracking/broadcasting. The enricher runs synchronously and
    /// must be cheap — see `EventEnricher` docs.
    pub fn with_enricher(enricher: Arc<dyn crate::enricher::EventEnricher>) -> Self {
        let (tx, _rx) = broadcast::channel(BROADCAST_CAPACITY);
        Self {
            tracker: Arc::new(RwLock::new(SessionTracker::new())),
            mediator: Arc::new(PromptMediator::new()),
            broadcaster: tx,
            enricher,
            history: None,
        }
    }

    pub fn with_components(
        tracker: Arc<RwLock<SessionTracker>>,
        mediator: Arc<PromptMediator>,
        broadcaster: broadcast::Sender<AgentEvent>,
    ) -> Self {
        Self {
            tracker,
            mediator,
            broadcaster,
            enricher: Arc::new(crate::enricher::NoopEnricher),
            history: None,
        }
    }

    /// Record forwarded hook observations (those carrying an origin label)
    /// through `history`. Local hooks are recorded by the shim itself. The
    /// recorder is opened by the caller so an unusable store fails before the
    /// daemon serves anything.
    pub fn with_history(mut self, history: Arc<HistoryRecorder>) -> Self {
        self.history = Some(history);
        self
    }

    /// Whether this daemon persists forwarded hook observations. Lets a
    /// supervisor detect a reused daemon started without `--history` (same
    /// version, no capture) via `status --json` / `ping`.
    pub fn history_enabled(&self) -> bool {
        self.history.is_some()
    }

    pub fn tracker(&self) -> &Arc<RwLock<SessionTracker>> {
        &self.tracker
    }

    pub fn mediator(&self) -> &Arc<PromptMediator> {
        &self.mediator
    }

    pub fn broadcaster(&self) -> &broadcast::Sender<AgentEvent> {
        &self.broadcaster
    }

    pub fn enricher_handle(&self) -> &Arc<dyn crate::enricher::EventEnricher> {
        &self.enricher
    }

    /// Run the daemon until the listener errors or the process receives a shutdown signal.
    /// This is the main entry point invoked by the CLI.
    pub async fn run(&self, socket_path: Option<PathBuf>) -> Result<(), CoreError> {
        let path = socket_path.unwrap_or_else(default_socket_path);
        self.run_on_path(&path).await
    }

    pub async fn run_on_path(&self, path: &Path) -> Result<(), CoreError> {
        #[cfg(unix)]
        {
            self.run_unix(path).await
        }
        #[cfg(windows)]
        {
            self.run_windows(path).await
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = path;
            Err(CoreError::Ipc("unsupported platform".to_string()))
        }
    }

    // -----------------------------------------------------------------------
    // Unix
    // -----------------------------------------------------------------------

    #[cfg(unix)]
    async fn run_unix(&self, path: &Path) -> Result<(), CoreError> {
        use tokio::net::UnixListener;

        // Single-owner: if a live daemon answers, do NOT steal its socket.
        // Only unlink when nothing is listening (stale file from crash/reboot).
        if path.exists() {
            if tokio::net::UnixStream::connect(path).await.is_ok() {
                return Err(CoreError::Ipc(format!(
                    "daemon already running on {}: refusing to steal live socket",
                    path.display()
                )));
            }
            let _ = std::fs::remove_file(path);
        }
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(CoreError::Io)?;
            }
        }

        let listener = UnixListener::bind(path)
            .map_err(|e| CoreError::Ipc(format!("bind {}: {}", path.display(), e)))?;
        info!(path = %path.display(), "daemon listening (unix)");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o777));
        }

        // Graceful shutdown signals (best-effort).
        #[cfg(unix)]
        let mut sigterm =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
        #[cfg(unix)]
        let mut sigint =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()).ok();

        loop {
            let accept = listener.accept();

            #[cfg(unix)]
            {
                tokio::select! {
                    res = accept => {
                        match res {
                            Ok((stream, _)) => {
                                let tracker = self.tracker.clone();
                                let mediator = self.mediator.clone();
                                let broadcaster = self.broadcaster.clone();
                                let enricher = self.enricher.clone();
                                let history = self.history.clone();
                                let conn_id = mediator.connect();
                                tokio::spawn(async move {
                                    if let Err(e) = handle_unix_connection(stream, tracker, mediator, broadcaster, enricher, history, conn_id).await {
                                        debug!(error = %e, "connection handler error");
                                    }
                                });
                            }
                            Err(e) => {
                                error!(error = %e, "accept failed");
                            }
                        }
                    }
                    _ = async {
                        if let Some(ref mut s) = sigterm { let _ = s.recv().await; } else { std::future::pending::<()>().await }
                    } => {
                        info!("received SIGTERM, shutting down");
                        break;
                    }
                    _ = async {
                        if let Some(ref mut s) = sigint { let _ = s.recv().await; } else { std::future::pending::<()>().await }
                    } => {
                        info!("received SIGINT, shutting down");
                        break;
                    }
                }
            }

            #[cfg(not(unix))]
            {
                match accept.await {
                    Ok((stream, _)) => {
                        let tracker = self.tracker.clone();
                        let mediator = self.mediator.clone();
                        let broadcaster = self.broadcaster.clone();
                        let enricher = self.enricher.clone();
                        let history = self.history.clone();
                        let conn_id = mediator.connect();
                        tokio::spawn(async move {
                            let _ = handle_unix_connection(
                                stream,
                                tracker,
                                mediator,
                                broadcaster,
                                enricher,
                                history,
                                conn_id,
                            )
                            .await;
                        });
                    }
                    Err(e) => error!(error = %e, "accept failed"),
                }
            }
        }

        let _ = std::fs::remove_file(path);
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Windows (named pipe via interprocess)
    // -----------------------------------------------------------------------

    #[cfg(windows)]
    async fn run_windows(&self, path: &Path) -> Result<(), CoreError> {
        use interprocess::local_socket::traits::tokio::Listener;
        use interprocess::local_socket::{GenericFilePath, ListenerOptions, ToFsName};

        let raw = path.to_string_lossy();
        let name = raw
            .to_fs_name::<GenericFilePath>()
            .map_err(|e| CoreError::Ipc(format!("invalid pipe name {}: {}", path.display(), e)))?;

        let listener = ListenerOptions::new()
            .name(name)
            .create_tokio()
            .map_err(|e| CoreError::Ipc(format!("bind {}: {}", path.display(), e)))?;

        info!(path = %path.display(), "daemon listening (windows pipe)");

        loop {
            let conn = listener.accept().await;
            match conn {
                Ok(stream) => {
                    let mediator = self.mediator.clone();
                    let tracker = self.tracker.clone();
                    let broadcaster = self.broadcaster.clone();
                    let enricher = self.enricher.clone();
                    let history = self.history.clone();
                    let conn_id = mediator.connect();
                    tokio::spawn(async move {
                        if let Err(e) = handle_interprocess_connection(
                            stream,
                            tracker,
                            mediator,
                            broadcaster,
                            enricher,
                            history,
                            conn_id,
                        )
                        .await
                        {
                            debug!(error = %e, "connection handler error");
                        }
                    });
                }
                Err(e) => {
                    error!(error = %e, "accept failed");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    }

    /// Handle a single hook synchronously (no IPC) — useful for tests
    /// and for in-process embeddings (e.g. Back2Vibing linking
    /// `agent-hooks-core` directly).
    pub async fn handle_raw_hook(
        &self,
        input: RawHookInput,
    ) -> Result<Option<PromptDecision>, CoreError> {
        let (mut event, capability, verdicts, budget) = parse_with_adapters(&input)?;
        // Preserve shim passthrough for client-side enrichment — daemon never interprets it.
        event.passthrough_env = input.passthrough_env.clone();
        self.enricher.enrich(&mut event, &input);
        // Sweep-on-insert: expire sessions idle past the TTL (bounded memory).
        // Holds on live prompts are protected while held.
        let held = self.mediator.pending_ids().await;
        {
            let mut tracker = self.tracker.write().await;
            tracker.track_protected(&event, &held);
        }
        // Open the hold (if any) BEFORE broadcasting, so the event subscribers
        // receive already carries it and is already answerable.
        let rx = open_hold(&self.mediator, &mut event, capability, verdicts, budget).await;

        // Internal broadcast keeps the full event (including raw_event) for
        // local-hook stdin and tracing. Subscriber-bound copies are stripped
        // at serialization (see subscriber_notification).
        let _ = self.broadcaster.send(event.clone());

        let decision = match rx {
            Some((rx, hold_for)) => {
                let pid = hold_prompt_id(&event);
                let interactive = event.is_interactive();
                debug!(prompt_id = %pid, "blocking hook waiting for decider verdict");
                Some(await_verdict(&self.mediator, pid, rx, hold_for, interactive).await)
            }
            None => None,
        };
        Ok(decision)
    }

    /// Broadcast helper for tests: returns a receiver.
    pub fn subscribe(&self) -> broadcast::Receiver<AgentEvent> {
        self.broadcaster.subscribe()
    }
}

/// Register the hold BEFORE the event is broadcast, and stamp it onto the
/// copy subscribers receive.
///
/// Order is load-bearing in both directions. Registering first closes the
/// race where a fast decider answers a `prompt_id` it saw in a broadcast the
/// daemon had not yet made pending — `respond` would find no entry, drop the
/// verdict, and leave the hook to burn the full timeout. Stamping makes the
/// hold self-describing, so "this event carries `hold`" and "a pending entry
/// exists" are the same fact rather than two that can disagree.
///
/// Returns the receiver to await, or `None` when nothing is held.
async fn open_hold(
    mediator: &Arc<PromptMediator>,
    event: &mut AgentEvent,
    capability: Option<Capability>,
    verdicts: Option<Vec<Verdict>>,
    harness_budget: Option<Duration>,
) -> Option<(tokio::sync::oneshot::Receiver<PromptDecision>, Duration)> {
    if !gate_hold(capability, mediator, event) {
        return None;
    }
    // Never hold longer than the harness will wait, less a margin so the
    // verdict lands in-band. A harness that declares no timeout falls back to
    // our own ceiling.
    let hold_for = harness_budget
        .map(|b| b.saturating_sub(HARNESS_MARGIN).max(Duration::from_secs(1)))
        .unwrap_or(PROMPT_TIMEOUT)
        .min(PROMPT_TIMEOUT);
    let prompt_id = hold_prompt_id(event);
    let rx = match mediator.register(prompt_id.clone(), hold_for).await {
        Ok(rx) => rx,
        Err(e) => {
            warn!(prompt_id = %prompt_id, error = %e, "could not register hold; passing through");
            return None;
        }
    };
    let (session_id, key, question, prompt) = prompt_info_from_event(event);
    mediator
        .note_info(
            &prompt_id,
            crate::prompt_mediator::PromptMeta {
                session_id,
                key,
                question,
                prompt,
                created_at: Instant::now(),
            },
        )
        .await;
    event.hold = Some(agent_hooks_protocol::PromptHold {
        prompt_id,
        deadline_ms: (chrono::Utc::now().timestamp_millis() as u64) + hold_for.as_millis() as u64,
        verdicts: verdicts
            .unwrap_or_else(|| vec![Verdict::Allow, Verdict::Deny, Verdict::Ask])
            .iter()
            .map(|v| {
                match v {
                    Verdict::Allow => "allow",
                    Verdict::Deny => "deny",
                    Verdict::Ask => "ask",
                }
                .to_string()
            })
            .collect(),
        interactive: event.is_interactive(),
    });
    Some((rx, hold_for))
}

/// Whether the daemon holds this event open for a decider verdict: gate
/// events only, and only while a decider claims this event. Interactivity is
/// the claim's call (`DecideScope::interactive_only`): plain `decide: true`
/// deciders only claim events a human is already waiting on, while an
/// explicit scope may name mid-flight gates like PreToolUse on purpose.
fn gate_hold(
    capability: Option<Capability>,
    mediator: &PromptMediator,
    event: &AgentEvent,
) -> bool {
    matches!(capability, Some(Capability::Gate)) && mediator.has_decider_for(event)
}

/// Rendezvous id deciders answer via `prompt.respond`: the broadcast
/// `event_id`, except PermissionRequest which keeps `prompt.prompt_id`.
/// Both are per-event ids (UUID event ids / per-request prompt ids), so two
/// harnesses reusing one session_id still get independent holds: answering
/// one never resolves the other.
fn hold_prompt_id(event: &AgentEvent) -> String {
    match &event.lifecycle {
        agent_hooks_protocol::AgentLifecycle::PermissionRequest { prompt } => {
            prompt.prompt_id.clone()
        }
        _ => event.event_id.clone(),
    }
}

/// Backlog metadata for a held gate: `(session_id, key, question)`.
/// Key mirrors the queue dedupe scope (tool for gates, lifecycle otherwise);
/// question is a truncated human-readable summary for `prompt.list`.
fn prompt_info_from_event(
    event: &AgentEvent,
) -> (
    String,
    String,
    Option<String>,
    Option<agent_hooks_protocol::PromptRequest>,
) {
    match &event.lifecycle {
        agent_hooks_protocol::AgentLifecycle::PreToolUse {
            tool_name,
            input,
            prompt,
            ..
        } => {
            let raw = serde_json::to_string(input).unwrap_or_default();
            (
                event.session_id.clone(),
                format!("tool:{tool_name}"),
                Some(cap_chars(&format!("{tool_name} {raw}"), 300)),
                prompt.clone(),
            )
        }
        agent_hooks_protocol::AgentLifecycle::PermissionRequest { prompt } => {
            let mut q = String::new();
            if let Some(t) = &prompt.tool_name {
                q.push_str(t);
                q.push(' ');
            }
            if let Some(c) = &prompt.command {
                q.push_str(c);
            } else if let Some(d) = &prompt.description {
                q.push_str(d);
            }
            for question in &prompt.questions {
                if !q.is_empty() {
                    q.push_str(" | ");
                }
                q.push_str(&question.question);
            }
            let key = prompt
                .tool_name
                .clone()
                .map(|t| format!("permission:{t}"))
                .unwrap_or_else(|| "permission".to_string());
            (
                event.session_id.clone(),
                key,
                Some(cap_chars(&q, 300)),
                Some(prompt.clone()),
            )
        }
        other => (
            event.session_id.clone(),
            other.name().to_string(),
            None,
            None,
        ),
    }
}

/// Wait for a decider verdict. Split failure mode: interactive holds
/// (PermissionRequest, PreToolUse with prompt) fail closed — timeout/orphan
/// denies, nobody approved. Non-interactive gates (plain PreToolUse, nobody
/// waiting) fail open — timeout/orphan allows, matching harness contract
/// where a silent hook lets the action proceed and the native dialog owns it.
/// Hold cleanup (pending-entry removal) runs exactly once inside
/// `wait_for_decision`; a late or duplicate answer afterwards finds no entry
/// and is ignored (`PromptNotFound`) — never double-resolved, never a panic.
///
/// The hold is registered and its backlog metadata recorded by `open_hold`
/// before the event is broadcast, so this only awaits the receiver.
async fn await_verdict(
    mediator: &Arc<PromptMediator>,
    prompt_id: String,
    rx: tokio::sync::oneshot::Receiver<PromptDecision>,
    hold_for: Duration,
    interactive: bool,
) -> PromptDecision {
    match mediator
        .await_registered(prompt_id.clone(), hold_for, rx)
        .await
    {
        Ok(d) => d,
        Err(CoreError::PromptTimeout(pid)) => {
            if interactive {
                warn!(prompt_id = %pid, "prompt timeout, denying");
                PromptDecision::deny(Some("timeout".to_string()))
            } else {
                warn!(prompt_id = %pid, "gate timeout with nobody waiting, deferring to harness");
                PromptDecision::defer()
            }
        }
        Err(e) => {
            if interactive {
                warn!(prompt_id = %prompt_id, error = %e, "orphaned hold, denying (fail-closed)");
                PromptDecision::deny(Some("timeout".to_string()))
            } else {
                warn!(prompt_id = %prompt_id, error = %e, "orphaned gate hold, deferring to harness");
                PromptDecision::defer()
            }
        }
    }
}

/// Subscriber-bound copy of an event: identical except `raw_event` is
/// stripped. The full raw payload may contain secrets — it stays populated on
/// the internal `AgentEvent` (tracker `last_event`, local-hook stdin,
/// tracing) but is NEVER serialized to subscribers: not on live broadcast,
/// not on replay/backlog, not on any snapshot.
fn subscriber_event(event: &AgentEvent) -> AgentEvent {
    let mut stripped = event.clone();
    stripped.raw_event = None;
    stripped
}

/// Build the `event` notification sent to subscribers (raw payload stripped).
fn subscriber_notification(event: &AgentEvent) -> JsonRpcNotification {
    JsonRpcNotification::event(subscriber_event(event))
}

/// Fail-closed orphan resolution: when the last decider goes away
/// (disconnect or `unsubscribe`), every still-pending hold resolves as deny
/// with the timeout reason instead of dangling until `PROMPT_TIMEOUT`.
/// `PromptMediator::respond` removes the pending entry before sending, so
/// each hold resolves exactly once: a concurrent or late answer finds no
/// entry and is ignored — never double-resolved, never a panic.
async fn deny_orphaned_holds(mediator: &Arc<PromptMediator>) {
    if mediator.has_deciders() {
        return;
    }
    let deny = PromptDecision::deny(Some("timeout".to_string()));
    for prompt_id in mediator.pending_ids().await {
        let _ = mediator.respond(&prompt_id, deny.clone()).await;
    }
}

/// Additive-only wire rule: a `schema_version` mismatch only warns — it never
/// drops the event or the connection. (Unknown fields are already ignored by
/// serde default on every inbound shape.)
fn warn_if_schema_mismatch(value: &Value) {
    if let Some(v) = value.get("schema_version").and_then(|v| v.as_u64()) {
        if v != u64::from(AgentEvent::SCHEMA_VERSION) {
            warn!(
                schema_version = v,
                expected = AgentEvent::SCHEMA_VERSION,
                "schema_version mismatch, continuing with best effort"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Connection handlers (unix)
// ---------------------------------------------------------------------------

#[cfg(unix)]
async fn handle_unix_connection(
    stream: tokio::net::UnixStream,
    tracker: Arc<RwLock<SessionTracker>>,
    mediator: Arc<PromptMediator>,
    broadcaster: broadcast::Sender<AgentEvent>,
    enricher: Arc<dyn crate::enricher::EventEnricher>,
    history: Option<Arc<HistoryRecorder>>,
    conn_id: u64,
) -> Result<(), CoreError> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    let (read_half, mut write_half) = stream.into_split();
    let _conn_guard = ConnGuard {
        mediator: mediator.clone(),
        conn_id,
    };
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();

    // For persistent subscriptions we keep the write_half open and stream events.
    // We need to hold it across the loop.
    let mut is_subscribed = false;
    let mut filter: Option<SubscriptionFilter> = None;
    // Broadcast receiver for this connection's subscription (created on subscribe).
    let mut sub_rx: Option<broadcast::Receiver<AgentEvent>> = None;

    loop {
        // If subscribed, we need to multiplex: wait for either a new line from client
        // OR a new broadcast event to forward.
        if is_subscribed {
            let Some(rx) = sub_rx.as_mut() else {
                is_subscribed = false;
                continue;
            };
            line.clear();
            tokio::select! {
                // New broadcast event
                ev = rx.recv() => {
                    match ev {
                        Ok(event) => {
                            if filter.as_ref().map_or(true, |f| event_passes_filter(&event, f)) {
                                let notif = subscriber_notification(&event);
                                let mut out = serde_json::to_string(&notif).unwrap_or_default();
                                out.push('\n');
                                if write_half.write_all(out.as_bytes()).await.is_err() {
                                    break;
                                }
                                let _ = write_half.flush().await;
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            warn!(skipped = n, "subscriber lagged");
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            break;
                        }
                    }
                    continue;
                }
                // New client line
                res = reader.read_line(&mut line) => {
                    match res {
                        Ok(0) => break, // client closed
                        Ok(_) => {
                            let trimmed = line.trim();
                            if trimmed.is_empty() { continue; }
                            // Client may send further RPCs (e.g., prompt.respond) on same conn.
                            let response = dispatch_rpc_line(trimmed, &tracker, &mediator, &broadcaster, &mut is_subscribed, &mut filter, &mut sub_rx, conn_id, history.is_some()).await;
                            if let Some(resp) = response {
                                let mut out = resp;
                                if !out.ends_with('\n') { out.push('\n'); }
                                if write_half.write_all(out.as_bytes()).await.is_err() { break; }
                                let _ = write_half.flush().await;
                            }
                            // If client unsubscribed or sent ping, loop continues.
                            continue;
                        }
                        Err(e) => {
                            debug!(error = %e, "read error on subscribed conn");
                            break;
                        }
                    }
                }
            }
        }

        // Not yet subscribed: blocking read for next request (one-shot or subscribe initiation).
        line.clear();
        let n = reader
            .read_line(&mut line)
            .await
            .map_err(|e| CoreError::Ipc(e.to_string()))?;
        if n == 0 {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Try to dispatch as RPC. If handled, continue loop (for persistent).
        // If it was a subscribe, the next iteration will enter the subscribed multiplex above.
        let rpc_handled = try_handle_as_rpc(
            trimmed,
            &tracker,
            &mediator,
            &broadcaster,
            &mut write_half,
            &mut is_subscribed,
            &mut filter,
            &mut sub_rx,
            conn_id,
            history.is_some(),
        )
        .await?;

        if rpc_handled {
            // If subscribe just started, next loop will multiplex.
            // Otherwise (ping, prompt.respond, etc.) we keep the connection open for more RPCs.
            // For unary RPCs the client typically closes after one response, but we allow persistence.
            if is_subscribed {
                continue;
            }
            // For non-subscribe RPCs, continue to allow pipelining; client may reuse connection.
            continue;
        }
        // Not an RPC — treat as hook.
        let hook_response = handle_hook_line(
            trimmed,
            &tracker,
            &mediator,
            &broadcaster,
            &enricher,
            &history,
        )
        .await;
        let out = hook_response.unwrap_or_else(|| r#"{"decision":"defer"}"#.to_string());
        let mut out_line = out;
        if !out_line.ends_with('\n') {
            out_line.push('\n');
        }
        write_half
            .write_all(out_line.as_bytes())
            .await
            .map_err(|e| CoreError::Ipc(e.to_string()))?;
        write_half
            .flush()
            .await
            .map_err(|e| CoreError::Ipc(e.to_string()))?;
        // One-shot hook: close after single response.
        break;
    }

    // Connection teardown: drop our decider flag and env request first so a
    // departing last decider fails its orphaned holds closed (deny with the
    // timeout reason) instead of leaving them dangling until the hold
    // timeout, and a departed subscriber stops contributing to the
    // env.requested union. Idempotent — ConnGuard repeats the disconnect on
    // drop.
    mediator.disconnect(conn_id);
    deny_orphaned_holds(&mediator).await;
    Ok(())
}

/// Removes the connection's decider flag on every exit path (including `?`).
struct ConnGuard {
    mediator: Arc<PromptMediator>,
    conn_id: u64,
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        self.mediator.disconnect(self.conn_id);
    }
}

#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
async fn try_handle_as_rpc(
    line: &str,
    tracker: &Arc<RwLock<SessionTracker>>,
    mediator: &Arc<PromptMediator>,
    broadcaster: &broadcast::Sender<AgentEvent>,
    write_half: &mut tokio::net::unix::OwnedWriteHalf,
    is_subscribed: &mut bool,
    filter: &mut Option<SubscriptionFilter>,
    sub_rx: &mut Option<broadcast::Receiver<AgentEvent>>,
    conn_id: u64,
    history_enabled: bool,
) -> Result<bool, CoreError> {
    let value: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return Ok(false),
    };
    warn_if_schema_mismatch(&value);
    // Must look like JSON-RPC (has jsonrpc or method)
    if value.get("method").is_none() && value.get("jsonrpc").is_none() {
        return Ok(false);
    }
    // Validate it parses as JsonRpcRequest before dispatching.
    if serde_json::from_value::<JsonRpcRequest>(value).is_err() {
        return Ok(false);
    }

    let response = dispatch_rpc_line(
        line,
        tracker,
        mediator,
        broadcaster,
        is_subscribed,
        filter,
        sub_rx,
        conn_id,
        history_enabled,
    )
    .await;
    if let Some(resp) = response {
        let mut out = resp;
        if !out.ends_with('\n') {
            out.push('\n');
        }
        write_half
            .write_all(out.as_bytes())
            .await
            .map_err(|e| CoreError::Ipc(e.to_string()))?;
        write_half
            .flush()
            .await
            .map_err(|e| CoreError::Ipc(e.to_string()))?;
    }
    Ok(true)
}

// ---------------------------------------------------------------------------
// Interprocess handler (windows)
// ---------------------------------------------------------------------------
#[cfg(windows)]
async fn handle_interprocess_connection(
    stream: interprocess::local_socket::tokio::Stream,
    tracker: Arc<RwLock<SessionTracker>>,
    mediator: Arc<PromptMediator>,
    broadcaster: broadcast::Sender<AgentEvent>,
    enricher: Arc<dyn crate::enricher::EventEnricher>,
    history: Option<Arc<HistoryRecorder>>,
    conn_id: u64,
) -> Result<(), CoreError> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    let _conn_guard = ConnGuard {
        mediator: mediator.clone(),
        conn_id,
    };
    let (read_half, mut write_half) = tokio::io::split(stream);
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    let mut is_subscribed = false;
    let mut filter: Option<SubscriptionFilter> = None;
    let mut sub_rx: Option<broadcast::Receiver<AgentEvent>> = None;

    loop {
        if is_subscribed {
            let Some(rx) = sub_rx.as_mut() else {
                is_subscribed = false;
                continue;
            };
            line.clear();
            tokio::select! {
                ev = rx.recv() => {
                    match ev {
                        Ok(event) => {
                            if filter.as_ref().map_or(true, |f| event_passes_filter(&event, f)) {
                                let notif = subscriber_notification(&event);
                                let mut out = serde_json::to_string(&notif).unwrap_or_default();
                                out.push('\n');
                                if write_half.write_all(out.as_bytes()).await.is_err() { break; }
                                let _ = write_half.flush().await;
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            warn!(skipped = n, "lagged");
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                    continue;
                }
                res = reader.read_line(&mut line) => {
                    match res {
                        Ok(0) => break,
                        Ok(_) => {
                            let trimmed = line.trim();
                            if trimmed.is_empty() { continue; }
                            let resp = dispatch_rpc_line(trimmed, &tracker, &mediator, &broadcaster, &mut is_subscribed, &mut filter, &mut sub_rx, conn_id, history.is_some()).await;
                            if let Some(r) = resp {
                                let mut out = r;
                                if !out.ends_with('\n') { out.push('\n'); }
                                if write_half.write_all(out.as_bytes()).await.is_err() { break; }
                                let _ = write_half.flush().await;
                            }
                            continue;
                        }
                        Err(e) => { debug!(error=%e, "read error"); break; }
                    }
                }
            }
        }

        line.clear();
        let n = reader
            .read_line(&mut line)
            .await
            .map_err(|e| CoreError::Ipc(e.to_string()))?;
        if n == 0 {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Try RPC first
        if let Ok(val) = serde_json::from_str::<Value>(trimmed) {
            if val.get("method").is_some() {
                let resp = dispatch_rpc_line(
                    trimmed,
                    &tracker,
                    &mediator,
                    &broadcaster,
                    &mut is_subscribed,
                    &mut filter,
                    &mut sub_rx,
                    conn_id,
                    history.is_some(),
                )
                .await;
                if let Some(r) = resp {
                    let mut out = r;
                    if !out.ends_with('\n') {
                        out.push('\n');
                    }
                    write_half
                        .write_all(out.as_bytes())
                        .await
                        .map_err(|e| CoreError::Ipc(e.to_string()))?;
                    write_half
                        .flush()
                        .await
                        .map_err(|e| CoreError::Ipc(e.to_string()))?;
                    if is_subscribed {
                        continue;
                    }
                    continue;
                }
            }
        }

        let hook_response = handle_hook_line(
            trimmed,
            &tracker,
            &mediator,
            &broadcaster,
            &enricher,
            &history,
        )
        .await;
        let out = hook_response.unwrap_or_else(|| r#"{"decision":"defer"}"#.to_string());
        let mut out_line = out;
        if !out_line.ends_with('\n') {
            out_line.push('\n');
        }
        write_half
            .write_all(out_line.as_bytes())
            .await
            .map_err(|e| CoreError::Ipc(e.to_string()))?;
        write_half
            .flush()
            .await
            .map_err(|e| CoreError::Ipc(e.to_string()))?;
        break;
    }
    // Connection teardown: drop our decider flag and env request first so a
    // departing last decider fails its orphaned holds closed (deny with the
    // timeout reason) instead of leaving them dangling until the hold
    // timeout, and a departed subscriber stops contributing to the
    // env.requested union. Idempotent — ConnGuard repeats the disconnect on
    // drop.
    mediator.disconnect(conn_id);
    deny_orphaned_holds(&mediator).await;
    Ok(())
}

// ---------------------------------------------------------------------------
// Shared handlers
// ---------------------------------------------------------------------------

async fn handle_hook_line(
    line: &str,
    tracker: &Arc<RwLock<SessionTracker>>,
    mediator: &Arc<PromptMediator>,
    broadcaster: &broadcast::Sender<AgentEvent>,
    enricher: &Arc<dyn crate::enricher::EventEnricher>,
    history: &Option<Arc<HistoryRecorder>>,
) -> Option<String> {
    // Additive-only wire rule: a schema_version mismatch only warns, never drops.
    let line_value = serde_json::from_str::<Value>(line).ok();
    if let Some(value) = line_value.as_ref() {
        warn_if_schema_mismatch(value);
    }
    // Shim-side extensions (`hook --context-cmd`) ride beside the RawHookInput
    // fields; only trusted when the line really is a hook envelope.
    let mut shim_extensions: Option<serde_json::Map<String, Value>> = None;
    // Try RawHookInput
    let input: Result<RawHookInput, _> = serde_json::from_str(line);
    let input = match input {
        Ok(v) => {
            shim_extensions = line_value
                .as_ref()
                .and_then(agent_hooks_protocol::hook_line_extensions)
                .cloned();
            v
        }
        Err(_) => {
            // Maybe the hook sent raw payload directly without envelope — wrap it.
            // Try to see if line itself is valid JSON with session_id etc.
            // For leniency, treat as raw_payload with generic harness hint.
            RawHookInput {
                harness_hint: "unknown".to_string(),
                event_hint: None,
                raw_payload: line.to_string(),
                context: Default::default(),
                request_id: None,
                passthrough_env: Default::default(),
            }
        }
    };
    let (mut event, capability, verdicts, budget) = match parse_with_adapters(&input) {
        Ok(pair) => pair,
        Err(e) => {
            warn!(error = %e, "failed to parse hook payload, deferring to harness");
            return Some(serde_json::json!({"decision":"defer"}).to_string());
        }
    };
    // Preserve shim passthrough for client-side enrichment — daemon never interprets it.
    event.passthrough_env = input.passthrough_env.clone();
    // Shim-side extensions merge key-wise before the enricher (see above).
    // The delivery id is internal and must not leak to subscribers. An origin
    // label marks a forwarded event: only those are recorded here, because a
    // local hook was already recorded by its own shim.
    let mut history_origin: Option<String> = None;
    if let Some(mut ext) = shim_extensions {
        if let Some(delivery_id) = agent_hooks_protocol::hook_line_delivery_id(&ext) {
            event
                .history_identity
                .get_or_insert_with(Default::default)
                .delivery_id = Some(delivery_id);
        }
        ext.remove(agent_hooks_protocol::DELIVERY_ID_EXTENSION);
        history_origin = ext
            .get(agent_hooks_protocol::ORIGIN_EXTENSION)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|o| !o.is_empty())
            .map(str::to_string);
        event.extensions.extend(ext);
    }
    // Consumer enrichment — Back2Vibing's terminal/mux targets, etc. Populates `extensions`.
    enricher.enrich(&mut event, &input);

    debug!(
        session = %event.session_id,
        harness = %event.harness,
        lifecycle = %event.lifecycle.name(),
        "hook event parsed"
    );

    // Sweep-on-insert: expire sessions idle past the TTL (bounded memory).
    // Holds on live prompts are protected while held.
    let held = mediator.pending_ids().await;
    {
        let mut t = tracker.write().await;
        // If event cwd is missing or points to an internal harness config dir (e.g. .gemini),
        // but the tracker already knows the real workspace cwd for this session, backfill it!
        // The lookup is composite (harness, session_id): same id on another harness is a
        // different session and must never donate state here.
        if let Some(existing_sess) = t.get(&event.harness, &event.session_id) {
            let is_config_dir = event
                .context
                .cwd
                .as_ref()
                .map(|p| p.to_string_lossy().contains(".gemini"))
                .unwrap_or(false);
            if event.context.cwd.is_none() || is_config_dir {
                if let Some(known_cwd) = existing_sess
                    .last_event
                    .as_ref()
                    .and_then(|e| e.context.cwd.clone())
                {
                    event.context.cwd = Some(known_cwd);
                    if event.context.git_worktree.is_none() {
                        event.context.git_worktree = existing_sess
                            .last_event
                            .as_ref()
                            .and_then(|e| e.context.git_worktree.clone());
                    }
                }
            }
        }
        t.track_protected(&event, &held);
    }

    // Persisted off the executor, awaited before the broadcast below so a
    // subscriber reacting to this event always finds it committed; the
    // verdict never depends on the database.
    if let (Some(history), Some(origin)) = (history, &history_origin) {
        history.record(&event, origin).await;
    }
    // Hold gate events for a decider verdict while one claims them. Opened
    // before the broadcast so the delivered event carries its own `hold`:
    // the decider is told it is being asked and which id to answer, instead
    // of re-deriving both from capability tables and id conventions.
    // Nothing claimed → no hold → the harness shows its native dialog.
    let rx = open_hold(mediator, &mut event, capability, verdicts, budget).await;

    // Internal broadcast keeps the full event (including raw_event) for
    // local-hook stdin and tracing. Subscriber-bound copies are stripped
    // at serialization (see subscriber_notification).
    let _ = broadcaster.send(event.clone());

    let response = if let Some((rx, hold_for)) = rx {
        let prompt_id = hold_prompt_id(&event);
        let interactive = event.is_interactive();
        info!(prompt_id = %prompt_id, "awaiting decider verdict");
        let decision = await_verdict(mediator, prompt_id, rx, hold_for, interactive).await;
        serde_json::json!({
            "decision": if decision.defer { "defer" } else if decision.ask { "ask" } else if decision.allow { "allow" } else { "deny" },
            "allow": decision.allow,
            "ask": decision.ask,
            // Hosts render "allow always" differently from a one-off allow
            // (Claude/Codex updatedPermissions, opencode "always"); the shim
            // can only do that if the verdict line carries it.
            "allow_always": decision.allow_always,
            "suggestion": decision.suggestion,
            "custom_input": decision.custom_input,
            "answers": decision.answers,
        })
    } else {
        // Nobody held this event: defer, so the harness proceeds as if no hook
        // were installed (native dialog on gates). An allow here would approve
        // every unclaimed gate — on Claude Code PreToolUse that bypasses its
        // permission checks. The shim renders defer in the harness dialect.
        serde_json::json!({"decision":"defer"})
    };
    Some(response.to_string())
}

async fn dispatch_rpc_line(
    line: &str,
    tracker: &Arc<RwLock<SessionTracker>>,
    mediator: &Arc<PromptMediator>,
    broadcaster: &broadcast::Sender<AgentEvent>,
    is_subscribed: &mut bool,
    filter: &mut Option<SubscriptionFilter>,
    sub_rx: &mut Option<broadcast::Receiver<AgentEvent>>,
    conn_id: u64,
    history_enabled: bool,
) -> Option<String> {
    // Additive-only wire rule: unknown fields are ignored by serde default on
    // every inbound shape; a schema_version mismatch only warns, never drops.
    if let Ok(value) = serde_json::from_str::<Value>(line) {
        warn_if_schema_mismatch(&value);
    }
    let req: Result<JsonRpcRequest, _> = serde_json::from_str(line);
    let req = match req {
        Ok(r) => r,
        Err(_) => {
            // Maybe it's a notification without id — ignore.
            return None;
        }
    };

    let id = req.id.clone();
    match req.method.as_str() {
        "ping" => {
            // Additive-only: `history_enabled` lets a supervisor detect a
            // reused daemon started without `--history`. Old clients ignore
            // unknown fields; old daemons simply omit it (read as false).
            let resp = JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id,
                result: Some(serde_json::json!({
                    "status": "ok",
                    "version": env!("CARGO_PKG_VERSION"),
                    "history_enabled": history_enabled,
                })),
                error: None,
            };
            Some(serde_json::to_string(&resp).unwrap_or_default())
        }
        "subscribe" => {
            // Parse filter from params if present.
            let f: Option<SubscriptionFilter> = req
                .params
                .as_ref()
                .and_then(|v| serde_json::from_value(v.clone()).ok());

            // `decide` is a declaration of what this subscriber will answer,
            // resolved against the subscription itself: `true` inherits the
            // subscription's own filter, since you cannot answer what you were
            // never sent. Unscoped events are never held for this connection.
            mediator.set_decide(conn_id, f.as_ref().and_then(|f| f.decide.resolve(f)));
            // Connection-scoped env request: a re-subscribe on this same
            // connection replaces the previous entry (see
            // `PromptMediator::set_env_requested`). Invalid names are
            // excluded by the shared normalize rule, matching the union.
            mediator.set_env_requested(
                conn_id,
                f.as_ref()
                    .map(|f| f.passthrough_env.clone())
                    .unwrap_or_default(),
            );
            *filter = f.clone();
            *sub_rx = Some(broadcaster.subscribe());
            *is_subscribed = true;
            // Ack
            let resp = JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id,
                result: Some(serde_json::json!({"subscribed": true})),
                error: None,
            };
            Some(serde_json::to_string(&resp).unwrap_or_default())
        }
        "prompt.respond" | "respond_prompt" | "prompt_respond" => {
            let params_val = req.params.clone().unwrap_or(Value::Null);
            let parsed: Result<agent_hooks_protocol::PromptRespondParams, _> =
                serde_json::from_value(params_val.clone());
            // Also support flat {prompt_id, decision} or {prompt_id, allow}
            let outcome = if let Ok(p) = parsed {
                mediator.respond(&p.prompt_id, p.decision).await
            } else if let Ok(v) = serde_json::from_value::<Value>(params_val.clone()) {
                // Try alternative shapes: {prompt_id, allow, ...}
                if let (Some(pid), Some(allow)) = (
                    v.get("prompt_id").and_then(|x| x.as_str()),
                    v.get("decision"),
                ) {
                    let decision: Result<PromptDecision, _> = serde_json::from_value(allow.clone());
                    if let Ok(d) = decision {
                        mediator.respond(pid, d).await
                    } else {
                        Err(CoreError::ConfigParse("invalid decision".to_string()))
                    }
                } else {
                    Err(CoreError::ConfigParse(
                        "invalid prompt.respond params".to_string(),
                    ))
                }
            } else {
                Err(CoreError::ConfigParse("invalid params".to_string()))
            };

            match outcome {
                Ok(()) => {
                    let resp = JsonRpcResponse {
                        jsonrpc: "2.0".to_string(),
                        id,
                        result: Some(serde_json::json!({"ok": true})),
                        error: None,
                    };
                    Some(serde_json::to_string(&resp).unwrap_or_default())
                }
                Err(e) => {
                    let resp = JsonRpcResponse {
                        jsonrpc: "2.0".to_string(),
                        id,
                        result: None,
                        error: Some(agent_hooks_protocol::rpc::JsonRpcError {
                            code: -32000,
                            message: e.to_string(),
                            data: None,
                        }),
                    };
                    Some(serde_json::to_string(&resp).unwrap_or_default())
                }
            }
        }
        "sessions.list" | "list_sessions" | "sessions_list" => {
            // Sweep-on-list: expire sessions idle past the TTL (bounded
            // memory). Holds on live prompts are protected while held.
            let held = mediator.pending_ids().await;
            let sessions = {
                let mut t = tracker.write().await;
                t.evict_idle_expired(SessionTracker::now_ms(), &held);
                t.all_sessions()
                    .into_iter()
                    .map(|s| {
                        serde_json::json!({
                            "session_id": s.session_id,
                            "harness": s.harness,
                            "status": s.status,
                            "event_count": s.event_count,
                            "last_event_at_ms": s.updated_at_ms,
                            "parent_session_id": s.parent_session_id,
                            "cwd": s.last_event.as_ref().and_then(|e| e.context.cwd.as_ref()).map(|p| p.display().to_string()),
                        })
                    })
                    .collect::<Vec<_>>()
            };
            let resp = JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id,
                result: Some(serde_json::json!({"sessions": sessions})),
                error: None,
            };
            Some(serde_json::to_string(&resp).unwrap_or_default())
        }
        "prompt.list" | "prompts.list" | "list_prompts" => {
            // Backlog for the `answer` CLI: every held question with text,
            // waiter count, parked depth, and age. Oldest first.
            let prompts = mediator.pending_prompts().await;
            let resp = JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id,
                result: Some(serde_json::json!({"prompts": prompts})),
                error: None,
            };
            Some(serde_json::to_string(&resp).unwrap_or_default())
        }
        "env.requested" => {
            // Active union of valid requested names across all connected
            // subscribers. Sorted, de-duplicated; empty when nobody asked.
            let names = mediator.env_requested_union();
            let resp = JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id,
                result: Some(
                    serde_json::to_value(agent_hooks_protocol::EnvRequestedResponse {
                        passthrough_env: names,
                    })
                    .unwrap_or(serde_json::json!({"passthrough_env": []})),
                ),
                error: None,
            };
            Some(serde_json::to_string(&resp).unwrap_or_default())
        }
        "unsubscribe" => {
            mediator.set_decide(conn_id, None);
            mediator.clear_env_requested(conn_id);
            // A departing decider fails its orphaned holds closed instead of
            // leaving them dangling until the hold timeout.
            deny_orphaned_holds(mediator).await;
            *is_subscribed = false;
            *sub_rx = None;
            let resp = JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id,
                result: Some(serde_json::json!({"unsubscribed": true})),
                error: None,
            };
            Some(serde_json::to_string(&resp).unwrap_or_default())
        }
        other => {
            let resp = JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id,
                result: None,
                error: Some(agent_hooks_protocol::rpc::JsonRpcError {
                    code: -32601,
                    message: format!("method not found: {}", other),
                    data: None,
                }),
            };
            Some(serde_json::to_string(&resp).unwrap_or_default())
        }
    }
}

/// A normalized event plus what its manifest says about it: the capability
/// that decides whether it may be held, and the verdicts the harness accepts.
type ParsedEvent = (
    AgentEvent,
    Option<Capability>,
    Option<Vec<Verdict>>,
    Option<Duration>,
);

/// Head start the daemon keeps over the harness's own hook timeout.
///
/// The hold must resolve BEFORE the harness gives up, or the fail-closed deny
/// is discarded along with the rest of our output and the harness applies its
/// own semantics — for `UserPromptSubmit` that means the prompt goes through,
/// which is the exact opposite of the verdict we were holding for. Answering
/// early costs one deny; answering late loses it entirely.
const HARNESS_MARGIN: Duration = Duration::from_secs(5);

fn parse_with_adapters(input: &RawHookInput) -> Result<ParsedEvent, CoreError> {
    // Prefer hinted adapter, fallback to trying all. The capability comes
    // from the same adapter instance that parsed (stateful last_* tracking).
    // An adapter's `ignore_when` verdict is final: no fallback adapter and no
    // synthetic event, so ignored payloads never reach subscribers.
    use agent_hooks_adapters::error::AdapterError;
    if let Some(adapter) = adapter_for_hint(&input.harness_hint) {
        match adapter.parse(input) {
            Ok(ev) => {
                let capability = adapter.capability_for(&ev);
                let verdicts = adapter.verdicts_for(&ev);
                let budget = adapter.entry_timeout_for(&ev);
                return Ok((ev, capability, verdicts, budget));
            }
            Err(e @ AdapterError::Ignored(_)) => return Err(e.into()),
            Err(_) => {}
        }
    }
    for adapter in all_adapters() {
        match adapter.parse(input) {
            Ok(ev) => {
                let capability = adapter.capability_for(&ev);
                let verdicts = adapter.verdicts_for(&ev);
                let budget = adapter.entry_timeout_for(&ev);
                return Ok((ev, capability, verdicts, budget));
            }
            Err(e @ AdapterError::Ignored(_)) => return Err(e.into()),
            Err(_) => {}
        }
    }
    // Last resort: minimal synthetic event so the hook never blocks forever.
    // Create a Notification lifecycle, but extract session_id and workspace cwd if present in payload.
    let parsed_json: Option<serde_json::Value> = serde_json::from_str(&input.raw_payload).ok();
    let session_id = parsed_json
        .as_ref()
        .and_then(|v| {
            v.get("conversationId")
                .or_else(|| v.get("conversation_id"))
                .or_else(|| v.get("sessionId"))
                .or_else(|| v.get("session_id"))
                .and_then(|s| s.as_str())
                .map(|s| s.to_string())
        })
        .or_else(|| input.context.pid.map(|p| format!("session-{}", p)))
        .unwrap_or_else(|| "unknown-session".to_string());

    let mut context = input.context.clone();
    if let Some(v) = &parsed_json {
        let ws_path = v
            .get("workspacePaths")
            .or_else(|| v.get("workspace_paths"))
            .and_then(|arr| arr.as_array())
            .and_then(|arr| arr.first())
            .and_then(|s| s.as_str())
            .or_else(|| v.get("cwd").and_then(|s| s.as_str()));
        if let Some(s) = ws_path {
            let cleaned = s.strip_prefix("file://").unwrap_or(s);
            if !cleaned.is_empty() && !cleaned.contains(".gemini") {
                let p = std::path::PathBuf::from(cleaned);
                context.git_worktree = Some(p.clone());
                context.cwd = Some(p);
            }
        }
    }

    // Canonicalize the stored harness: aliases (`oh-my-pi`, `omp`, ...) resolve
    // to the adapter's canonical id, so fallback rows merge with parsed
    // rows instead of splitting sessions per alias.
    let harness = adapter_for_hint(&input.harness_hint)
        .map(|a| a.id().to_string())
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| input.harness_hint.clone());
    let ev = AgentEvent::new(
        harness,
        session_id,
        agent_hooks_protocol::AgentLifecycle::Notification {
            message: input.raw_payload.clone(),
            title: None,
            notification_type: None,
        },
        context,
    );
    Ok((ev, None, None, None))
}
