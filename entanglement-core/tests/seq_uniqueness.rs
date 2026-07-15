//! `(session, seq)` uniqueness across authored events (#157).
//!
//! A runtime service authoring an event for a *parked* session mints a fresh seq
//! from the session's shared counter via [`Holly::emit_for_session`] instead of
//! reusing the parked `ToolExec` seq — so the minted seq strictly exceeds the
//! `ToolExec` seq and the resumed session continues *past* it (no collision).
//! Supervisor lifecycle errors for an id with no live session carry seq `0`.

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
            }],
        },
        LlmResponse {
            text: "done".into(),
            tool_calls: vec![],
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

/// A supervisor error for an id with no live session (a `Prompt` racing behind
/// its `CloseSession`) has no counter to draw from, so it carries seq `0` — a
/// value core never mints, so heads render it via the seq-0 bypass.
#[tokio::test]
async fn supervisor_error_for_closed_id_carries_seq_zero() {
    let holly = engine(vec![LlmResponse {
        text: "ok".into(),
        tool_calls: vec![],
    }]);
    let mut sub = holly.subscribe();
    let sid = SessionId::new("s1");

    // Create + run the session so the id is live and registered.
    holly.send(InMsg::prompt(sid.clone(), "hi")).await.unwrap();
    seq_of(&mut sub, &sid, |e| matches!(e, OutEvent::Done { .. })).await;

    // Close it; wait until `SessionEnded` so the counter is deregistered.
    holly
        .send(InMsg::CloseSession {
            session: sid.clone(),
        })
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while let Ok(Ok(ev)) = tokio::time::timeout_at(deadline, sub.recv()).await {
        if ev.session() == Some(&sid) && matches!(ev, OutEvent::SessionEnded { .. }) {
            break;
        }
    }

    // A `Prompt` to the retired id is refused by the supervisor with seq 0.
    holly
        .send(InMsg::prompt(sid.clone(), "again"))
        .await
        .unwrap();
    let err_seq = seq_of(&mut sub, &sid, |e| matches!(e, OutEvent::Error { .. })).await;
    assert_eq!(
        err_seq, 0,
        "a supervisor error for a closed id has no live counter, so seq is 0"
    );
}
