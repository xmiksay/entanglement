//! Per-connection WS handler for the `serve` head — split from `serve.rs`
//! (#674) along the 400-line file cap, the same sibling-child-module shape
//! `session_store/` uses. One socket = one [`handle_socket`] call: relay every
//! `OutEvent` out, route every inbound frame in through the untrusted
//! [`Holly::send_from_wire`] path.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use entanglement_core::{IdKind, InMsg, SessionId, WireError};
use futures::{SinkExt, StreamExt};
use tokio::sync::broadcast::error::RecvError;

use super::ServeState;

/// Keep an idle socket (and any NAT/proxy in front of a raw client) alive.
const PING_INTERVAL: Duration = Duration::from_secs(30);

/// One connection: relay every `OutEvent` out, route every inbound frame in.
pub(super) async fn handle_socket(socket: WebSocket, state: Arc<ServeState>) {
    let (mut sink, mut stream) = socket.split();
    let mut sub = state.holly.subscribe();
    // A per-connection default session lets a bare-text frame become a `Prompt`,
    // matching the stdio `pipe` head's scripting affordance.
    let default_session = SessionId::new(state.holly.next_id(IdKind::Session));
    // Identifies this connection for approval ownership (#402, ADR-0107).
    let conn_id = state.next_conn_id.fetch_add(1, Ordering::Relaxed);

    // Outbound pump: fan-out events as JSON text frames; a periodic ping keeps an
    // otherwise-silent socket alive.
    let out = tokio::spawn(async move {
        let mut ping = tokio::time::interval(PING_INTERVAL);
        ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                ev = sub.recv() => match ev {
                    Ok(ev) => {
                        let json = match serde_json::to_string(&ev) {
                            Ok(j) => j,
                            Err(e) => {
                                tracing::warn!("serve: unserializable OutEvent dropped: {e}");
                                continue;
                            }
                        };
                        if sink.send(Message::Text(json.into())).await.is_err() {
                            break; // client hung up
                        }
                    }
                    // A lag is a dropped-events gap, not end-of-stream (#158): keep
                    // relaying so the socket self-heals instead of dying silently.
                    Err(RecvError::Lagged(n)) => {
                        tracing::warn!("serve: ws relay lagged, skipped {n} events");
                    }
                    Err(RecvError::Closed) => break,
                },
                _ = ping.tick() => {
                    if sink.send(Message::Ping(Vec::new().into())).await.is_err() {
                        break;
                    }
                }
            }
        }
    });

    // Inbound pump: parse each text frame as an `InMsg` and route it through the
    // untrusted wire path (#155). A non-JSON line falls back to a `Prompt` on this
    // connection's default session (pipe parity). Ping/pong/binary are ignored
    // (axum answers pings itself).
    while let Some(Ok(msg)) = stream.next().await {
        match msg {
            Message::Text(text) => {
                let trimmed = text.trim();
                if trimmed.is_empty() {
                    continue;
                }
                match serde_json::from_str::<InMsg>(trimmed) {
                    Ok(m) => {
                        // Claim/verify ownership on every session-bearing frame so a
                        // session gets an owner as early as possible (typically the
                        // initiating `Prompt`); only the decision variants are
                        // actually gated on it (#402, ADR-0107). `RetractQuestion`/
                        // `ReplaceQuestion` (#515) join the gate alongside
                        // `AnswerQuestion` — all three resolve the same class of
                        // parked `ask_user` waiter, so a non-owning connection must
                        // not be able to retract/replace one either.
                        if let Some(session) = m.session() {
                            let owner_ok = state.session_owners.touch(session, conn_id);
                            if !owner_ok
                                && matches!(
                                    m,
                                    InMsg::Approve { .. }
                                        | InMsg::Reject { .. }
                                        | InMsg::AnswerQuestion { .. }
                                        | InMsg::RetractQuestion { .. }
                                        | InMsg::ReplaceQuestion { .. }
                                )
                            {
                                tracing::warn!(
                                    %session,
                                    conn_id,
                                    "serve: refused approval decision from a non-owning connection"
                                );
                                continue;
                            }
                        }
                        match state.holly.send_from_wire(m).await {
                            Ok(()) => {}
                            Err(WireError::Closed) => break, // engine gone
                            Err(e @ (WireError::Privileged(_) | WireError::OverlayEnable)) => {
                                tracing::warn!("serve: {e}");
                            }
                        }
                    }
                    Err(e) => {
                        tracing::debug!("serve: non-InMsg frame treated as prompt ({e})");
                        if state
                            .holly
                            .send(InMsg::prompt(default_session.clone(), trimmed.to_string()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }
    // Release this connection's session ownership so a still-parked approval
    // doesn't deadlock behind a client that just disconnected (#402, ADR-0107).
    state.session_owners.release(conn_id);
    out.abort();
}
