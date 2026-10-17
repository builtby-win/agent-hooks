use std::collections::{HashMap, HashSet};

use agent_hooks_protocol::{AgentEvent, AgentLifecycle, NormalizedStatus};

/// Per-session state maintained in memory.
#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub session_id: String,
    pub parent_session_id: Option<String>,
    pub harness: String,
    pub status: NormalizedStatus,
    pub last_event: Option<AgentEvent>,
    pub last_lifecycle_name: Option<String>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub event_count: u64,
}

impl SessionInfo {
    fn from_event(event: &AgentEvent) -> Self {
        Self {
            session_id: event.session_id.clone(),
            parent_session_id: event.parent_session_id.clone(),
            harness: event.harness.clone(),
            status: event.status,
            last_event: Some(event.clone()),
            last_lifecycle_name: Some(event.lifecycle.name().to_string()),
            created_at_ms: event.timestamp_ms,
            updated_at_ms: event.timestamp_ms,
            event_count: 1,
        }
    }

    fn update(&mut self, event: &AgentEvent) {
        // Preserve earliest created_at.
        self.parent_session_id = event
            .parent_session_id
            .clone()
            .or(self.parent_session_id.clone());
        self.harness = event.harness.clone();
        self.status = event.status;
        let mut event_to_store = event.clone();
        let is_config_dir = event
            .context
            .cwd
            .as_ref()
            .map(|p| p.to_string_lossy().contains(".gemini"))
            .unwrap_or(false);
        if event_to_store.context.cwd.is_none() || is_config_dir {
            if let Some(existing_cwd) = self.last_event.as_ref().and_then(|e| e.context.cwd.clone())
            {
                event_to_store.context.cwd = Some(existing_cwd);
                if event_to_store.context.git_worktree.is_none() {
                    event_to_store.context.git_worktree = self
                        .last_event
                        .as_ref()
                        .and_then(|e| e.context.git_worktree.clone());
                }
            }
        }
        self.last_event = Some(event_to_store);
        self.last_lifecycle_name = Some(event.lifecycle.name().to_string());
        self.updated_at_ms = event.timestamp_ms;
        self.event_count += 1;
        if self.created_at_ms == 0 {
            self.created_at_ms = event.timestamp_ms;
        }
    }
}

/// Sessions idle longer than this are expired from the tracker. Sweeps run on
/// insert (`track`) and on `sessions.list`, so tracker memory stays bounded
/// no matter how many sessions ever connect.
pub const SESSION_TTL_MS: u64 = 24 * 60 * 60 * 1000;

/// Composite session identity: a `session_id` is only unique within its
/// harness. Two harnesses reusing one `session_id` are two independent
/// sessions — they never merge and never answer each other's holds.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionKey {
    pub harness: String,
    pub session_id: String,
}

impl SessionKey {
    pub fn new(harness: &str, session_id: &str) -> Self {
        Self {
            harness: harness.to_string(),
            session_id: session_id.to_string(),
        }
    }

    fn for_event(event: &AgentEvent) -> Self {
        Self::new(&event.harness, &event.session_id)
    }
}

/// In-memory session registry with subagent parentage tree.
///
/// Thread-safety: this struct itself is not `Sync`; wrap in `RwLock` / `Mutex` for sharing.
#[derive(Debug, Default)]
pub struct SessionTracker {
    sessions: HashMap<SessionKey, SessionInfo>,
    /// parent session key -> child session keys (same-harness parentage)
    children: HashMap<SessionKey, HashSet<SessionKey>>,
}

impl SessionTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Wall-clock time in epoch milliseconds, matching `AgentEvent.timestamp_ms`.
    pub fn now_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    /// Record an event, creating or updating the session entry and maintaining
    /// the parentage tree. Sessions are keyed by the composite
    /// `(harness, session_id)`: the same id on two harnesses never merges.
    pub fn track(&mut self, event: &AgentEvent) {
        self.track_protected(event, &[]);
    }

    /// `track` plus hold protection: sessions whose live prompt hold is still
    /// pending (`protected_event_ids`, i.e. the mediator's pending prompt ids)
    /// are never expired by the insert sweep while held.
    pub fn track_protected(&mut self, event: &AgentEvent, protected_event_ids: &[String]) {
        let key = SessionKey::for_event(event);
        let parent = event.parent_session_id.clone();

        // Also infer parentage from SubagentStart / SubagentStop: the subagent_id is the child.
        let inferred_child = match &event.lifecycle {
            AgentLifecycle::SubagentStart { subagent_id, .. }
            | AgentLifecycle::SubagentStop { subagent_id, .. } => Some(subagent_id.clone()),
            _ => None,
        };

        let is_new = !self.sessions.contains_key(&key);

        if is_new {
            let mut info = SessionInfo::from_event(event);
            if let Some(created_ms) = event
                .extensions
                .get("session_created_at_ms")
                .and_then(|v| v.as_u64())
            {
                info.created_at_ms = created_ms;
            }
            self.sessions.insert(key.clone(), info);
        } else if let Some(info) = self.sessions.get_mut(&key) {
            info.update(event);
            if let Some(created_ms) = event
                .extensions
                .get("session_created_at_ms")
                .and_then(|v| v.as_u64())
            {
                if info.created_at_ms == 0 || info.created_at_ms > created_ms {
                    info.created_at_ms = created_ms;
                }
            }
        }

        // Maintain parent -> children edges for explicit parent_session_id.
        // Parentage resolves within the same harness: the composite identity
        // means a parent id from another harness names a different session.
        if let Some(parent_id) = parent {
            let parent_key = SessionKey::new(&event.harness, &parent_id);
            self.children
                .entry(parent_key)
                .or_default()
                .insert(key.clone());
        }

        // Maintain tree for subagent events: root session -> subagent child.
        if let Some(child_id) = inferred_child {
            let child_key = SessionKey::new(&event.harness, &child_id);
            // Ensure child session placeholder exists.
            if !self.sessions.contains_key(&child_key) {
                let mut placeholder = SessionInfo::from_event(event);
                placeholder.session_id = child_id.clone();
                placeholder.parent_session_id = Some(event.session_id.clone());
                // Status for subagent start is Running.
                self.sessions.insert(child_key.clone(), placeholder);
            } else if let Some(child_info) = self.sessions.get_mut(&child_key) {
                if child_info.parent_session_id.is_none() {
                    child_info.parent_session_id = Some(event.session_id.clone());
                }
            }
            self.children
                .entry(key.clone())
                .or_default()
                .insert(child_key);
        }

        // Sweep-on-insert: expire sessions idle past the TTL so tracker memory
        // stays bounded. The session just written is never expired by its own
        // insert, and sessions with live holds are protected while held.
        // Cleanup on SessionEnd / terminal Stop if desired — we keep the record but mark inactive.
        self.sweep_expired(Self::now_ms(), protected_event_ids, Some(&key));
    }

    /// Get a snapshot of a single session, addressed by composite identity.
    pub fn get(&self, harness: &str, session_id: &str) -> Option<&SessionInfo> {
        self.sessions.get(&SessionKey::new(harness, session_id))
    }

    /// Mutable access (e.g., to update status externally).
    pub fn get_mut(&mut self, harness: &str, session_id: &str) -> Option<&mut SessionInfo> {
        self.sessions.get_mut(&SessionKey::new(harness, session_id))
    }

    /// All known sessions.
    pub fn all_sessions(&self) -> Vec<&SessionInfo> {
        self.sessions.values().collect()
    }

    /// Active sessions (not ended / not failed terminally).
    pub fn active_sessions(&self) -> Vec<&SessionInfo> {
        self.sessions
            .values()
            .filter(|s| {
                matches!(
                    s.status,
                    NormalizedStatus::Idle
                        | NormalizedStatus::Running
                        | NormalizedStatus::WaitingForInput
                )
            })
            .collect()
    }

    /// Alternate view: return every session whose last lifecycle is not SessionEnd.
    pub fn active_session_ids(&self) -> Vec<String> {
        self.active_sessions()
            .into_iter()
            .map(|s| s.session_id.clone())
            .collect()
    }

    /// Immediate children of a parent session, addressed by composite identity.
    pub fn children_of(&self, harness: &str, parent_id: &str) -> Vec<&SessionInfo> {
        self.children
            .get(&SessionKey::new(harness, parent_id))
            .map(|set| {
                set.iter()
                    .filter_map(|key| self.sessions.get(key))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Whether a session is known.
    pub fn contains(&self, harness: &str, session_id: &str) -> bool {
        self.sessions
            .contains_key(&SessionKey::new(harness, session_id))
    }

    /// Whether the session is considered active (exists and not terminally stopped).
    pub fn is_active(&self, harness: &str, session_id: &str) -> bool {
        self.get(harness, session_id)
            .map(|s| {
                matches!(
                    s.status,
                    NormalizedStatus::Idle
                        | NormalizedStatus::Running
                        | NormalizedStatus::WaitingForInput
                )
            })
            .unwrap_or(false)
    }

    /// Total number of tracked sessions.
    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    /// Remove a session and its parentage edges. Returns the removed info if present.
    pub fn remove(&mut self, harness: &str, session_id: &str) -> Option<SessionInfo> {
        let key = SessionKey::new(harness, session_id);
        let info = self.sessions.remove(&key)?;
        // Remove from children maps where it appears as child.
        for set in self.children.values_mut() {
            set.remove(&key);
        }
        // Remove its own children entry (orphans remain as roots).
        self.children.remove(&key);
        // Clean parent entry if this session was a parent with empty set already handled.
        Some(info)
    }

    /// Walk ancestry chain from `session_id` up to the root, inclusive.
    /// Parentage follows within the same harness (composite identity).
    pub fn ancestry(&self, harness: &str, session_id: &str) -> Vec<&SessionInfo> {
        let mut chain = Vec::new();
        let mut current = Some(SessionKey::new(harness, session_id));
        let mut visited = HashSet::new();
        while let Some(key) = current {
            if !visited.insert(key.clone()) {
                break; // cycle guard
            }
            if let Some(info) = self.sessions.get(&key) {
                chain.push(info);
                current = info
                    .parent_session_id
                    .clone()
                    .map(|parent_id| SessionKey::new(&info.harness, &parent_id));
            } else {
                break;
            }
        }
        chain
    }

    /// Clear all state.
    pub fn clear(&mut self) {
        self.sessions.clear();
        self.children.clear();
    }
    /// Expire sessions idle longer than `SESSION_TTL_MS`. Sessions whose live
    /// prompt hold is still pending (`protected_event_ids`, i.e. the
    /// mediator's pending prompt ids) are never expired while held.
    /// Returns the number of sessions removed.
    pub fn evict_idle_expired(&mut self, now_ms: u64, protected_event_ids: &[String]) -> usize {
        self.sweep_expired(now_ms, protected_event_ids, None)
    }

    fn sweep_expired(
        &mut self,
        now_ms: u64,
        protected_event_ids: &[String],
        keep: Option<&SessionKey>,
    ) -> usize {
        let expired: Vec<SessionKey> = self
            .sessions
            .iter()
            .filter(|(key, info)| {
                if let Some(keep_key) = keep {
                    if *key == keep_key {
                        return false;
                    }
                }
                now_ms.saturating_sub(info.updated_at_ms) > SESSION_TTL_MS
                    && !Self::is_held(info, protected_event_ids)
            })
            .map(|(key, _)| key.clone())
            .collect();
        let removed = expired.len();
        for key in &expired {
            self.remove(&key.harness, &key.session_id);
        }
        removed
    }

    /// A session counts as held while its last event's hold rendezvous id is
    /// still pending: the broadcast `event_id`, or `prompt.prompt_id` for
    /// `PermissionRequest` (which keeps its own id — same rule as the daemon's
    /// hold key).
    fn is_held(info: &SessionInfo, protected_event_ids: &[String]) -> bool {
        let Some(last) = info.last_event.as_ref() else {
            return false;
        };
        if protected_event_ids.contains(&last.event_id) {
            return true;
        }
        if let AgentLifecycle::PermissionRequest { prompt } = &last.lifecycle {
            return protected_event_ids.contains(&prompt.prompt_id);
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_hooks_protocol::{AgentEvent, AgentLifecycle, EventContext};
    use std::path::PathBuf;

    fn mk_event(session: &str, parent: Option<&str>, lifecycle: AgentLifecycle) -> AgentEvent {
        let mut ev = AgentEvent::new("claude-code", session, lifecycle, EventContext::default());
        ev.parent_session_id = parent.map(|s| s.to_string());
        ev
    }

    #[test]
    fn tracks_and_links_parentage() {
        let mut tracker = SessionTracker::new();
        tracker.track(&mk_event(
            "root",
            None,
            AgentLifecycle::SessionStart {
                resume_command: None,
            },
        ));
        tracker.track(&mk_event(
            "child-1",
            Some("root"),
            AgentLifecycle::SubagentStart {
                subagent_id: "child-1".into(),
                subagent_type: "Explore".into(),
                description: None,
            },
        ));
        assert_eq!(tracker.len(), 2);
        assert_eq!(tracker.children_of("claude-code", "root").len(), 1);
        assert_eq!(tracker.ancestry("claude-code", "child-1").len(), 2);
    }

    #[test]
    fn preserves_known_workspace_cwd_when_subsequent_event_has_config_dir() {
        let mut tracker = SessionTracker::new();
        let mut ev1 = mk_event(
            "sess-1",
            None,
            AgentLifecycle::PreToolUse {
                tool_name: "run_command".into(),
                input: serde_json::json!({}),
                prompt: None,
            },
        );
        ev1.context.cwd = Some(PathBuf::from("/Users/developer/project"));
        ev1.context.git_worktree = Some(PathBuf::from("/Users/developer/project"));
        tracker.track(&ev1);

        // Subsequent event arrives with cwd pointing to internal config dir (e.g. ~/.gemini/config)
        let mut ev2 = mk_event(
            "sess-1",
            None,
            AgentLifecycle::Notification {
                message: "notification".into(),
                title: None,
                notification_type: None,
            },
        );
        ev2.context.cwd = Some(PathBuf::from("/Users/developer/.gemini/config"));
        tracker.track(&ev2);

        let sess = tracker
            .get("claude-code", "sess-1")
            .expect("session exists");
        let last_ev = sess.last_event.as_ref().expect("last_event exists");
        assert_eq!(
            last_ev.context.cwd,
            Some(PathBuf::from("/Users/developer/project"))
        );
        assert_eq!(
            last_ev.context.git_worktree,
            Some(PathBuf::from("/Users/developer/project"))
        );
    }

    #[test]
    fn active_filter() {
        let mut tracker = SessionTracker::new();
        tracker.track(&mk_event(
            "a",
            None,
            AgentLifecycle::SessionStart {
                resume_command: None,
            },
        ));
        tracker.track(&mk_event(
            "b",
            None,
            AgentLifecycle::Stop {
                summary: None,
                success: true,
            },
        ));
        tracker.track(&mk_event(
            "c",
            None,
            AgentLifecycle::SessionEnd { exit_code: Some(0) },
        ));
        tracker.track(&mk_event(
            "d",
            None,
            AgentLifecycle::SessionEnd { exit_code: Some(1) },
        ));
        assert!(tracker.is_active("claude-code", "a"));
        // Per-turn Stop ("model stopped talking") leaves the session alive
        // and idle — only SessionEnd terminates.
        assert!(tracker.is_active("claude-code", "b"));
        assert_eq!(
            tracker.get("claude-code", "b").unwrap().status,
            NormalizedStatus::Idle
        );
        assert!(!tracker.is_active("claude-code", "c"));
        assert!(!tracker.is_active("claude-code", "d"));
        assert_eq!(
            tracker.get("claude-code", "c").unwrap().status,
            NormalizedStatus::Closed
        );
        assert_eq!(
            tracker.get("claude-code", "d").unwrap().status,
            NormalizedStatus::Failed
        );
        let active_ids = tracker.active_session_ids();
        let mut sorted = active_ids.clone();
        sorted.sort();
        assert_eq!(sorted, vec!["a".to_string(), "b".to_string()]);
    }
    #[test]
    fn backfills_creation_time() {
        let mut tracker = SessionTracker::new();
        let mut ev = mk_event(
            "mid-session",
            None,
            AgentLifecycle::PreToolUse {
                tool_name: "edit".to_string(),
                input: serde_json::Value::Null,
                prompt: None,
            },
        );
        // Daemon started at now, but real session started 300,000ms earlier
        let real_start = ev.timestamp_ms - 300_000;
        ev.extensions.insert(
            "session_created_at_ms".to_string(),
            serde_json::json!(real_start),
        );
        tracker.track(&ev);

        let info = tracker
            .get("claude-code", "mid-session")
            .expect("session found");
        assert_eq!(info.created_at_ms, real_start);
        assert_eq!(info.updated_at_ms, ev.timestamp_ms);
        assert_eq!(info.status, NormalizedStatus::Running);
    }
    #[test]
    fn same_session_id_on_two_harnesses_stays_independent() {
        let mut tracker = SessionTracker::new();
        let mut a = mk_event(
            "shared",
            None,
            AgentLifecycle::SessionStart {
                resume_command: None,
            },
        );
        a.harness = "claude-code".to_string();
        let mut b = mk_event(
            "shared",
            None,
            AgentLifecycle::SessionStart {
                resume_command: None,
            },
        );
        b.harness = "codex".to_string();
        tracker.track(&a);
        tracker.track(&b);
        assert_eq!(tracker.len(), 2);
        let info_a = tracker
            .get("claude-code", "shared")
            .expect("harness A session");
        let info_b = tracker.get("codex", "shared").expect("harness B session");
        assert_eq!(info_a.harness, "claude-code");
        assert_eq!(info_b.harness, "codex");
        // An event for harness A afterwards must not touch harness B's record.
        let mut a2 = mk_event(
            "shared",
            None,
            AgentLifecycle::Stop {
                summary: None,
                success: true,
            },
        );
        a2.harness = "claude-code".to_string();
        tracker.track(&a2);
        assert_eq!(tracker.len(), 2);
        assert_eq!(tracker.get("claude-code", "shared").unwrap().event_count, 2);
        assert_eq!(tracker.get("codex", "shared").unwrap().event_count, 1);
    }

    #[test]
    fn idle_sessions_expire_on_insert_sweep() {
        let mut tracker = SessionTracker::new();
        let mut old = mk_event(
            "old",
            None,
            AgentLifecycle::SessionStart {
                resume_command: None,
            },
        );
        old.timestamp_ms = SessionTracker::now_ms() - SESSION_TTL_MS - 1000;
        tracker.track(&old);
        // The session just written is never expired by its own insert.
        assert!(tracker.get("claude-code", "old").is_some());
        // The next insert sweeps it once it is idle past the TTL.
        tracker.track(&mk_event(
            "fresh",
            None,
            AgentLifecycle::SessionStart {
                resume_command: None,
            },
        ));
        assert!(tracker.get("claude-code", "old").is_none());
        assert!(tracker.get("claude-code", "fresh").is_some());
    }

    #[test]
    fn held_sessions_survive_expiry_sweep_while_held() {
        let mut tracker = SessionTracker::new();
        let mut held = mk_event(
            "held",
            None,
            AgentLifecycle::SessionStart {
                resume_command: None,
            },
        );
        held.timestamp_ms = SessionTracker::now_ms() - SESSION_TTL_MS - 1000;
        tracker.track(&held);
        let hold_id = tracker
            .get("claude-code", "held")
            .expect("held session")
            .last_event
            .as_ref()
            .expect("last event")
            .event_id
            .clone();
        // Protected while its hold is live.
        assert_eq!(
            tracker.evict_idle_expired(SessionTracker::now_ms(), &[hold_id]),
            0
        );
        assert!(tracker.get("claude-code", "held").is_some());
        // Expires once the hold is gone.
        assert_eq!(tracker.evict_idle_expired(SessionTracker::now_ms(), &[]), 1);
        assert!(tracker.get("claude-code", "held").is_none());
    }
}
