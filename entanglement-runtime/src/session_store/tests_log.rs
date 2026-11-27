use super::*;
use entanglement_core::{InMsg, OutEvent};

#[test]
fn pair_records_associates_each_prompt_with_following_events() {
    let sid = SessionId::new("s");
    let prompt = |t: &str| {
        LogRecord::new(
            sid.clone(),
            LogPayload::In(InMsg::prompt(sid.clone(), t.to_string())),
        )
    };
    let text = |seq: u64, t: &str| {
        LogRecord::new(
            sid.clone(),
            LogPayload::Out(OutEvent::TextDelta {
                session: sid.clone(),
                seq,
                text: t.to_string(),
            }),
        )
    };
    let done = |seq: u64| {
        LogRecord::new(
            sid.clone(),
            LogPayload::Out(OutEvent::Done {
                session: sid.clone(),
                seq,
            }),
        )
    };

    let records = vec![
        prompt("hi"),
        text(1, "hello"),
        done(2),
        prompt("again"),
        text(3, "yo"),
        done(4),
    ];

    let paired = pair_records(&records);
    assert_eq!(paired.len(), 4);

    // First prompt pairs with the first out event; it's consumed so the
    // trailing events of that turn pair with `None`.
    match &paired[0] {
        (Some(InMsg::Prompt { content, .. }), OutEvent::TextDelta { .. }) => {
            assert_eq!(entanglement_core::content_text(content), "hi")
        }
        other => panic!("unexpected pairing: {other:?}"),
    }
    assert!(matches!(paired[1], (None, OutEvent::Done { .. })));
    match &paired[2] {
        (Some(InMsg::Prompt { content, .. }), OutEvent::TextDelta { .. }) => {
            assert_eq!(entanglement_core::content_text(content), "again")
        }
        other => panic!("unexpected pairing: {other:?}"),
    }
    assert!(matches!(paired[3], (None, OutEvent::Done { .. })));
}

#[test]
fn pair_records_drops_trailing_inbound_without_output() {
    let sid = SessionId::new("s");
    let records = vec![LogRecord::new(
        sid.clone(),
        LogPayload::In(InMsg::prompt(sid.clone(), "no reply yet".to_string())),
    )];
    assert!(pair_records(&records).is_empty());
}
#[test]
fn integrity_gap_detects_and_sums_tombstones() {
    let sid = SessionId::new("s");
    let records = vec![
        LogRecord::new(sid.clone(), LogPayload::Gap { dropped: 4 }),
        LogRecord::new(
            sid.clone(),
            LogPayload::Out(OutEvent::Done {
                session: sid.clone(),
                seq: 1,
            }),
        ),
        LogRecord::new(sid.clone(), LogPayload::Gap { dropped: 6 }),
    ];
    assert_eq!(integrity_gap(&records), Some(10));
}

#[test]
fn integrity_gap_none_for_clean_log() {
    let sid = SessionId::new("s");
    let records = vec![LogRecord::new(
        sid.clone(),
        LogPayload::Out(OutEvent::Done {
            session: sid.clone(),
            seq: 1,
        }),
    )];
    assert_eq!(integrity_gap(&records), None);
}

/// A prompt sent while a batch is parked lands just before the executor's
/// `ToolResult`, both ahead of the next event: the result must not displace
/// the prompt, and a second queued prompt pairs with the event after.
#[test]
fn pair_records_keeps_prompts_queued_behind_other_inbound() {
    let sid = SessionId::new("s");
    let inbound = |msg: InMsg| LogRecord::new(sid.clone(), LogPayload::In(msg));
    let done = |seq: u64| {
        LogRecord::new(
            sid.clone(),
            LogPayload::Out(OutEvent::Done {
                session: sid.clone(),
                seq,
            }),
        )
    };
    let records = vec![
        inbound(InMsg::prompt(sid.clone(), "steer")),
        inbound(InMsg::tool_result(sid.clone(), "c1", "out")),
        inbound(InMsg::Stop {
            session: sid.clone(),
        }),
        done(1),
        done(2),
        done(3),
    ];
    let paired = pair_records(&records);
    assert!(matches!(&paired[0].0, Some(InMsg::Prompt { content, .. })
        if entanglement_core::content_text(content) == "steer"));
    assert!(matches!(paired[1].0, Some(InMsg::Stop { .. })));
    assert!(paired[2].0.is_none());
}
