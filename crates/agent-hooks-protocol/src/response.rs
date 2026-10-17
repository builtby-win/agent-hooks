use serde::{Deserialize, Serialize};

use crate::prompt::PromptDecision;

/// Typed, harness-aware hook outcome.
///
/// This is the normalized response: what the agent *means*, resolved for the
/// specific harness + event before it is rendered into host JSON. Users match
/// on this instead of parsing `format_response` output; host vernacular
/// (`permissionDecision`, `decision.behavior`, `{}`-means-proceed, …) stays at
/// the rendering edge.
///
/// Wire tolerance: unknown JSON fields are ignored (serde default), so future
/// fields never break older readers at parse time. Variant payloads (`reason`,
/// `question`) stay required — an empty directive is corrupt, so it errors loud.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NormalizedResponse {
    /// Nothing blocks: gate allow, continue-event stop, observe no-op.
    /// Renders per-host (often `{}` — e.g. Codex rejects bare allow shapes).
    Proceed,
    /// Block the gated action with a reason (gate deny).
    Deny { reason: String },
    /// Escalate to a human. Only produced where the host + event supports
    /// ask; otherwise resolution degrades to [`NormalizedResponse::Deny`].
    Ask { question: String },
    /// Keep the agent going (deny on a continue-capability event, e.g. Stop).
    Continue { reason: String },
}

impl NormalizedResponse {
    /// Resolve a gate decision: `ask` survives only when the host + event
    /// supports it (`ask_supported` comes from the adapter's per-event
    /// verdicts); anything undecided fails safe to deny, never to a shape
    /// the host would reject-and-continue.
    pub fn from_gate_decision(decision: &PromptDecision, ask_supported: bool) -> Self {
        // Fail-safe direction (exit2): `PromptDecision::ask` carries allow=false
        // (verified at its constructor), so consumers that only read `allow`
        // fail closed. `ask` survives here only where the host + event supports
        // it; otherwise it degrades to Deny — never to a shape the host would
        // reject-and-continue.
        if decision.ask {
            if ask_supported {
                Self::Ask {
                    question: decision.custom_input.clone().unwrap_or_default(),
                }
            } else {
                Self::Deny {
                    reason: decision.custom_input.clone().unwrap_or_default(),
                }
            }
        } else if decision.allow {
            Self::Proceed
        } else {
            Self::Deny {
                reason: decision.custom_input.clone().unwrap_or_default(),
            }
        }
    }

    /// True when the agent proceeds unblocked.
    pub fn is_proceed(&self) -> bool {
        matches!(self, Self::Proceed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_resolution_matrix() {
        // allow proceeds regardless of ask support
        assert_eq!(
            NormalizedResponse::from_gate_decision(&PromptDecision::allow(), false),
            NormalizedResponse::Proceed
        );
        // deny keeps its reason
        assert_eq!(
            NormalizedResponse::from_gate_decision(
                &PromptDecision::deny(Some("no".to_string())),
                true
            ),
            NormalizedResponse::Deny {
                reason: "no".to_string()
            }
        );
        // ask survives only where supported, else degrades to deny (never a
        // shape the host would reject-and-continue)
        assert_eq!(
            NormalizedResponse::from_gate_decision(
                &PromptDecision::ask(Some("sure?".to_string())),
                true
            ),
            NormalizedResponse::Ask {
                question: "sure?".to_string()
            }
        );
        assert_eq!(
            NormalizedResponse::from_gate_decision(
                &PromptDecision::ask(Some("sure?".to_string())),
                false
            ),
            NormalizedResponse::Deny {
                reason: "sure?".to_string()
            }
        );
    }
}
