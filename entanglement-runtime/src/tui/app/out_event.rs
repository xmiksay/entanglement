//! Folding engine [`OutEvent`]s into the head: the app-global side effects
//! (model/generation persistence, MCP state, compaction lineage) before the
//! per-session reducer sees the event. Split out of `app.rs` (#745).

use entanglement_core::OutEvent;
use entanglement_provider::ModelInfo;

use super::App;
use crate::tui::session_view::ApprovalMode;

impl App {
    pub fn handle_out_event(&mut self, event: OutEvent) {
        tracing::debug!("App handling OutEvent: {:?}", event);
        // Token totals + cost are tracked per-session view (#192), so a resumed
        // session restores its accumulated counts — the `Usage` event is folded
        // into the active view by `sessions.handle_out_event` below.
        // A live model switch (#218) updates the head's model display (context
        // bar) directly — model_info is app-global, not per-session-view state.
        if let OutEvent::ModelChanged {
            session,
            provider,
            model,
            context_window,
        } = &event
        {
            self.active_provider = provider.clone();
            self.set_model_info(ModelInfo {
                id: model.clone(),
                display_name: model.clone(),
                context_window: context_window.map(|w| w as u32),
            });
            // Persist-on-confirmation (#323): a `/model` pick recorded a pending
            // persist; its matching `ModelChanged` commits the write. An
            // unrelated `ModelChanged` (e.g. the session-start pin re-announce)
            // has no pending, so it never writes.
            self.persist_model_if_pending(session, provider, model);
        }
        // A generation-knob change (#374/#376): always render a status line with
        // the current effective params, and — if this is the confirmation of a
        // pending `/set` — persist it to `agent-generation.yml` too.
        if let OutEvent::GenerationChanged {
            session,
            generation,
        } = &event
        {
            self.handle_generation_changed(session, *generation);
        }
        // A failed switch clears any pending persist without writing (#323/#376).
        if let OutEvent::Error { session, .. } = &event {
            self.clear_pending_model_persist_on_error(session);
            self.clear_pending_generation_persist_on_error(session);
        }
        // MCP ops (#375) are engine-global, not per-session — folded here
        // rather than routed through `sessions.handle_out_event` below (which
        // never sees them, since `event.session()` is `None` for both).
        if let OutEvent::McpList {
            correlation_id,
            servers,
        } = &event
        {
            self.handle_mcp_list(correlation_id, servers.clone());
        }
        if let OutEvent::McpChanged {
            name,
            action,
            error,
        } = &event
        {
            self.handle_mcp_changed(name, *action, error.as_deref());
        }
        // MCP OAuth progress (ADR-0153) — engine-global like `McpChanged`.
        if let OutEvent::McpAuthChanged { status } = &event {
            self.handle_mcp_auth_changed(status);
        }
        // Session-log outage transitions (#767) — engine-global, folded into
        // the status-line badge.
        self.handle_persistence(&event);
        // A session's live tool overlay changed (#539): track the head-side
        // mirror + render the confirmation (the per-session reducer folds no
        // view state for it).
        if let OutEvent::ToolOverlayChanged { session, entries } = &event {
            self.handle_tool_overlay_changed(session, entries.clone());
        }
        // Compaction (ADR-0205): every path forks a successor, and the engine
        // mints it — the head no longer sends the `Spawn` itself. All that is
        // left here is carrying the summary across to the successor, which
        // announces itself separately (below). Deduped by seq against the
        // source view's watermark so a replayed/lagged duplicate doesn't
        // overwrite a newer seed (mirrors the reducer's own dedupe guard).
        if let OutEvent::Compacted {
            session: source,
            seq,
            summary,
            ..
        } = &event
        {
            let is_new = self
                .sessions
                .view_for(source)
                .map(|v| *seq > v.last_seen_seq())
                .unwrap_or(true);
            if is_new {
                self.note_compaction_seed(source.clone(), summary.clone());
            }
        }
        // The successor of a compaction announces itself with `predecessor`
        // set (ADR-0205) — the head follows that lineage rather than tracking
        // a fork it issued, so this works for a session that compacted while
        // the user was looking somewhere else, and on a reconnect/replay.
        if let OutEvent::SessionStarted {
            session: successor,
            predecessor: Some(source),
            ..
        } = &event
        {
            self.follow_successor(source.clone(), successor.clone());
        }
        // An exec call may have switched branches or created a repo (#762).
        if let OutEvent::ToolOutput { tool, .. } = &event {
            if crate::tui::statusline::refreshes_git(tool) {
                self.refresh_status_line();
            }
        }
        // A `/fork` of the session being watched moves the user into the
        // fork (#786) — keyed on lineage, like a compaction successor above,
        // so a fork deferred behind a turn still lands where it was asked.
        let follow = match &event {
            OutEvent::SessionForked {
                session,
                forked_from,
                ..
            } if forked_from == self.sessions.active_id() => Some(session.clone()),
            _ => None,
        };
        let active = self.sessions.active_id().clone();
        let before = self.approval_mode().clone();
        if self.sessions.handle_out_event(event) {
            self.mark_dirty();
        }
        // The reducer moved the prompt off the card the user was on — a
        // terminal `Status` dropped it and promoted a kept background card
        // (ADR-0237). The pager showed the old card, so its decision keys
        // would answer the new one: close it. A half-typed reject reason
        // (in the shared input) must not pre-fill the new card's reject;
        // it stays in input history. Back at no card it is an ordinary
        // draft, as before.
        if self.sessions.active_id() == &active
            && prompted(&before) != prompted(self.approval_mode())
        {
            self.close_approval_pager();
            let rejecting = matches!(before, ApprovalMode::EnteringRejectReason { .. });
            if rejecting && prompted(self.approval_mode()).is_some() {
                self.take_input_text();
            }
        }
        if let Some(fork) = follow {
            self.sessions.switch_to(fork);
            self.mark_dirty();
        }
    }
}

/// The request the prompt (or its reject-reason box) is on, if any.
fn prompted(mode: &ApprovalMode) -> Option<&str> {
    match mode {
        ApprovalMode::Normal => None,
        ApprovalMode::WaitingForApproval { request_id }
        | ApprovalMode::EnteringRejectReason { request_id } => Some(request_id),
    }
}

#[cfg(test)]
mod tests {
    use entanglement_core::{AgentState, SessionId};

    use super::*;

    fn card(seq: u64, request_id: &str, detached: bool) -> OutEvent {
        OutEvent::ToolRequest {
            session: SessionId::new("s"),
            seq,
            request_id: request_id.into(),
            tool: "edit".into(),
            input: "{}".into(),
            reason: None,
            scopes: Vec::new(),
            detached,
        }
    }

    fn idle() -> OutEvent {
        OutEvent::Status {
            session: SessionId::new("s"),
            state: AgentState::Idle,
        }
    }

    fn rejecting(app: &mut App, request_id: &str) {
        app.set_approval_mode(ApprovalMode::EnteringRejectReason {
            request_id: request_id.into(),
        });
        app.set_input_text("half typed".into());
    }

    /// The dropped turn card's half-typed reason must not pre-fill the
    /// promoted background card's reject (ADR-0237).
    #[test]
    fn a_promoted_card_starts_with_an_empty_reason() {
        let mut app = App::new_for_test(SessionId::new("s"));
        app.handle_out_event(card(1, "turn", false));
        app.handle_out_event(card(2, "bg", true));
        rejecting(&mut app, "turn");
        app.handle_out_event(idle());
        assert!(matches!(
            app.approval_mode(),
            ApprovalMode::WaitingForApproval { request_id } if request_id == "bg"
        ));
        assert_eq!(app.input_text(), "");
    }

    /// The pager showed the dropped card: left open, its decision keys
    /// would answer the promoted one the user never read.
    #[test]
    fn a_promoted_card_closes_the_pager() {
        let mut app = App::new_for_test(SessionId::new("s"));
        app.handle_out_event(card(1, "turn", false));
        app.handle_out_event(card(2, "bg", true));
        app.open_approval_pager();
        app.handle_out_event(idle());
        assert!(!app.showing_approval_pager());
        assert!(matches!(
            app.approval_mode(),
            ApprovalMode::WaitingForApproval { request_id } if request_id == "bg"
        ));
    }

    /// A card queued behind the prompted one moves nothing: the pager stays.
    #[test]
    fn a_queued_card_leaves_the_pager_open() {
        let mut app = App::new_for_test(SessionId::new("s"));
        app.handle_out_event(card(1, "turn", false));
        app.open_approval_pager();
        app.handle_out_event(card(2, "bg", true));
        assert!(app.showing_approval_pager());
    }

    /// With nothing kept the reason stays as an ordinary draft, and a
    /// reason typed for the kept card itself is left alone.
    #[test]
    fn a_reason_survives_when_no_other_card_is_promoted() {
        let mut app = App::new_for_test(SessionId::new("s"));
        app.handle_out_event(card(1, "turn", false));
        rejecting(&mut app, "turn");
        app.handle_out_event(idle());
        assert!(matches!(app.approval_mode(), ApprovalMode::Normal));
        assert_eq!(app.input_text(), "half typed");

        app.handle_out_event(card(3, "bg", true));
        rejecting(&mut app, "bg");
        app.handle_out_event(idle());
        assert_eq!(app.input_text(), "half typed");
    }
}
