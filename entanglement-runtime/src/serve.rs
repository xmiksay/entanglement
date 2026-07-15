//! WebSocket `serve` head (#153, [ADR-0048]) — the browser twin of the TUI.
//!
//! A **local, single-user, loopback-bound** axum HTTP server exposing the
//! `InMsg`/`OutEvent` wire protocol over `GET /ws`. Each socket is a thin,
//! equal adapter over the ABI (ADR-0001): one [`Holly::subscribe`] fan-out per
//! connection relayed out as JSON text frames, each inbound frame parsed into an
//! [`InMsg`] and routed through the **untrusted** [`Holly::send_from_wire`] path
//! (#155) so a forged `ToolResult`/`Spawn`/`Resume` is refused, not executed.
//!
//! The WS is a **general protocol interface**, not SPA-coupled: the Vue SPA is
//! the primary but not exclusive client, so a raw local script/CLI/plugin can
//! drive it just as well. Consequences, all per [ADR-0048]:
//! - **Loopback bind is the one required control.** The head is reached via a
//!   `--port` only and always binds `127.0.0.1`, so it cannot be made public by
//!   construction.
//! - **Any `Origin` check is opt-in, never mandatory.** A browser handshake would
//!   break non-browser clients (which send no `Origin`); the browser-page attack
//!   surface is out of scope by decision. When `--allow-origin` is unset, every
//!   origin is accepted ([`origin_allowed`]).
//! - **A `broadcast` lag is a dropped-events gap, not end-of-stream** (#158): the
//!   relay logs and keeps going rather than silently dying mid-conversation.
//!
//! [ADR-0048]: ../../docs/adr/0048-serve-head-local-trust-model.md

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::get,
    Router,
};
use entanglement_core::{Holly, InMsg, SessionId, WireError};
use futures::{SinkExt, StreamExt};
use tokio::sync::broadcast::error::RecvError;

/// Keep an idle socket (and any NAT/proxy in front of a raw client) alive.
const PING_INTERVAL: Duration = Duration::from_secs(30);

/// Shared handler state: the engine handle every socket subscribes to and sends
/// into, plus the opt-in `Origin` allowlist.
struct ServeState {
    holly: Holly,
    /// `Some` → only browsers presenting this exact `Origin` may connect; `None`
    /// → accept every origin (raw clients send none). Opt-in per ADR-0048.
    allowed_origin: Option<String>,
}

/// Bind `127.0.0.1:port` (loopback-only, ADR-0048) and serve the WS head until
/// Ctrl-C. The bind is the required non-public control, so no non-loopback bind
/// is offered.
pub async fn serve(holly: Holly, port: u16, allowed_origin: Option<String>) -> Result<()> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding serve head to {addr}"))?;
    let local = listener.local_addr().unwrap_or(addr);
    tracing::info!(%local, "serve head listening (ws: /ws)");
    eprintln!("skutter serve: http://{local}  (WebSocket: ws://{local}/ws)");

    axum::serve(listener, router(holly, allowed_origin))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("serve head")
}

/// Build the axum router. Split from [`serve`] so tests can drive the handlers
/// over their own ephemeral listener.
pub fn router(holly: Holly, allowed_origin: Option<String>) -> Router {
    let state = Arc::new(ServeState {
        holly,
        allowed_origin,
    });
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/ws", get(ws_upgrade))
        .with_state(state)
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("serve head shutting down");
}

/// The opt-in `Origin` gate (ADR-0048). `None` allowlist → every origin passes,
/// including a raw client that sends no `Origin` header at all.
fn origin_allowed(allowed: Option<&str>, got: Option<&str>) -> bool {
    match allowed {
        None => true,
        Some(expected) => got == Some(expected),
    }
}

async fn ws_upgrade(
    State(state): State<Arc<ServeState>>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    let got = headers.get("origin").and_then(|v| v.to_str().ok());
    if !origin_allowed(state.allowed_origin.as_deref(), got) {
        tracing::warn!(
            origin = got,
            "serve: refused WS connect (origin not allowed)"
        );
        return (StatusCode::FORBIDDEN, "origin not allowed").into_response();
    }
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

/// One connection: relay every `OutEvent` out, route every inbound frame in.
async fn handle_socket(socket: WebSocket, state: Arc<ServeState>) {
    let (mut sink, mut stream) = socket.split();
    let mut sub = state.holly.subscribe();
    // A per-connection default session lets a bare-text frame become a `Prompt`,
    // matching the stdio `pipe` head's scripting affordance.
    let default_session = SessionId::new_uuid();

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
                    Ok(m) => match state.holly.send_from_wire(m).await {
                        Ok(()) => {}
                        Err(WireError::Closed) => break, // engine gone
                        Err(e @ WireError::Privileged(_)) => {
                            tracing::warn!("serve: {e}");
                        }
                    },
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
    out.abort();
}

#[cfg(test)]
mod tests {
    use super::origin_allowed;

    #[test]
    fn no_allowlist_accepts_any_origin_including_none() {
        // Opt-in per ADR-0048: an unset allowlist must not break raw clients
        // (which send no `Origin`) nor browsers.
        assert!(origin_allowed(None, None));
        assert!(origin_allowed(None, Some("http://localhost:5173")));
        assert!(origin_allowed(None, Some("http://evil.example")));
    }

    #[test]
    fn allowlist_requires_exact_origin() {
        let allowed = Some("http://localhost:5173");
        assert!(origin_allowed(allowed, Some("http://localhost:5173")));
        assert!(!origin_allowed(allowed, Some("http://localhost:5174")));
        // A configured allowlist rejects a client that presents no origin.
        assert!(!origin_allowed(allowed, None));
    }
}
