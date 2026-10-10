//! Key handling while a tool approval is parked: the decision keys, the
//! approval pager, the reject-reason field, and the transcript traces each
//! decision leaves.

use entanglement_core::{ApprovalScope, Holly, InMsg};
use ratatui::crossterm::event::{KeyCode, KeyEvent};

use super::apply_input_edit_key;
use crate::tui::app::App;
use crate::tui::session_view::ApprovalMode;

/// [`ApprovalMode::WaitingForApproval`]: a scope letter (or, for
/// `propose_plan`, a mode choice) answers `request_id`; `n`/`e` move to the
/// reject-reason field; `v` opens the full-body pager.
pub(super) async fn handle_waiting_for_approval(
    app: &mut App,
    holly: &Holly,
    key: KeyEvent,
    request_id: String,
) {
    match key.code {
        // `propose_plan` (#560, ADR-0207 §7 extension): acceptance
        // itself chooses the mode the session switches to, so this
        // tool gets its own two accept keys instead of the generic
        // scope letters below (its scope was already inert — the
        // runtime never records a grant for a plan approval). `u`
        // is the DEFAULT bare accept -> `auto` (bounded, so it's
        // the safe "go implement this" choice); `b` is the
        // explicit opt-in to `build`. A model's own `mode`
        // suggestion only pre-selects which one the footer marks
        // "(suggested)" (`transcript.rs`) — it never decides;
        // only this keystroke does.
        KeyCode::Char('u') if is_plan_request(app) => {
            send_plan_approval(app, holly, request_id.clone(), "auto").await;
        }
        KeyCode::Char('b') if is_plan_request(app) => {
            send_plan_approval(app, holly, request_id.clone(), "build").await;
        }
        // The generic scope letters mean nothing for `propose_plan`
        // (no grant is ever recorded for it) — swallow them here
        // rather than let `y` silently fire the old default.
        KeyCode::Char('y' | 's' | 'a' | 'd') if is_plan_request(app) => {}
        // Approve scopes (#174): `y` this once, `s` for the rest of
        // the session, `a` always (persisted).
        KeyCode::Char('y') => {
            send_approval(app, holly, request_id.clone(), ApprovalScope::Once).await;
        }
        KeyCode::Char('s') => {
            send_approval(app, holly, request_id.clone(), ApprovalScope::Session).await;
        }
        KeyCode::Char('a') => {
            send_approval(app, holly, request_id.clone(), ApprovalScope::Always).await;
        }
        // Allow the call's directory for the rest of the session
        // (#486, ADR-0126) — only offered while the pending call
        // is one of the read-only triad (`read`/`grep`/`glob`);
        // any other tool has no `[d]` to press.
        KeyCode::Char('d')
            if app
                .pending_tool_request()
                .is_some_and(|(_, tool, _)| crate::tool_names::is_read_capability_member(tool)) =>
        {
            send_approval(app, holly, request_id.clone(), ApprovalScope::SessionDir).await;
        }
        KeyCode::Char('n') => {
            app.set_approval_mode(ApprovalMode::EnteringRejectReason {
                request_id: request_id.clone(),
            });
        }
        KeyCode::Char('e') => {
            app.set_approval_mode(ApprovalMode::EnteringRejectReason {
                request_id: request_id.clone(),
            });
        }
        // Full-body pager (#B2): a long diff/plan body can
        // scroll off the transcript viewport, so `v` opens a
        // scrollable modal over the same rendering
        // (`transcript::render_approval_tool_body`). The
        // decision keys above are unchanged and still resolve
        // the approval while the pager is open — only
        // navigation below is pager-aware.
        KeyCode::Char('v') if !app.showing_approval_pager() => {
            app.open_approval_pager();
        }
        KeyCode::Char('j') | KeyCode::Down if app.showing_approval_pager() => {
            app.approval_pager_scroll_down(1);
        }
        KeyCode::Char('k') | KeyCode::Up if app.showing_approval_pager() => {
            app.approval_pager_scroll_up(1);
        }
        // PageUp/PageDown scroll the pager while it's open, or
        // the transcript (same ±5 as the Normal arm) otherwise
        // — both were previously unbound in this arm.
        KeyCode::PageUp if app.showing_approval_pager() => {
            app.approval_pager_scroll_up(10);
        }
        KeyCode::PageDown if app.showing_approval_pager() => {
            app.approval_pager_scroll_down(10);
        }
        KeyCode::PageUp => {
            app.scroll_up(5);
        }
        KeyCode::PageDown => {
            app.scroll_down(5);
        }
        KeyCode::Esc if app.showing_approval_pager() => {
            app.close_approval_pager();
        }
        KeyCode::Esc => {
            let _ = holly
                .send(InMsg::Stop {
                    session: app.active_session_id().clone(),
                })
                .await;
            app.clear_approval();
        }
        _ => {}
    }
}

/// [`ApprovalMode::EnteringRejectReason`]: typing the optional reason for
/// rejecting `request_id`; `Enter` sends the `Reject`, `Esc` goes back.
pub(super) async fn handle_reject_reason(
    app: &mut App,
    holly: &Holly,
    key: KeyEvent,
    request_id: String,
) {
    // Shared input-edit keys (Ctrl+arrows, Home/End, doc
    // jumps, Alt+Enter newline) — Enter stays = send.
    if apply_input_edit_key(app, &key) {
        return;
    }
    match key.code {
        KeyCode::Esc => {
            app.set_approval_mode(ApprovalMode::WaitingForApproval {
                request_id: request_id.clone(),
            });
            let text = app.take_input_text();
            if !text.is_empty() {
                app.input().insert_str(&text);
            }
        }
        KeyCode::Enter => {
            let text = app.take_input_text();
            let tool = app.pending_tool_request().map(|(_, tool, _)| tool.clone());
            let reason = if text.is_empty() { None } else { Some(text) };
            let _ = holly
                .send(InMsg::Reject {
                    session: app.active_session_id().clone(),
                    request_id: request_id.clone(),
                    reason: reason.clone(),
                })
                .await;
            // Rejecting answers only this request — parked ones
            // still need their own decision (#273).
            app.advance_approval();
            if let Some(tool) = tool {
                record_rejected(app, &tool, &reason);
            }
        }
        KeyCode::Char(c) => {
            app.input().insert_char(c);
        }
        KeyCode::Backspace => {
            app.input().delete_char();
        }
        KeyCode::Left => {
            app.input().move_cursor_left();
        }
        KeyCode::Right => {
            app.input().move_cursor_right();
        }
        // Same ±5 as the Normal arm — free while typing a
        // reason since Up/Down/PageUp/PageDown aren't
        // otherwise bound in this single-line field (#B1).
        KeyCode::PageUp => {
            app.scroll_up(5);
        }
        KeyCode::PageDown => {
            app.scroll_down(5);
        }
        _ => {}
    }
}

/// Send an [`InMsg::Approve`] with the chosen [`ApprovalScope`] (#174) and clear
/// the prompt. `mode` is always `None` here — a `propose_plan` request never
/// reaches this path any more (it has its own [`send_plan_approval`], which is
/// the only caller that sets `mode`); every other tool's scope is graded by
/// the generic permission path, which this just forwards.
pub(super) async fn send_approval(
    app: &mut App,
    holly: &Holly,
    request_id: String,
    scope: ApprovalScope,
) {
    let pending = app.pending_tool_request().cloned();
    let _ = holly
        .send(InMsg::Approve {
            session: app.active_session_id().clone(),
            request_id,
            scope,
            mode: None,
        })
        .await;
    // Pop the answered request and surface the next parked one, if any (#273).
    app.advance_approval();
    // Leave a one-line trace of the decision (#487) — the approval tail itself
    // clears once answered, so without this the scrollback shows no evidence a
    // call was ever approved.
    if let Some((_, tool, _)) = &pending {
        record_approved(app, tool, scope);
    }
}

/// Records an approval as a one-line transcript entry (#487) — the same
/// out-of-band-notice idiom `App::record_status` uses elsewhere (reducer.rs) —
/// so a scrollback through the transcript shows what was decided, not just a
/// tail that silently vanished. Mirrors [`record_rejected`].
pub(super) fn record_approved(app: &mut App, tool: &str, scope: ApprovalScope) {
    let scope_label = match scope {
        ApprovalScope::Once => "once",
        ApprovalScope::Session => "session",
        ApprovalScope::Always => "always",
        ApprovalScope::SessionDir => "session, dir",
    };
    app.record_status("approval", format!("✓ approved {tool} ({scope_label})"));
}

/// Whether the currently parked approval is a `propose_plan` request (#560,
/// ADR-0207 §7 extension) — the one tool whose accept keys diverge from the
/// generic `y`/`s`/`a`/`d` scope letters (see the match arms above).
pub(super) fn is_plan_request(app: &App) -> bool {
    app.pending_tool_request()
        .is_some_and(|(_, tool, _)| tool == crate::tool_names::PROPOSE_PLAN_TOOL)
}

/// Send the `propose_plan`-specific [`InMsg::Approve`] (#560, ADR-0207 §7
/// extension): here the *approver* chooses the mode the session switches to —
/// `mode` is `Some("auto")` for the `[u]` bare-accept default or
/// `Some("build")` for the explicit `[b]` choice, read by
/// `propose_plan::run_propose_plan` in place of the old hardcoded `build`
/// constant. Scope stays `Once` — it was already inert for this tool (the
/// runtime never records a grant for a plan approval), so there is nothing
/// for `[s]`/`[a]`/`[d]` to mean here; this function is the only accept path
/// `propose_plan` offers now.
pub(super) async fn send_plan_approval(
    app: &mut App,
    holly: &Holly,
    request_id: String,
    mode: &'static str,
) {
    let pending = app.pending_tool_request().cloned();
    let _ = holly
        .send(InMsg::Approve {
            session: app.active_session_id().clone(),
            request_id,
            scope: ApprovalScope::Once,
            mode: Some(mode.to_string()),
        })
        .await;
    app.advance_approval();
    if let Some((_, tool, _)) = &pending {
        app.record_status("approval", format!("✓ approved {tool} → mode `{mode}`"));
    }
}

/// Records a rejection (and its optional reason) as a one-line transcript
/// entry (#487). Mirrors [`record_approved`].
pub(super) fn record_rejected(app: &mut App, tool: &str, reason: &Option<String>) {
    let message = match reason {
        Some(r) => format!("✗ rejected {tool} — {r}"),
        None => format!("✗ rejected {tool}"),
    };
    app.record_status("approval", message);
}
