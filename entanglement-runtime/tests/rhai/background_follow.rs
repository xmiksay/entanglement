//! A background script follows its session's compaction (#886, ADR-0241):
//! nested calls dispatched after the switch grade, grant and park as the
//! successor, and a session grant earned on a card parked before the
//! compaction lands on the successor too — so the successor's next
//! identical call is not prompted again.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, ApprovalScope, Holly, InMsg, Llm, LlmRequest, LlmResponse, LlmStream,
    MessageRole, OutEvent, SessionId, ToolCall,
};
use entanglement_runtime::script_ops::ScriptRegistry;
use entanglement_runtime::tool_names::RHAI_TOOL;
use serde_json::json;
use tokio::sync::broadcast::Receiver;

use super::background_move::pending_under;
use super::background_park::engine_with;
use super::{is_launch_card, TempDir};

const WAIT: Duration = Duration::from_secs(5);

/// A fresh `go` turn launches `script` in the background; a fresh `edit`
/// turn makes the direct `edit` call `direct`, a fresh `poll` turn a
/// handle-less `poll`; every other round answers.
pub(super) struct Follow {
    pub(super) script: String,
    pub(super) direct: serde_json::Value,
}

#[async_trait]
impl Llm for Follow {
    async fn stream(&mut self, req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        let fresh = req
            .messages
            .iter()
            .rposition(|m| m.role == MessageRole::User && !m.text().starts_with("[mode: "))
            .filter(|&i| {
                !req.messages[i..]
                    .iter()
                    .any(|m| m.role == MessageRole::Tool)
            })
            .map(|i| req.messages[i].text());
        let call = |name: &str, input: String| ToolCall {
            id: "t1".into(),
            name: name.into(),
            input,
            provider_meta: None,
        };
        let next = match fresh.as_deref() {
            Some("go") => Some(call(
                RHAI_TOOL,
                json!({"script": self.script, "background": true}).to_string(),
            )),
            Some("edit") => Some(call("edit", self.direct.to_string())),
            Some("poll") => Some(call("poll", "{}".into())),
            _ => None,
        };
        Ok(stream_from_response(LlmResponse {
            text: if next.is_some() { "" } else { "ok" }.into(),
            tool_calls: next.into_iter().collect(),
            ..Default::default()
        }))
    }
}

fn engine(root: &Path, script: String, direct: serde_json::Value) -> Holly {
    let llm = Arc::new(move || {
        Box::new(Follow {
            script: script.clone(),
            direct: direct.clone(),
        }) as Box<dyn Llm>
    });
    engine_with(root, ScriptRegistry::new(), llm)
}

/// Read `sub` until `hit` matches, returning that event; panics on a
/// `ToolRequest` for `edit` that `allow_card` refuses, or on a timeout.
pub(super) async fn until(
    sub: &mut Receiver<OutEvent>,
    what: &str,
    allow_card: impl Fn(&OutEvent) -> bool,
    mut hit: impl FnMut(&OutEvent) -> bool,
) -> OutEvent {
    loop {
        match tokio::time::timeout(WAIT, sub.recv()).await {
            Ok(Ok(ev)) if hit(&ev) => return ev,
            Ok(Ok(ev)) if is_launch_card(&ev) => {}
            Ok(Ok(ev @ OutEvent::ToolRequest { .. })) if !allow_card(&ev) => {
                panic!("unexpected card while waiting for {what}: {ev:?}")
            }
            Ok(Ok(_)) => {}
            other => panic!("expected {what}: {other:?}"),
        }
    }
}

pub(super) fn any(_: &OutEvent) -> bool {
    true
}

pub(super) fn no_card(_: &OutEvent) -> bool {
    false
}

pub(super) fn edit_card_on(ev: &OutEvent, on: &SessionId) -> bool {
    matches!(ev, OutEvent::ToolRequest { session, tool, .. } if session == on && tool.contains("edit"))
}

/// Compact `pred` into `succ` — the fork's `Spawn` + `CloseSession` pair
/// (ADR-0205/0231) — and wait for the successor to start and `pred` to end.
pub(super) async fn compact(
    holly: &Holly,
    sub: &mut Receiver<OutEvent>,
    pred: &SessionId,
    succ: &SessionId,
) {
    holly
        .send(InMsg::Spawn {
            session: succ.clone(),
            parent: None,
            predecessor: Some(pred.clone()),
            agent: "general".into(),
            prompt: "continue".into(),
            user: None,
            model: None,
            mode: None,
            workdir: None,
        })
        .await
        .unwrap();
    holly
        .send(InMsg::CloseSession {
            session: pred.clone(),
        })
        .await
        .unwrap();
    let (mut started, mut ended) = (false, false);
    until(sub, "the compaction", any, |ev| {
        match ev {
            OutEvent::SessionStarted { session, .. } if session == succ => started = true,
            OutEvent::SessionEnded { session, .. } if session == pred => ended = true,
            _ => {}
        }
        started && ended
    })
    .await;
}

/// Wait until the executor has folded `succ`'s start: it folds events in
/// order, so once it routed this `poll` it has folded the successor's mode,
/// which the switch waits for. Any card is allowed (the move's re-announce).
pub(super) async fn executor_caught_up(
    holly: &Holly,
    sub: &mut Receiver<OutEvent>,
    succ: &SessionId,
) {
    holly
        .send(InMsg::prompt(succ.clone(), "poll"))
        .await
        .unwrap();
    until(sub, "the successor's poll", any, |ev| {
        matches!(ev, OutEvent::ToolOutput { session, tool, .. } if session == succ && tool == "poll")
    })
    .await;
}

pub(super) async fn approve(
    holly: &Holly,
    session: &SessionId,
    request_id: &str,
    scope: ApprovalScope,
) {
    holly
        .send(InMsg::Approve {
            session: session.clone(),
            request_id: request_id.to_string(),
            scope,
            mode: None,
            run_all: false,
        })
        .await
        .unwrap();
}

/// The model on `succ` edits `f.txt` directly: the script's session grant,
/// recorded under `succ`, clears it with no card.
async fn direct_edit_runs_unprompted(holly: &Holly, succ: &SessionId, root: &Path, want: &str) {
    let mut sub = holly.subscribe();
    holly
        .send(InMsg::prompt(succ.clone(), "edit"))
        .await
        .unwrap();
    until(
        &mut sub,
        "the direct edit's output",
        |ev| !edit_card_on(ev, succ),
        |ev| matches!(ev, OutEvent::ToolOutput { session, tool, .. } if session == succ && tool == "edit"),
    )
    .await;
    assert_eq!(std::fs::read_to_string(root.join("f.txt")).unwrap(), want);
}

pub(super) fn nested_edit_on(ev: &OutEvent, on: &SessionId, nth: u64) -> bool {
    matches!(ev, OutEvent::NestedCall { session, tool, index, is_error: false, .. }
        if session == on && tool == "edit" && *index == nth)
}

/// The issue's own scenario: the card parks on the predecessor, the session
/// compacts, the user approves it for the session on the successor. The
/// grant lands on the successor, so the script's next edit of the file
/// (dispatched after the switch) and the successor's own direct edit run
/// with no card.
#[tokio::test]
async fn a_session_grant_earned_across_the_compaction_lands_on_the_successor() {
    let dir = TempDir::new("bg-follow-grant");
    let script = r#"edit("f.txt", "", "one\n"); edit("f.txt", "one\n", "two\n"); "finished""#;
    let direct = json!({"path": "f.txt", "oldString": "two\n", "newString": "three\n"});
    let holly = engine(&dir.path, script.into(), direct);
    let (pred, succ) = (SessionId::new("pred"), SessionId::new("succ"));
    let mut sub = holly.subscribe();

    holly.send(InMsg::prompt(pred.clone(), "go")).await.unwrap();
    until(&mut sub, "the card on pred", no_card_but(&pred), |ev| {
        edit_card_on(ev, &pred)
    })
    .await;
    compact(&holly, &mut sub, &pred, &succ).await;
    executor_caught_up(&holly, &mut sub, &succ).await;
    let moved = pending_under(&holly, &succ).await;
    assert_eq!(moved.len(), 1, "ADR-0238 moved the card: {moved:?}");

    let id = moved[0].request_id.clone();
    approve(&holly, &succ, &id, ApprovalScope::Session).await;
    // Only the move's re-announce, which may land after the compaction wait.
    let reannounce =
        |ev: &OutEvent| matches!(ev, OutEvent::ToolRequest { request_id, .. } if request_id == &id);
    until(&mut sub, "the second edit on succ", reannounce, |ev| {
        nested_edit_on(ev, &succ, 2)
    })
    .await;
    assert_eq!(
        std::fs::read_to_string(dir.path.join("f.txt")).unwrap(),
        "two\n"
    );

    direct_edit_runs_unprompted(&holly, &succ, &dir.path, "three\n").await;
}

/// Cards on `pred` only (the predecessor's own, before the compaction).
fn no_card_but(pred: &SessionId) -> impl Fn(&OutEvent) -> bool + '_ {
    move |ev| matches!(ev, OutEvent::ToolRequest { session, .. } if session == pred)
}

/// A call dispatched after the switch parks *as* the successor: its card is
/// keyed there (so the predecessor's id never answers it), its live report
/// shows on the successor, and its session grant clears the successor's
/// direct call.
#[tokio::test]
async fn a_card_parked_after_the_switch_is_the_successors_own() {
    let dir = TempDir::new("bg-follow-own");
    let gate = dir.path.join("gate");
    let script = format!(
        r#"bash("while [ ! -f '{}' ]; do sleep 0.05; done"); edit("f.txt", "", "one\n"); "finished""#,
        gate.display()
    );
    let direct = json!({"path": "f.txt", "oldString": "one\n", "newString": "two\n"});
    let holly = engine(&dir.path, script, direct);
    let (pred, succ) = (SessionId::new("pred"), SessionId::new("succ"));
    let mut sub = holly.subscribe();

    holly.send(InMsg::prompt(pred.clone(), "go")).await.unwrap();
    until(&mut sub, "the launch's handle", no_card, |ev| {
        matches!(ev, OutEvent::ToolOutput { session, tool, .. } if session == &pred && tool == RHAI_TOOL)
    })
    .await;
    compact(&holly, &mut sub, &pred, &succ).await;
    executor_caught_up(&holly, &mut sub, &succ).await;

    std::fs::write(&gate, "").unwrap();
    let card = until(&mut sub, "the card on succ", no_card, |ev| {
        edit_card_on(ev, &succ)
    })
    .await;
    let OutEvent::ToolRequest {
        request_id,
        detached,
        ..
    } = card
    else {
        unreachable!("matched a ToolRequest");
    };
    assert!(detached, "still a background script's card");

    // The predecessor's id answers only a card it parked; this one it never did.
    approve(&holly, &pred, &request_id, ApprovalScope::Session).await;
    let listed = pending_under(&holly, &succ).await;
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0].request_id, request_id);
    assert!(!dir.path.join("f.txt").exists(), "the edit has not run");

    approve(&holly, &succ, &request_id, ApprovalScope::Session).await;
    until(&mut sub, "the edit's report on succ", no_card, |ev| {
        nested_edit_on(ev, &succ, 2)
    })
    .await;
    assert_eq!(
        std::fs::read_to_string(dir.path.join("f.txt")).unwrap(),
        "one\n"
    );

    direct_edit_runs_unprompted(&holly, &succ, &dir.path, "two\n").await;
}
