//! `Stop` dispatch (#6): the single entry point every interactive `Stop` site
//! (bare `Esc`, `/stop`, the sessions-modal `s` quick key, the command
//! palette) routes a target through, so there is one place that sends the
//! `InMsg`.
//!
//! Sessions used to offer a cascade-vs-detach confirm here for a
//! `propose_plan` sponsored build child (#626, ADR-0145). ADR-0207 §7
//! retires that handoff entirely — approving a plan switches the session's
//! own mode instead of spawning a child — so the confirm's premise is gone
//! and `Stop` now always sends immediately.

use entanglement_core::{Holly, InMsg, SessionId};

use super::app::App;

/// Send `Stop { session: target }` immediately. `_app` is kept in the
/// signature (unused) so every call site stays untouched if a future
/// interactive confirm needs it again.
pub(super) async fn request_stop(_app: &mut App, holly: &Holly, target: SessionId) {
    let _ = holly.send(InMsg::Stop { session: target }).await;
}
