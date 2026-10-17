use agent_hooks_protocol::{AgentEvent, RawHookInput};

/// Consumer-provided enrichment for terminal/mux/SSH targets.
///
/// `agent-hooks` normalizes harness lifecycle and derives `NormalizedStatus`.
/// It intentionally does **not** resolve Back2Vibing-specific targets like
/// `bundle_id`, `terminal_tab_id`, precise tmux chain, or SSH-forward state.
/// Those are implementation details of the consumer and belong behind this
/// callback — so the agent-hooks repo stays harness-agnostic.
///
/// Two usage modes:
///
/// 1. **In-process embedding** — Back2Vibing links `agent-hooks-core` as a
///    library and calls `AgentHooksDaemon::with_enricher(enricher)`. The
///    daemon invokes `enrich` synchronously after `adapter.parse_event`
///    and before `tracker.track` + `broadcast.send`, so enriched
///    `AgentEvent.extensions` are visible to every subscriber.
///
/// 2. **Out-of-process daemon** — the daemon has no enricher. Consumers
///    subscribe via JSON-RPC `subscribe` and do client-side enrichment
///    from `event.context` + `event.raw_event` + `event.passthrough_env`
///    (e.g. `back2vibing`'s `terminal_capture::resolve` + `tmux_context::capture`).
///    The `extensions` field is then the rendezvous for consumers that
///    choose to publish an enriched copy back via their own bus.
///
/// Implementations MUST be non-blocking and cheap (<1ms). Spawn work or
/// shell out is the consumer's problem, not the daemon's.
pub trait EventEnricher: Send + Sync + 'static {
    /// Enrich `event` in place. `raw` is the original `RawHookInput`
    /// (including `raw.passthrough_env`) for env-based resolution.
    ///
    /// Insert results into `event.extensions` under well-known keys, e.g.:
    /// - `b2v.terminal_target`: `{ bundle_id, terminal_tab_id, program }`
    /// - `b2v.mux_target`: `{ pane_id, session_name, window_id, client_pid }`
    /// - `b2v.ssh_forward`: `{ host, tty, forwarded_socket }`
    ///
    /// The daemon ignores unknown keys and never interprets them.
    fn enrich(&self, event: &mut AgentEvent, raw: &RawHookInput);
}

impl<F> EventEnricher for F
where
    F: Fn(&mut AgentEvent, &RawHookInput) + Send + Sync + 'static,
{
    fn enrich(&self, event: &mut AgentEvent, raw: &RawHookInput) {
        (self)(event, raw)
    }
}

/// No-op enricher — used when no consumer is registered.
#[derive(Debug, Clone, Default)]
pub struct NoopEnricher;

impl EventEnricher for NoopEnricher {
    fn enrich(&self, _event: &mut AgentEvent, _raw: &RawHookInput) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_hooks_protocol::{EventContext, RawHookInput};
    use std::collections::HashMap;
    use std::sync::Arc;

    fn mk_input(passthrough: &[(&str, &str)]) -> RawHookInput {
        let mut env = HashMap::new();
        for (k, v) in passthrough {
            env.insert(k.to_string(), v.to_string());
        }
        RawHookInput {
            harness_hint: "claude".to_string(),
            event_hint: Some("Stop".to_string()),
            raw_payload: r#"{"session_id":"ses_123","hook_event_name":"Stop"}"#.to_string(),
            context: EventContext {
                cwd: Some("/tmp/proj".into()),
                ..Default::default()
            },
            request_id: None,
            passthrough_env: env,
        }
    }

    #[tokio::test]
    async fn noop_preserves_passthrough_without_extensions() {
        let daemon = crate::daemon::AgentHooksDaemon::new();
        let mut rx = daemon.subscribe();
        let input = mk_input(&[("TERM_PROGRAM", "iTerm.app"), ("KITTY_WINDOW_ID", "42")]);
        daemon.handle_raw_hook(input).await.unwrap();
        let ev = rx.recv().await.unwrap();
        assert_eq!(ev.passthrough_env.get("TERM_PROGRAM").unwrap(), "iTerm.app");
        assert_eq!(ev.passthrough_env.get("KITTY_WINDOW_ID").unwrap(), "42");
        assert!(ev.extensions.is_empty());
        assert_eq!(ev.context.cwd, Some("/tmp/proj".into()));
    }

    #[tokio::test]
    async fn enricher_populates_extensions_and_preserves_passthrough() {
        struct B2vEnricher;
        impl EventEnricher for B2vEnricher {
            fn enrich(&self, event: &mut AgentEvent, raw: &RawHookInput) {
                let bundle = raw
                    .passthrough_env
                    .get("TERM_PROGRAM")
                    .cloned()
                    .unwrap_or_else(|| "unknown".into());
                event.extensions.insert(
                    "b2v.terminal_target".into(),
                    serde_json::json!({"bundle_id": bundle}),
                );
                if let Some(pane) = raw.passthrough_env.get("TMUX_PANE") {
                    event.extensions.insert(
                        "b2v.mux_target".into(),
                        serde_json::json!({"pane_id": pane}),
                    );
                }
            }
        }

        let daemon = crate::daemon::AgentHooksDaemon::with_enricher(Arc::new(B2vEnricher));
        let mut rx = daemon.subscribe();
        let input = mk_input(&[("TERM_PROGRAM", "ghostty"), ("TMUX_PANE", "%5")]);
        daemon.handle_raw_hook(input).await.unwrap();
        let ev = rx.recv().await.unwrap();
        assert_eq!(ev.passthrough_env.get("TERM_PROGRAM").unwrap(), "ghostty");
        assert_eq!(
            ev.extensions.get("b2v.terminal_target").unwrap()["bundle_id"],
            "ghostty"
        );
        assert_eq!(
            ev.extensions.get("b2v.mux_target").unwrap()["pane_id"],
            "%5"
        );
    }

    #[test]
    fn closure_enricher_works() {
        let enricher = |ev: &mut AgentEvent, raw: &RawHookInput| {
            if raw.passthrough_env.contains_key("ITERM_SESSION_ID") {
                ev.extensions
                    .insert("test.iterm".into(), serde_json::json!(true));
            }
        };
        let mut ev = AgentEvent::new(
            "claude-code",
            "ses_1",
            agent_hooks_protocol::AgentLifecycle::Stop {
                summary: None,
                success: true,
            },
            EventContext::default(),
        );
        let raw = mk_input(&[("ITERM_SESSION_ID", "w0t0p0")]);
        enricher.enrich(&mut ev, &raw);
        assert_eq!(
            ev.extensions.get("test.iterm").unwrap(),
            &serde_json::json!(true)
        );
    }
}
