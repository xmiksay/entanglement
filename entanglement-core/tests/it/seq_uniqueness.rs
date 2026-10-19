//! `(session, seq)` uniqueness across authored events (#157).
//!
//! A runtime service authoring an event for a *parked* session mints a fresh seq
//! from the session's shared counter via [`Holly::emit_for_session`] instead of
//! reusing the parked `ToolExec` seq — so the minted seq strictly exceeds the
//! `ToolExec` seq and the resumed session continues *past* it (no collision).
//! A retired session (hibernated, ended or closed) keeps minting above its last
//! seq (#884, ADR-0239); only an id that never started carries seq `0`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, EngineConfig, Holly, InMsg, Llm, LlmRequest, LlmResponse, LlmStream,
    OutEvent, SessionId, ToolCall,
};

struct ScriptedLlm {
    responses: Mutex<Vec<LlmResponse>>,
}

#[async_trait]
impl Llm for ScriptedLlm {
    async fn stream(&mut self, _req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        let resp = self.responses.lock().unwrap().pop().unwrap_or(LlmResponse {
            text: "ok".into(),
            tool_calls: vec![],
            ..Default::default()
        });
        Ok(stream_from_response(resp))
    }
}

fn engine(mut responses: Vec<LlmResponse>) -> Holly {
    responses.reverse();
    let responses = Arc::new(responses);
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(ScriptedLlm {
                responses: Mutex::new((*responses).clone()),
            }) as Box<dyn Llm>
        }),
        ..EngineConfig::default()
    };
    Holly::spawn(cfg)
}

/// Wait for the first event for `sid` matching `pred` and return its `seq`.
async fn seq_of(
    sub: &mut tokio::sync::broadcast::Receiver<OutEvent>,
    sid: &SessionId,
    pred: impl Fn(&OutEvent) -> bool,
) -> u64 {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while let Ok(Ok(ev)) = tokio::time::timeout_at(deadline, sub.recv()).await {
        if ev.session() == Some(sid) && pred(&ev) {
            return ev.seq().expect("a matched content event carries a seq");
        }
    }
    panic!("timed out waiting for a matching event");
}

/// A runtime-authored event minted while the session is parked on a `ToolExec`
/// draws a fresh seq that exceeds the parked call's, and the session then
/// continues *past* that minted seq — so `(session, seq)` never collides.
#[tokio::test]
async fn emit_for_session_mints_unique_seq_above_parked_toolexec() {
    let holly = engine(vec![
        LlmResponse {
            text: String::new(),
            tool_calls: vec![ToolCall {
                id: "c1".into(),
                name: "read".into(),
                input: "{}".into(),
                provider_meta: None,
            }],
            ..Default::default()
        },
        LlmResponse {
            text: "done".into(),
            tool_calls: vec![],
            ..Default::default()
        },
    ]);
    let mut sub = holly.subscribe();
    let sid = SessionId::new("s1");
    holly.send(InMsg::prompt(sid.clone(), "hi")).await.unwrap();

    // The round ends in a tool call: core emits `ToolExec` and parks.
    let toolexec_seq = seq_of(&mut sub, &sid, |e| matches!(e, OutEvent::ToolExec { .. })).await;

    // Simulate the runtime authoring an approval prompt for the parked call.
    holly.emit_for_session(&sid, |seq| OutEvent::Error {
        session: sid.clone(),
        seq,
        message: "runtime-authored".into(),
    });
    let runtime_seq = seq_of(
        &mut sub,
        &sid,
        |e| matches!(e, OutEvent::Error { message, .. } if message == "runtime-authored"),
    )
    .await;
    assert!(
        runtime_seq > toolexec_seq,
        "runtime-minted seq {runtime_seq} must exceed the parked ToolExec seq {toolexec_seq}"
    );

    // Resolve the tool: the session un-parks and continues the turn. Its next
    // content event must carry a seq *past* the runtime-minted one — proving the
    // counter is shared, not reused (the pre-#157 defect).
    holly
        .send(InMsg::tool_result(sid.clone(), "c1", "ok"))
        .await
        .unwrap();
    let output_seq = seq_of(&mut sub, &sid, |e| matches!(e, OutEvent::ToolOutput { .. })).await;
    assert!(
        output_seq > runtime_seq,
        "resumed session seq {output_seq} must continue past the runtime-minted seq {runtime_seq}"
    );
}

/// A supervisor error for a closed id (a `Prompt` racing behind its
/// `CloseSession`) draws from the id's retired high-water (ADR-0239), so it
/// takes a real seq above the session's last; an id that never started has
/// no sequence at all and carries the seq-`0` sentinel.
#[tokio::test]
async fn supervisor_error_for_closed_id_continues_the_sequence() {
    let holly = engine(vec![]);
    let mut sub = holly.subscribe();
    let sid = SessionId::new("s1");

    holly.send(InMsg::prompt(sid.clone(), "hi")).await.unwrap();
    let done_seq = seq_of(&mut sub, &sid, |e| matches!(e, OutEvent::Done { .. })).await;

    holly
        .send(InMsg::CloseSession {
            session: sid.clone(),
        })
        .await
        .unwrap();
    await_retired(&mut sub, &sid).await;

    holly
        .send(InMsg::prompt(sid.clone(), "again"))
        .await
        .unwrap();
    let err_seq = seq_of(&mut sub, &sid, |e| matches!(e, OutEvent::Error { .. })).await;
    assert!(
        err_seq > done_seq,
        "a closed id's error seq {err_seq} continues past its last seq {done_seq}"
    );

    let ghost = SessionId::new("never-started");
    holly.emit_for_session(&ghost, |seq| OutEvent::Error {
        session: ghost.clone(),
        seq,
        message: "ghost".into(),
    });
    let ghost_seq = seq_of(&mut sub, &ghost, |e| matches!(e, OutEvent::Error { .. })).await;
    assert_eq!(ghost_seq, 0, "an id that never started has no sequence");
    assert!(!holly.is_live(&ghost));
}

/// Wait for `sid`'s `SessionEnded`/`SessionHibernated`: its counter is retired.
async fn await_retired(sub: &mut tokio::sync::broadcast::Receiver<OutEvent>, sid: &SessionId) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while let Ok(Ok(ev)) = tokio::time::timeout_at(deadline, sub.recv()).await {
        if ev.session() == Some(sid)
            && matches!(
                ev,
                OutEvent::SessionEnded { .. } | OutEvent::SessionHibernated { .. }
            )
        {
            return;
        }
    }
    panic!("timed out waiting for {sid} to retire");
}

/// Mint one runtime-authored event for `sid` and return its seq.
async fn emit_one(
    holly: &Holly,
    sub: &mut tokio::sync::broadcast::Receiver<OutEvent>,
    sid: &SessionId,
    tag: &str,
) -> u64 {
    holly.emit_for_session(sid, |seq| OutEvent::Error {
        session: sid.clone(),
        seq,
        message: tag.into(),
    });
    seq_of(
        sub,
        sid,
        |e| matches!(e, OutEvent::Error { message, .. } if message == tag),
    )
    .await
}

/// A detached emit for a hibernated session (a background script's card
/// parked after the idle sweep, #884) gets a real seq above every one the
/// session used, the session stays non-live, and a resume — even from a log
/// that never recorded those emits — continues above them: no seq is reused.
#[tokio::test]
async fn a_hibernated_session_keeps_a_real_seq_and_resume_never_reuses_one() {
    let holly = engine(vec![]);
    let mut sub = holly.subscribe();
    let sid = SessionId::new("hib");
    let prompt = InMsg::prompt(sid.clone(), "one");
    holly.send(prompt.clone()).await.unwrap();

    // The turn's own log, as an embedder would persist it.
    let (mut log, mut last) = (Vec::new(), 0);
    let mut paired = Some(prompt);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while let Ok(Ok(ev)) = tokio::time::timeout_at(deadline, sub.recv()).await {
        if ev.session() != Some(&sid) {
            continue;
        }
        last = last.max(ev.seq().unwrap_or(0));
        let done = matches!(ev, OutEvent::Done { .. });
        log.push((paired.take(), ev));
        if done {
            break;
        }
    }
    assert!(last > 0, "the turn authored seq-bearing events");

    holly.hibernate(sid.clone()).await.unwrap();
    await_retired(&mut sub, &sid).await;
    assert!(!holly.is_live(&sid), "a retired high-water is not liveness");

    let first = emit_one(&holly, &mut sub, &sid, "while-hibernated-1").await;
    let second = emit_one(&holly, &mut sub, &sid, "while-hibernated-2").await;
    assert!(first > last, "hibernated emit {first} must exceed {last}");
    assert_eq!(second, first + 1, "the retired sequence stays monotonic");

    // Resume from a log that misses both emits (the worst case: they were
    // never persisted). The counter still continues above them.
    holly.resume(sid.clone(), log).await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while let Ok(Ok(ev)) = tokio::time::timeout_at(deadline, sub.recv()).await {
        if matches!(&ev, OutEvent::SessionStarted { session, .. } if *session == sid) {
            break;
        }
    }
    assert!(holly.is_live(&sid));
    let after = emit_one(&holly, &mut sub, &sid, "after-resume").await;
    assert!(after > second, "resume's seq {after} must exceed {second}");

    holly.send(InMsg::prompt(sid.clone(), "two")).await.unwrap();
    let turn = seq_of(&mut sub, &sid, |e| matches!(e, OutEvent::Done { .. })).await;
    assert!(
        turn > after,
        "the resumed turn continues the sequence: {turn}"
    );
}
