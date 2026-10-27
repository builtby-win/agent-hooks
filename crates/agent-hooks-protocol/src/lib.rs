//! # agent-hooks-protocol
//!
//! Universal protocol and agent event definitions for AI coding agent harnesses.
//! Aligned with modern agent lifecycles (Session, Turn, Tool, Prompt, Subagent).
//!
//! ## Wire-compatibility rule
//!
//! Schema evolution is additive-only: senders may add fields, readers ignore
//! unknown ones (serde default — no `deny_unknown_fields` on inbound event and
//! request shapes). The one deliberate exception is [`PromptDecision`]: a
//! verdict is consumer-authored, so it is parsed strictly and a typo or stale
//! shape errors rather than being silently dropped. A `schema_version`
//! mismatch only warns at the consumer; it never
//! drops the event or the connection. Missing optional fields fill defaults
//! instead of erroring, while required identity fields (`event_id`, `harness`,
//! `session_id`, `prompt_id`, `lifecycle`, …) keep erroring loudly.

pub mod context;
pub mod error;
pub mod event;
pub mod history;
pub mod lifecycle;
pub mod plugin;
pub mod prompt;
pub mod response;
pub mod rpc;

pub use context::EventContext;
pub use error::ProtocolError;
pub use event::{
    hook_line_delivery_id, hook_line_extensions, hook_line_with_extensions, AgentEvent,
    HistoryIdentity, RawHookInput, DELIVERY_ID_EXTENSION, HOOK_LINE_EXTENSIONS_KEY,
    MAX_DELIVERY_ID_LEN, ORIGIN_EXTENSION,
};
pub use history::{
    HistoryCaptureInfo, HistoryEntry, HistoryEntryKind, HistoryErrorBody, HistoryErrorCode,
    HistoryErrorResponse, HistoryMessageHit, HistoryMessagesPage, HistorySearchPage,
    HistorySession, HistorySessionKey, HistorySessionsPage, HistoryStatus, HistoryToolStatus,
    HISTORY_API_VERSION, HISTORY_ASSISTANT_CONTENT_SOURCE, HISTORY_CAPTURE_SOURCE, LOCAL_ORIGIN,
};
pub use lifecycle::{AgentLifecycle, NormalizedStatus};
pub use plugin::{PluginConfigProperty, PluginHookDefinition, PluginHookMatch, PluginManifest};
pub use prompt::{
    PendingPromptInfo, PromptAnswer, PromptDecision, PromptKind, PromptOption, PromptQuestion,
    PromptRequest, PromptResponse, PromptSuggestion, SuggestionKind,
};
pub use response::NormalizedResponse;
pub use rpc::{
    default_socket_path, dev_socket_path, is_valid_env_name, normalize_env_names, Decide,
    DecideScope, EnvRequestedResponse, JsonRpcError, JsonRpcNotification, JsonRpcRequest,
    JsonRpcResponse, PromptHold, PromptRespondParams, SubscriptionFilter, DEFAULT_UNIX_SOCKET_PATH,
    DEFAULT_WINDOWS_PIPE_PATH, DEV_UNIX_SOCKET_PREFIX, ENV_AGENT_HOOKS_SOCKET,
    ENV_REQUESTED_METHOD,
};

#[cfg(test)]
mod wire_tolerance_tests {
    use serde_json::json;

    use crate::{AgentEvent, NormalizedStatus, PromptDecision, RawHookInput, SubscriptionFilter};

    fn minimal_event() -> serde_json::Value {
        json!({
            "event_id": "evt-1",
            "harness": "claude-code",
            "session_id": "ses-1",
            "lifecycle": { "type": "Interrupt" },
        })
    }

    #[test]
    fn hook_payload_with_unknown_fields_parses() {
        // Wire tolerance: a newer shim may add fields; older readers ignore them.
        let v = json!({
            "harness_hint": "claude-code",
            "raw_payload": "{}",
            "future_field": { "nested": [1, 2, 3] },
        });
        let input: RawHookInput = serde_json::from_value(v).expect("unknown fields ignored");
        assert_eq!(input.harness_hint, "claude-code");
    }

    #[test]
    fn subscriber_message_with_unknown_fields_parses() {
        // Wire tolerance: subscriber filter + event each ignore unknown fields.
        let filter: SubscriptionFilter =
            serde_json::from_value(json!({ "decide": true, "future_flag": "x" }))
                .expect("filter ignores unknown fields");
        assert!(filter.decide.enabled());
        let mut v = minimal_event();
        v["future_field"] = json!("added by a newer daemon");
        let event: AgentEvent = serde_json::from_value(v).expect("event ignores unknown fields");
        assert_eq!(event.session_id, "ses-1");
    }

    #[test]
    fn missing_optional_fields_fill_defaults() {
        // Wire tolerance: absent optionals default instead of erroring.
        let event: AgentEvent = serde_json::from_value(minimal_event()).expect("optionals default");
        assert_eq!(event.schema_version, AgentEvent::SCHEMA_VERSION);
        assert_eq!(event.timestamp_ms, 0);
        assert_eq!(event.status, NormalizedStatus::Running);
        assert!(event.turn_id.is_none());
    }

    #[test]
    fn newer_schema_version_still_parses() {
        // Wire tolerance: version skew warns at the consumer, never drops here.
        let mut v = minimal_event();
        v["schema_version"] = json!(AgentEvent::SCHEMA_VERSION + 99);
        let event: AgentEvent = serde_json::from_value(v).expect("newer schema_version parses");
        assert_eq!(event.schema_version, AgentEvent::SCHEMA_VERSION + 99);
    }

    #[test]
    fn missing_identity_fields_error_loudly() {
        // Required identity stays strict: no silent misrouting.
        let mut v = minimal_event();
        v.as_object_mut().unwrap().remove("session_id");
        assert!(serde_json::from_value::<AgentEvent>(v).is_err());
        let v = json!({ "raw_payload": "{}" });
        assert!(serde_json::from_value::<RawHookInput>(v).is_err());
    }

    #[test]
    fn ask_is_fail_safe_allow_false() {
        // Exit2 direction: ask carries allow=false so allow-only consumers deny.
        let ask = PromptDecision::ask(Some("sure?".to_string()));
        assert!(ask.ask);
        assert!(!ask.allow);
        // allow/deny constructor semantics unchanged.
        assert!(PromptDecision::allow().allow);
        assert!(!PromptDecision::deny(None).allow);
    }
}
