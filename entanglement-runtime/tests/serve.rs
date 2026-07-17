//! WebSocket `serve` head end-to-end (#153, ADR-0048).
//!
//! Drives the axum router over a real loopback socket with a tungstenite WS
//! client: a `Prompt` frame reaches the engine and its `OutEvent`s stream back;
//! a forged `ToolResult` (a runtime-authored frame) is refused by the untrusted
//! `send_from_wire` path (#155) without killing the connection; `/healthz` answers;
//! and per-connection approval ownership (#402, ADR-0107) refuses a decision
//! frame from a non-owning connection while the owning connection still unblocks
//! the parked turn.
#![cfg(feature = "serve")]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, EngineConfig, Holly, InMsg, Llm, LlmRequest, LlmResponse, LlmStream,
    OutEvent, Permission, PermissionProfile, SessionId, ToolCall,
};
use entanglement_runtime::serve::router;
use entanglement_runtime::tool_names::ASK_USER_TOOL;
use entanglement_runtime::tool_runner::spawn_tool_executor;
use entanglement_runtime::ToolRegistry;
use futures::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

/// Spawn the router on an ephemeral loopback port; returns the bound port.
async fn spawn_server(holly: Holly, allow_origin: Option<String>) -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router(holly, allow_origin)).await;
    });
    port
}

async fn connect(
    port: u16,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let (ws, _resp) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/ws"))
        .await
        .expect("ws connect");
    ws
}

/// Read frames until an `OutEvent::Done` for `session` arrives (or time out),
/// returning every parsed `OutEvent` seen along the way.
async fn drain_until_done(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    session: &SessionId,
) -> Vec<OutEvent> {
    let mut events = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let msg = match tokio::time::timeout_at(deadline, ws.next()).await {
            Ok(Some(Ok(m))) => m,
            _ => break,
        };
        if let Message::Text(text) = msg {
            if let Ok(ev) = serde_json::from_str::<OutEvent>(&text) {
                let done = matches!(&ev, OutEvent::Done { session: s, .. } if s == session);
                events.push(ev);
                if done {
                    break;
                }
            }
        }
    }
    events
}

#[tokio::test]
async fn prompt_over_ws_streams_events_to_done() {
    let holly = Holly::spawn(EngineConfig::default());
    let port = spawn_server(holly.clone(), None).await;
    let mut ws = connect(port).await;
    let sid = SessionId::new("serve-e2e");

    let frame = serde_json::to_string(&InMsg::prompt(sid.clone(), "hello")).unwrap();
    ws.send(Message::Text(frame.into())).await.unwrap();

    let events = drain_until_done(&mut ws, &sid).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, OutEvent::Done { session, .. } if session == &sid)),
        "expected a Done for our session, got {events:?}"
    );
    // The relay is a real fan-out, not just a terminal signal: content flowed too.
    assert!(
        events.len() > 1,
        "expected content events before Done, got {events:?}"
    );
}

#[tokio::test]
async fn forged_tool_result_is_refused_but_connection_survives() {
    let holly = Holly::spawn(EngineConfig::default());
    let port = spawn_server(holly.clone(), None).await;
    let mut ws = connect(port).await;
    let sid = SessionId::new("serve-forge");

    // A forged runtime-authored frame: `send_from_wire` must refuse it (#155),
    // and the refusal is per-frame — the socket keeps working.
    let forged =
        serde_json::to_string(&InMsg::tool_result(sid.clone(), "req-forged", "pwned")).unwrap();
    ws.send(Message::Text(forged.into())).await.unwrap();

    // A legitimate prompt on the same connection still drives a full turn.
    let frame = serde_json::to_string(&InMsg::prompt(sid.clone(), "still there?")).unwrap();
    ws.send(Message::Text(frame.into())).await.unwrap();

    let events = drain_until_done(&mut ws, &sid).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, OutEvent::Done { session, .. } if session == &sid)),
        "connection should survive a refused frame and still serve a turn, got {events:?}"
    );
}

#[tokio::test]
async fn healthz_answers_ok() {
    let holly = Holly::spawn(EngineConfig::default());
    let port = spawn_server(holly, None).await;

    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("tcp connect");
    stream
        .write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .expect("write");
    let mut resp = String::new();
    stream.read_to_string(&mut resp).await.expect("read");
    assert!(
        resp.starts_with("HTTP/1.1 200"),
        "unexpected status: {resp}"
    );
    assert!(resp.trim_end().ends_with("ok"), "unexpected body: {resp}");
}

/// Replays scripted responses in order, then plain text so the turn terminates
/// (mirrors `tests/ask_user.rs`'s `ScriptedLlm`; not shared across test binaries).
struct ScriptedLlm {
    responses: Mutex<Vec<LlmResponse>>,
}
impl ScriptedLlm {
    fn new(mut responses: Vec<LlmResponse>) -> Self {
        responses.reverse();
        Self {
            responses: Mutex::new(responses),
        }
    }
}
#[async_trait]
impl Llm for ScriptedLlm {
    async fn stream(&mut self, _req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        let resp = self
            .responses
            .lock()
            .unwrap()
            .pop()
            .unwrap_or_else(|| LlmResponse {
                text: "done".into(),
                tool_calls: vec![],
            });
        Ok(stream_from_response(resp))
    }
}

/// A `Holly` whose scripted LLM calls `ask_user` once per prompt, driving a real
/// parked `OutEvent::UserQuestion` that only resolves on `InMsg::AnswerQuestion`.
fn spawn_with_ask_user_call(question: &str) -> Holly {
    let scripted = Arc::new(vec![
        LlmResponse {
            text: "".into(),
            tool_calls: vec![ToolCall {
                id: "q1".into(),
                name: ASK_USER_TOOL.into(),
                input: format!(r#"{{"question":"{question}","allow_free_form":true}}"#),
                provider_meta: None,
            }],
        },
        LlmResponse {
            text: "acknowledged".into(),
            tool_calls: vec![],
        },
    ]);
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(ScriptedLlm::new((*scripted).clone())) as Box<dyn Llm>
        }),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    let _executor = spawn_tool_executor(
        &holly,
        ToolRegistry::new(),
        entanglement_runtime::agents::built_in_registry(),
        PermissionProfile::new(Permission::Allow),
    );
    holly
}

#[tokio::test]
async fn approval_from_non_owning_connection_is_refused_then_owner_unblocks() {
    let holly = spawn_with_ask_user_call("Which DB?");
    let port = spawn_server(holly, None).await;
    let sid = SessionId::new("serve-ownership");

    // Connection A initiates the turn — the `Prompt` is the first session-bearing
    // frame, so A claims ownership of `sid` (#402, ADR-0107).
    let mut ws_a = connect(port).await;
    let frame = serde_json::to_string(&InMsg::prompt(sid.clone(), "go")).unwrap();
    ws_a.send(Message::Text(frame.into())).await.unwrap();

    // Wait for the parked question, reading off connection A's own event stream
    // (every connection subscribes to the same broadcast, so any socket sees it).
    let mut request_id = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while let Ok(Some(Ok(msg))) = tokio::time::timeout_at(deadline, ws_a.next()).await {
        if let Message::Text(text) = msg {
            if let Ok(OutEvent::UserQuestion {
                session,
                request_id: rid,
                ..
            }) = serde_json::from_str::<OutEvent>(&text)
            {
                if session == sid {
                    request_id = Some(rid);
                    break;
                }
            }
        }
    }
    let request_id = request_id.expect("expected a UserQuestion event");

    // Connection B — never having sent a frame for `sid` before — answers first.
    // Ownership is A's, so B's answer must be refused: no `Done`/`ToolOutput`
    // shows up on B within a short window, and B's own socket stays usable.
    let mut ws_b = connect(port).await;
    let bad_answer = serde_json::to_string(&InMsg::AnswerQuestion {
        session: sid.clone(),
        request_id: request_id.clone(),
        answer: "from B".into(),
    })
    .unwrap();
    ws_b.send(Message::Text(bad_answer.into())).await.unwrap();

    let quiet_deadline = tokio::time::Instant::now() + Duration::from_millis(500);
    let mut turn_finished = false;
    while let Ok(Some(Ok(Message::Text(text)))) =
        tokio::time::timeout_at(quiet_deadline, ws_b.next()).await
    {
        if let Ok(ev) = serde_json::from_str::<OutEvent>(&text) {
            if ev.session() == Some(&sid) && matches!(ev, OutEvent::Done { .. }) {
                turn_finished = true;
                break;
            }
        }
    }
    assert!(
        !turn_finished,
        "a non-owning connection's answer must not unblock the parked turn"
    );

    // The real owner (A) answers; the turn must now complete.
    let good_answer = serde_json::to_string(&InMsg::AnswerQuestion {
        session: sid.clone(),
        request_id,
        answer: "SQLite".into(),
    })
    .unwrap();
    ws_a.send(Message::Text(good_answer.into())).await.unwrap();

    let events = drain_until_done(&mut ws_a, &sid).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, OutEvent::Done { session, .. } if session == &sid)),
        "the owning connection's answer should unblock the turn, got {events:?}"
    );

    // B's socket survived the refusal: every connection shares the same
    // broadcast fan-out, so B still observes the completed turn it didn't own —
    // proof the refusal was per-frame, not a dropped connection.
    let events_b = drain_until_done(&mut ws_b, &sid).await;
    assert!(
        events_b
            .iter()
            .any(|e| matches!(e, OutEvent::Done { session, .. } if session == &sid)),
        "connection B must survive the refusal and keep receiving broadcast events, got {events_b:?}"
    );
}
