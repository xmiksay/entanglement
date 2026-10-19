//! A background script whose nested card parks only *after* its session
//! retired (#884, ADR-0239) — the idle sweep hibernated it, or it was closed
//! — still gets a real seq: above every seq the session already used, so a
//! head's `seq > last_seen` dedupe folds the card instead of dropping a
//! seq-`0` one and leaving the script waiting forever.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, InMsg, Llm, LlmRequest, LlmResponse, LlmStream, MessageRole, OutEvent,
    SessionId, ToolCall,
};
use entanglement_runtime::script_ops::ScriptRegistry;
use entanglement_runtime::tool_names::RHAI_TOOL;

use super::background_park::{approve_and_finish, engine_with};
use super::{is_launch_card, TempDir};

const WAIT: Duration = Duration::from_secs(5);

/// A fresh `go` turn launches a background script that waits for `gate` to
/// exist before its nested `edit` asks; every other round just answers.
struct GatedLaunch {
    gate: PathBuf,
}

#[async_trait]
impl Llm for GatedLaunch {
    async fn stream(&mut self, req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        // The last real user prompt, with no tool round after it yet.
        let fresh_go = req
            .messages
            .iter()
            .rposition(|m| m.role == MessageRole::User && !m.text().starts_with("[mode: "))
            .is_some_and(|i| {
                req.messages[i].text() == "go"
                    && !req.messages[i..]
                        .iter()
                        .any(|m| m.role == MessageRole::Tool)
            });
        let script = format!(
            r#"bash("while [ ! -f '{}' ]; do sleep 0.05; done"); edit("f.txt", "", "approved\n"); "finished""#,
            self.gate.display()
        );
        let call = fresh_go.then(|| ToolCall {
            id: "t1".into(),
            name: RHAI_TOOL.into(),
            input: serde_json::json!({"script": script, "background": true}).to_string(),
            provider_meta: None,
        });
        Ok(stream_from_response(LlmResponse {
            text: if call.is_some() { "" } else { "ok" }.into(),
            tool_calls: call.into_iter().collect(),
            ..Default::default()
        }))
    }
}

/// How the session goes away before the script's card parks.
enum Retire {
    Hibernate,
    Close,
}

/// Launch the gated script, retire the session, open the gate, and return
/// the engine, the (detached) card's request id and seq, and the highest seq
/// the session authored before the card.
async fn card_after_retire(
    dir: &TempDir,
    scripts: &ScriptRegistry,
    sid: &SessionId,
    retire: Retire,
) -> (entanglement_core::Holly, String, u64, u64) {
    let gate = dir.path.join("gate");
    let holly = engine_with(
        &dir.path,
        scripts.clone(),
        Arc::new(move || Box::new(GatedLaunch { gate: gate.clone() }) as Box<dyn Llm>),
    );
    let mut sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();

    let mut last = 0;
    let track = |last: &mut u64, ev: &OutEvent| {
        if ev.session() == Some(sid) {
            *last = (*last).max(ev.seq().unwrap_or(0));
        }
    };
    // The turn ends once the launch returns its handle; the script waits.
    loop {
        match tokio::time::timeout(WAIT, sub.recv()).await {
            Ok(Ok(ev)) => {
                track(&mut last, &ev);
                if matches!(&ev, OutEvent::Done { session, .. } if session == sid) {
                    break;
                }
            }
            other => panic!("expected the launching turn to finish: {other:?}"),
        }
    }
    let msg = match retire {
        Retire::Hibernate => InMsg::HibernateSession {
            session: sid.clone(),
        },
        Retire::Close => InMsg::CloseSession {
            session: sid.clone(),
        },
    };
    holly.send(msg).await.unwrap();
    loop {
        match tokio::time::timeout(WAIT, sub.recv()).await {
            Ok(Ok(ev)) => {
                track(&mut last, &ev);
                if matches!(
                    &ev,
                    OutEvent::SessionHibernated { session, .. }
                        | OutEvent::SessionEnded { session, .. } if session == sid
                ) {
                    break;
                }
            }
            other => panic!("expected {sid} to retire: {other:?}"),
        }
    }
    assert!(!holly.is_live(sid), "the retired session is not live");

    std::fs::write(dir.path.join("gate"), "").unwrap();
    loop {
        match tokio::time::timeout(WAIT, sub.recv()).await {
            Ok(Ok(ev)) if is_launch_card(&ev) => {}
            Ok(Ok(OutEvent::ToolRequest {
                session,
                request_id,
                tool,
                seq,
                detached,
                ..
            })) if &session == sid && tool.contains("edit") => {
                assert!(detached, "a background script's card is detached");
                return (holly, request_id, seq, last);
            }
            Ok(Ok(ev)) => track(&mut last, &ev),
            other => panic!("expected the script's nested edit card: {other:?}"),
        }
    }
}

#[tokio::test]
async fn a_card_parked_after_a_hibernate_gets_a_real_seq() {
    let dir = TempDir::new("bg-retired-hib");
    let scripts = ScriptRegistry::new();
    let sid = SessionId::new("hib");
    let (holly, request_id, seq, last) =
        card_after_retire(&dir, &scripts, &sid, Retire::Hibernate).await;
    assert!(last > 0);
    assert!(
        seq > last,
        "card seq {seq} must exceed the session's {last}"
    );

    approve_and_finish(&holly, &scripts, &sid, &sid, request_id, &dir.path).await;
}

#[tokio::test]
async fn a_card_parked_after_a_close_gets_a_real_seq() {
    let dir = TempDir::new("bg-retired-close");
    let scripts = ScriptRegistry::new();
    let sid = SessionId::new("closed");
    let (holly, request_id, seq, last) =
        card_after_retire(&dir, &scripts, &sid, Retire::Close).await;
    assert!(last > 0);
    assert!(
        seq > last,
        "card seq {seq} must exceed the session's {last}"
    );

    approve_and_finish(&holly, &scripts, &sid, &sid, request_id, &dir.path).await;
}
