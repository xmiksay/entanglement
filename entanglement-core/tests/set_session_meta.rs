//! Settable session display metadata (ADR-0151): an `InMsg::SetSessionMeta`
//! merges `name`/`action` onto the session (`None` leaves a field untouched,
//! `Some("")` clears it), always acks with `OutEvent::SessionMetaChanged`
//! carrying the full merged values, and — unlike `SetGeneration` — applies
//! **immediately**, even while a turn is live.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::session::Session;
use entanglement_core::{
    stream_from_response, EngineConfig, Holly, InMsg, Llm, LlmRequest, LlmResponse, LlmStream,
    OutEvent, SessionId,
};

async fn recv_until(
    sub: &mut tokio::sync::broadcast::Receiver<OutEvent>,
    pred: impl Fn(&OutEvent) -> bool,
) -> OutEvent {
    loop {
        let recv = tokio::time::timeout(Duration::from_secs(3), sub.recv())
            .await
            .expect("timed out waiting for a matching event");
        match recv {
            Ok(ev) if pred(&ev) => return ev,
            Ok(_) => {}
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
            Err(_) => panic!("event stream closed before a matching event"),
        }
    }
}

fn is_meta_changed(e: &OutEvent) -> bool {
    matches!(e, OutEvent::SessionMetaChanged { .. })
}

fn meta_of(ev: OutEvent) -> (Option<String>, Option<String>) {
    let OutEvent::SessionMetaChanged { name, action, .. } = ev else {
        unreachable!()
    };
    (name, action)
}

async fn set_meta(holly: &Holly, sid: &SessionId, name: Option<&str>, action: Option<&str>) {
    holly
        .send(InMsg::SetSessionMeta {
            session: sid.clone(),
            name: name.map(str::to_string),
            action: action.map(str::to_string),
            if_unset: false,
        })
        .await
        .unwrap();
}

/// Mirrors the auto session-title generator's write
/// (`entanglement-runtime::session_title`): a name sent with `if_unset: true`.
async fn set_name_if_unset(holly: &Holly, sid: &SessionId, name: &str) {
    holly
        .send(InMsg::SetSessionMeta {
            session: sid.clone(),
            name: Some(name.to_string()),
            action: None,
            if_unset: true,
        })
        .await
        .unwrap();
}

/// #556: an unbounded `name`/`action` is persisted and re-broadcast on every
/// set — cap it rather than let one write bloat the event log/session store.
#[tokio::test]
async fn oversized_name_and_action_are_capped_not_stored_verbatim() {
    let holly = Holly::spawn(EngineConfig::default());
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();

    let huge_name = "n".repeat(10_000);
    let huge_action = "a".repeat(10_000);
    set_meta(&holly, &sid, Some(&huge_name), Some(&huge_action)).await;
    let (name, action) = meta_of(recv_until(&mut sub, is_meta_changed).await);

    let name = name.expect("name set");
    let action = action.expect("action set");
    assert!(
        name.len() < huge_name.len(),
        "an oversized name must be capped, got {} bytes",
        name.len()
    );
    assert!(
        action.len() < huge_action.len(),
        "an oversized action must be capped, got {} bytes",
        action.len()
    );
}

#[tokio::test]
async fn fields_merge_and_the_ack_carries_full_state() {
    let holly = Holly::spawn(EngineConfig::default());
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();

    // Name only: action stays unset.
    set_meta(&holly, &sid, Some("fix login bug"), None).await;
    let (name, action) = meta_of(recv_until(&mut sub, is_meta_changed).await);
    assert_eq!(name.as_deref(), Some("fix login bug"));
    assert_eq!(action, None);

    // Action only: the earlier name survives (merge, not replace).
    set_meta(&holly, &sid, None, Some("running tests")).await;
    let (name, action) = meta_of(recv_until(&mut sub, is_meta_changed).await);
    assert_eq!(name.as_deref(), Some("fix login bug"));
    assert_eq!(action.as_deref(), Some("running tests"));

    // `Some("")` clears a field; the other is untouched.
    set_meta(&holly, &sid, None, Some("")).await;
    let (name, action) = meta_of(recv_until(&mut sub, is_meta_changed).await);
    assert_eq!(name.as_deref(), Some("fix login bug"));
    assert_eq!(action, None);
}

/// A scripted `Llm`: first round ends in a tool call (parking the turn), the
/// second round replies with text.
struct ScriptedLlm {
    responses: Arc<Mutex<VecDeque<LlmResponse>>>,
}

#[async_trait]
impl Llm for ScriptedLlm {
    async fn stream(&mut self, _req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        let resp = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| LlmResponse {
                text: "ok".into(),
                tool_calls: vec![],
            });
        Ok(stream_from_response(resp))
    }
}

#[tokio::test]
async fn set_session_meta_applies_immediately_while_a_turn_is_parked() {
    // The contrast with `SetGeneration`'s stash gate: with the turn parked on
    // an unresolved tool call (`Session::turn` is `Some`), `SetGeneration`
    // defers until the turn ends — `SetSessionMeta` must ack right away,
    // before the tool result resolves the turn. (A mid-*stream* arrival is
    // deferred by the generic stash like every command — the session task is
    // single-threaded — so the parked state is where immediacy is observable.)
    let responses: Arc<Mutex<VecDeque<LlmResponse>>> = Arc::new(Mutex::new(
        vec![
            LlmResponse {
                text: String::new(),
                tool_calls: vec![entanglement_core::ToolCall {
                    id: "r1".into(),
                    name: "echo".into(),
                    input: "{}".into(),
                    provider_meta: None,
                }],
            },
            LlmResponse {
                text: "done".into(),
                tool_calls: vec![],
            },
        ]
        .into(),
    ));
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(ScriptedLlm {
                responses: responses.clone(),
            }) as Box<dyn Llm>
        }),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();

    holly.send(InMsg::prompt(sid.clone(), "hi")).await.unwrap();
    // The turn parks once the tool call is offered.
    recv_until(&mut sub, |e| matches!(e, OutEvent::ToolExec { .. })).await;

    set_meta(&holly, &sid, None, Some("running the tool")).await;
    let (_, action) = meta_of(recv_until(&mut sub, is_meta_changed).await);
    assert_eq!(
        action.as_deref(),
        Some("running the tool"),
        "the ack must land while the turn is still parked"
    );

    // Only now resolve the parked call; the turn continues to Done.
    holly
        .send(InMsg::ToolResult {
            session: sid.clone(),
            request_id: "r1".to_string(),
            content: vec![entanglement_core::ContentPart::text("tool says hi")],
            is_error: false,
            duration_ms: None,
        })
        .await
        .unwrap();
    recv_until(&mut sub, |e| matches!(e, OutEvent::Done { .. })).await;
}

#[test]
fn replay_folds_the_last_meta_record() {
    let sid = SessionId::new("s1");
    let records = vec![
        (
            None,
            OutEvent::SessionMetaChanged {
                session: sid.clone(),
                name: Some("first".to_string()),
                action: Some("step one".to_string()),
            },
        ),
        (
            None,
            OutEvent::SessionMetaChanged {
                session: sid.clone(),
                name: Some("second".to_string()),
                action: None,
            },
        ),
    ];
    let session = Session::replay(&records, &EngineConfig::default(), &sid).unwrap();
    // Last write wins, both fields overwritten from the final record's full state.
    assert_eq!(session.name.as_deref(), Some("second"));
    assert_eq!(session.action, None);
}

#[test]
fn set_session_meta_is_wire_allowed() {
    let msg = InMsg::SetSessionMeta {
        session: SessionId::new("s1"),
        name: Some("n".to_string()),
        action: None,
        if_unset: false,
    };
    assert!(msg.wire_allowed(), "cosmetic metadata is wire-settable");
    assert_eq!(msg.variant_name(), "set_session_meta");
}

#[test]
fn wire_tag_is_snake_case_and_none_fields_are_skipped() {
    let msg = InMsg::SetSessionMeta {
        session: SessionId::new("s1"),
        name: Some("my session".to_string()),
        action: None,
        if_unset: false,
    };
    let json = serde_json::to_string(&msg).unwrap();
    assert!(json.contains("\"kind\":\"set_session_meta\""), "{json}");
    assert!(json.contains("\"name\":\"my session\""), "{json}");
    assert!(
        !json.contains("action"),
        "None field must be skipped: {json}"
    );
    assert!(
        !json.contains("if_unset"),
        "default-false if_unset must be skipped: {json}"
    );
    let back: InMsg = serde_json::from_str(&json).unwrap();
    assert_eq!(back, msg);
}

/// Issue #553, scenario 1: a late-arriving auto-generated title must never win
/// a race against (or clobber) a name the user set via `/name`.
#[tokio::test]
async fn if_unset_generated_title_never_clobbers_a_user_set_name() {
    let holly = Holly::spawn(EngineConfig::default());
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();

    // The user names the session first ...
    set_meta(&holly, &sid, Some("My Session"), None).await;
    let (name, _) = meta_of(recv_until(&mut sub, is_meta_changed).await);
    assert_eq!(name.as_deref(), Some("My Session"));

    // ... then the generator's late write arrives. It must no-op, not clobber.
    set_name_if_unset(&holly, &sid, "Generated Title").await;
    let (name, _) = meta_of(recv_until(&mut sub, is_meta_changed).await);
    assert_eq!(
        name.as_deref(),
        Some("My Session"),
        "an if_unset write must never overwrite an existing name"
    );
}

/// Issue #553, scenario 1 (reverse order): the generator's write still applies
/// when it lands *before* any user-set name.
#[tokio::test]
async fn if_unset_generated_title_applies_to_an_unnamed_session() {
    let holly = Holly::spawn(EngineConfig::default());
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();

    set_name_if_unset(&holly, &sid, "Generated Title").await;
    let (name, _) = meta_of(recv_until(&mut sub, is_meta_changed).await);
    assert_eq!(name.as_deref(), Some("Generated Title"));
}

/// Issue #553, scenario 2, at the pure-fold level: a name [`Session::replay`]
/// reconstructs from a persisted log is exactly as durable against a later
/// `if_unset` write as one set live in the same process — the fold doesn't
/// know or care whether the name arrived this run or was restored from disk.
#[test]
fn replayed_name_survives_a_later_if_unset_fold() {
    let sid = SessionId::new("s1");
    let records = vec![(
        None,
        OutEvent::SessionMetaChanged {
            session: sid.clone(),
            name: Some("Resumed Session Name".to_string()),
            action: None,
        },
    )];
    let session = Session::replay(&records, &EngineConfig::default(), &sid).unwrap();
    assert_eq!(session.name.as_deref(), Some("Resumed Session Name"));
}

/// Issue #553, scenario 2, end-to-end: hibernate a named session, resume it
/// (a fresh `Session` rebuilt via [`Session::replay`], the same path an
/// on-disk resume after a process restart takes), then send the generator's
/// `if_unset` write. The name restored by resume must win, exactly like a
/// name set earlier in the same process.
#[tokio::test]
async fn if_unset_write_after_resume_does_not_clobber_the_restored_name() {
    let holly = Holly::spawn(EngineConfig::default());
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();

    // Name the session, then hibernate it, capturing a resumable log
    // (`SessionStarted` + the `SetSessionMeta` fold) exactly as a runtime's
    // persistence layer would.
    holly.send(InMsg::prompt(sid.clone(), "hi")).await.unwrap();
    let mut log = Vec::new();
    let started = recv_until(
        &mut sub,
        |e| matches!(e, OutEvent::SessionStarted { session, .. } if *session == sid),
    )
    .await;
    log.push((Some(InMsg::prompt(sid.clone(), "hi")), started));
    log.push((
        None,
        recv_until(
            &mut sub,
            |e| matches!(e, OutEvent::Done { session, .. } if *session == sid),
        )
        .await,
    ));

    set_meta(&holly, &sid, Some("My Named Session"), None).await;
    let meta_changed = recv_until(&mut sub, is_meta_changed).await;
    log.push((None, meta_changed));

    holly.hibernate(sid.clone()).await.unwrap();
    recv_until(
        &mut sub,
        |e| matches!(e, OutEvent::SessionHibernated { session, .. } if *session == sid),
    )
    .await;

    // Resume: a fresh `Session` rebuilt from the log — the in-process stand-in
    // for "a new process resumed this session from its persisted file".
    holly.resume(sid.clone(), log).await.unwrap();
    recv_until(
        &mut sub,
        |e| matches!(e, OutEvent::Status { session, .. } if *session == sid),
    )
    .await;

    // The auto-title generator's write must not clobber the restored name.
    set_name_if_unset(&holly, &sid, "Auto-Generated Title").await;
    let (name, _) = meta_of(recv_until(&mut sub, is_meta_changed).await);
    assert_eq!(
        name.as_deref(),
        Some("My Named Session"),
        "an if_unset write must not overwrite a name restored by resume"
    );
}
