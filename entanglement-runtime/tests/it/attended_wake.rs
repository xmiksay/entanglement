//! A `serve` listing wakes a session this process hibernated (ADR-0247):
//! the owner's filtered `ListPending` resumes it — with the sub-agents its
//! hibernate put down — from the log and answers once core's re-offers are
//! back (none for a stopped turn), off the connection's read loop. One wake
//! per session; the connection's frames for it queue behind the listing. A
//! non-owner's listing, or one for a closed id, wakes nothing.

use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use entanglement_core::{Holly, InMsg, OutEvent, PendingApproval, Permission, SessionId};
use entanglement_runtime::attendance::Attendance;
use entanglement_runtime::persistence::spawn_persistence_subscriber;
use entanglement_runtime::policy::PermissionResolver;
use entanglement_runtime::serve::{router, Rejoin};
use entanglement_runtime::session_store::{LogPayload, SessionStore};
use entanglement_runtime::ToolRegistry;
use tokio::net::TcpListener;

use crate::attended_mode::{approve, asking};
use crate::attended_serve::{connect, first, send, Ws};
use crate::unattended_mode::{bash_call, done_text, spawn_wrapped, EchoBash};

/// Grades like its inner resolver, after a pause: a re-offered call's card
/// then arrives well after the resume, so a listing that did not wait for
/// it would miss it.
struct Slow(Arc<dyn PermissionResolver>);

#[async_trait]
impl PermissionResolver for Slow {
    async fn resolve(&self, session: &SessionId, tool: &str, input: &str) -> Permission {
        tokio::time::sleep(Duration::from_millis(400)).await;
        self.0.resolve(session, tool, input).await
    }
}

/// An engine logging to a temp store, served with a [`Rejoin`] over it.
pub(crate) struct Served {
    pub(crate) holly: Holly,
    pub(crate) port: u16,
    pub(crate) store: SessionStore,
    _tmp: tempfile::TempDir,
    _tap: tokio::task::JoinHandle<()>,
}

pub(crate) async fn served(slow: bool) -> Served {
    served_with(slow, vec![bash_call("t1", "echo hi"), done_text()]).await
}

/// [`served`] whose sessions' model makes `calls`.
pub(crate) async fn served_with(slow: bool, calls: Vec<entanglement_core::LlmResponse>) -> Served {
    let tmp = tempfile::tempdir().expect("tempdir");
    let store = SessionStore::at(tmp.path());
    let attendance = Attendance::new();
    let mut reg = ToolRegistry::new();
    reg.register(EchoBash);
    let holly = spawn_wrapped(reg, asking(1), calls, attendance.clone(), |inner| {
        if slow {
            Arc::new(Slow(inner))
        } else {
            inner
        }
    });
    let tap = spawn_persistence_subscriber(&holly, store.clone());
    let agents = Arc::new(RwLock::new(
        entanglement_runtime::agents::built_in_registry().expect("built-in agents"),
    ));
    let rejoin = Rejoin::new(store.clone(), agents);
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let app = router(holly.clone(), None, attendance, Some(rejoin));
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Served {
        holly,
        port,
        store,
        _tmp: tmp,
        _tap: tap,
    }
}

/// Prompt `sid` over `ws` (claiming it) and wait for its card.
pub(crate) async fn park(ws: &mut Ws, sid: &SessionId) -> String {
    send(ws, &InMsg::prompt(sid.clone(), "go")).await;
    first(ws, |e| match e {
        OutEvent::ToolRequest { request_id, .. } => Some(request_id),
        _ => None,
    })
    .await
}

/// Hibernate `sid` and wait until the log records it.
pub(crate) async fn hibernate(s: &Served, sid: &SessionId) {
    s.holly.hibernate(sid.clone()).await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !s.store.read(sid).unwrap_or_default().iter().any(|r| {
        matches!(
            &r.payload,
            LogPayload::Out(OutEvent::SessionHibernated { .. })
        )
    }) {
        assert!(tokio::time::Instant::now() < deadline, "never hibernated");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

pub(crate) async fn list(ws: &mut Ws, sid: &SessionId) -> Vec<PendingApproval> {
    let list = InMsg::ListPending {
        correlation_id: "c".into(),
        root: Some(sid.clone()),
    };
    send(ws, &list).await;
    first(ws, |e| match e {
        OutEvent::PendingList { approvals, .. } => Some(approvals),
        _ => None,
    })
    .await
}

/// A card waits on an attended session; its client leaves and the session
/// hibernates. A client that reconnects and sends only `ListPending{root}`
/// wakes the session: the re-offered card is on that one listing, and its
/// approve runs the call.
#[tokio::test]
async fn a_listing_wakes_a_hibernated_session_and_shows_its_card() {
    let s = served(false).await;
    let sid = SessionId::new("ws-hib");
    let mut ws = connect(s.port).await;
    park(&mut ws, &sid).await;
    drop(ws);
    hibernate(&s, &sid).await;

    let mut ws = connect(s.port).await;
    let started = Instant::now();
    let listed = list(&mut ws, &sid).await;
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "it waited out its bound"
    );
    let rid = listed
        .first()
        .map(|a| a.request_id.clone())
        .expect("the woken session's card is listed");
    send(&mut ws, &approve(&sid, &rid)).await;
    let output = first(&mut ws, |e| match e {
        OutEvent::ToolOutput { output, .. } => Some(output),
        _ => None,
    })
    .await;
    assert_eq!(output, "ran: echo hi");
}

/// A stopped turn re-offers nothing, so the listing answers at once (empty)
/// — and a frame sent right behind it is never held up by the wake.
#[tokio::test]
async fn a_stopped_sessions_listing_answers_at_once() {
    let s = served(false).await;
    let sid = SessionId::new("ws-stop");
    let mut ws = connect(s.port).await;
    park(&mut ws, &sid).await;
    send(
        &mut ws,
        &InMsg::Stop {
            session: sid.clone(),
        },
    )
    .await;
    first(&mut ws, |e| match e {
        OutEvent::ApprovalResolved { .. } => Some(()),
        _ => None,
    })
    .await;
    drop(ws);
    hibernate(&s, &sid).await;

    let mut ws = connect(s.port).await;
    let started = Instant::now();
    let listing = InMsg::ListPending {
        correlation_id: "c".into(),
        root: Some(sid.clone()),
    };
    send(&mut ws, &listing).await;
    let sessions = InMsg::ListSessions {
        correlation_id: "q".into(),
    };
    send(&mut ws, &sessions).await;
    let (mut listed, mut answered) = (None, None);
    while listed.is_none() || answered.is_none() {
        match first(&mut ws, |e| match e {
            OutEvent::PendingList { approvals, .. } => Some(Some(approvals)),
            OutEvent::SessionList { .. } => Some(None),
            _ => None,
        })
        .await
        {
            Some(approvals) => listed = Some((approvals, started.elapsed())),
            None => answered = Some(started.elapsed()),
        }
    }
    let (approvals, listed_in) = listed.expect("listed");
    assert!(approvals.is_empty(), "{approvals:?}");
    assert!(
        listed_in < Duration::from_secs(2),
        "listing took {listed_in:?}"
    );
    let answered_in = answered.expect("answered");
    assert!(
        answered_in < Duration::from_secs(2),
        "ListSessions took {answered_in:?}"
    );
}

/// A connection that does not own the session lists it: no wake, so the
/// session stays hibernated and the listing is empty.
#[tokio::test]
async fn a_non_owners_listing_does_not_wake_the_session() {
    let s = served(false).await;
    let sid = SessionId::new("ws-owned");
    let mut owner = connect(s.port).await;
    park(&mut owner, &sid).await;
    hibernate(&s, &sid).await;

    let mut other = connect(s.port).await;
    assert!(list(&mut other, &sid).await.is_empty());
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!s.holly.is_live(&sid), "a non-owner's listing woke it");
    drop(owner);
}

/// A card parked under `sid`, owned by a client that then left, and the
/// session hibernated: returns the card's request id.
pub(crate) async fn asleep_with_card(s: &Served, sid: &SessionId) -> String {
    let mut ws = connect(s.port).await;
    let rid = park(&mut ws, sid).await;
    drop(ws);
    hibernate(s, sid).await;
    rid
}

/// Every event on `ws` for `window`.
pub(crate) async fn drain(ws: &mut Ws, window: Duration) -> Vec<OutEvent> {
    let deadline = tokio::time::Instant::now() + window;
    let mut seen = Vec::new();
    while let Ok(Some(Ok(msg))) =
        tokio::time::timeout_at(deadline, futures::StreamExt::next(ws)).await
    {
        if let tokio_tungstenite::tungstenite::Message::Text(text) = msg {
            if let Ok(ev) = serde_json::from_str::<OutEvent>(&text) {
                seen.push(ev);
            }
        }
    }
    seen
}

pub(crate) fn errors(seen: &[OutEvent]) -> Vec<&String> {
    seen.iter()
        .filter_map(|e| match e {
            OutEvent::Error { message, .. } => Some(message),
            _ => None,
        })
        .collect()
}
