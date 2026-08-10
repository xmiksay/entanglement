//! Authenticated `serve` head end-to-end (#674, ADR-0174).
//!
//! Drives `router_with_auth` over a real loopback socket: a missing/bad bearer
//! token is refused at the WS upgrade (401, never reaching the WS loop); a
//! good token connects and its first `Prompt` to a fresh session id makes the
//! head author a trusted `Spawn` binding that user (visible on
//! `SessionStarted.user`, no duplicate spawn on a second `Prompt`); a
//! cross-user frame into another tenant's session is refused ahead of
//! ADR-0107's `SessionOwners`, which keeps arbitrating *same-user*
//! multi-connection robustness unchanged; the unauthenticated router is
//! byte-for-byte untouched; and the registry maintainer forgets a closed
//! session.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, EngineConfig, Holly, InMsg, Llm, LlmRequest, LlmResponse, LlmStream,
    OutEvent, Permission, PermissionProfile, SessionId, ToolCall, UserId,
};
use entanglement_runtime::multi_user::SessionUserRegistry;
use entanglement_runtime::serve::{
    router_with_auth, ServeAuth, StaticTokenAuthenticator, WireAuthenticator,
};
use entanglement_runtime::tool_names::ASK_USER_TOOL;
use entanglement_runtime::tool_runner::spawn_tool_executor;
use entanglement_runtime::ToolRegistry;
use futures::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

fn two_user_auth() -> ServeAuth {
    ServeAuth {
        authenticator: Arc::new(StaticTokenAuthenticator::new(
            [
                ("tok-a".to_string(), UserId::new("alice")),
                ("tok-b".to_string(), UserId::new("bob")),
            ]
            .into_iter()
            .collect(),
        )),
        registry: SessionUserRegistry::new(),
    }
}

/// Spawn the authed router on an ephemeral loopback port.
async fn spawn_auth_server(holly: Holly, auth: ServeAuth) -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router_with_auth(holly, None, Some(auth))).await;
    });
    port
}

/// Handshake with an optional bearer token; the `Err` carries the HTTP
/// response on refusal.
async fn connect_with_token(port: u16, token: Option<&str>) -> Result<Ws, WsError> {
    let mut request = format!("ws://127.0.0.1:{port}/ws")
        .into_client_request()
        .expect("request");
    if let Some(token) = token {
        request.headers_mut().insert(
            "authorization",
            format!("Bearer {token}").parse().expect("header"),
        );
    }
    tokio_tungstenite::connect_async(request)
        .await
        .map(|(ws, _)| ws)
}

/// Read frames until an `OutEvent::Done` for `session` arrives (or time out),
/// returning every parsed `OutEvent` seen along the way. (Copied from
/// `tests/serve.rs` — helpers aren't shared across test binaries.)
async fn drain_until_done(ws: &mut Ws, session: &SessionId) -> Vec<OutEvent> {
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
async fn missing_or_bad_token_is_refused_at_upgrade_with_401() {
    let holly = Holly::spawn(EngineConfig::default());
    let port = spawn_auth_server(holly, two_user_auth()).await;

    for token in [None, Some("tok-wrong")] {
        match connect_with_token(port, token).await {
            Err(WsError::Http(resp)) => {
                assert_eq!(resp.status(), 401, "expected 401 for token {token:?}")
            }
            other => panic!("expected an HTTP 401 refusal for token {token:?}, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn good_token_connects_and_healthz_needs_no_auth() {
    let holly = Holly::spawn(EngineConfig::default());
    let port = spawn_auth_server(holly, two_user_auth()).await;

    // /healthz stays credential-free — auth gates the WS, not liveness.
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("tcp connect");
    stream
        .write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .expect("write");
    let mut resp = String::new();
    stream.read_to_string(&mut resp).await.expect("read");
    assert!(resp.starts_with("HTTP/1.1 200"), "unexpected: {resp}");

    connect_with_token(port, Some("tok-a"))
        .await
        .expect("a valid token must connect");
}

#[tokio::test]
async fn first_prompt_spawn_authors_the_users_session_without_duplicates() {
    let holly = Holly::spawn(EngineConfig::default());
    let auth = two_user_auth();
    let registry = auth.registry.clone();
    let port = spawn_auth_server(holly, auth).await;
    let mut ws = connect_with_token(port, Some("tok-a"))
        .await
        .expect("connect");
    let sid = SessionId::new("auth-e2e");

    let frame = serde_json::to_string(&InMsg::prompt(sid.clone(), "hello")).unwrap();
    ws.send(Message::Text(frame.into())).await.unwrap();
    let events = drain_until_done(&mut ws, &sid).await;

    let started: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            OutEvent::SessionStarted {
                session,
                user,
                root,
                profile,
                ..
            } if *session == sid => Some((user.clone(), *root, profile.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(started.len(), 1, "exactly one spawn, got {events:?}");
    assert_eq!(started[0].0, Some(UserId::new("alice")));
    assert!(started[0].1, "the authored spawn is a root");
    assert_eq!(started[0].2, "build", "the lazy-path default profile");
    assert_eq!(registry.user_for(&sid), Some(UserId::new("alice")));

    // A second prompt reuses the live session — no second SessionStarted.
    let frame = serde_json::to_string(&InMsg::prompt(sid.clone(), "again")).unwrap();
    ws.send(Message::Text(frame.into())).await.unwrap();
    let events = drain_until_done(&mut ws, &sid).await;
    assert!(
        events
            .iter()
            .all(|e| !matches!(e, OutEvent::SessionStarted { session, .. } if *session == sid)),
        "no duplicate spawn on a second prompt, got {events:?}"
    );
}

#[tokio::test]
async fn cross_user_prompt_into_a_foreign_session_is_refused() {
    let holly = Holly::spawn(EngineConfig::default());
    let port = spawn_auth_server(holly, two_user_auth()).await;
    let sid = SessionId::new("alice-owned");

    // Alice creates and finishes a turn on her session.
    let mut ws_a = connect_with_token(port, Some("tok-a")).await.expect("a");
    let frame = serde_json::to_string(&InMsg::prompt(sid.clone(), "mine")).unwrap();
    ws_a.send(Message::Text(frame.into())).await.unwrap();
    drain_until_done(&mut ws_a, &sid).await;

    // Bob injects a Prompt into Alice's session: the hard tenant gate refuses
    // it — no new turn (no Done) shows up within a quiet window.
    let mut ws_b = connect_with_token(port, Some("tok-b")).await.expect("b");
    let frame = serde_json::to_string(&InMsg::prompt(sid.clone(), "gimme")).unwrap();
    ws_b.send(Message::Text(frame.into())).await.unwrap();
    let quiet = tokio::time::Instant::now() + Duration::from_millis(500);
    let mut foreign_turn = false;
    while let Ok(Some(Ok(Message::Text(text)))) = tokio::time::timeout_at(quiet, ws_b.next()).await
    {
        if let Ok(ev) = serde_json::from_str::<OutEvent>(&text) {
            if matches!(&ev, OutEvent::Done { session, .. } if *session == sid) {
                foreign_turn = true;
                break;
            }
        }
    }
    assert!(!foreign_turn, "a cross-user prompt must not run a turn");

    // Alice's session is unharmed — she can still drive it.
    let frame = serde_json::to_string(&InMsg::prompt(sid.clone(), "still mine")).unwrap();
    ws_a.send(Message::Text(frame.into())).await.unwrap();
    let events = drain_until_done(&mut ws_a, &sid).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, OutEvent::Done { session, .. } if *session == sid)),
        "the owner still drives their session, got {events:?}"
    );
}

/// Replays scripted responses in order, then plain text so the turn terminates
/// (copied from `tests/serve.rs`; not shared across test binaries).
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

/// A `Holly` whose scripted LLM calls `ask_user` once per prompt, driving a
/// real parked `OutEvent::UserQuestion` that only resolves on
/// `InMsg::AnswerQuestion` (copied from `tests/serve.rs`).
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
        entanglement_runtime::agents::built_in_registry().expect("built-in agents must parse"),
        PermissionProfile::new(Permission::Allow),
    );
    holly
}

/// Park a question on `sid` via `ws`, returning the question's `request_id`.
async fn park_question(ws: &mut Ws, sid: &SessionId) -> String {
    let frame = serde_json::to_string(&InMsg::prompt(sid.clone(), "go")).unwrap();
    ws.send(Message::Text(frame.into())).await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while let Ok(Some(Ok(msg))) = tokio::time::timeout_at(deadline, ws.next()).await {
        if let Message::Text(text) = msg {
            if let Ok(OutEvent::UserQuestion {
                session,
                request_id,
                ..
            }) = serde_json::from_str::<OutEvent>(&text)
            {
                if session == *sid {
                    return request_id;
                }
            }
        }
    }
    panic!("expected a UserQuestion event");
}

/// Answer `request_id` on `sid` over `ws` and report whether the turn
/// completed (a `Done`) within `window`.
async fn answer_and_wait_done(
    ws: &mut Ws,
    sid: &SessionId,
    request_id: String,
    text: &str,
    window: Duration,
) -> bool {
    let frame = serde_json::to_string(&InMsg::answer_question(
        sid.clone(),
        request_id,
        vec![vec![text.into()]],
    ))
    .unwrap();
    ws.send(Message::Text(frame.into())).await.unwrap();
    let quiet = tokio::time::Instant::now() + window;
    loop {
        let msg = match tokio::time::timeout_at(quiet, ws.next()).await {
            Ok(Some(Ok(m))) => m,
            _ => return false,
        };
        if let Message::Text(text) = msg {
            if let Ok(ev) = serde_json::from_str::<OutEvent>(&text) {
                if matches!(&ev, OutEvent::Done { session, .. } if session == sid) {
                    return true;
                }
            }
        }
    }
}

#[tokio::test]
async fn cross_user_answer_is_refused_then_the_owner_unblocks() {
    let holly = spawn_with_ask_user_call("Which DB?");
    let port = spawn_auth_server(holly, two_user_auth()).await;
    let sid = SessionId::new("auth-ownership");

    let mut ws_a = connect_with_token(port, Some("tok-a")).await.expect("a");
    let request_id = park_question(&mut ws_a, &sid).await;

    // Bob answers first: refused by the hard tenant gate (his frame never even
    // reaches SessionOwners), the parked turn stays parked.
    let mut ws_b = connect_with_token(port, Some("tok-b")).await.expect("b");
    assert!(
        !answer_and_wait_done(
            &mut ws_b,
            &sid,
            request_id.clone(),
            "from bob",
            Duration::from_millis(600)
        )
        .await,
        "another tenant's answer must not unblock the parked turn"
    );

    // Alice answers: the turn completes.
    assert!(
        answer_and_wait_done(
            &mut ws_a,
            &sid,
            request_id,
            "SQLite",
            Duration::from_secs(10)
        )
        .await,
        "the owner's answer should unblock the turn"
    );
}

#[tokio::test]
async fn same_user_multi_connection_is_still_arbitrated_by_session_owners() {
    let holly = spawn_with_ask_user_call("Which DB?");
    let port = spawn_auth_server(holly, two_user_auth()).await;
    let sid = SessionId::new("auth-same-user");

    // Two connections, same token: the per-user gate passes both, so ADR-0107's
    // first-writer-wins still arbitrates within the tenant.
    let mut conn1 = connect_with_token(port, Some("tok-a")).await.expect("c1");
    let request_id = park_question(&mut conn1, &sid).await;

    let mut conn2 = connect_with_token(port, Some("tok-a")).await.expect("c2");
    assert!(
        !answer_and_wait_done(
            &mut conn2,
            &sid,
            request_id.clone(),
            "from tab 2",
            Duration::from_millis(600)
        )
        .await,
        "a same-user non-owning connection is still refused by SessionOwners"
    );
    assert!(
        answer_and_wait_done(
            &mut conn1,
            &sid,
            request_id,
            "SQLite",
            Duration::from_secs(10)
        )
        .await,
        "the owning connection's answer should unblock the turn"
    );
}

#[tokio::test]
async fn unauthenticated_router_is_unchanged_and_sessions_carry_no_user() {
    // The default posture (`router`, no auth): no header needed, and the
    // lazily-created session has no user.
    let holly = Holly::spawn(EngineConfig::default());
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move {
        let _ = axum::serve(listener, entanglement_runtime::serve::router(holly, None)).await;
    });
    let mut ws = connect_with_token(port, None).await.expect("connect");
    let sid = SessionId::new("unauth-e2e");
    let frame = serde_json::to_string(&InMsg::prompt(sid.clone(), "hello")).unwrap();
    ws.send(Message::Text(frame.into())).await.unwrap();
    let events = drain_until_done(&mut ws, &sid).await;
    let user = events.iter().find_map(|e| match e {
        OutEvent::SessionStarted { session, user, .. } if *session == sid => Some(user.clone()),
        _ => None,
    });
    assert_eq!(user, Some(None), "an unauthenticated session has no user");
}

#[tokio::test]
async fn registry_forgets_a_closed_session() {
    let holly = Holly::spawn(EngineConfig::default());
    let auth = two_user_auth();
    let registry = auth.registry.clone();
    let port = spawn_auth_server(holly, auth).await;
    let mut ws = connect_with_token(port, Some("tok-a"))
        .await
        .expect("connect");
    let sid = SessionId::new("auth-lifecycle");

    let frame = serde_json::to_string(&InMsg::prompt(sid.clone(), "hello")).unwrap();
    ws.send(Message::Text(frame.into())).await.unwrap();
    drain_until_done(&mut ws, &sid).await;
    assert_eq!(registry.user_for(&sid), Some(UserId::new("alice")));

    let frame = serde_json::to_string(&InMsg::CloseSession {
        session: sid.clone(),
    })
    .unwrap();
    ws.send(Message::Text(frame.into())).await.unwrap();
    // The broadcast maintainer folds SessionEnded → forget; poll for it.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while registry.user_for(&sid).is_some() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "registry should forget a closed session"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[test]
fn custom_authenticator_trait_object_is_usable() {
    // The pluggable seam: a deployment's own resolver, no static map.
    struct Suffix;
    impl WireAuthenticator for Suffix {
        fn authenticate(&self, credential: &str) -> Option<UserId> {
            credential.strip_prefix("user:").map(UserId::new)
        }
    }
    let auth: Arc<dyn WireAuthenticator> = Arc::new(Suffix);
    assert_eq!(auth.authenticate("user:carol"), Some(UserId::new("carol")));
    assert_eq!(auth.authenticate("nope"), None);
}
