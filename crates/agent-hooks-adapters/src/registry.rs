use agent_hooks_protocol::RawHookInput;

use crate::declarative::DeclarativeAdapter;
use crate::manifest::AdapterManifest;
use crate::traits::{normalize_hint, HarnessAdapter};

/// Registry of all harness adapters.
///
/// Provides normalization for `get(id_or_alias)` and auto-detection from raw input.
pub struct AdapterRegistry;
impl Default for AdapterRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl AdapterRegistry {
    pub fn new() -> Self {
        Self
    }

    /// All tiers load here: embedded < user (`~/.config/agent-hooks/adapters`)
    /// < project (`.agent-hooks/adapters`), ordered so first-match lookup
    /// gives project > user > embedded. `extends` chains resolve after the
    /// full set loads so a base may come from any tier; every returned
    /// manifest is fully materialized (`extends: None`).
    fn manifest_adapters() -> Vec<AdapterManifest> {
        let raw = AdapterManifest::load_tiered();
        match AdapterManifest::resolve_all(&raw) {
            Ok(resolved) => resolved,
            Err(e) => {
                tracing::warn!(
                    "adapter extends resolution failed ({e}); using unresolved manifests"
                );
                raw
            }
        }
    }

    /// All registered adapters in deterministic order.
    ///
    /// Every adapter is a [`DeclarativeAdapter`] built from an embedded
    /// [`AdapterManifest`]; there are no hardcoded adapters.
    pub fn all() -> Vec<Box<dyn HarnessAdapter>> {
        let mut out: Vec<Box<dyn HarnessAdapter>> = Vec::new();
        for m in Self::manifest_adapters() {
            let default_ver = m.default_version.clone();
            if let Ok(adapter) = DeclarativeAdapter::new(m, default_ver) {
                out.push(Box::new(adapter));
            }
        }
        out
    }

    /// Normalize an id/alias and look up the matching adapter.
    ///
    /// Normalization: lower-case, trim, `_` → `-`, collapse `--`, strip surrounding `-`.
    pub fn get(id_or_alias: &str) -> Option<Box<dyn HarnessAdapter>> {
        Self::get_versioned(id_or_alias, None)
    }

    pub fn get_versioned(
        id_or_alias: &str,
        adapter_version: Option<&str>,
    ) -> Option<Box<dyn HarnessAdapter>> {
        let norm = normalize_hint(id_or_alias);
        if norm.is_empty() {
            return None;
        }
        // Manifest first (wins)
        for m in Self::manifest_adapters() {
            let id_norm = normalize_hint(&m.id);
            let mut matched = norm == id_norm;
            if !matched {
                for alias in &m.aliases {
                    if norm == normalize_hint(alias) {
                        matched = true;
                        break;
                    }
                }
            }
            if matched {
                let version = adapter_version.unwrap_or(&m.default_version).to_string();
                if m.get_version(&version).is_some() {
                    if let Ok(adapter) = DeclarativeAdapter::new(m, version) {
                        return Some(Box::new(adapter));
                    }
                } else {
                    return None;
                }
            }
        }
        // Manifest lenient substring matching — after all exact matches.
        // Guard against short hints (e.g. "pi" would otherwise substring-match
        // the "anthropic" alias). Exact matches above already handle short ids
        // like "pi", so skipping lenient matching here is safe.
        if norm.len() >= 3 {
            for m in Self::manifest_adapters() {
                let id_norm = normalize_hint(&m.id);
                if id_norm.contains(&norm) || norm.contains(&id_norm) {
                    let version = adapter_version.unwrap_or(&m.default_version).to_string();
                    if m.get_version(&version).is_some() {
                        if let Ok(adapter) = DeclarativeAdapter::new(m.clone(), version) {
                            return Some(Box::new(adapter));
                        }
                    }
                }
                for alias in &m.aliases {
                    let a_norm = normalize_hint(alias);
                    if a_norm.contains(&norm) || norm.contains(&a_norm) {
                        let version = adapter_version.unwrap_or(&m.default_version).to_string();
                        if m.get_version(&version).is_some() {
                            if let Ok(adapter) = DeclarativeAdapter::new(m.clone(), version) {
                                return Some(Box::new(adapter));
                            }
                        }
                    }
                }
            }
        }
        None
    }

    /// Detect the best adapter for a given raw hook input.
    ///
    /// Strategy:
    /// 1. If `harness_hint` normalizes to a known id/alias, return it.
    /// 2. Score DetectSpec of loaded embedded manifests (payload_keys_any OR-groups, event_names against event_hint).
    /// 3. Check event_hint heuristics (camelCase vs dot-notation).
    /// 4. Ask each adapter's `matches_input`.
    /// 5. Fallback: inspect payload JSON for harness-specific keys.
    pub fn detect_from_input(input: &RawHookInput) -> Option<Box<dyn HarnessAdapter>> {
        // 1. Direct hint
        let hint_norm = normalize_hint(&input.harness_hint);
        if !hint_norm.is_empty() {
            if let Some(adapter) = Self::get(&hint_norm) {
                return Some(adapter);
            }
            if let Some(adapter) = Self::get(&input.harness_hint) {
                return Some(adapter);
            }
        }

        // 2. Score manifest DetectSpec before generic event-hint heuristics
        let manifests = Self::manifest_adapters();
        if !manifests.is_empty() {
            let payload_val: Option<serde_json::Value> = {
                let raw = input.raw_payload.trim();
                if raw.is_empty() {
                    None
                } else {
                    serde_json::from_str::<serde_json::Value>(raw).ok()
                }
            };
            let mut best_score: i32 = 0;
            let mut second_best: i32 = 0;
            let mut best_idx: Option<usize> = None;
            for (idx, m) in manifests.iter().enumerate() {
                if let Some(detect) = &m.detect {
                    let mut score: i32 = 0;
                    // harness_hint match
                    for h in &detect.harness_hint {
                        if normalize_hint(h) == hint_norm && !hint_norm.is_empty() {
                            score += 100;
                        }
                    }
                    // payload_keys_any OR-groups
                    for (group_idx, group) in detect.payload_keys_any.iter().enumerate() {
                        let mut group_hit = false;
                        for key in group {
                            let mut exists = false;
                            if let Some(ref val) = payload_val {
                                // support dotted paths strictly for detection? Use simple get
                                if key.contains('.') {
                                    let mut cur = val;
                                    let mut found = true;
                                    for seg in key.split('.') {
                                        if let Some(n) = cur.get(seg) {
                                            cur = n;
                                        } else {
                                            found = false;
                                            break;
                                        }
                                    }
                                    if found && !cur.is_null() {
                                        exists = true;
                                    }
                                } else if val.get(key).is_some() {
                                    exists = true;
                                } else {
                                    // check nested containers like payload/params
                                    for container in
                                        ["payload", "params", "info", "data", "properties"]
                                    {
                                        if let Some(obj) = val.get(container) {
                                            if obj.get(key).is_some() {
                                                exists = true;
                                                break;
                                            }
                                        }
                                    }
                                }
                            }
                            if exists {
                                group_hit = true;
                                break;
                            }
                        }
                        if group_hit {
                            // earlier groups weight higher
                            let weight = 50 - (group_idx as i32 * 20);
                            score += weight.max(10);
                        }
                    }
                    // event_names against event_hint
                    if let Some(ev) = &input.event_hint {
                        let norm_ev = crate::traits::normalize_event(ev);
                        for en in &detect.event_names {
                            if crate::traits::normalize_event(en) == norm_ev {
                                score += 40;
                                break;
                            }
                            if en == ev {
                                score += 40;
                                break;
                            }
                        }
                    }
                    // also check payload event fields for event_names
                    if let Some(ref val) = payload_val {
                        if let Some(ev_str) = crate::traits::resolve_event_name(input, val) {
                            let norm_ev = crate::traits::normalize_event(&ev_str);
                            for en in &detect.event_names {
                                if crate::traits::normalize_event(en) == norm_ev {
                                    score += 30;
                                    break;
                                }
                            }
                        }
                    }
                    if score > best_score {
                        second_best = best_score;
                        best_score = score;
                        best_idx = Some(idx);
                    } else if score > second_best {
                        second_best = score;
                    }
                }
            }
            if let Some(idx) = best_idx {
                if best_score >= 30 && best_score > second_best {
                    let m = manifests.into_iter().nth(idx).unwrap();
                    let ver = m.default_version.clone();
                    if let Ok(adapter) = DeclarativeAdapter::new(m, ver) {
                        return Some(Box::new(adapter));
                    }
                }
            }
        }

        // 3. Event hint heuristics – covers cases where harness_hint is empty but event is distinctive
        if let Some(ev) = &input.event_hint {
            let norm = crate::traits::normalize_event(ev);
            // Copilot
            if matches!(norm.as_str(), "userpromptsubmitted" | "agentstop") {
                return Self::get("copilot");
            }
            // Cursor
            if norm == "beforesubmitprompt" {
                return Self::get("cursor");
            }
            // Gemini
            if norm == "preinvocation" {
                return Self::get("gemini-cli");
            }
            // JCode
            if matches!(
                norm.as_str(),
                "sessionstart" | "turnstart" | "posttool" | "turnend" | "sessionend"
            ) {
                // ambiguous – but if hint empty, prefer jcode when payload suggests JCODE_HOOK_PAYLOAD style
                // check payload for jcode shape later; for now don't eagerly return
            }
            // Opencode dot notation
            if ev.contains('.')
                && (ev.contains("session.")
                    || ev.contains("tool.execute")
                    || ev.contains("permission.asked"))
            {
                return Self::get("opencode");
            }
        }

        // 4. Each adapter's matches_input (via all() which includes manifest adapters)
        for adapter in Self::all() {
            if adapter.matches_input(input) {
                return Some(adapter);
            }
        }

        // 5. Payload inspection fallback
        let raw = input.raw_payload.trim();
        if !raw.is_empty() {
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(raw) {
                // Gemini hooks.json envelope has top-level keys like "BeforeTool", "AfterTool", etc.
                if val.get("BeforeTool").is_some()
                    || val.get("AfterTool").is_some()
                    || val.get("BeforeAgent").is_some()
                {
                    return Self::get("gemini-cli");
                }
                // Codex hooks.json has PreToolUse etc at top level
                if val.get("PreToolUse").is_some() && val.get("SessionStart").is_some() {
                    // could be claude or codex; prefer codex if hint was codex-ish, but fallback to claude
                    if hint_norm.contains("codex") {
                        return Self::get("codex");
                    }
                }
                // Claude payload often has hook_event_name
                if val.get("hook_event_name").is_some() {
                    return Self::get("claude-code");
                }
                // JCode payload may be under JCODE_HOOK_PAYLOAD env var
                if val.get("JCODE_HOOK_PAYLOAD").is_some() {
                    return Self::get("jcode");
                }
            }
        }

        // If raw is empty and we have no hint, try JCODE env var as last resort
        if std::env::var("JCODE_HOOK_PAYLOAD").is_ok() {
            return Self::get("jcode");
        }

        None
    }

    /// Normalize helper exposed for tests / external callers
    pub fn normalize(id: &str) -> String {
        normalize_hint(id)
    }

    /// List all adapter IDs
    pub fn ids() -> Vec<String> {
        Self::all()
            .into_iter()
            .map(|a| a.id().to_string())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_hooks_protocol::{EventContext, RawHookInput};

    fn raw(hint: &str, event: Option<&str>, payload: &str) -> RawHookInput {
        RawHookInput {
            harness_hint: hint.to_string(),
            event_hint: event.map(|s| s.to_string()),
            raw_payload: payload.to_string(),
            context: EventContext::default(),
            request_id: None,
            passthrough_env: Default::default(),
        }
    }

    #[test]
    fn normalize_cases() {
        assert_eq!(AdapterRegistry::normalize("Claude_Code"), "claude-code");
        assert_eq!(AdapterRegistry::normalize("  GEMINI_CLI  "), "gemini-cli");
        assert_eq!(AdapterRegistry::normalize("openai-codex"), "openai-codex");
        assert_eq!(AdapterRegistry::normalize("OH_MY_PI"), "oh-my-pi");
    }

    #[test]
    fn get_aliases() {
        assert!(AdapterRegistry::get("claude").is_some());
        assert_eq!(AdapterRegistry::get("claude").unwrap().id(), "claude-code");
        assert!(AdapterRegistry::get("CLAUDE_CODE").is_some());
        assert!(AdapterRegistry::get("anthropic").is_some());
        assert!(AdapterRegistry::get("openai-codex").is_some());
        assert_eq!(AdapterRegistry::get("openai-codex").unwrap().id(), "codex");
        assert_eq!(AdapterRegistry::get("gemini").unwrap().id(), "gemini-cli");
        assert_eq!(
            AdapterRegistry::get("antigravity").unwrap().id(),
            "antigravity"
        );
        assert_eq!(
            AdapterRegistry::get("google-antigravity").unwrap().id(),
            "antigravity"
        );
        assert!(AdapterRegistry::get("oh-my-pi").is_some());
        assert!(AdapterRegistry::get("omp").is_some());
        assert!(AdapterRegistry::get("github-copilot").is_some());
        assert!(AdapterRegistry::get("cursor").is_some());
        assert!(AdapterRegistry::get("ampcode").is_some());
        assert!(AdapterRegistry::get("unknown-harness").is_none());
    }

    #[test]
    fn detect_from_input_harness_hint() {
        let input = raw(
            "claude",
            None,
            r#"{"session_id":"s1","hook_event_name":"Stop"}"#,
        );
        let detected = AdapterRegistry::detect_from_input(&input).unwrap();
        assert_eq!(detected.id(), "claude-code");

        let input = raw("codex", None, r#"{"session_id":"s1"}"#);
        assert_eq!(
            AdapterRegistry::detect_from_input(&input).unwrap().id(),
            "codex"
        );

        let input = raw("gemini", Some("PreInvocation"), r#"{"session_id":"s1"}"#);
        assert_eq!(
            AdapterRegistry::detect_from_input(&input).unwrap().id(),
            "gemini-cli"
        );

        let input = raw(
            "copilot",
            Some("userPromptSubmitted"),
            r#"{"session_id":"s1"}"#,
        );
        assert_eq!(
            AdapterRegistry::detect_from_input(&input).unwrap().id(),
            "copilot"
        );

        let input = raw(
            "cursor",
            Some("beforeSubmitPrompt"),
            r#"{"session_id":"s1"}"#,
        );
        assert_eq!(
            AdapterRegistry::detect_from_input(&input).unwrap().id(),
            "cursor"
        );

        let input = raw(
            "opencode",
            Some("session.created"),
            r#"{"session_id":"s1"}"#,
        );
        assert_eq!(
            AdapterRegistry::detect_from_input(&input).unwrap().id(),
            "opencode"
        );

        let input = raw("pi", None, r#"{"session_id":"s1"}"#);
        assert_eq!(
            AdapterRegistry::detect_from_input(&input).unwrap().id(),
            "pi"
        );

        let input = raw("jcode", Some("turn_start"), r#"{"session_id":"s1"}"#);
        assert_eq!(
            AdapterRegistry::detect_from_input(&input).unwrap().id(),
            "jcode"
        );

        let input = raw("amp", None, r#"{"session_id":"s1"}"#);
        assert_eq!(
            AdapterRegistry::detect_from_input(&input).unwrap().id(),
            "amp"
        );

        let input = raw("hermes", None, r#"{"session_id":"s1"}"#);
        assert_eq!(
            AdapterRegistry::detect_from_input(&input).unwrap().id(),
            "hermes"
        );
    }

    #[test]
    fn detect_from_input_event_hint_only() {
        // No harness hint, but distinctive event
        let input = raw("", Some("beforeSubmitPrompt"), r#"{"session_id":"s1"}"#);
        assert_eq!(
            AdapterRegistry::detect_from_input(&input).unwrap().id(),
            "cursor"
        );

        let input = raw(
            "",
            Some("userPromptSubmitted"),
            r#"{"session_id":"s1","prompt":"hi"}"#,
        );
        assert_eq!(
            AdapterRegistry::detect_from_input(&input).unwrap().id(),
            "copilot"
        );

        let input = raw(
            "",
            Some("session.created"),
            r#"{"type":"session.created","properties":{"info":{"id":"s1"}}}"#,
        );
        assert_eq!(
            AdapterRegistry::detect_from_input(&input).unwrap().id(),
            "opencode"
        );
    }
}
