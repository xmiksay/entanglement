//! Per-connection WS handler for the `serve` head — split from `serve.rs`
//! (#674) along the 400-line file cap, the same sibling-child-module shape
//! `session_store/` uses. One socket = one [`handle_socket`] call: relay every
//! `OutEvent` out, route every inbound frame in through the untrusted
//! [`Holly::send_from_wire`] path.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use entanglement_core::{IdKind, InMsg, SessionId, UserId, WireError, DEFAULT_PROFILE};
use futures::{SinkExt, StreamExt};
use tokio::sync::broadcast::error::RecvError;

use crate::multi_user::SessionUserRegistry;

use super::ServeState;

/// Keep an idle socket (and any NAT/proxy in front of a raw client) alive.
const PING_INTERVAL: Duration = Duration::from_secs(30);

/// One connection: relay every `OutEvent` out, route every inbound frame in.
/// `user` is the connection's authenticated identity (#674, ADR-0174) —
/// `None` in the default unauthenticated posture, where every auth-related
/// branch below is structurally a no-op.
pub(super) async fn handle_socket(socket: WebSocket, state: Arc<ServeState>, user: Option<UserId>) {
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
                        if let (Some(uid), Some(session)) = (&user, m.session()) {
                            let registry =
                                &state.auth.as_ref().expect("user implies auth").registry;
                            // Hard tenant boundary (#674, ADR-0174 §4), checked
                            // BEFORE the cooperative ADR-0107 ownership below:
                            // any session-bearing frame into a session
                            // registered to a *different* user is refused
                            // outright. Wider than the ADR's literal
                            // decision-frame list — a cross-tenant `Prompt`/
                            // `Stop`/`CloseSession` is injection into someone
                            // else's session, which a hard boundary cannot
                            // allow either.
                            if user_gate_refuses(registry, session, uid) {
                                tracing::warn!(
                                    %session,
                                    conn_id,
                                    "serve: refused cross-user frame in authenticated mode"
                                );
                                continue;
                            }
                            // The trusted-`Spawn`-author path (ADR-0174 §2): a
                            // `Prompt` naming a session the registry doesn't
                            // know yet is this user's session-creation entry
                            // point. Register synchronously (so the very next
                            // frame already sees the binding), then author the
                            // privileged `Spawn` that binds `user` — with an
                            // empty prompt, so no turn runs until the client's
                            // real `Prompt` relays through `send_from_wire`
                            // below. A repeat `Prompt` on the now-registered id
                            // skips this; a duplicate `Spawn` would be a
                            // supervisor no-op anyway. Zero change to
                            // `wire_allowed()` — the head holds the privileged
                            // handle, exactly like an embedder.
                            if matches!(m, InMsg::Prompt { .. })
                                && registry.user_for(session).is_none()
                            {
                                registry.register(session.clone(), uid.clone());
                                if spawn_for_user(&state, session.clone(), uid.clone())
                                    .await
                                    .is_err()
                                {
                                    break; // engine gone
                                }
                            }
                        }
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
                        // The bare-text fallback creates a session too — in
                        // authenticated mode it must go through the same
                        // spawn-author step, or it would silently mint a
                        // `user: None` session for an authenticated client.
                        if let Some(uid) = &user {
                            let registry =
                                &state.auth.as_ref().expect("user implies auth").registry;
                            if registry.user_for(&default_session).is_none() {
                                registry.register(default_session.clone(), uid.clone());
                                if spawn_for_user(&state, default_session.clone(), uid.clone())
                                    .await
                                    .is_err()
                                {
                                    break;
                                }
                            }
                        }
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

/// Whether the hard per-user boundary refuses this frame (#674, ADR-0174 §4):
/// only when the session is registered to a *different* user. An unregistered
/// session passes (it is about to be spawn-authored, or predates auth), and
/// the unauthenticated posture never calls this at all.
fn user_gate_refuses(registry: &SessionUserRegistry, session: &SessionId, user: &UserId) -> bool {
    matches!(registry.user_for(session), Some(owner) if owner != *user)
}

/// Author the privileged root `Spawn` binding `session` to `user` (ADR-0174
/// §2) — empty prompt (starts idle, #674 core guard), the same
/// `DEFAULT_PROFILE` the lazy-`Prompt` path resolves. `Err` means the engine
/// inbox closed.
async fn spawn_for_user(
    state: &ServeState,
    session: SessionId,
    user: UserId,
) -> Result<(), tokio::sync::mpsc::error::SendError<InMsg>> {
    state
        .holly
        .send(InMsg::Spawn {
            session,
            parent: None,
            predecessor: None,
            agent: DEFAULT_PROFILE.to_string(),
            prompt: String::new(),
            user: Some(user),
            sponsored: false,
        })
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_gate_refuses_only_a_foreign_registered_session() {
        let registry = SessionUserRegistry::new();
        let session = SessionId::new("s1");
        let alice = UserId::new("alice");
        let bob = UserId::new("bob");
        // Unregistered session: passes (about to be spawn-authored).
        assert!(!user_gate_refuses(&registry, &session, &alice));
        registry.register(session.clone(), alice.clone());
        // The owner passes; another tenant is refused.
        assert!(!user_gate_refuses(&registry, &session, &alice));
        assert!(user_gate_refuses(&registry, &session, &bob));
        // Forgetting (session ended) reopens the id.
        registry.forget(&session);
        assert!(!user_gate_refuses(&registry, &session, &bob));
    }
}
