use std::path::{Path, PathBuf};

use agent_hooks_history::{
    init_store, resolve_history_dir, Capture, CaptureEntry, HistoryError, HistoryReader,
    HistoryWriter, MessagesQuery, RecordOutcome, SessionsQuery, DB_FILE, SCHEMA_VERSION,
};
use agent_hooks_protocol::event::cap_chars;
use agent_hooks_protocol::{
    AgentEvent, AgentLifecycle, EventContext, HistoryEntryKind, HistoryErrorCode, HistoryIdentity,
    HistoryMessagesPage, HistoryToolStatus, LOCAL_ORIGIN,
};
use rusqlite::Connection;
use tempfile::TempDir;

fn store() -> (TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("history");
    init_store(&dir).unwrap();
    (tmp, dir)
}

fn raw(dir: &Path) -> Connection {
    Connection::open(dir.join(DB_FILE)).unwrap()
}

fn count(dir: &Path, table: &str) -> i64 {
    raw(dir)
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

fn base(session: &str, delivery: &str, ts: u64, entry: Option<CaptureEntry>) -> Capture {
    Capture {
        origin: LOCAL_ORIGIN.into(),
        harness: "claude-code".into(),
        session_id: session.into(),
        parent_session_id: None,
        workspace: None,
        title: None,
        status: "running".into(),
        delivery_id: delivery.into(),
        timestamp_ms: ts,
        turn_id: None,
        entry,
    }
}

fn user(text: &str, msg_id: Option<&str>) -> CaptureEntry {
    CaptureEntry {
        kind: HistoryEntryKind::User,
        text: Some(text.into()),
        native_message_id: msg_id.map(Into::into),
        tool_call_id: None,
        tool_name: None,
        tool_status: None,
    }
}

fn assistant(text: &str, msg_id: Option<&str>) -> CaptureEntry {
    CaptureEntry {
        kind: HistoryEntryKind::Assistant,
        text: Some(text.into()),
        native_message_id: msg_id.map(Into::into),
        tool_call_id: None,
        tool_name: None,
        tool_status: None,
    }
}

fn tool(call_id: Option<&str>, status: HistoryToolStatus) -> CaptureEntry {
    CaptureEntry {
        kind: HistoryEntryKind::Tool,
        text: None,
        native_message_id: None,
        tool_call_id: call_id.map(Into::into),
        tool_name: Some("Bash".into()),
        tool_status: Some(status),
    }
}

fn tool_text(call_id: &str, status: HistoryToolStatus, text: Option<&str>) -> CaptureEntry {
    CaptureEntry {
        kind: HistoryEntryKind::Tool,
        text: text.map(Into::into),
        native_message_id: None,
        tool_call_id: Some(call_id.into()),
        tool_name: Some("Bash".into()),
        tool_status: Some(status),
    }
}

fn msgs(dir: &Path, session: &str) -> HistoryMessagesPage {
    HistoryReader::open(dir)
        .unwrap()
        .messages(&MessagesQuery {
            session: session.into(),
            harness: None,
            origin: None,
            after: None,
            before: None,
            limit: None,
            kinds: Vec::new(),
        })
        .unwrap()
}

fn mq(session: &str, after: Option<String>, limit: Option<u32>) -> MessagesQuery {
    MessagesQuery {
        session: session.into(),
        harness: None,
        origin: None,
        after,
        before: None,
        limit,
        kinds: Vec::new(),
    }
}

fn mqb(session: &str, before: i64, limit: Option<u32>) -> MessagesQuery {
    MessagesQuery {
        session: session.into(),
        harness: None,
        origin: None,
        after: None,
        before: Some(before),
        limit,
        kinds: Vec::new(),
    }
}

fn mqk(
    session: &str,
    kinds: Vec<HistoryEntryKind>,
    after: Option<String>,
    limit: Option<u32>,
) -> MessagesQuery {
    MessagesQuery {
        session: session.into(),
        harness: None,
        origin: None,
        after,
        before: None,
        limit,
        kinds,
    }
}

fn native_event(session: &str, lifecycle: AgentLifecycle) -> AgentEvent {
    let mut ev = AgentEvent::new("claude-code", session, lifecycle, EventContext::default());
    ev.history_identity = Some(HistoryIdentity {
        native_session: true,
        ..Default::default()
    });
    ev
}

// ---------------------------------------------------------------- projector

#[test]
fn from_event_requires_native_session() {
    let mut ev = native_event(
        "s1",
        AgentLifecycle::UserPromptSubmit {
            user_prompt: Some("hi".into()),
        },
    );
    assert!(Capture::from_event(&ev, LOCAL_ORIGIN).is_some());
    ev.history_identity = Some(HistoryIdentity::default());
    assert!(Capture::from_event(&ev, LOCAL_ORIGIN).is_none());
    ev.history_identity = None;
    assert!(Capture::from_event(&ev, LOCAL_ORIGIN).is_none());
}

#[test]
fn from_event_turn_id_is_native_only() {
    let mut ev = native_event(
        "s1",
        AgentLifecycle::UserPromptSubmit {
            user_prompt: Some("hi".into()),
        },
    );
    // The event-level (possibly synthesized) turn id must not leak in.
    ev.turn_id = Some("synthetic-turn".into());
    let cap = Capture::from_event(&ev, LOCAL_ORIGIN).unwrap();
    assert_eq!(cap.turn_id, None);
    ev.history_identity.as_mut().unwrap().native_turn_id = Some("t-native".into());
    let cap = Capture::from_event(&ev, LOCAL_ORIGIN).unwrap();
    assert_eq!(cap.turn_id.as_deref(), Some("t-native"));
}

#[test]
fn from_event_delivery_id_is_event_id_and_origin_passes_through() {
    let ev = native_event("s1", AgentLifecycle::PostToolBatch);
    let cap = Capture::from_event(&ev, "remote-a").unwrap();
    assert_eq!(cap.delivery_id, ev.event_id);
    assert_eq!(cap.origin, "remote-a");
}

#[test]
fn from_event_projects_entries_and_metadata_only() {
    let mut ev = native_event(
        "s1",
        AgentLifecycle::PreToolUse {
            tool_name: "Bash".into(),
            input: serde_json::json!({"command": "secret"}),
            prompt: None,
        },
    );
    ev.history_identity.as_mut().unwrap().native_tool_call_id = Some("call-1".into());
    ev.history_identity.as_mut().unwrap().native_message_id = Some("m-1".into());
    ev.context.cwd = Some(PathBuf::from("/work/space"));
    ev.session_title = Some("T".into());
    ev.parent_session_id = Some("p".into());
    let cap = Capture::from_event(&ev, LOCAL_ORIGIN).unwrap();
    assert_eq!(cap.workspace.as_deref(), Some("/work/space"));
    assert_eq!(cap.title.as_deref(), Some("T"));
    assert_eq!(cap.parent_session_id.as_deref(), Some("p"));
    assert_eq!(cap.status, "running");
    let entry = cap.entry.unwrap();
    assert_eq!(entry.kind, HistoryEntryKind::Tool);
    assert_eq!(entry.tool_status, Some(HistoryToolStatus::Running));
    assert_eq!(entry.tool_call_id.as_deref(), Some("call-1"));
    assert_eq!(entry.tool_name.as_deref(), Some("Bash"));
    assert_eq!(entry.text.as_deref(), Some(r#"{"command":"secret"}"#));
    assert_eq!(entry.native_message_id, None);

    let done = AgentLifecycle::PostToolUse {
        tool_name: "Bash".into(),
        output_preview: Some("out".into()),
    };
    let failed = AgentLifecycle::PostToolUseFailure {
        tool_name: "Bash".into(),
        error: "boom".into(),
    };
    for (lc, st, text) in [
        (done, HistoryToolStatus::Completed, Some("out")),
        (failed, HistoryToolStatus::Failed, Some("boom")),
    ] {
        let cap = Capture::from_event(&native_event("s1", lc), LOCAL_ORIGIN).unwrap();
        let e = cap.entry.unwrap();
        assert_eq!(
            (e.kind, e.tool_status, e.text.as_deref()),
            (HistoryEntryKind::Tool, Some(st), text)
        );
    }

    let notif = native_event(
        "s1",
        AgentLifecycle::Notification {
            message: "m".into(),
            title: None,
            notification_type: None,
        },
    );
    assert!(Capture::from_event(&notif, LOCAL_ORIGIN)
        .unwrap()
        .entry
        .is_none());
    let sub = native_event(
        "s1",
        AgentLifecycle::SubagentStop {
            subagent_id: "x".into(),
            success: true,
            summary: Some("sum".into()),
        },
    );
    assert!(Capture::from_event(&sub, LOCAL_ORIGIN)
        .unwrap()
        .entry
        .is_none());
}

#[test]
fn from_event_message_ids_only_on_user_and_assistant() {
    let mut ev = native_event(
        "s1",
        AgentLifecycle::Stop {
            summary: Some("done".into()),
            success: true,
        },
    );
    let id = ev.history_identity.as_mut().unwrap();
    id.native_message_id = Some("m-9".into());
    id.native_tool_call_id = Some("c-9".into());
    let e = Capture::from_event(&ev, LOCAL_ORIGIN)
        .unwrap()
        .entry
        .unwrap();
    assert_eq!(e.kind, HistoryEntryKind::Assistant);
    assert_eq!(e.native_message_id.as_deref(), Some("m-9"));
    assert_eq!(e.tool_call_id, None);
}

#[test]
fn stop_without_nonblank_summary_adds_no_assistant_row() {
    let (_t, dir) = store();
    let mut w = HistoryWriter::open(&dir).unwrap();
    for summary in [None, Some("   \n\t ".to_string())] {
        let ev = native_event(
            "s1",
            AgentLifecycle::Stop {
                summary,
                success: true,
            },
        );
        let cap = Capture::from_event(&ev, LOCAL_ORIGIN).unwrap();
        assert!(cap.entry.is_none());
        w.record(&cap).unwrap();
    }
    assert_eq!(count(&dir, "sessions"), 1);
    assert_eq!(count(&dir, "entries"), 0);
    assert!(msgs(&dir, "s1").items.is_empty());

    // Whitespace-only prompts are skipped too.
    let ev = native_event(
        "s1",
        AgentLifecycle::UserPromptSubmit {
            user_prompt: Some("  ".into()),
        },
    );
    assert!(Capture::from_event(&ev, LOCAL_ORIGIN)
        .unwrap()
        .entry
        .is_none());
}

// ------------------------------------------------------------------- writer

#[test]
fn duplicate_delivery_is_a_complete_noop() {
    let (_t, dir) = store();
    let mut w = HistoryWriter::open(&dir).unwrap();
    let mut c = base("s1", "d1", 100, Some(user("hello", None)));
    assert_eq!(w.record(&c).unwrap(), RecordOutcome::Stored);
    c.timestamp_ms = 999;
    c.title = Some("changed".into());
    c.status = "idle".into();
    c.entry = Some(user("different", None));
    let seq_before = last_seq(&dir);
    let persisted_before = persisted_ms(&dir);
    assert_eq!(w.record(&c).unwrap(), RecordOutcome::Duplicate);

    let r = HistoryReader::open(&dir).unwrap();
    let s = &r.sessions(&SessionsQuery::default()).unwrap().items[0];
    assert_eq!(
        (s.last_seen_ms, s.status.as_str(), s.title.clone()),
        (100, "running", None)
    );
    let items = msgs(&dir, "s1").items;
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].text.as_deref(), Some("hello"));
    assert_eq!(count(&dir, "deliveries"), 1);
    // Nothing changed, including the sequence counter and index metadata.
    assert_eq!(last_seq(&dir), seq_before);
    assert_eq!(persisted_ms(&dir), persisted_before);
}

fn persisted_ms(dir: &Path) -> Option<i64> {
    raw(dir)
        .query_row(
            "SELECT last_persisted_ms FROM index_meta WHERE id = 1",
            [],
            |r| r.get(0),
        )
        .unwrap()
}

#[test]
fn failure_rolls_back_receipt_so_retry_stores() {
    let (_t, dir) = store();
    let mut w = HistoryWriter::open(&dir).unwrap();
    let side = raw(&dir);
    side.execute_batch(
        "CREATE TRIGGER fail_entries BEFORE INSERT ON entries
         BEGIN SELECT RAISE(ABORT, 'injected'); END;",
    )
    .unwrap();
    let c = base("s1", "d1", 5, Some(user("hello", None)));
    assert!(matches!(w.record(&c), Err(HistoryError::Sqlite(_))));
    assert_eq!(count(&dir, "deliveries"), 0);
    assert_eq!(count(&dir, "sessions"), 0);
    assert_eq!(count(&dir, "entries"), 0);
    assert_eq!(last_seq(&dir), 0);

    side.execute_batch("DROP TRIGGER fail_entries").unwrap();
    assert_eq!(w.record(&c).unwrap(), RecordOutcome::Stored);
    assert_eq!(msgs(&dir, "s1").items.len(), 1);
    assert_eq!(last_seq(&dir), 2);
}

#[test]
fn identical_text_with_distinct_identities_stays_distinct() {
    let (_t, dir) = store();
    let mut w = HistoryWriter::open(&dir).unwrap();
    w.record(&base("s1", "d1", 1, Some(user("same", None))))
        .unwrap();
    w.record(&base("s1", "d2", 2, Some(user("same", None))))
        .unwrap();
    w.record(&base("s1", "d3", 3, Some(user("same", Some("m1")))))
        .unwrap();
    w.record(&base("s1", "d4", 4, Some(user("same", Some("m2")))))
        .unwrap();
    let items = msgs(&dir, "s1").items;
    assert_eq!(items.len(), 4);
    assert!(items.windows(2).all(|p| p[0].seq < p[1].seq));
}

#[test]
fn native_message_id_replaces_text_in_place() {
    let (_t, dir) = store();
    let mut w = HistoryWriter::open(&dir).unwrap();
    w.record(&base("s1", "d1", 10, Some(user("draft", Some("m1")))))
        .unwrap();
    w.record(&base("s1", "d2", 11, Some(user("other", None))))
        .unwrap();
    let mut c = base("s1", "d3", 50, Some(user("final", Some("m1"))));
    c.turn_id = Some("t1".into());
    w.record(&c).unwrap();
    let items = msgs(&dir, "s1").items;
    assert_eq!(items.len(), 2);
    let first = &items[0];
    assert_eq!(first.text.as_deref(), Some("final"));
    assert_eq!(first.native_message_id.as_deref(), Some("m1"));
    assert_eq!(first.turn_id.as_deref(), Some("t1"));
    assert_eq!((first.first_seen_ms, first.last_seen_ms), (10, 50));
    assert_eq!(items[1].text.as_deref(), Some("other"));
    assert!(first.seq < items[1].seq);
}

#[test]
fn tool_lifecycle_with_call_id_is_one_row() {
    let (_t, dir) = store();
    let mut w = HistoryWriter::open(&dir).unwrap();
    w.record(&base(
        "s1",
        "d1",
        1,
        Some(tool(Some("c1"), HistoryToolStatus::Running)),
    ))
    .unwrap();
    w.record(&base(
        "s1",
        "d2",
        2,
        Some(tool(Some("c1"), HistoryToolStatus::Completed)),
    ))
    .unwrap();
    let items = msgs(&dir, "s1").items;
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].tool_status, Some(HistoryToolStatus::Completed));
    assert_eq!(items[0].tool_call_id.as_deref(), Some("c1"));
    assert_eq!(items[0].tool_name.as_deref(), Some("Bash"));
    assert_eq!((items[0].first_seen_ms, items[0].last_seen_ms), (1, 2));
}

#[test]
fn tool_input_output_and_error_are_projected() {
    let pre = native_event(
        "s1",
        AgentLifecycle::PreToolUse {
            tool_name: "Bash".into(),
            input: serde_json::json!({"command": "ls"}),
            prompt: None,
        },
    );
    let cap = Capture::from_event(&pre, LOCAL_ORIGIN).unwrap();
    assert_eq!(
        cap.entry.as_ref().unwrap().text.as_deref(),
        Some(r#"{"command":"ls"}"#)
    );

    let post = native_event(
        "s1",
        AgentLifecycle::PostToolUse {
            tool_name: "Bash".into(),
            output_preview: Some("out".into()),
        },
    );
    let cap = Capture::from_event(&post, LOCAL_ORIGIN).unwrap();
    assert_eq!(cap.entry.as_ref().unwrap().text.as_deref(), Some("out"));

    let fail = native_event(
        "s1",
        AgentLifecycle::PostToolUseFailure {
            tool_name: "Bash".into(),
            error: "boom".into(),
        },
    );
    let cap = Capture::from_event(&fail, LOCAL_ORIGIN).unwrap();
    assert_eq!(cap.entry.as_ref().unwrap().text.as_deref(), Some("boom"));

    // Blank payloads stay marker-only, like blank user/assistant text.
    let blank = native_event(
        "s1",
        AgentLifecycle::PostToolUse {
            tool_name: "Bash".into(),
            output_preview: Some("  ".into()),
        },
    );
    let cap = Capture::from_event(&blank, LOCAL_ORIGIN).unwrap();
    assert_eq!(cap.entry.as_ref().unwrap().text, None);
}

fn last_seq(dir: &Path) -> i64 {
    raw(dir)
        .query_row("SELECT last_seq FROM index_meta WHERE id = 1", [], |r| {
            r.get(0)
        })
        .unwrap()
}

const INPUT: &str = r#"{"command":"ls"}"#;

#[test]
fn tool_input_and_output_are_separate_phases_of_one_row() {
    let (_t, dir) = store();
    let mut w = HistoryWriter::open(&dir).unwrap();
    w.record(&base(
        "s1",
        "d1",
        1,
        Some(tool_text("c1", HistoryToolStatus::Running, Some(INPUT))),
    ))
    .unwrap();
    w.record(&base(
        "s1",
        "d2",
        2,
        Some(tool_text("c1", HistoryToolStatus::Completed, Some("out"))),
    ))
    .unwrap();
    let items = msgs(&dir, "s1").items;
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].text, None);
    assert_eq!(items[0].tool_input.as_deref(), Some(INPUT));
    assert_eq!(items[0].tool_output.as_deref(), Some("out"));
    assert_eq!(items[0].tool_status, Some(HistoryToolStatus::Completed));
}

#[test]
fn repeated_pre_post_with_fresh_deliveries_is_idempotent() {
    let (_t, dir) = store();
    let mut w = HistoryWriter::open(&dir).unwrap();
    let pre = |d: &str, ts| {
        base(
            "s1",
            d,
            ts,
            Some(tool_text("c1", HistoryToolStatus::Running, Some(INPUT))),
        )
    };
    let post = |d: &str, ts| {
        base(
            "s1",
            d,
            ts,
            Some(tool_text("c1", HistoryToolStatus::Completed, Some("out"))),
        )
    };
    for c in [pre("d1", 1), post("d2", 2), pre("d3", 1), post("d4", 2)] {
        assert_eq!(w.record(&c).unwrap(), RecordOutcome::Stored);
    }
    let items = msgs(&dir, "s1").items;
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].tool_input.as_deref(), Some(INPUT));
    assert_eq!(items[0].tool_output.as_deref(), Some("out"));
    assert_eq!(items[0].tool_status, Some(HistoryToolStatus::Completed));
    assert_eq!(count(&dir, "entries"), 1);
    assert_eq!(count(&dir, "deliveries"), 4);
    // One session plus one entry: repeated phases allocate nothing.
    assert_eq!(last_seq(&dir), 2);
}

#[test]
fn post_before_pre_keeps_both_phases_and_terminal_status() {
    let (_t, dir) = store();
    let mut w = HistoryWriter::open(&dir).unwrap();
    w.record(&base(
        "s1",
        "d1",
        5,
        Some(tool_text("c1", HistoryToolStatus::Failed, Some("boom"))),
    ))
    .unwrap();
    // Even a running observation stamped later than the terminal one cannot
    // regress the status.
    w.record(&base(
        "s1",
        "d2",
        9,
        Some(tool_text("c1", HistoryToolStatus::Running, Some(INPUT))),
    ))
    .unwrap();
    let items = msgs(&dir, "s1").items;
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].tool_input.as_deref(), Some(INPUT));
    assert_eq!(items[0].tool_output.as_deref(), Some("boom"));
    assert_eq!(items[0].tool_status, Some(HistoryToolStatus::Failed));
}

#[test]
fn stale_tool_input_and_output_cannot_replace_newer_payloads() {
    let (_t, dir) = store();
    let mut w = HistoryWriter::open(&dir).unwrap();
    let rec = |w: &mut HistoryWriter, d: &str, ts, st, text: Option<&str>| {
        w.record(&base("s1", d, ts, Some(tool_text("c1", st, text))))
            .unwrap();
    };
    let state = |dir: &Path| {
        let item = msgs(dir, "s1").items.remove(0);
        (item.tool_input, item.tool_output, item.tool_status)
    };
    let owned = |s: &str| Some(s.to_string());

    rec(&mut w, "d1", 5, HistoryToolStatus::Running, Some("in-5"));
    rec(&mut w, "d2", 3, HistoryToolStatus::Running, Some("in-3"));
    assert_eq!(state(&dir).0, owned("in-5"));
    rec(&mut w, "d3", 9, HistoryToolStatus::Running, Some("in-9"));
    assert_eq!(state(&dir).0, owned("in-9"));

    rec(
        &mut w,
        "d4",
        10,
        HistoryToolStatus::Completed,
        Some("out-10"),
    );
    rec(&mut w, "d5", 8, HistoryToolStatus::Failed, Some("err-8"));
    assert_eq!(
        state(&dir),
        (
            owned("in-9"),
            owned("out-10"),
            Some(HistoryToolStatus::Completed)
        )
    );
    rec(&mut w, "d6", 11, HistoryToolStatus::Failed, Some("err-11"));
    assert_eq!(
        state(&dir),
        (
            owned("in-9"),
            owned("err-11"),
            Some(HistoryToolStatus::Failed)
        )
    );

    // A newer observation without payload never erases what was captured.
    rec(&mut w, "d7", 12, HistoryToolStatus::Completed, None);
    rec(&mut w, "d8", 13, HistoryToolStatus::Running, None);
    assert_eq!(
        state(&dir),
        (
            owned("in-9"),
            owned("err-11"),
            Some(HistoryToolStatus::Completed)
        )
    );
}

#[test]
fn stale_observation_fills_missing_payload_without_regressing_status() {
    let (_t, dir) = store();
    let mut w = HistoryWriter::open(&dir).unwrap();
    w.record(&base(
        "s1",
        "d1",
        20,
        Some(tool_text("c1", HistoryToolStatus::Completed, None)),
    ))
    .unwrap();
    w.record(&base(
        "s1",
        "d2",
        10,
        Some(tool_text("c1", HistoryToolStatus::Failed, Some("late"))),
    ))
    .unwrap();
    let item = msgs(&dir, "s1").items.remove(0);
    assert_eq!(item.tool_output.as_deref(), Some("late"));
    assert_eq!(item.tool_status, Some(HistoryToolStatus::Completed));
}

#[test]
fn marker_then_older_payloads_keep_newest_payload_and_marker_status() {
    let (_t, dir) = store();
    let mut w = HistoryWriter::open(&dir).unwrap();
    let rec = |w: &mut HistoryWriter, d: &str, ts, st, text: Option<&str>| {
        w.record(&base("s1", d, ts, Some(tool_text("c1", st, text))))
            .unwrap();
    };
    // Payload-less markers (ts 30) first, then older payloads arriving out of
    // order: the newest payload (ts 20) wins for each phase, while the
    // terminal status follows the marker (ts 30).
    rec(&mut w, "d1", 30, HistoryToolStatus::Running, None);
    rec(&mut w, "d2", 30, HistoryToolStatus::Completed, None);
    rec(&mut w, "d3", 10, HistoryToolStatus::Running, Some("in-10"));
    rec(&mut w, "d4", 10, HistoryToolStatus::Failed, Some("out-10"));
    rec(&mut w, "d5", 20, HistoryToolStatus::Running, Some("in-20"));
    rec(&mut w, "d6", 20, HistoryToolStatus::Failed, Some("out-20"));
    // And an older payload after the newer one still loses.
    rec(&mut w, "d7", 15, HistoryToolStatus::Running, Some("in-15"));
    rec(&mut w, "d8", 15, HistoryToolStatus::Failed, Some("out-15"));
    let item = msgs(&dir, "s1").items.remove(0);
    assert_eq!(item.tool_input.as_deref(), Some("in-20"));
    assert_eq!(item.tool_output.as_deref(), Some("out-20"));
    assert_eq!(item.tool_status, Some(HistoryToolStatus::Completed));
}

#[test]
fn large_unicode_tool_phases_roundtrip_separately() {
    let (_t, dir) = store();
    let mut w = HistoryWriter::open(&dir).unwrap();
    let input = long_text("input");
    let output = long_text("output");
    w.record(&base(
        "s1",
        "d1",
        1,
        Some(tool_text("c1", HistoryToolStatus::Running, Some(&input))),
    ))
    .unwrap();
    w.record(&base(
        "s1",
        "d2",
        2,
        Some(tool_text("c1", HistoryToolStatus::Completed, Some(&output))),
    ))
    .unwrap();
    let item = msgs(&dir, "s1").items.remove(0);
    assert_eq!(item.text, None);
    assert_eq!(item.tool_input.as_deref(), Some(input.as_str()));
    assert_eq!(item.tool_output.as_deref(), Some(output.as_str()));
}

#[test]
fn late_start_does_not_regress_terminal_tool_status() {
    let (_t, dir) = store();
    let mut w = HistoryWriter::open(&dir).unwrap();
    w.record(&base(
        "s1",
        "d1",
        5,
        Some(tool(Some("c1"), HistoryToolStatus::Failed)),
    ))
    .unwrap();
    w.record(&base(
        "s1",
        "d2",
        4,
        Some(tool(Some("c1"), HistoryToolStatus::Running)),
    ))
    .unwrap();
    let items = msgs(&dir, "s1").items;
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].tool_status, Some(HistoryToolStatus::Failed));
    assert_eq!(items[0].last_seen_ms, 5);
    // Terminal-to-terminal: last write wins.
    w.record(&base(
        "s1",
        "d3",
        6,
        Some(tool(Some("c1"), HistoryToolStatus::Completed)),
    ))
    .unwrap();
    assert_eq!(
        msgs(&dir, "s1").items[0].tool_status,
        Some(HistoryToolStatus::Completed)
    );
}

#[test]
fn tool_markers_without_call_ids_stay_separate() {
    let (_t, dir) = store();
    let mut w = HistoryWriter::open(&dir).unwrap();
    w.record(&base(
        "s1",
        "d1",
        1,
        Some(tool(None, HistoryToolStatus::Running)),
    ))
    .unwrap();
    w.record(&base(
        "s1",
        "d2",
        2,
        Some(tool(None, HistoryToolStatus::Completed)),
    ))
    .unwrap();
    let items = msgs(&dir, "s1").items;
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].tool_status, Some(HistoryToolStatus::Running));
    assert_eq!(items[1].tool_status, Some(HistoryToolStatus::Completed));
}

#[test]
fn session_metadata_coalesces_and_stale_observations_cannot_override() {
    let (_t, dir) = store();
    let mut w = HistoryWriter::open(&dir).unwrap();
    let mut a = base("s1", "d1", 10, None);
    a.title = Some("T".into());
    a.parent_session_id = Some("p".into());
    w.record(&a).unwrap();

    // Older observation: fills the missing workspace, but cannot replace the
    // known title/parent or the newer status.
    let mut b = base("s1", "d2", 5, None);
    b.status = "idle".into();
    b.title = Some("old".into());
    b.workspace = Some("/old".into());
    b.parent_session_id = Some("old-p".into());
    w.record(&b).unwrap();
    let r = HistoryReader::open(&dir).unwrap();
    let s = &r.sessions(&SessionsQuery::default()).unwrap().items[0];
    assert_eq!(s.title.as_deref(), Some("T"));
    assert_eq!(s.workspace.as_deref(), Some("/old"));
    assert_eq!(s.parent_session_id.as_deref(), Some("p"));
    assert_eq!(s.status, "running");
    assert_eq!((s.first_seen_ms, s.last_seen_ms), (10, 10));

    // Newer observation: replaces what it carries, keeps what it omits.
    let mut c = base("s1", "d3", 20, None);
    c.title = Some("T2".into());
    c.status = "completed".into();
    w.record(&c).unwrap();
    let s = &r.sessions(&SessionsQuery::default()).unwrap().items[0];
    assert_eq!((s.title.as_deref(), s.last_seen_ms), (Some("T2"), 20));
    assert_eq!(s.workspace.as_deref(), Some("/old"));
    assert_eq!(s.status, "completed");
}

#[test]
fn rollback_restores_counter_receipts_and_phase_state() {
    let (_t, dir) = store();
    let mut w = HistoryWriter::open(&dir).unwrap();
    w.record(&base("s1", "d1", 1, None)).unwrap();
    assert_eq!(last_seq(&dir), 1);

    // Failure while inserting a new entry: its sequence value, the receipt
    // and the session update all roll back together.
    let side = raw(&dir);
    side.execute_batch(
        "CREATE TRIGGER fail_entries BEFORE INSERT ON entries
         BEGIN SELECT RAISE(ABORT, 'injected'); END;",
    )
    .unwrap();
    let mut c = base(
        "s1",
        "d2",
        9,
        Some(tool_text("c1", HistoryToolStatus::Running, Some(INPUT))),
    );
    c.status = "idle".into();
    assert!(matches!(w.record(&c), Err(HistoryError::Sqlite(_))));
    assert_eq!(last_seq(&dir), 1);
    assert_eq!(count(&dir, "deliveries"), 1);
    assert_eq!(count(&dir, "entries"), 0);
    let r = HistoryReader::open(&dir).unwrap();
    let s = &r.sessions(&SessionsQuery::default()).unwrap().items[0];
    assert_eq!((s.status.as_str(), s.last_seen_ms), ("running", 1));

    side.execute_batch("DROP TRIGGER fail_entries").unwrap();
    assert_eq!(w.record(&c).unwrap(), RecordOutcome::Stored);
    assert_eq!(last_seq(&dir), 2);
    assert_eq!(count(&dir, "deliveries"), 2);

    // Failure while updating an existing entry: payload, status and receipt
    // are unchanged and the retry applies cleanly.
    side.execute_batch(
        "CREATE TRIGGER fail_updates BEFORE UPDATE ON entries
         BEGIN SELECT RAISE(ABORT, 'injected'); END;",
    )
    .unwrap();
    let post = base(
        "s1",
        "d3",
        10,
        Some(tool_text("c1", HistoryToolStatus::Completed, Some("out"))),
    );
    assert!(matches!(w.record(&post), Err(HistoryError::Sqlite(_))));
    assert_eq!(count(&dir, "deliveries"), 2);
    let item = msgs(&dir, "s1").items.remove(0);
    assert_eq!(item.tool_output, None);
    assert_eq!(item.tool_status, Some(HistoryToolStatus::Running));

    side.execute_batch("DROP TRIGGER fail_updates").unwrap();
    assert_eq!(w.record(&post).unwrap(), RecordOutcome::Stored);
    let item = msgs(&dir, "s1").items.remove(0);
    assert_eq!(item.tool_output.as_deref(), Some("out"));
    assert_eq!(item.tool_status, Some(HistoryToolStatus::Completed));
    assert_eq!(last_seq(&dir), 2);

    // The sequence stays a single allocation point shared by sessions and
    // entries: the highest allocated value is the counter.
    let max_seq: i64 = side
        .query_row(
            "SELECT max((SELECT max(first_seq) FROM sessions), (SELECT max(seq) FROM entries))",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(max_seq, last_seq(&dir));
}

#[test]
fn newer_schema_is_refused_by_open_and_record() {
    let (_t, dir) = store();
    let mut w = HistoryWriter::open(&dir).unwrap();
    raw(&dir).pragma_update(None, "user_version", 99).unwrap();
    let err = w.record(&base("s1", "d1", 1, None)).unwrap_err();
    assert!(matches!(
        err,
        HistoryError::UnsupportedNewerSchema {
            found: 99,
            expected: SCHEMA_VERSION
        }
    ));
    assert_eq!(count(&dir, "deliveries"), 0);
    assert!(matches!(
        HistoryWriter::open(&dir),
        Err(HistoryError::UnsupportedNewerSchema { .. })
    ));
    assert!(matches!(
        HistoryReader::open(&dir),
        Err(HistoryError::UnsupportedNewerSchema { .. })
    ));
    assert!(matches!(
        init_store(&dir),
        Err(HistoryError::UnsupportedNewerSchema { .. })
    ));
    assert_eq!(
        HistoryError::UnsupportedNewerSchema {
            found: 99,
            expected: SCHEMA_VERSION
        }
        .to_response()
        .error
        .code,
        HistoryErrorCode::StoreUnavailable
    );
}

#[test]
fn init_is_idempotent_and_keeps_index_id() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("h");
    let first = init_store(&dir).unwrap();
    assert!(first.created);
    assert_eq!(first.schema_version, SCHEMA_VERSION);
    assert_eq!(first.path, dir.join(DB_FILE));
    let id = |d: &Path| -> String {
        raw(d)
            .query_row("SELECT index_id FROM index_meta", [], |r| r.get(0))
            .unwrap()
    };
    let before = id(&dir);
    let second = init_store(&dir).unwrap();
    assert!(!second.created);
    assert_eq!(id(&dir), before);
    assert_eq!(count(&dir, "index_meta"), 1);
}

// ------------------------------------------------------------------- reader

#[test]
fn same_native_session_id_across_origins_and_harnesses_is_separate() {
    let (_t, dir) = store();
    let mut w = HistoryWriter::open(&dir).unwrap();
    let mut a = base("dup", "d1", 1, Some(user("local-claude", None)));
    let mut b = base("dup", "d1", 2, Some(user("remote-claude", None)));
    b.origin = "ssh:box".into();
    let mut c = base("dup", "d1", 3, Some(user("local-codex", None)));
    c.harness = "codex".into();
    for cap in [&mut a, &mut b, &mut c] {
        assert_eq!(w.record(cap).unwrap(), RecordOutcome::Stored);
    }
    let r = HistoryReader::open(&dir).unwrap();
    assert_eq!(
        r.sessions(&SessionsQuery::default()).unwrap().items.len(),
        3
    );

    match r.messages(&mq("dup", None, None)) {
        Err(HistoryError::AmbiguousSession(keys)) => {
            assert_eq!(keys.len(), 3);
            assert_eq!(keys[0].origin, LOCAL_ORIGIN);
        }
        other => panic!("expected ambiguous, got {other:?}"),
    }
    let resp = r
        .messages(&mq("dup", None, None))
        .unwrap_err()
        .to_response();
    assert_eq!(resp.error.code, HistoryErrorCode::AmbiguousSession);
    assert_eq!(resp.error.candidates.len(), 3);

    let mut q = mq("dup", None, None);
    q.harness = Some("claude-code".into());
    assert!(matches!(r.messages(&q), Err(HistoryError::AmbiguousSession(k)) if k.len() == 2));
    q.origin = Some("ssh:box".into());
    let page = r.messages(&q).unwrap();
    assert_eq!(page.session.origin, "ssh:box");
    assert_eq!(page.items[0].text.as_deref(), Some("remote-claude"));
    q.harness = Some("codex".into());
    assert!(matches!(r.messages(&q), Err(HistoryError::NotFound)));

    let sessions = r
        .sessions(&SessionsQuery {
            harness: Some("codex".into()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(sessions.items.len(), 1);
    assert_eq!(sessions.items[0].key.harness, "codex");
}

#[test]
fn sessions_session_filter_matches_exactly_and_composes_with_harness_origin() {
    let (_t, dir) = store();
    let mut w = HistoryWriter::open(&dir).unwrap();
    let mut a = base("dup", "d1", 1, Some(user("local-claude", None)));
    let mut b = base("dup", "d1", 2, Some(user("remote-claude", None)));
    b.origin = "ssh:box".into();
    let mut c = base("dup", "d1", 3, Some(user("local-codex", None)));
    c.harness = "codex".into();
    let mut other = base("other", "d2", 4, Some(user("solo", None)));
    for cap in [&mut a, &mut b, &mut c, &mut other] {
        assert_eq!(w.record(cap).unwrap(), RecordOutcome::Stored);
    }
    let r = HistoryReader::open(&dir).unwrap();
    let sq = |session: Option<&str>, harness: Option<&str>, origin: Option<&str>| SessionsQuery {
        session: session.map(Into::into),
        harness: harness.map(Into::into),
        origin: origin.map(Into::into),
        after: None,
        search: None,
        limit: None,
    };

    // Exact match returns only that session, across origins and harnesses.
    let page = r.sessions(&sq(Some("dup"), None, None)).unwrap();
    assert_eq!(page.items.len(), 3);
    assert!(page.items.iter().all(|s| s.key.session_id == "dup"));
    assert!(page.next_cursor.is_none());

    // The session filter narrows further with harness and origin.
    let page = r.sessions(&sq(Some("dup"), Some("codex"), None)).unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].key.harness, "codex");
    let page = r
        .sessions(&sq(Some("dup"), Some("claude-code"), Some("ssh:box")))
        .unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].key.origin, "ssh:box");
    let page = r
        .sessions(&sq(Some("dup"), Some("codex"), Some("ssh:box")))
        .unwrap();
    assert!(page.items.is_empty());
    assert!(page.next_cursor.is_none());

    // Unknown id is a normal empty page, never an error.
    let page = r.sessions(&sq(Some("does-not-exist"), None, None)).unwrap();
    assert!(page.items.is_empty());
    assert!(page.next_cursor.is_none());

    // Blank filters count as absent: the full list comes back.
    for blank in [Some(""), Some("   ")] {
        let page = r.sessions(&sq(blank, None, None)).unwrap();
        assert_eq!(page.items.len(), 4);
    }
    let page = r.sessions(&sq(Some("  dup  "), None, None)).unwrap();
    assert_eq!(page.items.len(), 3);
}

#[test]
fn sessions_search_is_literal_case_insensitive_and_composes_with_filters() {
    let (_t, dir) = store();
    let mut w = HistoryWriter::open(&dir).unwrap();
    let mut a = base("alpha-001", "d1", 1, None);
    a.workspace = Some("/Users/me/Proj_X".into());
    let mut b = base("beta-002", "d2", 2, None);
    b.title = Some("Fix 100% login".into());
    b.workspace = Some("/tmp/other".into());
    let mut c = base("gamma-003", "d3", 3, None);
    c.harness = "codex".into();
    c.title = Some("Login Refactor".into());
    c.workspace = Some("/home/dev/app".into());
    let mut d = base("delta-004", "d4", 4, None);
    d.origin = "ssh:box".into();
    d.title = Some("a_b".into());
    for cap in [&a, &b, &c, &d] {
        assert_eq!(w.record(cap).unwrap(), RecordOutcome::Stored);
    }
    let r = HistoryReader::open(&dir).unwrap();
    let search = |text: &str| SessionsQuery {
        search: Some(text.into()),
        ..Default::default()
    };
    let ids = |q: &SessionsQuery| -> Vec<String> {
        let page = r.sessions(q).unwrap();
        assert!(page.next_cursor.is_none());
        page.items.into_iter().map(|s| s.key.session_id).collect()
    };

    // Session id, title and workspace each match, ignoring ASCII case.
    assert_eq!(ids(&search("ALPHA")), ["alpha-001"]);
    assert_eq!(ids(&search("proj_x")), ["alpha-001"]);
    assert_eq!(ids(&search("login")), ["beta-002", "gamma-003"]);
    assert_eq!(ids(&search("zzz-no-match")), Vec::<String>::new());

    // `%` and `_` are literal characters, not wildcards.
    assert_eq!(ids(&search("%")), ["beta-002"]);
    assert_eq!(ids(&search("_")), ["alpha-001", "delta-004"]);
    assert_eq!(ids(&search("a_b")), ["delta-004"]);
    assert_eq!(ids(&search("a%b")), Vec::<String>::new());
    assert_eq!(ids(&search("100%")), ["beta-002"]);

    // Blank search is no filter; surrounding whitespace is trimmed.
    for blank in ["", "   "] {
        assert_eq!(ids(&search(blank)).len(), 4);
    }
    assert_eq!(ids(&search("  login  ")), ["beta-002", "gamma-003"]);

    // AND with every other filter.
    let mut q = search("login");
    q.harness = Some("codex".into());
    assert_eq!(ids(&q), ["gamma-003"]);
    q.origin = Some("ssh:box".into());
    assert_eq!(ids(&q), Vec::<String>::new());
    let mut q = search("login");
    q.session = Some("beta-002".into());
    assert_eq!(ids(&q), ["beta-002"]);
    q.session = Some("alpha-001".into());
    assert_eq!(ids(&q), Vec::<String>::new());

    // Search follows the current title: a later observation that sets or
    // renames the title makes the session findable by it (and not by the old).
    assert_eq!(ids(&search("zebra")), Vec::<String>::new());
    let mut a2 = base("alpha-001", "d5", 5, None);
    a2.title = Some("Renamed Zebra".into());
    w.record(&a2).unwrap();
    assert_eq!(ids(&search("zebra")), ["alpha-001"]);
    let mut b2 = base("beta-002", "d6", 6, None);
    b2.title = Some("Now about billing".into());
    w.record(&b2).unwrap();
    assert_eq!(ids(&search("login")), ["gamma-003"]);
    assert_eq!(ids(&search("BILLING")), ["beta-002"]);
    // Workspace from the first observation is still searchable.
    assert_eq!(ids(&search("proj_x")), ["alpha-001"]);
}

#[test]
fn sessions_search_pages_and_cursor_is_tied_to_search() {
    let (_t, dir) = store();
    let mut w = HistoryWriter::open(&dir).unwrap();
    for (i, (id, title)) in [
        ("s1", "login one"),
        ("s2", "other"),
        ("s3", "LOGIN two"),
        ("s4", "login three"),
    ]
    .into_iter()
    .enumerate()
    {
        let mut cap = base(id, &format!("d{i}"), i as u64 + 1, None);
        cap.title = Some(title.into());
        w.record(&cap).unwrap();
    }
    let r = HistoryReader::open(&dir).unwrap();
    let q = |search: Option<&str>, after: Option<String>| SessionsQuery {
        search: search.map(Into::into),
        after,
        limit: Some(2),
        ..Default::default()
    };
    let ids = |page: &agent_hooks_protocol::HistorySessionsPage| -> Vec<String> {
        page.items
            .iter()
            .map(|s| s.key.session_id.clone())
            .collect()
    };

    // Non-matching rows never consume page slots; the cursor resumes after the
    // last returned match. Trimmed search text is the filter identity.
    let p1 = r.sessions(&q(Some("login"), None)).unwrap();
    assert_eq!(ids(&p1), ["s1", "s3"]);
    let cur = p1.next_cursor.clone().unwrap();
    let p2 = r.sessions(&q(Some("  login "), Some(cur.clone()))).unwrap();
    assert_eq!(ids(&p2), ["s4"]);
    assert!(p2.next_cursor.is_none());

    // A cursor is rejected under any different (or missing) search text.
    for other in [Some("LOGIN"), Some("other"), None] {
        assert!(
            matches!(
                r.sessions(&q(other, Some(cur.clone()))),
                Err(HistoryError::InvalidArgument(_))
            ),
            "cursor must not cross search filters: {other:?}"
        );
    }
    // ...and a cursor from an unsearched listing cannot be used with a search.
    let plain = r.sessions(&q(None, None)).unwrap();
    let plain_cur = plain.next_cursor.unwrap();
    for blank in [Some("login"), Some("")] {
        let res = r.sessions(&q(blank, Some(plain_cur.clone())));
        // A blank search is the same filter as no search.
        assert_eq!(res.is_ok(), blank == Some(""), "search {blank:?}");
    }

    // Cursors minted before `search` existed carry no such key: they decode
    // as "no search".
    let mut state: serde_json::Value = serde_json::from_slice(&hex_decode(&plain_cur)).unwrap();
    state["filters"].as_object_mut().unwrap().remove("search");
    let legacy = hex_encode(&serde_json::to_vec(&state).unwrap());
    let page = r.sessions(&q(None, Some(legacy.clone()))).unwrap();
    assert_eq!(ids(&page), ["s3", "s4"]);
    assert!(matches!(
        r.sessions(&q(Some("login"), Some(legacy))),
        Err(HistoryError::InvalidArgument(_))
    ));
}

#[test]
fn messages_unknown_session_not_found_and_known_empty_ok() {
    let (_t, dir) = store();
    let mut w = HistoryWriter::open(&dir).unwrap();
    w.record(&base("empty", "d1", 1, None)).unwrap();
    let r = HistoryReader::open(&dir).unwrap();
    let page = r.messages(&mq("empty", None, None)).unwrap();
    assert!(page.items.is_empty() && page.next_cursor.is_none());
    assert_eq!(page.capture, Default::default());
    assert!(matches!(
        r.messages(&mq("nope", None, None)),
        Err(HistoryError::NotFound)
    ));
    assert_eq!(
        r.messages(&mq("nope", None, None))
            .unwrap_err()
            .to_response()
            .error
            .code,
        HistoryErrorCode::NotFound
    );
}

#[test]
fn pagination_is_stable_and_excludes_rows_added_later() {
    let (_t, dir) = store();
    let mut w = HistoryWriter::open(&dir).unwrap();
    for i in 0..5 {
        w.record(&base(
            "s1",
            &format!("d{i}"),
            i,
            Some(user(&format!("m{i}"), None)),
        ))
        .unwrap();
        w.record(&base(&format!("sess{i}"), &format!("x{i}"), i, None))
            .unwrap();
    }
    let r = HistoryReader::open(&dir).unwrap();

    let p1 = r.messages(&mq("s1", None, Some(2))).unwrap();
    assert_eq!(p1.items.len(), 2);
    let cur = p1.next_cursor.clone().unwrap();

    // Appended after the first page: must not appear on later pages.
    w.record(&base("s1", "late", 99, Some(user("late", None))))
        .unwrap();
    w.record(&base("brand-new", "late2", 99, None)).unwrap();

    let p2 = r.messages(&mq("s1", Some(cur), Some(2))).unwrap();
    let p3 = r
        .messages(&mq("s1", p2.next_cursor.clone(), Some(2)))
        .unwrap();
    let texts: Vec<_> = p1
        .items
        .iter()
        .chain(&p2.items)
        .chain(&p3.items)
        .map(|e| e.text.clone().unwrap())
        .collect();
    assert_eq!(texts, ["m0", "m1", "m2", "m3", "m4"]);
    assert!(p3.next_cursor.is_none());

    // A fresh query sees the new row.
    assert_eq!(r.messages(&mq("s1", None, None)).unwrap().items.len(), 6);

    // Sessions paging: same guarantees, ascending by first_seq.
    let s1 = r
        .sessions(&SessionsQuery {
            limit: Some(4),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(s1.items.len(), 4);
    w.record(&base("brand-new-2", "late3", 100, None)).unwrap();
    let s2 = r
        .sessions(&SessionsQuery {
            limit: Some(4),
            after: s1.next_cursor.clone(),
            ..Default::default()
        })
        .unwrap();
    let ids: Vec<_> = s1
        .items
        .iter()
        .chain(&s2.items)
        .map(|s| s.key.session_id.clone())
        .collect();
    assert_eq!(
        ids,
        [
            "s1",
            "sess0",
            "sess1",
            "sess2",
            "sess3",
            "sess4",
            "brand-new"
        ]
    );
    assert!(s2.next_cursor.is_none());
    assert!(s1.items.windows(2).all(|p| p[0].seq < p[1].seq));
}
#[test]
fn messages_before_returns_newest_window_oldest_first() {
    let (_t, dir) = store();
    let mut w = HistoryWriter::open(&dir).unwrap();
    for i in 0..5 {
        w.record(&base(
            "s1",
            &format!("d{i}"),
            i,
            Some(user(&format!("m{i}"), None)),
        ))
        .unwrap();
    }
    let r = HistoryReader::open(&dir).unwrap();
    let all = r.messages(&mq("s1", None, None)).unwrap();
    assert_eq!(all.items.len(), 5);
    let seq = |i: usize| all.items[i].seq;
    let texts = |page: &HistoryMessagesPage| {
        page.items
            .iter()
            .map(|e| e.text.clone().unwrap())
            .collect::<Vec<_>>()
    };

    // Newest two below the fourth message, still oldest first, one page.
    let page = r.messages(&mqb("s1", seq(3), Some(2))).unwrap();
    assert_eq!(texts(&page), ["m1", "m2"]);
    assert!(page.next_cursor.is_none());

    // A bound past the end reads the tail; a bound at the first row is empty.
    let page = r.messages(&mqb("s1", i64::MAX, Some(2))).unwrap();
    assert_eq!(texts(&page), ["m3", "m4"]);
    let page = r.messages(&mqb("s1", seq(0), None)).unwrap();
    assert!(page.items.is_empty() && page.next_cursor.is_none());
}

#[test]
fn messages_before_is_stable_as_new_rows_land() {
    let (_t, dir) = store();
    let mut w = HistoryWriter::open(&dir).unwrap();
    for i in 0..3 {
        w.record(&base(
            "s1",
            &format!("d{i}"),
            i,
            Some(user(&format!("m{i}"), None)),
        ))
        .unwrap();
    }
    let r = HistoryReader::open(&dir).unwrap();
    let bound = r.messages(&mq("s1", None, None)).unwrap().items[2].seq;
    let first = r.messages(&mqb("s1", bound, None)).unwrap();
    assert_eq!(first.items.len(), 2);

    w.record(&base("s1", "late", 99, Some(user("late", None))))
        .unwrap();
    let second = r.messages(&mqb("s1", bound, None)).unwrap();
    assert_eq!(
        second.items.iter().map(|e| e.seq).collect::<Vec<_>>(),
        first.items.iter().map(|e| e.seq).collect::<Vec<_>>()
    );
}

#[test]
fn messages_before_composes_with_kinds_and_rejects_bad_bounds() {
    let (_t, dir) = store();
    let mut w = HistoryWriter::open(&dir).unwrap();
    w.record(&base("s1", "d0", 0, Some(user("m0", None))))
        .unwrap();
    w.record(&base(
        "s1",
        "d1",
        1,
        Some(tool(Some("c1"), HistoryToolStatus::Completed)),
    ))
    .unwrap();
    w.record(&base("s1", "d2", 2, Some(user("m2", None))))
        .unwrap();
    let r = HistoryReader::open(&dir).unwrap();

    // Kind filtering happens before the window is cut: the chat view sees
    // both user rows even though a tool row sits between them in seq order.
    let mut q = mqb("s1", i64::MAX, None);
    q.kinds = vec![HistoryEntryKind::User, HistoryEntryKind::Assistant];
    let page = r.messages(&q).unwrap();
    assert_eq!(page.items.len(), 2);
    assert!(page.items.iter().all(|e| e.kind == HistoryEntryKind::User));

    // Unknown sessions stay not-found; after+before and non-positive bounds
    // are usage errors, never silent windows.
    assert!(matches!(
        r.messages(&mqb("nope", 10, None)),
        Err(HistoryError::NotFound)
    ));
    let mut both = mqb("s1", 10, None);
    both.after = Some("cursor".into());
    assert!(matches!(
        r.messages(&both),
        Err(HistoryError::InvalidArgument(_))
    ));
    for bad in [0, -5] {
        assert!(matches!(
            r.messages(&mqb("s1", bad, None)),
            Err(HistoryError::InvalidArgument(_))
        ));
    }
}

#[test]
fn pagination_pins_session_resolution() {
    let (_t, dir) = store();
    let mut w = HistoryWriter::open(&dir).unwrap();
    for i in 0..3 {
        w.record(&base("s1", &format!("d{i}"), i, Some(user("x", None))))
            .unwrap();
    }
    let r = HistoryReader::open(&dir).unwrap();
    let p1 = r.messages(&mq("s1", None, Some(1))).unwrap();
    let mut other = base("s1", "o1", 9, Some(user("other", None)));
    other.origin = "elsewhere".into();
    w.record(&other).unwrap();
    // Page two would be ambiguous if resolved afresh.
    let p2 = r.messages(&mq("s1", p1.next_cursor, Some(5))).unwrap();
    assert_eq!(p2.items.len(), 2);
    assert_eq!(p2.session.origin, LOCAL_ORIGIN);
}

#[test]
fn cursor_from_other_index_or_changed_filters_is_rejected() {
    let (_t, dir) = store();
    let mut w = HistoryWriter::open(&dir).unwrap();
    for i in 0..3 {
        w.record(&base("s1", &format!("d{i}"), i, Some(user("x", None))))
            .unwrap();
    }
    let r = HistoryReader::open(&dir).unwrap();
    let cur = r
        .messages(&mq("s1", None, Some(1)))
        .unwrap()
        .next_cursor
        .unwrap();

    let mut changed = mq("s1", Some(cur.clone()), Some(1));
    changed.harness = Some("claude-code".into());
    assert!(matches!(
        r.messages(&changed),
        Err(HistoryError::InvalidArgument(_))
    ));
    assert!(matches!(
        r.messages(&mq("s2", Some(cur.clone()), Some(1))),
        Err(HistoryError::InvalidArgument(_))
    ));
    // A messages cursor is not a sessions cursor.
    assert!(matches!(
        r.sessions(&SessionsQuery {
            after: Some(cur.clone()),
            ..Default::default()
        }),
        Err(HistoryError::InvalidArgument(_))
    ));

    let other_tmp = tempfile::tempdir().unwrap();
    let other = other_tmp.path().join("h");
    init_store(&other).unwrap();
    let mut w2 = HistoryWriter::open(&other).unwrap();
    for i in 0..3 {
        w2.record(&base("s1", &format!("d{i}"), i, Some(user("x", None))))
            .unwrap();
    }
    let r2 = HistoryReader::open(&other).unwrap();
    assert!(matches!(
        r2.messages(&mq("s1", Some(cur), Some(1))),
        Err(HistoryError::InvalidArgument(_))
    ));
    for bad in ["zz", "abc", "", "00ff", "7b7d"] {
        assert!(matches!(
            r.messages(&mq("s1", Some(bad.into()), None)),
            Err(HistoryError::InvalidArgument(_))
        ));
        assert!(matches!(
            r.sessions(&SessionsQuery {
                after: Some(bad.into()),
                ..Default::default()
            }),
            Err(HistoryError::InvalidArgument(_))
        ));
    }
    assert_eq!(
        r.messages(&mq("s1", Some("zz".into()), None))
            .unwrap_err()
            .to_response()
            .error
            .code,
        HistoryErrorCode::InvalidArgument
    );
}

#[test]
fn kind_filter_pages_at_sql_level_and_cursor_is_tied_to_filter() {
    use HistoryEntryKind::{Assistant, Tool, User};

    let (_t, dir) = store();
    let mut w = HistoryWriter::open(&dir).unwrap();
    // Interleaved chat and tool entries; the tool bodies are large so a
    // chat view paging with `user,assistant` must skip them in SQL, before
    // decoding payloads and before the page limit applies.
    let big = "tool-body-".repeat(8 * 1024);
    let entries = [
        user("u0", None),
        tool_text("c0", HistoryToolStatus::Completed, Some(big.as_str())),
        assistant("a0", None),
        user("u1", None),
        tool_text("c1", HistoryToolStatus::Completed, Some(big.as_str())),
        assistant("a1", None),
    ];
    for (i, entry) in entries.into_iter().enumerate() {
        w.record(&base("s1", &format!("d{i}"), i as u64, Some(entry)))
            .unwrap();
    }
    let r = HistoryReader::open(&dir).unwrap();

    // Default (no kinds) is unchanged: every entry, tool bodies intact.
    let all = r.messages(&mq("s1", None, None)).unwrap();
    assert_eq!(
        all.items.iter().map(|e| e.kind).collect::<Vec<_>>(),
        [User, Tool, Assistant, User, Tool, Assistant]
    );
    assert_eq!(all.items[1].tool_output.as_deref(), Some(big.as_str()));
    assert!(all.next_cursor.is_none());

    // Filtered pages skip tool rows: the limit counts filtered rows only, so
    // page boundaries land on chat entries, not on raw row positions.
    let chat = vec![User, Assistant];
    let p1 = r.messages(&mqk("s1", chat.clone(), None, Some(2))).unwrap();
    assert_eq!(
        p1.items
            .iter()
            .map(|e| e.text.clone().unwrap())
            .collect::<Vec<_>>(),
        ["u0", "a0"]
    );
    assert!(p1.items.iter().all(|e| e.kind != Tool));
    let cur = p1.next_cursor.clone().expect("second page");

    // Same set in another order (and with duplicates) is the same filter:
    // the cursor stays valid.
    let p2 = r
        .messages(&mqk(
            "s1",
            vec![Assistant, User, User],
            Some(cur.clone()),
            Some(2),
        ))
        .unwrap();
    assert_eq!(
        p2.items
            .iter()
            .map(|e| e.text.clone().unwrap())
            .collect::<Vec<_>>(),
        ["u1", "a1"]
    );
    assert!(p2.next_cursor.is_none());

    // The cursor is tied to its filter: a narrower, wider, or empty kind set
    // is rejected, never silently reinterpreted.
    for kinds in [
        vec![User],
        vec![Assistant],
        vec![Tool],
        Vec::new(),
        vec![User, Assistant, Tool],
    ] {
        assert!(
            matches!(
                r.messages(&mqk("s1", kinds, Some(cur.clone()), Some(2))),
                Err(HistoryError::InvalidArgument(_))
            ),
            "cursor must not cross kind filters"
        );
    }

    // A cursor carrying anything but the fixed kind names is rejected before
    // it can reach SQL: kinds enter the query only via `HistoryEntryKind`.
    let mut state: serde_json::Value = serde_json::from_slice(&hex_decode(&cur)).unwrap();
    state["filters"]["kinds"] = serde_json::json!(["tool' OR '1'='1"]);
    let forged = hex_encode(&serde_json::to_vec(&state).unwrap());
    assert!(matches!(
        r.messages(&mqk("s1", chat.clone(), Some(forged), Some(2))),
        Err(HistoryError::InvalidArgument(_))
    ));
    // The store is untouched: the full history still reads back whole.
    assert_eq!(r.messages(&mq("s1", None, None)).unwrap().items.len(), 6);
}

fn hex_decode(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

fn hex_encode(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(DIGITS[usize::from(*b >> 4)] as char);
        out.push(DIGITS[usize::from(*b & 0xf)] as char);
    }
    out
}

#[test]
fn limit_bounds() {
    let (_t, dir) = store();
    let mut w = HistoryWriter::open(&dir).unwrap();
    w.record(&base("s1", "d1", 1, Some(user("x", None))))
        .unwrap();
    let r = HistoryReader::open(&dir).unwrap();
    for bad in [0u32, 1001, u32::MAX] {
        assert!(matches!(
            r.messages(&mq("s1", None, Some(bad))),
            Err(HistoryError::InvalidArgument(_))
        ));
        assert!(matches!(
            r.sessions(&SessionsQuery {
                limit: Some(bad),
                ..Default::default()
            }),
            Err(HistoryError::InvalidArgument(_))
        ));
    }
    for ok in [1u32, 1000] {
        r.messages(&mq("s1", None, Some(ok))).unwrap();
    }
    // limit == rows remaining => no cursor.
    assert!(r
        .messages(&mq("s1", None, Some(1)))
        .unwrap()
        .next_cursor
        .is_none());
}

#[test]
fn status_reports_counts() {
    let (_t, dir) = store();
    let r = HistoryReader::open(&dir).unwrap();
    let s = r.status().unwrap();
    assert_eq!((s.sessions, s.entries, s.last_persisted_ms), (0, 0, None));
    assert_eq!(s.schema_version, SCHEMA_VERSION);
    let mut w = HistoryWriter::open(&dir).unwrap();
    w.record(&base("s1", "d1", 1, Some(user("x", None))))
        .unwrap();
    w.record(&base("s1", "d1", 1, Some(user("x", None))))
        .unwrap();
    let s = r.status().unwrap();
    assert_eq!((s.sessions, s.entries), (1, 1));
    assert!(s.last_persisted_ms.is_some());
}

#[test]
fn reader_missing_db_is_disabled_and_creates_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let missing = tmp.path().join("nope");
    let err = HistoryReader::open(&missing).err().unwrap();
    assert!(matches!(err, HistoryError::Disabled(_)));
    assert_eq!(
        err.to_response().error.code,
        HistoryErrorCode::HistoryDisabled
    );
    assert!(!missing.exists());
    assert!(matches!(
        HistoryWriter::open(&missing),
        Err(HistoryError::Disabled(_))
    ));
    assert!(!missing.exists());

    // Directory present, database absent: still no file is created.
    let dir = tmp.path().join("empty");
    std::fs::create_dir(&dir).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    assert!(matches!(
        HistoryReader::open(&dir),
        Err(HistoryError::Disabled(_))
    ));
    assert!(matches!(
        HistoryWriter::open(&dir),
        Err(HistoryError::Disabled(_))
    ));
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
}

#[test]
fn reader_leaves_database_bytes_untouched() {
    let (_t, dir) = store();
    {
        let mut w = HistoryWriter::open(&dir).unwrap();
        for i in 0..4 {
            w.record(&base("s1", &format!("d{i}"), i, Some(user("x", None))))
                .unwrap();
        }
    }
    let listing = |d: &Path| {
        let mut v: Vec<_> = std::fs::read_dir(d)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        v.sort();
        v
    };
    let bytes_before = std::fs::read(dir.join(DB_FILE)).unwrap();
    {
        let r = HistoryReader::open(&dir).unwrap();
        r.sessions(&SessionsQuery::default()).unwrap();
        r.messages(&mq("s1", None, Some(2))).unwrap();
        r.status().unwrap();
        // query_only blocks writes through the reader connection.
        assert!(r.messages(&mq("s1", None, None)).is_ok());
    }
    assert_eq!(std::fs::read(dir.join(DB_FILE)).unwrap(), bytes_before);
    // A read-only WAL connection may materialize the -wal/-shm sidecars (an
    // SQLite requirement); nothing else may appear and the database file
    // itself must stay byte-identical.
    let allowed = [DB_FILE, "history.sqlite3-wal", "history.sqlite3-shm"];
    for name in listing(&dir) {
        assert!(allowed.contains(&name.to_str().unwrap()), "{name:?}");
    }
}

// ---------------------------------------------------- no-truncation roundtrip

fn long_text(seed: &str) -> String {
    let mut s = format!("  \n\t{seed} leading whitespace\n");
    while s.chars().count() < 21_000 {
        s.push_str("héllo wörld 🦀 日本語 line\r\n  indented\ttab ");
    }
    s.push_str("tail with trailing whitespace \n\n  ");
    s
}

#[test]
fn full_text_roundtrips_byte_for_byte() {
    let (_t, dir) = store();
    let mut w = HistoryWriter::open(&dir).unwrap();
    let prompt = long_text("prompt");
    let summary = long_text("summary");
    assert!(prompt.chars().count() > 20_000 && summary.chars().count() > 20_000);

    let mut ev1 = native_event(
        "s-long",
        AgentLifecycle::UserPromptSubmit {
            user_prompt: Some(prompt.clone()),
        },
    );
    ev1.flavor_text = Some(cap_chars(&prompt, 300));
    let mut ev2 = native_event(
        "s-long",
        AgentLifecycle::Stop {
            summary: Some(summary.clone()),
            success: true,
        },
    );
    ev2.flavor_text = Some(cap_chars(&summary, 300));
    for ev in [&ev1, &ev2] {
        let cap = Capture::from_event(ev, LOCAL_ORIGIN).unwrap();
        assert_eq!(w.record(&cap).unwrap(), RecordOutcome::Stored);
    }

    let page = msgs(&dir, "s-long");
    let json = serde_json::to_string(&page).unwrap();
    let back: HistoryMessagesPage = serde_json::from_str(&json).unwrap();
    assert_eq!(back, page);
    assert_eq!(back.items.len(), 2);
    assert_eq!(back.items[0].kind, HistoryEntryKind::User);
    assert_eq!(back.items[0].text.as_deref(), Some(prompt.as_str()));
    assert_eq!(back.items[1].kind, HistoryEntryKind::Assistant);
    assert_eq!(back.items[1].text.as_deref(), Some(summary.as_str()));
}

// ------------------------------------------------------------------- misc

#[test]
fn resolve_history_dir_prefers_override() {
    let p = Path::new("/explicit/dir");
    assert_eq!(resolve_history_dir(Some(p)), p);
}

#[test]
fn error_messages_do_not_leak_text() {
    let secret = "TOP-SECRET-TEXT";
    let (_t, dir) = store();
    let mut w = HistoryWriter::open(&dir).unwrap();
    let c = base("s1", "d1", 1, Some(user(secret, None)));
    w.record(&c).unwrap();
    raw(&dir).pragma_update(None, "user_version", 99).unwrap();
    let msg = serde_json::to_string(&w.record(&c).unwrap_err().to_response()).unwrap();
    assert!(!msg.contains(secret));
}

#[cfg(unix)]
mod unix {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};

    fn mode(p: &Path) -> u32 {
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn init_creates_private_dir_and_db() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("a").join("history");
        init_store(&dir).unwrap();
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join(DB_FILE)), 0o600);

        // Hold a writer so WAL/SHM exist, then check every file.
        let mut w = HistoryWriter::open(&dir).unwrap();
        w.record(&base("s1", "d1", 1, Some(user("x", None))))
            .unwrap();
        for entry in std::fs::read_dir(&dir).unwrap() {
            let p = entry.unwrap().path();
            assert_eq!(mode(&p), 0o600, "{}", p.display());
        }
    }

    #[test]
    fn init_tightens_loose_dir_and_open_rejects_it() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("h");
        init_store(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(matches!(
            HistoryReader::open(&dir),
            Err(HistoryError::Insecure(_))
        ));
        assert!(matches!(
            HistoryWriter::open(&dir),
            Err(HistoryError::Insecure(_))
        ));
        init_store(&dir).unwrap();
        assert_eq!(mode(&dir), 0o700);
        HistoryReader::open(&dir).unwrap();
    }

    #[test]
    fn symlinks_are_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        init_store(&real).unwrap();
        let link = tmp.path().join("link");
        symlink(&real, &link).unwrap();
        assert!(matches!(init_store(&link), Err(HistoryError::Insecure(_))));
        assert!(matches!(
            HistoryReader::open(&link),
            Err(HistoryError::Insecure(_))
        ));
        assert!(matches!(
            HistoryWriter::open(&link),
            Err(HistoryError::Insecure(_))
        ));

        let dir = tmp.path().join("d");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        symlink(real.join(DB_FILE), dir.join(DB_FILE)).unwrap();
        assert!(matches!(init_store(&dir), Err(HistoryError::Insecure(_))));
        assert!(matches!(
            HistoryReader::open(&dir),
            Err(HistoryError::Insecure(_))
        ));
        assert!(matches!(
            HistoryWriter::open(&dir),
            Err(HistoryError::Insecure(_))
        ));
        assert_eq!(
            init_store(&link).unwrap_err().to_response().error.code,
            HistoryErrorCode::StoreUnavailable
        );
    }
}
