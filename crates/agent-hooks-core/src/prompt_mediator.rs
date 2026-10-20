use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_hooks_protocol::{AgentEvent, PromptDecision};
use tokio::sync::{oneshot, Mutex};
use tracing::{debug, warn};

use crate::error::CoreError;

/// Outcome of attempting to ask a question for an event invocation.
///
/// First-claims-the-turn: the first asker per `event_id` claims the slot and
/// prompts the human. Late askers on the same event park (never parallel
/// prompts); same `(session_id, key)` askers join the in-flight question and
/// share its single answer.
#[derive(Debug)]
pub enum ClaimOutcome {
    /// This caller claimed the turn; await the receiver for the verdict.
    Claimed(oneshot::Receiver<PromptDecision>),
    /// Same question already in flight; await the receiver (one answer fans
    /// out to every joiner).
    Joined(oneshot::Receiver<PromptDecision>),
    /// Another question owns this event; parked for retry on the next gated
    /// event after the claim resolves.
    Parked,
}

/// A parked question waiting for the current claim on an event to resolve.
#[derive(Debug, Clone)]
pub struct ParkedQuestion {
    pub session_id: String,
    pub key: String,
    pub prompt_id: String,
    pub question: Option<String>,
    pub expires_at: Instant,
}

/// Queue snapshot entry for status UI (`Q: [key*] [key …]`).
#[derive(Debug, Clone)]
pub struct QueueEntry {
    pub prompt_id: String,
    pub session_id: String,
    pub key: String,
    pub waiters: usize,
    pub parked: usize,
}

/// Manages pending interactive prompts (PermissionRequest / Question).
///
/// Single shared owner for the whole daemon (one global socket): all harness
/// shims and all consumer apps submit here, so fan-out collapses before it
/// reaches any harness-native dialog. Consumers speak canonical
/// [`PromptDecision`]; per-harness rendering stays at the adapter edge.
///
/// Rules, in order: dedupe by `(session_id, key)` → first-claims-the-turn per
/// `event_id` → timeout/orphan resolves deny (fail-closed) → parked questions
/// retry on the next gated event, never in parallel.
#[derive(Debug, Default)]
pub struct PromptMediator {
    /// prompt_id -> all waiter halves. One answer broadcasts to every joiner.
    pending: Mutex<HashMap<String, Vec<oneshot::Sender<PromptDecision>>>>,
    /// prompt_id -> display metadata for `prompt.list` (backlog UI).
    info: Mutex<HashMap<String, PromptMeta>>,
    /// (session_id, key) -> in-flight prompt_id. Same key = same question.
    key_index: Mutex<HashMap<(String, String), String>>,
    /// prompt_id -> (session_id, key) for index cleanup on resolve.
    prompt_key: Mutex<HashMap<String, (String, String)>>,
    /// event_id -> claiming prompt_id. First asker wins the turn.
    claimed: Mutex<HashMap<String, String>>,
    /// event_id -> parked questions, arrival order.
    parked: Mutex<HashMap<String, VecDeque<ParkedQuestion>>>,
    /// Connection ids subscribed as deciders, each with the scope of gate
    /// events it declared it would answer.
    deciders: parking_lot::Mutex<HashMap<u64, agent_hooks_protocol::DecideScope>>,
    /// Connection-scoped passthrough-env requests: conn id -> normalized
    /// valid names that connection declared via `subscribe`. A re-subscribe
    /// on the same connection replaces the previous entry. Removed on every
    /// disconnect path via [`PromptMediator::disconnect`] and on
    /// `unsubscribe` via [`PromptMediator::clear_env_requested`].
    env_requests: parking_lot::Mutex<HashMap<u64, Vec<String>>>,
    /// Connection id source.
    next_conn: AtomicU64,
}

/// Display metadata for one held prompt (powers `prompt.list`).
#[derive(Debug, Clone)]
pub struct PromptMeta {
    pub session_id: String,
    pub key: String,
    pub question: Option<String>,
    /// Full question payload for rich UIs (checkboxes, free text).
    pub prompt: Option<agent_hooks_protocol::PromptRequest>,
    pub created_at: Instant,
}

impl PromptMediator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_shared() -> Arc<Self> {
        Arc::new(Self::new())
    }

    /// Claim the question slot for an event, join an identical in-flight
    /// question, or park behind the current claim.
    ///
    /// `event_id` scopes first-claims-the-turn (one prompt per event
    /// invocation); `(session_id, key)` scopes dedupe (same question from N
    /// consumers = one prompt, one answer broadcast to all joiners).
    pub async fn claim_or_park(
        &self,
        event_id: String,
        session_id: String,
        key: String,
        prompt_id: String,
        question: Option<String>,
        ttl: Duration,
    ) -> ClaimOutcome {
        // Same question already in flight → join it.
        if let Some(existing) = self
            .key_index
            .lock()
            .await
            .get(&(session_id.clone(), key.clone()))
            .cloned()
        {
            let (tx, rx) = oneshot::channel();
            if let Some(waiters) = self.pending.lock().await.get_mut(&existing) {
                waiters.push(tx);
                debug!(prompt_id = %existing, "joined in-flight question");
                return ClaimOutcome::Joined(rx);
            }
        }
        // Another question owns this event → park for the next turn.
        if let Some(owner) = self.claimed.lock().await.get(&event_id).cloned() {
            if owner != prompt_id {
                let mut parked = self.parked.lock().await;
                parked
                    .entry(event_id)
                    .or_default()
                    .push_back(ParkedQuestion {
                        session_id,
                        key,
                        prompt_id,
                        question,
                        expires_at: Instant::now() + ttl,
                    });
                debug!("parked question behind claim");
                return ClaimOutcome::Parked;
            }
        }
        // First asker (or re-claim of own prompt_id) → claim the turn.
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .await
            .entry(prompt_id.clone())
            .or_default()
            .push(tx);
        self.prompt_key
            .lock()
            .await
            .insert(prompt_id.clone(), (session_id.clone(), key.clone()));
        self.info.lock().await.insert(
            prompt_id.clone(),
            PromptMeta {
                session_id: session_id.clone(),
                key: key.clone(),
                question: question.clone(),
                prompt: None,
                created_at: Instant::now(),
            },
        );
        self.key_index
            .lock()
            .await
            .insert((session_id, key), prompt_id.clone());
        self.claimed
            .lock()
            .await
            .insert(event_id, prompt_id.clone());
        debug!(prompt_id = %prompt_id, "claimed question turn");
        ClaimOutcome::Claimed(rx)
    }

    /// Pop the next live parked question for an event (skips TTL-expired).
    /// Call after the claim resolves with allow; on deny the caller should
    /// `drop_parked` instead — no point asking once the action is vetoed.
    pub async fn next_parked(&self, event_id: &str) -> Option<ParkedQuestion> {
        let mut parked = self.parked.lock().await;
        let queue = parked.get_mut(event_id)?;
        while let Some(front) = queue.pop_front() {
            if front.expires_at > Instant::now() {
                if queue.is_empty() {
                    parked.remove(event_id);
                }
                return Some(front);
            }
            debug!(prompt_id = %front.prompt_id, "dropping expired parked question");
        }
        parked.remove(event_id);
        None
    }

    /// Drop all parked questions for an event (claim denied → rest are moot).
    pub async fn drop_parked(&self, event_id: &str) -> usize {
        self.parked
            .lock()
            .await
            .remove(event_id)
            .map(|q| q.len())
            .unwrap_or(0)
    }

    /// Register a pending prompt together with its backlog metadata in one
    /// critical section (pending, then info — the same order
    /// `pending_prompts` and `respond` take). The hold is never visible
    /// without its question payload, so a `prompt.list` consumer answering
    /// the instant it appears is validated against the real questions, not an
    /// empty set.
    ///
    /// Same `prompt_id` from a second waiter joins (shared answer) instead of
    /// erroring — dedupe, not duplicate — and the first registrant's
    /// metadata is kept.
    pub async fn register_with_info(
        &self,
        prompt_id: String,
        meta: PromptMeta,
    ) -> oneshot::Receiver<PromptDecision> {
        let (tx, rx) = oneshot::channel();
        let mut pending = self.pending.lock().await;
        let mut info = self.info.lock().await;
        info.entry(prompt_id.clone()).or_insert(meta);
        pending.entry(prompt_id.clone()).or_default().push(tx);
        debug!(prompt_id = %prompt_id, "registered pending prompt");
        rx
    }

    /// Await a receiver from an earlier
    /// [`register_with_info`](Self::register_with_info), with a timeout. Use
    /// this when the hold had to exist before the caller could continue — the
    /// daemon registers first so the event it broadcasts is already
    /// answerable — and the wait happens afterwards. The pending entry is
    /// always removed, exactly once.
    pub async fn await_registered(
        self: &Arc<Self>,
        prompt_id: String,
        timeout: Duration,
        rx: oneshot::Receiver<PromptDecision>,
    ) -> Result<PromptDecision, CoreError> {
        let result = tokio::time::timeout(timeout, rx).await;
        self.remove_prompt(&prompt_id).await;
        match result {
            Ok(Ok(decision)) => Ok(decision),
            Ok(Err(_closed)) => Err(CoreError::ChannelClosed(prompt_id)),
            Err(_elapsed) => Err(CoreError::PromptTimeout(prompt_id)),
        }
    }

    /// Backlog snapshot for `prompt.list`: every held prompt with its
    /// question text, waiter count, parked depth, and age. Sorted oldest
    /// first so numbering is stable for the `answer` CLI.
    pub async fn pending_prompts(&self) -> Vec<agent_hooks_protocol::PendingPromptInfo> {
        let pending = self.pending.lock().await;
        let info = self.info.lock().await;
        let claimed = self.claimed.lock().await;
        let parked = self.parked.lock().await;
        let now = Instant::now();
        let mut out: Vec<agent_hooks_protocol::PendingPromptInfo> = pending
            .iter()
            .map(|(prompt_id, waiters)| {
                let meta = info.get(prompt_id);
                let parked_depth: usize = claimed
                    .iter()
                    .filter(|(_, owner)| *owner == prompt_id)
                    .filter_map(|(event_id, _)| parked.get(event_id))
                    .map(|q| q.len())
                    .sum();
                agent_hooks_protocol::PendingPromptInfo {
                    prompt_id: prompt_id.clone(),
                    session_id: meta.map(|m| m.session_id.clone()).unwrap_or_default(),
                    key: meta.map(|m| m.key.clone()).unwrap_or_default(),
                    question: meta.and_then(|m| m.question.clone()),
                    waiters: waiters.len(),
                    parked: parked_depth,
                    age_ms: meta
                        .map(|m| now.saturating_duration_since(m.created_at).as_millis() as u64)
                        .unwrap_or(0),
                    prompt: meta.and_then(|m| m.prompt.clone()),
                }
            })
            .collect();
        out.sort_by_key(|p| p.age_ms);
        out.reverse();
        out
    }

    /// Dispatch a human decision to every waiter on `prompt_id` (one answer
    /// fans out to all joiners). Cleans key/claim indexes exactly once; a
    /// late or duplicate answer finds no entry (`PromptNotFound`).
    ///
    /// The decision is validated against the questions stored with the hold
    /// ([`PromptDecision::resolve_questions`]) BEFORE the pending entry is
    /// consumed: a rejected decision returns
    /// [`CoreError::InvalidDecision`] and leaves the prompt pending so the
    /// decider can retry. Holds without question metadata (permission gates,
    /// plain tool gates) accept only answer-less decisions. Waiters receive
    /// the canonical, question-ordered decision.
    pub async fn respond(
        &self,
        prompt_id: &str,
        decision: PromptDecision,
    ) -> Result<(), CoreError> {
        let (waiters, decision) = {
            let mut guard = self.pending.lock().await;
            let decision = match guard.get(prompt_id) {
                Some(txs) if !txs.is_empty() => {
                    let info = self.info.lock().await;
                    let questions = info
                        .get(prompt_id)
                        .and_then(|meta| meta.prompt.as_ref())
                        .map_or(&[][..], |prompt| prompt.questions.as_slice());
                    decision.resolve_questions(questions).map_err(|reason| {
                        warn!(prompt_id = %prompt_id, %reason, "rejected invalid prompt decision; prompt stays pending");
                        CoreError::InvalidDecision {
                            prompt_id: prompt_id.to_string(),
                            reason,
                        }
                    })?
                }
                _ => decision,
            };
            (guard.remove(prompt_id), decision)
        };
        match waiters {
            Some(txs) if !txs.is_empty() => {
                // Deny vetoes the action → parked questions behind this claim
                // are moot. Allow/ask/defer leave them for the next turn.
                let denied = decision.is_deny();
                let owned = self.owned_events(prompt_id).await;
                self.remove_prompt(prompt_id).await;
                if denied {
                    for event_id in owned {
                        self.drop_parked(&event_id).await;
                    }
                }
                let mut delivered = 0usize;
                for tx in txs {
                    if tx.send(decision.clone()).is_ok() {
                        delivered += 1;
                    }
                }
                if delivered == 0 {
                    return Err(CoreError::ChannelClosed(prompt_id.to_string()));
                }
                debug!(prompt_id = %prompt_id, waiters = delivered, "prompt response broadcast");
                Ok(())
            }
            _ => {
                warn!(prompt_id = %prompt_id, "respond for unknown prompt");
                Err(CoreError::PromptNotFound(prompt_id.to_string()))
            }
        }
    }

    /// Remove a prompt and its key/claim index rows. Idempotent.
    async fn remove_prompt(&self, prompt_id: &str) {
        if let Some((session_id, key)) = self.prompt_key.lock().await.remove(prompt_id) {
            let mut index = self.key_index.lock().await;
            if index.get(&(session_id.clone(), key.clone())) == Some(&prompt_id.to_string()) {
                index.remove(&(session_id, key));
            }
        }
        self.info.lock().await.remove(prompt_id);
        self.claimed
            .lock()
            .await
            .retain(|_, owner| owner != prompt_id);
    }
    /// Event ids currently claimed by `prompt_id` (usually ≤1).
    async fn owned_events(&self, prompt_id: &str) -> Vec<String> {
        self.claimed
            .lock()
            .await
            .iter()
            .filter(|(_, owner)| owner.as_str() == prompt_id)
            .map(|(event_id, _)| event_id.clone())
            .collect()
    }

    /// List all currently pending prompt IDs.
    pub async fn pending_ids(&self) -> Vec<String> {
        self.pending.lock().await.keys().cloned().collect()
    }

    /// Register a new connection; returns its id for decider bookkeeping.
    pub fn connect(&self) -> u64 {
        self.next_conn.fetch_add(1, Ordering::Relaxed)
    }

    /// Forget a connection (and its decider flag). Idempotent.
    pub fn disconnect(&self, conn_id: u64) {
        self.deciders.lock().remove(&conn_id);
        self.env_requests.lock().remove(&conn_id);
    }

    /// Record (replacing any prior entry) the normalized valid env names
    /// this connection requested. Invalid names are excluded via the shared
    /// [`agent_hooks_protocol::normalize_env_names`] rule, matching what
    /// [`PromptMediator::env_requested_union`] serves.
    pub fn set_env_requested(&self, conn_id: u64, names: Vec<String>) {
        let normalized = agent_hooks_protocol::normalize_env_names(&names);
        let mut guard = self.env_requests.lock();
        if normalized.is_empty() {
            guard.remove(&conn_id);
        } else {
            guard.insert(conn_id, normalized);
        }
    }

    /// Drop one connection's env request without touching its decider flag.
    /// Used on `unsubscribe` while the connection stays open. Idempotent.
    pub fn clear_env_requested(&self, conn_id: u64) {
        self.env_requests.lock().remove(&conn_id);
    }

    /// Sorted, de-duplicated union of valid requested names across all
    /// currently connected subscribers. Empty when nobody requested any.
    pub fn env_requested_union(&self) -> Vec<String> {
        let guard = self.env_requests.lock();
        let mut out: Vec<String> = guard.values().flatten().cloned().collect();
        out.sort();
        out.dedup();
        out.retain(|n| agent_hooks_protocol::is_valid_env_name(n));
        out
    }

    /// Declare (or clear) what this connection will answer. `None` clears.
    pub fn set_decide(&self, conn_id: u64, scope: Option<agent_hooks_protocol::DecideScope>) {
        let mut guard = self.deciders.lock();
        match scope {
            Some(s) => {
                guard.insert(conn_id, s);
            }
            None => {
                guard.remove(&conn_id);
            }
        }
    }

    /// Whether any connected decider declared it would answer THIS event.
    ///
    /// This is the gate predicate. It deliberately mirrors the delivery
    /// filter: holding an event nobody was asked about is a deadlock that
    /// only the timeout can clear, so "held" and "someone was asked" must not
    /// be able to diverge. An event no decider claims passes straight
    /// through, which is the harness contract — a silent hook lets the action
    /// proceed.
    pub fn has_decider_for(&self, event: &AgentEvent) -> bool {
        self.deciders.lock().values().any(|s| s.claims(event))
    }

    /// Whether any connected decider is holding gates at all. Coarse; used
    /// only for status/reporting, never to decide a hold.
    pub fn has_deciders(&self) -> bool {
        !self.deciders.lock().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_hooks_protocol::PromptDecision;

    #[tokio::test]
    async fn register_and_respond() {
        let mediator = Arc::new(PromptMediator::new());
        let rx = mediator
            .register_with_info("p1".to_string(), meta(None))
            .await;
        mediator
            .respond("p1", PromptDecision::allow())
            .await
            .unwrap();
        let decision = rx.await.unwrap();
        assert!(decision.allow);
    }

    #[tokio::test]
    async fn duplicate_register_joins_and_broadcasts() {
        let mediator = PromptMediator::new();
        let rx1 = mediator
            .register_with_info("dup".to_string(), meta(None))
            .await;
        // Second waiter on the same prompt joins instead of erroring.
        let rx2 = mediator
            .register_with_info("dup".to_string(), meta(None))
            .await;
        mediator
            .respond("dup", PromptDecision::allow())
            .await
            .unwrap();
        assert!(rx1.await.unwrap().allow);
        assert!(rx2.await.unwrap().allow);
    }

    #[tokio::test]
    async fn same_key_dedupes_first_claim_wins_rest_park() {
        let mediator = PromptMediator::new();
        let ttl = Duration::from_secs(5);
        // Consumer A claims the turn.
        let rx_a = match mediator
            .claim_or_park(
                "ev1".to_string(),
                "ses1".to_string(),
                "prod-deploy".to_string(),
                "p-a".to_string(),
                Some("Deploy to prod?".to_string()),
                ttl,
            )
            .await
        {
            ClaimOutcome::Claimed(rx) => rx,
            _ => panic!("A should claim"),
        };
        // Consumer B asks the same key → joins, shares the one answer.
        let rx_b = match mediator
            .claim_or_park(
                "ev1".to_string(),
                "ses1".to_string(),
                "prod-deploy".to_string(),
                "p-b".to_string(),
                Some("Deploy to prod??".to_string()),
                ttl,
            )
            .await
        {
            ClaimOutcome::Joined(rx) => rx,
            _ => panic!("B should join"),
        };
        // Consumer C asks a different key on the same event → parked.
        assert!(matches!(
            mediator
                .claim_or_park(
                    "ev1".to_string(),
                    "ses1".to_string(),
                    "push-key".to_string(),
                    "p-c".to_string(),
                    Some("Push key?".to_string()),
                    ttl,
                )
                .await,
            ClaimOutcome::Parked
        ));
        mediator
            .respond("p-a", PromptDecision::allow())
            .await
            .unwrap();
        assert!(rx_a.await.unwrap().allow);
        assert!(rx_b.await.unwrap().allow);
        // Parked question survives the claim for the next turn.
        let parked = mediator.next_parked("ev1").await.unwrap();
        assert_eq!(parked.key, "push-key");
    }

    #[tokio::test]
    async fn expired_parked_is_skipped() {
        let mediator = PromptMediator::new();
        let ttl = Duration::from_secs(5);
        match mediator
            .claim_or_park(
                "ev2".to_string(),
                "ses1".to_string(),
                "q1".to_string(),
                "p-1".to_string(),
                None,
                ttl,
            )
            .await
        {
            ClaimOutcome::Claimed(_) => {}
            _ => panic!("should claim"),
        }
        // Park with an already-expired TTL.
        assert!(matches!(
            mediator
                .claim_or_park(
                    "ev2".to_string(),
                    "ses1".to_string(),
                    "q2".to_string(),
                    "p-2".to_string(),
                    None,
                    Duration::from_nanos(1),
                )
                .await,
            ClaimOutcome::Parked
        ));
        tokio::time::sleep(Duration::from_millis(5)).await;
        assert!(mediator.next_parked("ev2").await.is_none());
    }

    #[tokio::test]
    async fn deny_drops_parked_allow_keeps() {
        let mediator = PromptMediator::new();
        let ttl = Duration::from_secs(5);
        // Claim + park on ev3.
        let _rx3 = match mediator
            .claim_or_park(
                "ev3".to_string(),
                "ses1".to_string(),
                "q1".to_string(),
                "p-1".to_string(),
                None,
                ttl,
            )
            .await
        {
            ClaimOutcome::Claimed(rx) => rx,
            _ => panic!("should claim"),
        };
        assert!(matches!(
            mediator
                .claim_or_park(
                    "ev3".to_string(),
                    "ses1".to_string(),
                    "q2".to_string(),
                    "p-2".to_string(),
                    None,
                    ttl,
                )
                .await,
            ClaimOutcome::Parked
        ));
        // Allow keeps the parked question for the next turn.
        mediator
            .respond("p-1", PromptDecision::allow())
            .await
            .unwrap();
        assert!(mediator.next_parked("ev3").await.is_some());
        let _rx4 = match mediator
            .claim_or_park(
                "ev4".to_string(),
                "ses1".to_string(),
                "q1".to_string(),
                "p-3".to_string(),
                None,
                ttl,
            )
            .await
        {
            ClaimOutcome::Claimed(rx) => rx,
            _ => panic!("should claim"),
        };
        assert!(matches!(
            mediator
                .claim_or_park(
                    "ev4".to_string(),
                    "ses1".to_string(),
                    "q2".to_string(),
                    "p-4".to_string(),
                    None,
                    ttl,
                )
                .await,
            ClaimOutcome::Parked
        ));
        mediator
            .respond("p-3", PromptDecision::deny(Some("no".to_string())))
            .await
            .unwrap();
        assert!(mediator.next_parked("ev4").await.is_none());
    }

    /// Hold metadata with the given prompt payload.
    fn meta(prompt: Option<agent_hooks_protocol::PromptRequest>) -> PromptMeta {
        PromptMeta {
            session_id: "ses".to_string(),
            key: "tool:Ask".to_string(),
            question: None,
            prompt,
            created_at: Instant::now(),
        }
    }

    #[tokio::test]
    async fn pending_prompts_lists_backlog() {
        let mediator = PromptMediator::new();
        let _rx = mediator
            .register_with_info(
                "bp".to_string(),
                PromptMeta {
                    session_id: "ses1".to_string(),
                    key: "tool:Bash".to_string(),
                    question: Some("Bash rm -rf?".to_string()),
                    prompt: None,
                    created_at: Instant::now(),
                },
            )
            .await;
        let list = mediator.pending_prompts().await;
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].session_id, "ses1");
        assert_eq!(list[0].question.as_deref(), Some("Bash rm -rf?"));
        assert_eq!(list[0].waiters, 1);
    }

    /// Register a hold carrying one single-select question `color`
    /// (`Red`/`Blue`, free text allowed) and return its receiver.
    async fn hold_question(
        mediator: &PromptMediator,
        prompt_id: &str,
    ) -> oneshot::Receiver<PromptDecision> {
        let option = |label: &str| agent_hooks_protocol::PromptOption {
            label: label.to_string(),
            description: None,
            preview: None,
        };
        mediator
            .register_with_info(
                prompt_id.to_string(),
                meta(Some(agent_hooks_protocol::PromptRequest {
                    prompt_id: prompt_id.to_string(),
                    kind: agent_hooks_protocol::PromptKind::Question,
                    tool_name: None,
                    command: None,
                    file_path: None,
                    description: None,
                    questions: vec![agent_hooks_protocol::PromptQuestion {
                        id: "color".to_string(),
                        header: None,
                        question: "Color?".to_string(),
                        options: vec![option("Red"), option("Blue")],
                        multiple: false,
                        custom: true,
                        recommended: None,
                    }],
                    suggestions: Vec::new(),
                })),
            )
            .await
    }

    fn answer(question_id: &str, option: &str) -> PromptDecision {
        PromptDecision {
            answers: vec![agent_hooks_protocol::PromptAnswer {
                question_id: question_id.to_string(),
                selected_options: vec![option.to_string()],
                custom_input: None,
            }],
            ..PromptDecision::allow()
        }
    }

    #[tokio::test]
    async fn invalid_answer_is_rejected_and_prompt_stays_pending() {
        let mediator = PromptMediator::new();
        let mut rx = hold_question(&mediator, "q1").await;

        for bad in [answer("color", "Green"), answer("ghost", "Red")] {
            let err = mediator.respond("q1", bad).await.unwrap_err();
            assert!(matches!(err, CoreError::InvalidDecision { .. }), "{err}");
            assert!(err.to_string().contains("q1"), "{err}");
            // Nothing was consumed: still listed, still unresolved.
            assert_eq!(mediator.pending_ids().await, vec!["q1".to_string()]);
            assert_eq!(mediator.pending_prompts().await[0].waiters, 1);
            assert!(rx.try_recv().is_err());
        }

        // The decider can retry with a valid answer.
        mediator
            .respond("q1", answer("color", "Blue"))
            .await
            .unwrap();
        let got = rx.await.unwrap();
        assert_eq!(got.answers[0].selected_options, ["Blue"]);
        assert!(mediator.pending_ids().await.is_empty());
    }

    #[tokio::test]
    async fn rejected_response_keeps_every_joined_waiter() {
        let mediator = PromptMediator::new();
        let mut rx1 = hold_question(&mediator, "q2").await;
        let mut rx2 = mediator
            .register_with_info("q2".to_string(), meta(None))
            .await;

        assert!(mediator
            .respond("q2", answer("color", "Nope"))
            .await
            .is_err());
        assert!(rx1.try_recv().is_err() && rx2.try_recv().is_err());
        assert_eq!(mediator.pending_prompts().await[0].waiters, 2);

        mediator
            .respond("q2", answer("color", "Red"))
            .await
            .unwrap();
        assert!(rx1.await.unwrap().allow);
        assert!(rx2.await.unwrap().allow);
    }

    #[tokio::test]
    async fn conflicting_flags_are_rejected_without_consuming_the_hold() {
        let mediator = PromptMediator::new();
        let mut rx = hold_question(&mediator, "q3").await;
        let conflict = PromptDecision {
            ask: true,
            ..PromptDecision::allow()
        };
        let err = mediator.respond("q3", conflict).await.unwrap_err();
        assert!(matches!(err, CoreError::InvalidDecision { .. }), "{err}");
        assert!(rx.try_recv().is_err());
        mediator
            .respond("q3", PromptDecision::allow())
            .await
            .unwrap();
        assert!(rx.await.unwrap().allow);
    }

    #[tokio::test]
    async fn permission_only_hold_rejects_answers_but_accepts_plain_verdicts() {
        let mediator = PromptMediator::new();
        // A permission gate: its prompt carries no questions.
        let mut plain = mediator
            .register_with_info(
                "perm".to_string(),
                meta(Some(agent_hooks_protocol::PromptRequest {
                    prompt_id: "perm".to_string(),
                    kind: agent_hooks_protocol::PromptKind::Permission,
                    tool_name: Some("Bash".to_string()),
                    command: Some("ls".to_string()),
                    file_path: None,
                    description: None,
                    questions: Vec::new(),
                    suggestions: Vec::new(),
                })),
            )
            .await;
        let err = mediator
            .respond("perm", answer("color", "Red"))
            .await
            .unwrap_err();
        assert!(matches!(err, CoreError::InvalidDecision { .. }), "{err}");
        assert!(plain.try_recv().is_err());
        assert_eq!(mediator.pending_ids().await, vec!["perm".to_string()]);

        mediator
            .respond("perm", PromptDecision::allow_always())
            .await
            .unwrap();
        assert!(plain.await.unwrap().allow_always);
    }

    #[tokio::test]
    async fn answer_less_allow_is_valid_on_a_question_hold() {
        let mediator = PromptMediator::new();
        let rx = hold_question(&mediator, "q4").await;
        mediator
            .respond("q4", PromptDecision::allow())
            .await
            .unwrap();
        assert!(rx.await.unwrap().answers.is_empty());
    }

    #[tokio::test]
    async fn respond_to_unknown_prompt_is_still_not_found() {
        let mediator = PromptMediator::new();
        let err = mediator
            .respond("missing", answer("color", "Red"))
            .await
            .unwrap_err();
        assert!(matches!(err, CoreError::PromptNotFound(_)), "{err}");
    }

    #[test]
    fn decider_presence_tracks_connections() {
        let mediator = PromptMediator::new();
        let all = agent_hooks_protocol::DecideScope::default();
        assert!(!mediator.has_deciders());
        let a = mediator.connect();
        let b = mediator.connect();
        assert_ne!(a, b);
        assert!(!mediator.has_deciders());
        mediator.set_decide(a, Some(all.clone()));
        assert!(mediator.has_deciders());
        mediator.set_decide(b, Some(all.clone()));
        mediator.set_decide(a, None);
        assert!(mediator.has_deciders());
        mediator.disconnect(b);
        assert!(!mediator.has_deciders());
        // Idempotent disconnects never panic.
        mediator.disconnect(b);
    }

    fn ev(lifecycle: agent_hooks_protocol::AgentLifecycle) -> AgentEvent {
        AgentEvent::new(
            "claude-code".to_string(),
            "ses1".to_string(),
            lifecycle,
            agent_hooks_protocol::EventContext::default(),
        )
    }

    fn pre_tool(tool: &str) -> AgentEvent {
        ev(agent_hooks_protocol::AgentLifecycle::PreToolUse {
            tool_name: tool.to_string(),
            input: serde_json::json!({}),
            prompt: None,
        })
    }

    fn prompt_submit() -> AgentEvent {
        ev(agent_hooks_protocol::AgentLifecycle::UserPromptSubmit {
            user_prompt: Some("hi".to_string()),
        })
    }

    #[test]
    fn decider_only_gates_what_it_declared() {
        // The deadlock this prevents: a PreToolUse-only decider used to hold
        // UserPromptSubmit too, waiting for a verdict on an event it was
        // never even delivered. Unclaimed events must pass through.
        let mediator = PromptMediator::new();
        let c = mediator.connect();
        mediator.set_decide(
            c,
            Some(agent_hooks_protocol::DecideScope {
                event_types: vec!["PreToolUse".to_string()],
                ..Default::default()
            }),
        );
        assert!(mediator.has_decider_for(&pre_tool("Bash")));
        assert!(!mediator.has_decider_for(&prompt_submit()));
    }

    #[test]
    fn tool_scope_narrows_within_an_event_type() {
        let mediator = PromptMediator::new();
        let c = mediator.connect();
        mediator.set_decide(
            c,
            Some(agent_hooks_protocol::DecideScope {
                event_types: vec!["PreToolUse".to_string()],
                tools: vec!["Bash".to_string()],
                ..Default::default()
            }),
        );
        assert!(mediator.has_decider_for(&pre_tool("Bash")));
        assert!(!mediator.has_decider_for(&pre_tool("Read")));
        // A tool-scoped decider never gates an event that names no tool.
        assert!(!mediator.has_decider_for(&prompt_submit()));
    }

    #[test]
    fn empty_scope_claims_everything_and_disconnect_releases() {
        // `decide: true` resolves to an unconstrained scope: still a valid
        // (if broad) claim, and it must stop claiming on disconnect.
        let mediator = PromptMediator::new();
        let c = mediator.connect();
        mediator.set_decide(c, Some(agent_hooks_protocol::DecideScope::default()));
        assert!(mediator.has_decider_for(&pre_tool("Bash")));
        assert!(mediator.has_decider_for(&prompt_submit()));
        mediator.disconnect(c);
        assert!(!mediator.has_decider_for(&prompt_submit()));
    }

    fn permission_request() -> AgentEvent {
        ev(agent_hooks_protocol::AgentLifecycle::PermissionRequest {
            prompt: agent_hooks_protocol::PromptRequest {
                prompt_id: "p-1".to_string(),
                kind: agent_hooks_protocol::PromptKind::Permission,
                tool_name: Some("Bash".to_string()),
                command: Some("rm -rf /".to_string()),
                file_path: None,
                description: None,
                questions: Vec::new(),
                suggestions: Vec::new(),
            },
        })
    }

    #[test]
    fn decide_true_only_claims_what_was_already_waiting() {
        // The default must never manufacture a wait. A permission request had
        // already stopped for a human, so answering it costs the agent
        // nothing; a mid-flight PreToolUse or prompt submission has nobody
        // waiting, and holding it would introduce a stall that did not exist.
        let mediator = PromptMediator::new();
        let c = mediator.connect();
        let scope = agent_hooks_protocol::Decide::Enabled(true)
            .resolve(&agent_hooks_protocol::SubscriptionFilter::default())
            .expect("decide:true is a decider");
        assert!(scope.interactive_only);
        mediator.set_decide(c, Some(scope));

        assert!(mediator.has_decider_for(&permission_request()));
        assert!(!mediator.has_decider_for(&pre_tool("Bash")));
        assert!(!mediator.has_decider_for(&prompt_submit()));
    }

    #[test]
    fn explicit_scope_may_claim_mid_flight_events() {
        // Naming an event is opting into the stall — that stays possible, it
        // just cannot be inherited from `decide: true`.
        let mediator = PromptMediator::new();
        let c = mediator.connect();
        mediator.set_decide(
            c,
            Some(agent_hooks_protocol::DecideScope {
                event_types: vec!["PreToolUse".to_string()],
                ..Default::default()
            }),
        );
        assert!(mediator.has_decider_for(&pre_tool("Bash")));
    }

    #[test]
    fn session_scope_isolates_other_sessions() {
        let mediator = PromptMediator::new();
        let c = mediator.connect();
        mediator.set_decide(
            c,
            Some(agent_hooks_protocol::DecideScope {
                session_ids: vec!["ses-other".to_string()],
                ..Default::default()
            }),
        );
        assert!(!mediator.has_decider_for(&prompt_submit()));
    }

    #[test]
    fn env_requested_union_merges_connections_sorted_and_deduped() {
        // Two subscribers declare overlapping names; the union serves each
        // valid name once, sorted — the shape the shim captures against.
        let mediator = PromptMediator::new();
        let a = mediator.connect();
        let b = mediator.connect();
        mediator.set_env_requested(a, vec!["TMUX".to_string(), "TERM_PROGRAM".to_string()]);
        mediator.set_env_requested(b, vec!["TMUX".to_string(), "MY_SESSION_ID".to_string()]);
        assert_eq!(
            mediator.env_requested_union(),
            vec!["MY_SESSION_ID", "TERM_PROGRAM", "TMUX"]
        );
    }

    #[test]
    fn env_requested_resubscribe_replaces_prior_entry() {
        // Re-subscribing on the same connection replaces, never accumulates:
        // stale names must stop being captured for the next shim event.
        let mediator = PromptMediator::new();
        let a = mediator.connect();
        mediator.set_env_requested(a, vec!["OLD_NAME".to_string()]);
        assert_eq!(mediator.env_requested_union(), vec!["OLD_NAME"]);
        mediator.set_env_requested(a, vec!["NEW_NAME".to_string()]);
        assert_eq!(mediator.env_requested_union(), vec!["NEW_NAME"]);
    }

    #[test]
    fn env_requested_excludes_invalid_names_consistently() {
        // Invalid names are excluded at store time by the same shared rule
        // the wire parser uses, so they never reach the union — excluded
        // everywhere, not rejected in one place and honored in another.
        let mediator = PromptMediator::new();
        let a = mediator.connect();
        mediator.set_env_requested(
            a,
            vec![
                "GOOD_NAME".to_string(),
                "bad-name!".to_string(),
                "".to_string(),
                "1NOPE".to_string(),
                " GOOD_NAME ".to_string(),
            ],
        );
        assert_eq!(mediator.env_requested_union(), vec!["GOOD_NAME"]);
        // An all-invalid declaration stores nothing at all.
        let b = mediator.connect();
        mediator.set_env_requested(b, vec!["has-dash".to_string()]);
        assert_eq!(mediator.env_requested_union(), vec!["GOOD_NAME"]);
    }

    #[test]
    fn env_requested_disconnect_and_unsubscribe_release_names() {
        // Departed connections stop contributing to the union on every exit
        // path: disconnect drops the entry, and unsubscribe clears it while
        // the connection stays open — without touching the decider flag.
        let mediator = PromptMediator::new();
        let a = mediator.connect();
        let b = mediator.connect();
        mediator.set_env_requested(a, vec!["FROM_A".to_string()]);
        mediator.set_env_requested(b, vec!["FROM_B".to_string()]);
        mediator.set_decide(b, Some(agent_hooks_protocol::DecideScope::default()));
        mediator.clear_env_requested(b);
        assert_eq!(mediator.env_requested_union(), vec!["FROM_A"]);
        assert!(mediator.has_deciders());
        mediator.disconnect(a);
        assert!(mediator.env_requested_union().is_empty());
        // Idempotent teardown never panics.
        mediator.disconnect(a);
        mediator.clear_env_requested(a);
    }
}
