//! `InMsg::RevertFiles` (#731, ADR-0212): the session hands the request to the
//! runtime's `file_reverter` seam only when idle, announces the outcome as a
//! persisted `FilesReverted`, and tells the model through a user-role note —
//! which replay rebuilds from the event without ever calling the seam again.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    content_text, EngineConfig, FileRevertReport, FileReverter, Holly, InMsg, Llm, LlmEvent,
    LlmRequest, LlmStream, OutEvent, RevertSkip, SessionId, StopReason, ToolCall, Usage,
};
use futures::{stream, FutureExt, StreamExt};

use crate::common::collect_until_done;

/// Records every request's message texts; the first call of `tool_first`
/// streams a tool call so the turn parks, every other call ends the turn.
struct Recording {
    seen: Arc<Mutex<Vec<Vec<String>>>>,
    calls: usize,
    tool_first: bool,
}

#[async_trait]
impl Llm for Recording {
    async fn stream(&mut self, req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        self.seen.lock().unwrap().push(
            req.messages
                .iter()
                .map(|m| content_text(&m.content))
                .collect(),
        );
        self.calls += 1;
        let events = if self.tool_first && self.calls == 1 {
            vec![
                Ok(LlmEvent::ToolCall(ToolCall {
                    id: "t1".into(),
                    name: "write".into(),
                    input: "{}".into(),
                    provider_meta: None,
                })),
                Ok(LlmEvent::Finish {
                    stop_reason: Some(StopReason::ToolUse),
                    usage: Usage::default(),
                }),
            ]
        } else {
            vec![
                Ok(LlmEvent::Text("ok".into())),
                Ok(LlmEvent::Finish {
                    stop_reason: Some(StopReason::EndTurn),
                    usage: Usage::default(),
                }),
            ]
        };
        Ok(stream::iter(events).boxed())
    }
}

fn report() -> FileRevertReport {
    FileRevertReport {
        to_turn: 1,
        restored: vec!["a.rs".into()],
        skipped: vec![RevertSkip {
            path: "b.rs".into(),
            reason: "modified since the agent's edit".into(),
        }],
    }
}

/// A seam that counts calls and records the requested `to_turn`.
fn counting_reverter(calls: Arc<AtomicUsize>, asked: Arc<Mutex<Vec<Option<u32>>>>) -> FileReverter {
    Arc::new(move |_session, to_turn| {
        calls.fetch_add(1, Ordering::SeqCst);
        asked.lock().unwrap().push(to_turn);
        async move { Ok(report()) }.boxed()
    })
}

fn config(
    seen: Arc<Mutex<Vec<Vec<String>>>>,
    tool_first: bool,
    reverter: Option<FileReverter>,
) -> EngineConfig {
    let seen_for_factory = seen.clone();
    EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(Recording {
                seen: seen_for_factory.clone(),
                calls: 0,
                tool_first,
            }) as Box<dyn Llm>
        }),
        file_reverter: reverter,
        reoffer_interval: None,
        ..Default::default()
    }
}

async fn next_matching(
    sub: &mut tokio::sync::broadcast::Receiver<OutEvent>,
    mut pred: impl FnMut(&OutEvent) -> bool,
) -> Option<OutEvent> {
    loop {
        match tokio::time::timeout(Duration::from_secs(3), sub.recv()).await {
            Ok(Ok(ev)) if pred(&ev) => return Some(ev),
            Ok(Ok(_)) => {}
            _ => return None,
        }
    }
}

#[tokio::test]
async fn revert_emits_the_report_and_notes_it_for_the_next_turn() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let calls = Arc::new(AtomicUsize::new(0));
    let asked = Arc::new(Mutex::new(Vec::new()));
    let holly = Holly::spawn(config(
        seen.clone(),
        false,
        Some(counting_reverter(calls.clone(), asked.clone())),
    ));
    let sid = SessionId::new("s-revert");

    let sub = holly.subscribe();
    holly
        .send(InMsg::prompt(sid.clone(), "first"))
        .await
        .unwrap();
    collect_until_done(sub, &sid).await;

    let mut sub = holly.subscribe();
    holly
        .send(InMsg::RevertFiles {
            session: sid.clone(),
            to_turn: None,
        })
        .await
        .unwrap();
    let ev = next_matching(&mut sub, |e| matches!(e, OutEvent::FilesReverted { .. }))
        .await
        .expect("FilesReverted");
    let OutEvent::FilesReverted {
        to_turn,
        restored,
        skipped,
        ..
    } = ev
    else {
        unreachable!()
    };
    assert_eq!((to_turn, restored), (1, vec!["a.rs".to_string()]));
    assert_eq!(skipped.len(), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(*asked.lock().unwrap(), vec![None], "undo passes no turn");

    let sub = holly.subscribe();
    holly
        .send(InMsg::prompt(sid.clone(), "second"))
        .await
        .unwrap();
    collect_until_done(sub, &sid).await;
    let requests = seen.lock().unwrap().clone();
    let last = requests.last().expect("second turn request");
    let n = last.len();
    assert!(
        last[n - 2].contains("[files reverted]") && last[n - 2].contains("a.rs"),
        "the note precedes the next prompt: {last:?}"
    );
    assert_eq!(last[n - 1], "second");
}

#[tokio::test]
async fn revert_without_a_seam_is_a_recoverable_error() {
    let holly = Holly::spawn(config(Arc::new(Mutex::new(Vec::new())), false, None));
    let sid = SessionId::new("s-no-seam");
    let sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "hi")).await.unwrap();
    collect_until_done(sub, &sid).await;

    let mut sub = holly.subscribe();
    holly
        .send(InMsg::RevertFiles {
            session: sid.clone(),
            to_turn: Some(0),
        })
        .await
        .unwrap();
    let ev = next_matching(&mut sub, |e| matches!(e, OutEvent::Error { .. }))
        .await
        .expect("Error");
    assert!(
        matches!(&ev, OutEvent::Error { message, .. } if message.contains("not available")),
        "{ev:?}"
    );
}

/// A revert arriving while a turn is parked on a tool call waits for the turn
/// to end — it must never race the turn's own writes.
#[tokio::test]
async fn revert_mid_turn_is_deferred_until_the_turn_ends() {
    let calls = Arc::new(AtomicUsize::new(0));
    let holly = Holly::spawn(config(
        Arc::new(Mutex::new(Vec::new())),
        true,
        Some(counting_reverter(
            calls.clone(),
            Arc::new(Mutex::new(Vec::new())),
        )),
    ));
    let sid = SessionId::new("s-deferred");
    let mut sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let exec = next_matching(&mut sub, |e| matches!(e, OutEvent::ToolExec { .. }))
        .await
        .expect("turn parks on a ToolExec");
    let OutEvent::ToolExec { request_id, .. } = exec else {
        unreachable!()
    };

    holly
        .send(InMsg::RevertFiles {
            session: sid.clone(),
            to_turn: Some(0),
        })
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(calls.load(Ordering::SeqCst), 0, "not applied mid-turn");

    holly
        .send(InMsg::tool_result(sid.clone(), request_id, "done"))
        .await
        .unwrap();
    let mut saw_done = false;
    let reverted = next_matching(&mut sub, |e| {
        if matches!(e, OutEvent::Done { .. }) {
            saw_done = true;
        }
        matches!(e, OutEvent::FilesReverted { .. })
    })
    .await;
    assert!(reverted.is_some(), "applied once the turn ended");
    assert!(saw_done, "Done precedes the deferred revert");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn revert_files_is_refused_from_the_wire() {
    let holly = Holly::spawn(EngineConfig::default());
    let err = holly
        .send_from_wire(InMsg::RevertFiles {
            session: SessionId::new("s"),
            to_turn: None,
        })
        .await
        .expect_err("trusted-only");
    assert!(err.to_string().contains("revert_files"), "{err}");
}

/// Replay re-appends the note from the logged event and never calls the seam:
/// a log containing a revert must not touch the disk on resume.
#[test]
fn replay_rebuilds_the_note_without_calling_the_seam() {
    let calls = Arc::new(AtomicUsize::new(0));
    let cfg = config(
        Arc::new(Mutex::new(Vec::new())),
        false,
        Some(counting_reverter(
            calls.clone(),
            Arc::new(Mutex::new(Vec::new())),
        )),
    );
    let sid = SessionId::new("s-replay");
    let r = report();
    let records = vec![
        (
            Some(InMsg::prompt(sid.clone(), "first")),
            OutEvent::TextDelta {
                session: sid.clone(),
                seq: 1,
                text: "ok".into(),
            },
        ),
        (
            None,
            OutEvent::Done {
                session: sid.clone(),
                seq: 2,
            },
        ),
        (
            None,
            OutEvent::FilesReverted {
                session: sid.clone(),
                seq: 3,
                to_turn: r.to_turn,
                restored: r.restored,
                skipped: r.skipped,
            },
        ),
    ];
    let session = entanglement_core::session::Session::replay(&records, &cfg, &sid).unwrap();
    let last = session.ctx.messages().last().expect("note message");
    assert!(content_text(&last.content).contains("[files reverted]"));
    assert_eq!(calls.load(Ordering::SeqCst), 0, "replay never reverts");

    // A prompt sent while the revert ran is logged ahead of `FilesReverted`
    // and pairs with it — yet live, the note came first.
    let mut paired = records;
    let (_, reverted) = paired.pop().unwrap();
    paired.push((Some(InMsg::prompt(sid.clone(), "second")), reverted));
    let session = entanglement_core::session::Session::replay(&paired, &cfg, &sid).unwrap();
    let texts: Vec<String> = session
        .ctx
        .messages()
        .iter()
        .map(|m| content_text(&m.content))
        .collect();
    let n = texts.len();
    assert!(texts[n - 2].contains("[files reverted]"), "{texts:?}");
    assert_eq!(texts[n - 1], "second");
}
