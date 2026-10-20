use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use std::time::Duration;

use agent_hooks_protocol::rpc::event_passes_filter;
use agent_hooks_protocol::{
    AgentEvent, Decide, DecideScope, NormalizedStatus, PromptDecision, SubscriptionFilter,
};
use futures::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use crate::error::ClientError;

// ---------------------------------------------------------------------------
// socket path helpers
// ---------------------------------------------------------------------------

/// Return the default IPC path for this platform, checking `AGENT_HOOKS_SOCKET` first.
pub fn default_socket_path() -> String {
    agent_hooks_protocol::default_socket_path()
}

pub use agent_hooks_protocol::{
    Decide as DecideDecl, DecideScope as DeciderScope, PromptHold, DEFAULT_UNIX_SOCKET_PATH,
    DEFAULT_WINDOWS_PIPE_PATH, ENV_AGENT_HOOKS_SOCKET,
};

// ---------------------------------------------------------------------------
// SessionSummary
// ---------------------------------------------------------------------------

/// Summary of a single agent session as returned by `list_sessions`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionSummary {
    pub session_id: String,
    pub harness: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_version: Option<String>,

    pub status: NormalizedStatus,

    #[serde(default)]
    pub event_count: u64,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_event_at_ms: Option<u64>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// Resilient async client for the `agent-hooks` daemon.
///
/// The client communicates over a Unix Domain Socket (`/tmp/agent-hooks.sock`)
/// on Unix/macOS and a Named Pipe (`\\.\pipe\agent-hooks`) on Windows.
/// Wire format is JSON-RPC 2.0 with an NDJSON line-delimited fallback for
/// event streams.
///
/// All public methods are zero-panic: they return `Result` or log-and-continue
/// for streaming paths. Transient I/O failures are retried with exponential
/// back-off.
#[derive(Debug, Clone)]
pub struct AgentHooksClient {
    path: String,
    next_id: Arc<AtomicU64>,
}

impl AgentHooksClient {
    // -- constructors ---------------------------------------------------------

    /// Connect to the daemon at the platform-default socket path.
    ///
    /// Performs a single probe connection to validate reachability; the
    /// resulting handle re-connects on demand for each RPC (and therefore
    /// survives daemon restarts).
    pub async fn connect() -> Result<Self, ClientError> {
        Self::connect_to(default_socket_path()).await
    }

    /// Connect to the daemon at a custom socket path.
    ///
    /// `path` is a filesystem path on Unix and a named-pipe path
    /// (`\\.\pipe\...`) on Windows.
    pub async fn connect_to(path: impl Into<String>) -> Result<Self, ClientError> {
        let path = path.into();
        // Probe that the name is valid and the daemon is reachable.  If the
        // daemon is not yet listening we still return a handle — the error is
        // surfaced only after retries are exhausted — but we do validate that
        // the path can be converted to an IPC name.
        if let Err(e) = validate_socket_name(&path) {
            return Err(ClientError::Connection(e));
        }
        // Best-effort probe: try to connect once with a short timeout.  Failure
        // is not fatal — the client will retry on the first real RPC — but a
        // successful probe helps fail-fast in tests.
        let _ = tokio::time::timeout(Duration::from_millis(200), connect_stream(&path)).await;
        Ok(Self {
            path,
            next_id: Arc::new(AtomicU64::new(1)),
        })
    }

    /// Create a client without probing the socket.  Useful when the caller
    /// knows the daemon will appear shortly.
    pub fn new(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            next_id: Arc::new(AtomicU64::new(1)),
        }
    }

    /// Return the socket path this client targets.
    pub fn path(&self) -> &str {
        &self.path
    }

    fn next_request_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    // -- unary RPCs -----------------------------------------------------------

    /// Version the live daemon reports in its `ping` reply (`{"version": ..}`).
    /// `Ok(None)` when it answers without one (older daemons). Lets a
    /// supervisor replace a daemon left running by an older build.
    pub async fn daemon_version(&self) -> Result<Option<String>, ClientError> {
        let v = self.rpc_call("ping", None).await?;
        Ok(v.get("version")
            .and_then(|v| v.as_str())
            .map(str::to_string))
    }

    /// Check that the daemon is reachable.  Returns `true` on a successful
    /// `ping`/`pong` round-trip.
    pub async fn ping(&self) -> Result<bool, ClientError> {
        let v = self.rpc_call("ping", None).await?;
        // Server may return `true`, `"pong"`, `{"pong": true}` or `null` on
        // success — accept any truthy / pong-like value.
        if v.is_null() {
            return Ok(true);
        }
        if let Some(b) = v.as_bool() {
            return Ok(b);
        }
        if let Some(s) = v.as_str() {
            return Ok(s.eq_ignore_ascii_case("pong") || s.eq_ignore_ascii_case("ok"));
        }
        if let Some(obj) = v.as_object() {
            if obj.contains_key("pong") || obj.contains_key("ok") {
                return Ok(true);
            }
        }
        // Non-null, non-error response counts as alive.
        Ok(true)
    }

    /// List active / recent sessions known to the daemon.
    pub async fn list_sessions(&self) -> Result<Vec<SessionSummary>, ClientError> {
        let v = self
            .rpc_call_with_fallback("sessions.list", "list_sessions", None)
            .await?;
        if v.is_null() {
            return Ok(Vec::new());
        }
        // Server may wrap the list as `{ "sessions": [...] }`
        let arr = if let Some(obj) = v.as_object() {
            if let Some(sessions) = obj.get("sessions") {
                sessions.clone()
            } else if let Some(data) = obj.get("data") {
                data.clone()
            } else {
                v.clone()
            }
        } else {
            v.clone()
        };
        let sessions: Vec<SessionSummary> = serde_json::from_value(arr).map_err(|e| {
            ClientError::InvalidResponse(format!("failed to parse SessionSummary list: {e}"))
        })?;
        Ok(sessions)
    }

    /// List currently held questions (backlog for the `answer` CLI).
    pub async fn list_pending_prompts(
        &self,
    ) -> Result<Vec<agent_hooks_protocol::PendingPromptInfo>, ClientError> {
        let v = self
            .rpc_call_with_fallback("prompt.list", "list_prompts", None)
            .await?;
        if v.is_null() {
            return Ok(Vec::new());
        }
        let arr = if let Some(obj) = v.as_object() {
            if let Some(prompts) = obj.get("prompts") {
                prompts.clone()
            } else {
                v.clone()
            }
        } else {
            v.clone()
        };
        let prompts: Vec<agent_hooks_protocol::PendingPromptInfo> = serde_json::from_value(arr)
            .map_err(|e| {
                ClientError::InvalidResponse(format!("failed to parse PendingPromptInfo list: {e}"))
            })?;
        Ok(prompts)
    }

    /// Active union of env var names requested by connected subscribers.
    ///
    /// Returns the sorted, deduplicated union of valid subscription requests.
    /// These names are requests, not permission to capture environment values.
    pub async fn requested_env_names(&self) -> Result<Vec<String>, ClientError> {
        let value = self.rpc_call("env.requested", None).await?;
        let response: agent_hooks_protocol::EnvRequestedResponse = serde_json::from_value(value)
            .map_err(|e| {
                ClientError::InvalidResponse(format!("failed to parse env requests: {e}"))
            })?;
        Ok(agent_hooks_protocol::normalize_env_names(
            &response.passthrough_env,
        ))
    }

    /// Resolve a pending interactive prompt.
    pub async fn respond_prompt(
        &self,
        prompt_id: &str,
        decision: PromptDecision,
    ) -> Result<(), ClientError> {
        if prompt_id.is_empty() {
            return Err(ClientError::InvalidResponse(
                "prompt_id must not be empty".to_string(),
            ));
        }
        let params = serde_json::to_value(serde_json::json!({
            "prompt_id": prompt_id,
            "decision": decision,
        }))
        .map_err(ClientError::Serialization)?;
        // Try primary method first, fall back to legacy naming.
        let res = self
            .rpc_call_with_fallback("prompt.respond", "respond_prompt", Some(params))
            .await;
        match res {
            Ok(_) => Ok(()),
            Err(ClientError::Rpc { code, message }) => Err(ClientError::Rpc { code, message }),
            Err(e) => Err(e),
        }
    }

    /// Send a raw hook input directly to the daemon (used by hook shims or test harnesses).
    pub async fn send_raw_hook(
        &self,
        input: &agent_hooks_protocol::RawHookInput,
    ) -> Result<Option<String>, ClientError> {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let mut stream = connect_stream(&self.path).await?;
        let mut line = agent_hooks_protocol::hook_line_with_extensions(input, &Default::default())
            .map_err(ClientError::Serialization)?;
        line.push('\n');

        stream
            .write_all(line.as_bytes())
            .await
            .map_err(ClientError::Io)?;
        stream.flush().await.map_err(ClientError::Io)?;

        let mut reader = BufReader::new(stream);
        let mut buf = String::new();
        let n = reader.read_line(&mut buf).await.map_err(ClientError::Io)?;
        if n == 0 {
            Ok(None)
        } else {
            Ok(Some(buf.trim().to_string()))
        }
    }

    // -- subscription --------------------------------------------------------

    /// Subscribe to the daemon's event stream.
    ///
    /// Returns a `Stream<Item = AgentEvent>` that resiliently reconnects
    /// on disconnect with exponential back-off.  The optional `filter` is sent
    /// to the server (if supported) and also applied client-side as a
    /// fallback.
    ///
    /// The stream is lazy: the background task is spawned on call and the
    /// first poll will trigger a connection attempt.  Dropping the stream
    /// terminates the background task.
    pub fn subscribe(
        &self,
        filter: Option<SubscriptionFilter>,
    ) -> impl Stream<Item = AgentEvent> + Send + 'static {
        let path = self.path.clone();
        let (tx, rx) = mpsc::channel::<AgentEvent>(256);
        let filter_for_task = filter.clone();

        tokio::spawn(async move {
            let mut backoff = Duration::from_millis(200);
            const MAX_BACKOFF: Duration = Duration::from_secs(5);

            loop {
                // If receiver was dropped, stop the task.
                if tx.is_closed() {
                    break;
                }

                match subscribe_once(&path, filter_for_task.clone(), &tx).await {
                    Ok(()) => {
                        tracing::debug!("event stream ended cleanly, reconnecting");
                    }
                    Err(ClientError::InvalidResponse(_)) => {
                        // Parse-level errors are not retryable at transport level
                        // but the stream itself should still reconnect.
                        tracing::warn!("event stream parse error, reconnecting");
                    }
                    Err(e) => {
                        tracing::warn!(
                            "event stream disconnected: {e}, reconnecting in {backoff:?}"
                        );
                    }
                }

                if tx.is_closed() {
                    break;
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
                // Jitter could be added here; simple capped exponential is
                // sufficient for resilience without panics.
            }
        });

        tokio_stream::wrappers::ReceiverStream::new(rx)
    }

    /// Subscribe with client-side enrichment. The closure is applied to every
    /// event **before** it is delivered to the stream — ideal for
    /// terminal/mux resolution that belongs to the consumer, not the daemon.
    ///
    /// The daemon is still `NoopEnricher`; enrichment runs in the consumer
    /// process and can `spawn_blocking` or shell out without blocking the
    /// hook hot path. `event.passthrough_env` + `event.context` are the
    /// inputs; write results into `event.extensions` (e.g. `b2v.terminal_target`).
    ///
    /// Example:
    /// ```ignore
    /// let stream = client.subscribe_enriched(None, |ev| {
    ///     if let Some(t) = my_resolve_terminal(&ev.context, &ev.passthrough_env) {
    ///         ev.extensions.insert("my.terminal".into(), json!(t));
    ///     }
    /// });
    /// ```
    pub fn subscribe_enriched<F>(
        &self,
        filter: Option<SubscriptionFilter>,
        enrich: F,
    ) -> impl Stream<Item = AgentEvent> + Send + 'static
    where
        F: Fn(&mut AgentEvent) + Send + Sync + 'static,
    {
        self.subscribe(filter).map(move |mut ev| {
            enrich(&mut ev);
            ev
        })
    }

    /// Convenience: subscribe with a harness filter.
    pub fn subscribe_harness(
        &self,
        harness: impl Into<String>,
    ) -> impl Stream<Item = AgentEvent> + Send + 'static {
        let filter = SubscriptionFilter {
            harnesses: vec![harness.into()],
            ..Default::default()
        };
        self.subscribe(Some(filter))
    }

    /// Subscribe as a verdict decider for the events the harness is ALREADY
    /// blocked on: its permission requests and its questions.
    ///
    /// This never makes an agent wait that was not already waiting. Those
    /// events had stopped for a human regardless; all this changes is that
    /// anyone connected here may answer, rather than only the person at the
    /// terminal. Mid-flight events — a tool about to run, a prompt being
    /// submitted — pass straight through untouched.
    ///
    /// Answer with [`respond_prompt`](Self::respond_prompt), passing the
    /// `prompt_id` from the event's [`hold`](agent_hooks_protocol::PromptHold).
    /// Every held event carries one; events without a `hold` are observations
    /// and must not be answered.
    ///
    /// To claim mid-flight events too, use [`subscribe_decider_for`] and name
    /// them — that introduces a stall, so it is never inherited.
    ///
    /// [`subscribe_decider_for`]: Self::subscribe_decider_for
    pub fn subscribe_decider(&self) -> impl Stream<Item = AgentEvent> + Send + 'static {
        let filter = SubscriptionFilter {
            decide: Decide::Enabled(true),
            ..Default::default()
        };
        self.subscribe(Some(filter))
    }

    /// Subscribe as a verdict decider for a declared slice of gate events.
    ///
    /// Events outside `scope` are never held for this connection: they pass
    /// through at daemon speed, exactly as if you were not connected.
    ///
    /// Unlike [`subscribe_decider`](Self::subscribe_decider), an explicit
    /// scope may claim mid-flight events, where nothing was waiting until you
    /// said so. Every event you claim there is latency you are adding to the
    /// agent, so claim narrowly and answer fast — or set
    /// [`interactive_only`](agent_hooks_protocol::DecideScope::interactive_only)
    /// and restrict yourself to the moments it had already stopped.
    ///
    /// ```no_run
    /// # use agent_hooks_client::AgentHooksClient;
    /// # use agent_hooks_protocol::DecideScope;
    /// # async fn f(client: AgentHooksClient) {
    /// // Gate shell commands; leave prompt submission alone.
    /// let events = client.subscribe_decider_for(DecideScope {
    ///     event_types: vec!["PreToolUse".into()],
    ///     tools: vec!["Bash".into()],
    ///     ..Default::default()
    /// });
    /// # }
    /// ```
    pub fn subscribe_decider_for(
        &self,
        scope: DecideScope,
    ) -> impl Stream<Item = AgentEvent> + Send + 'static {
        // Deliver what we decide on, so the decider sees every event it is
        // expected to answer.
        let filter = SubscriptionFilter {
            harnesses: scope.harnesses.clone(),
            session_ids: scope.session_ids.clone(),
            event_types: scope.event_types.clone(),
            decide: Decide::Scope(scope),
            passthrough_env: Vec::new(),
        };
        self.subscribe(Some(filter))
    }

    // -- internal RPC helpers -------------------------------------------------

    async fn rpc_call(&self, method: &str, params: Option<Value>) -> Result<Value, ClientError> {
        self.rpc_call_with_retry(method, params, 3).await
    }

    async fn rpc_call_with_fallback(
        &self,
        primary: &str,
        fallback: &str,
        params: Option<Value>,
    ) -> Result<Value, ClientError> {
        match self.rpc_call(primary, params.clone()).await {
            Ok(v) => Ok(v),
            Err(ClientError::Rpc { code: -32601, .. }) => {
                // Method not found — try fallback name.
                tracing::debug!("method {primary} not found, trying {fallback}");
                self.rpc_call(fallback, params).await
            }
            Err(e) => Err(e),
        }
    }

    async fn rpc_call_with_retry(
        &self,
        method: &str,
        params: Option<Value>,
        max_attempts: usize,
    ) -> Result<Value, ClientError> {
        let mut last_err: Option<ClientError> = None;
        for attempt in 0..max_attempts {
            match try_rpc_call(&self.path, self.next_request_id(), method, params.clone()).await {
                Ok(v) => return Ok(v),
                Err(e) if is_retryable(&e) && attempt + 1 < max_attempts => {
                    let delay = Duration::from_millis(100 * (1u64 << attempt));
                    tracing::debug!(
                        "rpc {method} attempt {}/{} failed: {e}, retrying in {delay:?}",
                        attempt + 1,
                        max_attempts
                    );
                    tokio::time::sleep(delay).await;
                    last_err = Some(e);
                }
                Err(e) => return Err(e),
            }
        }
        Err(last_err.unwrap_or(ClientError::Disconnected))
    }
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn is_retryable(err: &ClientError) -> bool {
    matches!(
        err,
        ClientError::Io(_)
            | ClientError::Connection(_)
            | ClientError::Disconnected
            | ClientError::Timeout
            | ClientError::NotConnected(_)
    )
}

fn validate_socket_name(path: &str) -> Result<(), String> {
    if path.is_empty() {
        return Err("socket path must not be empty".to_string());
    }
    // Use interprocess validation without actually connecting.
    use interprocess::local_socket::{GenericFilePath, ToFsName};
    path.to_fs_name::<GenericFilePath>()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

async fn connect_stream(
    path: &str,
) -> Result<interprocess::local_socket::tokio::Stream, ClientError> {
    use interprocess::local_socket::tokio::prelude::*;
    use interprocess::local_socket::{GenericFilePath, ToFsName};

    let name = path
        .to_fs_name::<GenericFilePath>()
        .map_err(|e| ClientError::Connection(format!("invalid socket name {path}: {e}")))?;

    // Stream::connect is provided via the local_socket::tokio::prelude Stream trait.
    let fut = interprocess::local_socket::tokio::Stream::connect(name);
    let stream = tokio::time::timeout(Duration::from_secs(5), fut)
        .await
        .map_err(|_| ClientError::Timeout)?
        .map_err(|e| ClientError::Connection(format!("failed to connect to {path}: {e}")))?;
    Ok(stream)
}

async fn try_rpc_call(
    path: &str,
    id: u64,
    method: &str,
    params: Option<Value>,
) -> Result<Value, ClientError> {
    use agent_hooks_protocol::{JsonRpcRequest, JsonRpcResponse};

    let mut stream = connect_stream(path).await?;

    let req = JsonRpcRequest::new(serde_json::json!(id), method.to_string(), params);
    let mut line = serde_json::to_string(&req).map_err(ClientError::Serialization)?;
    line.push('\n');

    // Write with timeout
    tokio::time::timeout(Duration::from_secs(5), async {
        stream.write_all(line.as_bytes()).await?;
        stream.flush().await?;
        Ok::<(), std::io::Error>(())
    })
    .await
    .map_err(|_| ClientError::Timeout)?
    .map_err(ClientError::Io)?;

    // Read one line response with timeout
    let mut reader = BufReader::new(stream);
    let mut buf = String::new();
    let n = tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut buf))
        .await
        .map_err(|_| ClientError::Timeout)?
        .map_err(ClientError::Io)?;

    if n == 0 {
        return Err(ClientError::Disconnected);
    }
    let trimmed = buf.trim();
    if trimmed.is_empty() {
        return Err(ClientError::InvalidResponse("empty response".to_string()));
    }

    let resp: JsonRpcResponse = serde_json::from_str(trimmed).map_err(|e| {
        ClientError::InvalidResponse(format!(
            "failed to parse JSON-RPC response: {e}; raw: {trimmed}"
        ))
    })?;

    if let Some(err) = resp.error {
        return Err(ClientError::Rpc {
            code: err.code,
            message: err.message,
        });
    }

    Ok(resp.result.unwrap_or(Value::Null))
}

async fn subscribe_once(
    path: &str,
    filter: Option<SubscriptionFilter>,
    tx: &mpsc::Sender<AgentEvent>,
) -> Result<(), ClientError> {
    use agent_hooks_protocol::{JsonRpcNotification, JsonRpcRequest, JsonRpcResponse};

    let mut stream = connect_stream(path).await?;

    // Send subscribe request
    let params = filter.as_ref().and_then(|f| serde_json::to_value(f).ok());
    let req = JsonRpcRequest::new(serde_json::json!(1), "subscribe", params);
    let mut line = match serde_json::to_string(&req) {
        Ok(s) => s,
        Err(e) => return Err(ClientError::Serialization(e)),
    };
    line.push('\n');

    if let Err(e) = stream.write_all(line.as_bytes()).await {
        return Err(ClientError::Io(e));
    }
    if let Err(e) = stream.flush().await {
        return Err(ClientError::Io(e));
    }

    let mut reader = BufReader::new(stream);
    let mut buf = String::new();

    // Try to consume an initial ack response if the server sends one.
    // We peek with a short timeout; if no line arrives in 500ms we assume
    // the server streams events without ack (NDJSON mode) and continue.
    // To avoid losing the first event, we loop with a timeout per read.
    // Instead, we just enter the main loop: the first line will be either
    // a JsonRpcResponse ack or an event — the loop handles both.
    loop {
        buf.clear();

        // Check if receiver still wants data before blocking on read.
        if tx.is_closed() {
            return Ok(());
        }

        let n = reader.read_line(&mut buf).await.map_err(ClientError::Io)?;
        if n == 0 {
            return Err(ClientError::Disconnected);
        }
        let trimmed = buf.trim();
        if trimmed.is_empty() {
            continue;
        }

        // 1) Try direct AgentEvent (NDJSON mode)
        if let Ok(event) = serde_json::from_str::<AgentEvent>(trimmed) {
            if filter
                .as_ref()
                .map_or(true, |f| event_passes_filter(&event, f))
                && tx.send(event).await.is_err()
            {
                return Ok(());
            }
            continue;
        }

        // 2) Try JsonRpcNotification with method "event"
        if let Ok(notif) = serde_json::from_str::<JsonRpcNotification>(trimmed) {
            if notif.method == "event" {
                if let Ok(event) = serde_json::from_value::<AgentEvent>(notif.params.clone()) {
                    if filter
                        .as_ref()
                        .map_or(true, |f| event_passes_filter(&event, f))
                        && tx.send(event).await.is_err()
                    {
                        return Ok(());
                    }
                    continue;
                }
                // Some servers wrap event as { "event": {...} }
                if let Some(obj) = notif.params.as_object() {
                    if let Some(inner) = obj.get("event") {
                        if let Ok(event) = serde_json::from_value::<AgentEvent>(inner.clone()) {
                            if filter
                                .as_ref()
                                .map_or(true, |f| event_passes_filter(&event, f))
                                && tx.send(event).await.is_err()
                            {
                                return Ok(());
                            }
                            continue;
                        }
                    }
                }
            }
            // Ignore other notification types
            continue;
        }

        // 3) Try JsonRpcResponse (ack) — ignore success acks
        if let Ok(resp) = serde_json::from_str::<JsonRpcResponse>(trimmed) {
            if let Some(err) = resp.error {
                // Server rejected the subscribe
                return Err(ClientError::Rpc {
                    code: err.code,
                    message: err.message,
                });
            }
            // Ack with result — continue to event stream
            continue;
        }

        // 4) Try wrapped NDJSON where line is { "result": <event> } or similar
        if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
            if let Some(obj) = v.as_object() {
                // Common server envelope: {"type":"event","event":{...}}
                if let Some(ev) = obj.get("event") {
                    if let Ok(event) = serde_json::from_value::<AgentEvent>(ev.clone()) {
                        if filter
                            .as_ref()
                            .map_or(true, |f| event_passes_filter(&event, f))
                            && tx.send(event).await.is_err()
                        {
                            return Ok(());
                        }
                        continue;
                    }
                }
            }
            tracing::debug!("unrecognized event envelope: {trimmed}");
            continue;
        }

        tracing::debug!("unrecognized line in event stream: {trimmed}");
    }
}
