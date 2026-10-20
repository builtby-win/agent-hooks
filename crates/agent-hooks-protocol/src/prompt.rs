use serde::{Deserialize, Serialize};

/// Type of interactive prompt requiring human attention or action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptKind {
    /// Permission gate (e.g. bash execution, file modification, web search)
    Permission,
    /// Agent asking a question with possible predefined options or free text
    Question,
}

/// An option in a multiple choice question prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptOption {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
}

/// Single question inside a Question prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptQuestion {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
    pub question: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<PromptOption>,
    #[serde(default)]
    pub multiple: bool,
    #[serde(default)]
    pub custom: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recommended: Option<usize>,
}

/// What accepting a [`PromptSuggestion`] does beyond approving this one call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SuggestionKind {
    /// Remember an allow/deny rule for matching calls.
    Rule,
    /// Switch the harness's permission mode (e.g. auto, accept-edits).
    Mode,
    /// Grant access to extra directories.
    Directory,
    /// A host suggestion this protocol has no category for.
    Other,
}

/// A "remember this" choice the host offered alongside a permission gate.
///
/// Normalised across harnesses: `label` is display-ready, and the host's own
/// payload stays inside the adapter. Select one with
/// [`PromptDecision::allow_with_suggestion`], using its index in
/// [`PromptRequest::suggestions`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptSuggestion {
    pub label: String,
    pub kind: SuggestionKind,
}

/// A structured interactive prompt request from the agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptRequest {
    /// Unique identifier for this prompt (used to route responses)
    pub prompt_id: String,
    pub kind: PromptKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub questions: Vec<PromptQuestion>,
    /// Host-offered "remember this" choices, in the host's order. Empty when
    /// the harness offers none or cannot apply them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub suggestions: Vec<PromptSuggestion>,
}

/// User's answer to a Question prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptAnswer {
    pub question_id: String,
    #[serde(default)]
    pub selected_options: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_input: Option<String>,
}

/// Human decision sent back to the agent hook (the normalized response
/// parameter: answer a held event — one stamped `answerable` with a `hold`
/// carrying its `prompt_id` — via `prompt.respond` with this shape).
///
/// Response contract per prompt kind:
/// - `question` prompt: set `allow` and fill `answers[]` (one entry per
///   question, matched by `question_id`; pick option labels into
///   `selected_options`, or put unmatched text in `custom_input`).
/// - `permission` prompt: `allow` proceeds, `deny` blocks with the reason
///   in `custom_input`.
/// - `ask: true` instead of deciding pauses for a human; it survives only
///   where the event's `hold.verdicts` contains `ask`, otherwise it degrades
///   to deny.
///
/// Precedence when merging sources is deny > ask > allow > defer: an explicit veto
/// always wins, and an escalation beats a default-allow. `ask` means "pause
/// and route this to a human/another agent": the `custom_input` carries the
/// question, and the holder (native harness dialog or daemon mediator) waits
/// for the verdict instead of deciding now.
///
/// Unlike inbound events, a decision is parsed strictly: unknown fields are an
/// error (`deny_unknown_fields`), so the daemon RPC and every local reader
/// (config `respond`, command stdout) accept exactly the same shapes. Missing
/// optional fields still default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptDecision {
    pub allow: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_input: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub answers: Vec<PromptAnswer>,
    /// Escalate instead of deciding. Additive field: older JSON without it
    /// parses as `false`, preserving wire compatibility.
    #[serde(default)]
    pub ask: bool,
    /// Grant beyond this one call — the host remembers the approval.
    ///
    /// Distinct from `allow`, which approves exactly this request. Several
    /// harnesses offer it natively (opencode's `reply: "always"`, pi/omp's
    /// `allow_always`) and a decider that can only say `allow` has to
    /// re-approve the same action forever, one call at a time. Additive:
    /// absent parses as `false`, and a host without the concept renders it
    /// as a plain allow.
    #[serde(default)]
    pub allow_always: bool,
    /// With `allow_always`: apply only this entry of
    /// [`PromptRequest::suggestions`]. `None` applies every suggestion the host
    /// offered (the behaviour before suggestions were selectable); an index
    /// the host no longer has degrades to a plain allow.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggestion: Option<usize>,
    /// No opinion: the harness proceeds exactly as if no hook were installed
    /// (its native permission dialog, its own policy). Distinct from `allow`,
    /// which on hosts like Claude Code's PreToolUse *bypasses* the host's
    /// permission checks. A decider that won't answer a hold defers; so does
    /// the daemon for gates nobody claimed. Additive: absent parses as `false`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub defer: bool,
    /// Text for the harness to put in the model's context, independent of the
    /// verdict. Rendered through the adapter's `inject` template for events
    /// that declare one, dropped silently elsewhere, and never sent with a
    /// deny or ask. Additive: absent parses as `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub additional_context: Option<String>,
}

impl PromptDecision {
    pub fn allow() -> Self {
        Self {
            allow: true,
            custom_input: None,
            answers: Vec::new(),
            ask: false,
            allow_always: false,
            suggestion: None,
            defer: false,
            additional_context: None,
        }
    }

    /// Approve this call and every later one like it, where the host can
    /// remember. Falls back to a plain allow where it cannot.
    pub fn allow_always() -> Self {
        Self {
            allow_always: true,
            suggestion: None,
            ..Self::allow()
        }
    }

    /// Approve this call and apply one of the host's offered suggestions.
    pub fn allow_with_suggestion(index: usize) -> Self {
        Self {
            suggestion: Some(index),
            ..Self::allow_always()
        }
    }

    pub fn deny(reason: Option<String>) -> Self {
        Self {
            allow: false,
            custom_input: reason,
            answers: Vec::new(),
            ask: false,
            allow_always: false,
            suggestion: None,
            defer: false,
            additional_context: None,
        }
    }

    /// Pause and ask: `custom_input` is the question for the decider.
    pub fn ask(question: Option<String>) -> Self {
        Self {
            allow: false,
            ask: true,
            custom_input: question,
            answers: Vec::new(),
            allow_always: false,
            suggestion: None,
            defer: false,
            additional_context: None,
        }
    }

    /// Hand the call back to the harness's own handling (see `defer`).
    pub fn defer() -> Self {
        Self {
            allow: false,
            ask: false,
            custom_input: None,
            answers: Vec::new(),
            allow_always: false,
            suggestion: None,
            defer: true,
            additional_context: None,
        }
    }

    /// Attach context for the model (see `additional_context`).
    pub fn with_additional_context(mut self, context: impl Into<String>) -> Self {
        let context = context.into();
        self.additional_context = (!context.trim().is_empty()).then_some(context);
        self
    }

    /// An explicit veto: neither allow, ask nor defer.
    pub fn is_deny(&self) -> bool {
        !self.allow && !self.ask && !self.defer
    }

    /// Validate this decision against the questions of the prompt it
    /// answers and return the canonical form to deliver.
    ///
    /// Every consumer (daemon mediator, offline `~/.agent-hooks/hooks.json`
    /// respond, command stdout) calls this instead of re-implementing
    /// matching, so an answer means the same thing everywhere.
    ///
    /// - Verdict flags must agree: at most one of `allow` / `ask` / `defer`;
    ///   `allow_always` needs `allow`; `suggestion` needs `allow_always`.
    /// - `answers` are only valid on an allow. An empty `answers` is always
    ///   valid (permission approval, or "no opinion" so the harness falls back
    ///   to its native dialog).
    /// - Non-empty `answers` are matched by `question_id` only (never by
    ///   position), must cover every question exactly once, and are returned
    ///   in the prompt's question order. Unknown, duplicate or missing ids are
    ///   rejected, so a prompt with no questions accepts no answers.
    /// - `selected_options` must be exact labels of that question's options
    ///   (no trimming or case folding), without repeats. A question without
    ///   `multiple` takes one pick in total, counting `custom_input`.
    ///   `custom_input` needs `custom` on the question. An answer must pick
    ///   something.
    ///
    /// Text is never trimmed or truncated. The error is a human-readable,
    /// actionable message naming the offending question or flag.
    pub fn resolve_questions(
        &self,
        questions: &[PromptQuestion],
    ) -> Result<PromptDecision, String> {
        let verdicts = [self.allow, self.ask, self.defer]
            .iter()
            .filter(|set| **set)
            .count();
        if verdicts > 1 {
            return Err(format!(
                "conflicting verdict flags (allow={}, ask={}, defer={}): set at most one",
                self.allow, self.ask, self.defer
            ));
        }
        if self.allow_always && !self.allow {
            return Err("allow_always requires allow=true".to_string());
        }
        if self.suggestion.is_some() && !self.allow_always {
            return Err("suggestion requires allow_always=true".to_string());
        }
        if self.answers.is_empty() {
            return Ok(self.clone());
        }
        if !self.allow {
            return Err("answers are only valid with allow=true".to_string());
        }
        if questions.is_empty() {
            let id = &self.answers[0].question_id;
            return Err(format!(
                "unknown question_id {id:?}: this prompt has no questions, so it accepts no answers"
            ));
        }

        let mut ordered = Vec::with_capacity(questions.len());
        let mut used = vec![false; self.answers.len()];
        for (i, question) in questions.iter().enumerate() {
            if questions[..i].iter().any(|q| q.id == question.id) {
                return Err(format!(
                    "prompt has duplicate question id {:?}; it cannot be answered by id",
                    question.id
                ));
            }
            let mut matches = self
                .answers
                .iter()
                .enumerate()
                .filter(|(_, a)| a.question_id == question.id);
            let Some((idx, answer)) = matches.next() else {
                return Err(format!(
                    "missing answer for question_id {:?}: answer every question or send none",
                    question.id
                ));
            };
            if matches.next().is_some() {
                return Err(format!(
                    "duplicate answers for question_id {:?}",
                    question.id
                ));
            }
            used[idx] = true;
            validate_answer(question, answer)?;
            ordered.push(answer.clone());
        }
        if let Some(idx) = used.iter().position(|u| !u) {
            let known: Vec<&str> = questions.iter().map(|q| q.id.as_str()).collect();
            return Err(format!(
                "unknown question_id {:?}: expected one of {known:?}",
                self.answers[idx].question_id
            ));
        }

        Ok(PromptDecision {
            answers: ordered,
            ..self.clone()
        })
    }
}

/// Check one answer against its question; see
/// [`PromptDecision::resolve_questions`] for the rules.
fn validate_answer(question: &PromptQuestion, answer: &PromptAnswer) -> Result<(), String> {
    let id = &question.id;
    for (i, label) in answer.selected_options.iter().enumerate() {
        if !question.options.iter().any(|o| &o.label == label) {
            let valid: Vec<&str> = question.options.iter().map(|o| o.label.as_str()).collect();
            return Err(format!(
                "question_id {id:?}: {label:?} is not an option (valid: {valid:?})"
            ));
        }
        if answer.selected_options[..i].contains(label) {
            return Err(format!(
                "question_id {id:?}: option {label:?} selected twice"
            ));
        }
    }
    let custom = answer.custom_input.as_deref().filter(|c| !c.is_empty());
    if custom.is_some() && !question.custom {
        return Err(format!(
            "question_id {id:?} does not accept custom_input: pick one of its options"
        ));
    }
    let picks = answer.selected_options.len() + usize::from(custom.is_some());
    if picks == 0 {
        return Err(format!(
            "question_id {id:?}: answer selects nothing (set selected_options or custom_input)"
        ));
    }
    if picks > 1 && !question.multiple {
        return Err(format!(
            "question_id {id:?} is single-select: give exactly one of selected_options or custom_input"
        ));
    }
    Ok(())
}

/// Complete response object sent across RPC to resolve a prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptResponse {
    pub prompt_id: String,
    pub decision: PromptDecision,
}

/// Snapshot of one held question for `prompt.list` (backlog UI).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingPromptInfo {
    pub prompt_id: String,
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question: Option<String>,
    #[serde(default)]
    pub waiters: usize,
    #[serde(default)]
    pub parked: usize,
    #[serde(default)]
    pub age_ms: u64,
    /// Full question payload (options, multi-select, free-text) for rich UIs.
    /// None for holds registered without metadata (older shims) or plain
    /// tool gates with no questions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<PromptRequest>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn option(label: &str) -> PromptOption {
        PromptOption {
            label: label.to_string(),
            description: None,
            preview: None,
        }
    }

    fn question(id: &str, labels: &[&str], multiple: bool, custom: bool) -> PromptQuestion {
        PromptQuestion {
            id: id.to_string(),
            header: None,
            question: format!("{id}?"),
            options: labels.iter().map(|l| option(l)).collect(),
            multiple,
            custom,
            recommended: None,
        }
    }

    fn pick(id: &str, labels: &[&str], custom: Option<&str>) -> PromptAnswer {
        PromptAnswer {
            question_id: id.to_string(),
            selected_options: labels.iter().map(|l| l.to_string()).collect(),
            custom_input: custom.map(str::to_string),
        }
    }

    fn allow_with(answers: Vec<PromptAnswer>) -> PromptDecision {
        PromptDecision {
            answers,
            ..PromptDecision::allow()
        }
    }

    fn two_questions() -> Vec<PromptQuestion> {
        vec![
            question("color", &["Red", "Blue"], false, true),
            question("tags", &["a", "b", "c"], true, false),
        ]
    }

    #[test]
    fn plain_allow_without_answers_is_valid_for_any_prompt() {
        let d = PromptDecision::allow();
        assert_eq!(d.resolve_questions(&[]).unwrap(), d);
        assert_eq!(d.resolve_questions(&two_questions()).unwrap(), d);
    }

    #[test]
    fn deny_ask_defer_without_answers_pass_through_unchanged() {
        for d in [
            PromptDecision::deny(Some("no".to_string())),
            PromptDecision::ask(Some("why?".to_string())),
            PromptDecision::defer(),
            PromptDecision::allow_always(),
            PromptDecision::allow_with_suggestion(1),
        ] {
            assert_eq!(d.resolve_questions(&two_questions()).unwrap(), d);
        }
    }

    #[test]
    fn answers_are_reordered_to_question_order_by_id() {
        let d = allow_with(vec![
            pick("tags", &["c", "a"], None),
            pick("color", &["Blue"], None),
        ]);
        let resolved = d.resolve_questions(&two_questions()).unwrap();
        let ids: Vec<&str> = resolved
            .answers
            .iter()
            .map(|a| a.question_id.as_str())
            .collect();
        assert_eq!(ids, ["color", "tags"]);
        assert_eq!(resolved.answers[1].selected_options, ["c", "a"]);
    }

    #[test]
    fn other_fields_survive_resolution() {
        let d = PromptDecision {
            custom_input: Some("note".to_string()),
            additional_context: Some("ctx".to_string()),
            ..allow_with(vec![
                pick("color", &["Red"], None),
                pick("tags", &["b"], None),
            ])
        };
        let resolved = d.resolve_questions(&two_questions()).unwrap();
        assert_eq!(resolved.custom_input.as_deref(), Some("note"));
        assert_eq!(resolved.additional_context.as_deref(), Some("ctx"));
        assert!(resolved.allow);
    }

    #[test]
    fn rejects_unknown_question_id() {
        let d = allow_with(vec![
            pick("color", &["Red"], None),
            pick("tags", &["a"], None),
            pick("ghost", &["x"], None),
        ]);
        let err = d.resolve_questions(&two_questions()).unwrap_err();
        assert!(err.contains("ghost"), "{err}");
    }

    #[test]
    fn rejects_duplicate_answer_ids() {
        let d = allow_with(vec![
            pick("color", &["Red"], None),
            pick("color", &["Blue"], None),
            pick("tags", &["a"], None),
        ]);
        let err = d.resolve_questions(&two_questions()).unwrap_err();
        assert!(err.contains("duplicate") && err.contains("color"), "{err}");
    }

    #[test]
    fn rejects_missing_answer_ids() {
        let d = allow_with(vec![pick("color", &["Red"], None)]);
        let err = d.resolve_questions(&two_questions()).unwrap_err();
        assert!(err.contains("missing") && err.contains("tags"), "{err}");
    }

    #[test]
    fn rejects_duplicate_question_ids_in_the_prompt() {
        let qs = vec![
            question("q", &["x"], false, false),
            question("q", &["x"], false, false),
        ];
        let err = allow_with(vec![pick("q", &["x"], None)])
            .resolve_questions(&qs)
            .unwrap_err();
        assert!(err.contains("duplicate question id"), "{err}");
    }

    #[test]
    fn does_not_fall_back_to_position() {
        // Right count, wrong ids: positional matching would accept this.
        let d = allow_with(vec![pick("0", &["Red"], None), pick("1", &["a"], None)]);
        assert!(d.resolve_questions(&two_questions()).is_err());
    }

    #[test]
    fn rejects_invalid_option_label_exactly() {
        for bad in ["Green", "red", " Red", "Red "] {
            let d = allow_with(vec![
                pick("color", &[bad], None),
                pick("tags", &["a"], None),
            ]);
            let err = d.resolve_questions(&two_questions()).unwrap_err();
            assert!(
                err.contains("not an option") && err.contains("Red"),
                "{bad:?}: {err}"
            );
        }
    }

    #[test]
    fn rejects_repeated_option_pick() {
        let d = allow_with(vec![
            pick("color", &["Red"], None),
            pick("tags", &["a", "a"], None),
        ]);
        let err = d.resolve_questions(&two_questions()).unwrap_err();
        assert!(err.contains("selected twice"), "{err}");
    }

    #[test]
    fn rejects_multiple_picks_on_single_select() {
        let d = allow_with(vec![
            pick("color", &["Red", "Blue"], None),
            pick("tags", &["a"], None),
        ]);
        let err = d.resolve_questions(&two_questions()).unwrap_err();
        assert!(
            err.contains("single-select") && err.contains("color"),
            "{err}"
        );

        // An option plus custom text is also two picks.
        let d = allow_with(vec![
            pick("color", &["Red"], Some("teal")),
            pick("tags", &["a"], None),
        ]);
        assert!(d.resolve_questions(&two_questions()).is_err());
    }

    #[test]
    fn multi_select_accepts_several_picks_and_custom_when_permitted() {
        let qs = vec![question("tags", &["a", "b"], true, true)];
        let d = allow_with(vec![pick("tags", &["a", "b"], Some("mine"))]);
        assert_eq!(d.resolve_questions(&qs).unwrap(), d);
    }

    #[test]
    fn rejects_custom_input_when_not_permitted() {
        let d = allow_with(vec![
            pick("color", &["Red"], None),
            pick("tags", &[], Some("zzz")),
        ]);
        let err = d.resolve_questions(&two_questions()).unwrap_err();
        assert!(
            err.contains("custom_input") && err.contains("tags"),
            "{err}"
        );
    }

    #[test]
    fn custom_only_answer_is_valid_when_permitted_and_text_is_verbatim() {
        let text = "  teal \n with  spacing ";
        let d = allow_with(vec![
            pick("color", &[], Some(text)),
            pick("tags", &["b"], None),
        ]);
        let resolved = d.resolve_questions(&two_questions()).unwrap();
        assert_eq!(resolved.answers[0].custom_input.as_deref(), Some(text));
    }

    #[test]
    fn rejects_answer_that_selects_nothing() {
        let d = allow_with(vec![pick("color", &[], None), pick("tags", &["a"], None)]);
        let err = d.resolve_questions(&two_questions()).unwrap_err();
        assert!(
            err.contains("selects nothing") && err.contains("color"),
            "{err}"
        );

        let d = allow_with(vec![
            pick("color", &[], Some("")),
            pick("tags", &["a"], None),
        ]);
        assert!(d.resolve_questions(&two_questions()).is_err());
    }

    #[test]
    fn permission_only_prompt_accepts_no_answers() {
        let d = allow_with(vec![pick("color", &["Red"], None)]);
        let err = d.resolve_questions(&[]).unwrap_err();
        assert!(err.contains("no questions"), "{err}");
    }

    #[test]
    fn rejects_answers_on_non_allow_verdicts() {
        let answers = vec![pick("color", &["Red"], None), pick("tags", &["a"], None)];
        for base in [
            PromptDecision::deny(Some("no".to_string())),
            PromptDecision::ask(None),
            PromptDecision::defer(),
        ] {
            let d = PromptDecision {
                answers: answers.clone(),
                ..base
            };
            let err = d.resolve_questions(&two_questions()).unwrap_err();
            assert!(err.contains("allow=true"), "{err}");
        }
    }

    #[test]
    fn rejects_conflicting_verdict_flags() {
        let cases = [
            PromptDecision {
                ask: true,
                ..PromptDecision::allow()
            },
            PromptDecision {
                defer: true,
                ..PromptDecision::allow()
            },
            PromptDecision {
                defer: true,
                ..PromptDecision::ask(None)
            },
            PromptDecision {
                allow_always: true,
                ..PromptDecision::deny(None)
            },
            PromptDecision {
                allow_always: true,
                ..PromptDecision::defer()
            },
            PromptDecision {
                suggestion: Some(0),
                ..PromptDecision::allow()
            },
        ];
        for d in cases {
            // Conflicts are rejected with or without a question list.
            assert!(d.resolve_questions(&[]).is_err(), "{d:?}");
            assert!(d.resolve_questions(&two_questions()).is_err(), "{d:?}");
        }
    }

    #[test]
    fn decision_rejects_unknown_fields_but_defaults_missing_ones() {
        // Verdicts are consumer-authored (config `respond`, command stdout,
        // `prompt.respond`), so a typo or a stale `{decision, reason}` shape
        // must fail loudly instead of silently dropping the field.
        for bad in [
            serde_json::json!({"allow": true, "reason": "stale"}),
            serde_json::json!({"allow": true, "decision": "allow"}),
            serde_json::json!({"allow": true, "allow_alwayss": true}),
        ] {
            let err = serde_json::from_value::<PromptDecision>(bad).unwrap_err();
            assert!(err.to_string().contains("unknown field"), "{err}");
        }
        // Every optional field still defaults when absent, and every
        // serialized decision parses back.
        let d: PromptDecision = serde_json::from_value(serde_json::json!({"allow": true})).unwrap();
        assert_eq!(d, PromptDecision::allow());
        let full = PromptDecision {
            custom_input: Some("x".to_string()),
            additional_context: Some("ctx".to_string()),
            ..PromptDecision::allow_with_suggestion(2)
        };
        let back: PromptDecision =
            serde_json::from_str(&serde_json::to_string(&full).unwrap()).unwrap();
        assert_eq!(back, full);
    }
}
