//! A `serve` client owns — so attends (ADR-0247) — the session it prompts.
//! It drops and reconnects after the mode's timeout, finds the still-waiting
//! card or question with `ListPending`/`ListQuestions`, and answers it.

use std::time::Duration;

use entanglement_core::{Holly, InMsg, OutEvent, SessionId};
use entanglement_runtime::attendance::Attendance;
use entanglement_runtime::serve::router;
use futures::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

use crate::attended_mode::approve;
use crate::attended_rejoin::engine;
use crate::unattended_mode::{ask_user_call, done_text, spawn_attended};

pub(crate) type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn serve(holly: Holly, attendance: Attendance) -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router(holly, None, attendance, None)).await;
    });
    port
}

pub(crate) async fn connect(port: u16) -> Ws {
    tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/ws"))
        .await
        .expect("ws connect")
        .0
}

pub(crate) async fn send(ws: &mut Ws, msg: &InMsg) {
    let frame = serde_json::to_string(msg).expect("serializes");
    ws.send(Message::Text(frame.into())).await.expect("sent");
}

/// The first `OutEvent` on `ws` that `pick` maps to `Some`.
pub(crate) async fn first<T>(ws: &mut Ws, pick: impl Fn(OutEvent) -> Option<T>) -> T {
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("a frame arrives")
            .expect("socket open")
            .expect("frame ok");
        if let Message::Text(text) = msg {
            if let Some(found) = serde_json::from_str::<OutEvent>(&text).ok().and_then(&pick) {
                return found;
            }
        }
    }
}

/// A `serve` client owns (so attends) the session it prompts; it drops
/// and reconnects after the timeout, finds the card with `ListPending`,
/// and its approve runs the call.
#[tokio::test]
async fn a_reconnecting_serve_client_lists_and_answers_the_card() {
    let attendance = Attendance::new();
    let holly = engine(attendance.clone());
    let port = serve(holly.clone(), attendance).await;
    let sid = SessionId::new("ws-1");
    let mut ws = connect(port).await;
    send(&mut ws, &InMsg::prompt(sid.clone(), "go")).await;
    first(&mut ws, |e| match e {
        OutEvent::ToolRequest { request_id, .. } => Some(request_id),
        _ => None,
    })
    .await;
    drop(ws);
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let mut ws = connect(port).await;
    let list = InMsg::ListPending {
        correlation_id: "c".into(),
        root: Some(sid.clone()),
    };
    send(&mut ws, &list).await;
    let rid = first(&mut ws, |e| match e {
        OutEvent::PendingList { approvals, .. } => approvals.first().map(|a| a.request_id.clone()),
        _ => None,
    })
    .await;
    send(&mut ws, &approve(&sid, &rid)).await;
    let output = first(&mut ws, |e| match e {
        OutEvent::ToolOutput { output, .. } => Some(output),
        _ => None,
    })
    .await;
    assert_eq!(output, "ran: echo hi");
}

/// The same for an `ask_user` question and `ListQuestions`.
#[tokio::test]
async fn a_reconnecting_serve_client_lists_and_answers_the_question() {
    let attendance = Attendance::new();
    let holly = spawn_attended(
        entanglement_runtime::ToolRegistry::new(),
        crate::unattended_mode::one_mode_table(
            entanglement_core::Permission::Allow,
            entanglement_runtime::mode::Limits {
                question_timeout: 1,
                ..Default::default()
            },
        ),
        vec![
            ask_user_call(
                "q1",
                r#"{"questions":[{"question":"Which?","options":[{"label":"A"},{"label":"B"}]}]}"#,
            ),
            done_text(),
        ],
        attendance.clone(),
    );
    let port = serve(holly, attendance).await;
    let sid = SessionId::new("ws-q");
    let mut ws = connect(port).await;
    send(&mut ws, &InMsg::prompt(sid.clone(), "go")).await;
    first(&mut ws, |e| match e {
        OutEvent::UserQuestion { request_id, .. } => Some(request_id),
        _ => None,
    })
    .await;
    drop(ws);
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let mut ws = connect(port).await;
    let list = InMsg::ListQuestions {
        correlation_id: "c".into(),
        session: Some(sid.clone()),
    };
    send(&mut ws, &list).await;
    let rid = first(&mut ws, |e| match e {
        OutEvent::QuestionList { questions, .. } => questions.first().map(|q| q.request_id.clone()),
        _ => None,
    })
    .await;
    send(
        &mut ws,
        &InMsg::answer_question(sid.clone(), rid, vec![vec!["B".into()]]),
    )
    .await;
    let output = first(&mut ws, |e| match e {
        OutEvent::ToolOutput { output, .. } => Some(output),
        _ => None,
    })
    .await;
    assert_eq!(output, "B");
}

/// A card parked while nobody owned the session (its repeat's timer
/// running): a client that only lists it claims and attends the session,
/// so the card waits past the timeout and its approve runs the call.
#[tokio::test]
async fn listing_alone_attends_and_keeps_the_card_waiting() {
    let attendance = Attendance::new();
    let holly = crate::attended_mode::bash_engine(
        crate::attended_mode::asking(2),
        vec![
            crate::unattended_mode::bash_call("t1", "echo hi"),
            crate::unattended_mode::bash_call("t2", "echo hi"),
            done_text(),
        ],
        attendance.clone(),
    );
    let port = serve(holly.clone(), attendance).await;
    let sid = SessionId::new("ws-unowned");
    let mut sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    crate::attended_mode::card(&mut sub, &sid).await;

    let mut ws = connect(port).await;
    let list = InMsg::ListPending {
        correlation_id: "c".into(),
        root: Some(sid.clone()),
    };
    send(&mut ws, &list).await;
    let rid = first(&mut ws, |e| match e {
        OutEvent::PendingList { approvals, .. } => approvals.first().map(|a| a.request_id.clone()),
        _ => None,
    })
    .await;
    tokio::time::sleep(Duration::from_millis(2500)).await;
    send(&mut ws, &approve(&sid, &rid)).await;
    let output = first(&mut ws, |e| match e {
        OutEvent::ToolOutput { output, .. } if !output.contains("denied") => Some(output),
        _ => None,
    })
    .await;
    assert_eq!(output, "ran: echo hi");
}
