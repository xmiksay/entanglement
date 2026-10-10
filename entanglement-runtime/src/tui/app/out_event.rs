//! Folding engine [`OutEvent`]s into the head: the app-global side effects
//! (model/generation persistence, MCP state, compaction lineage) before the
//! per-session reducer sees the event. Split out of `app.rs` (#745).

use entanglement_core::OutEvent;
use entanglement_provider::ModelInfo;

use super::App;

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
        if let OutEvent::McpChanged { name, action } = &event {
            self.handle_mcp_changed(name, *action);
        }
        // MCP OAuth progress (ADR-0153) — engine-global like `McpChanged`.
        if let OutEvent::McpAuthChanged { status } = &event {
            self.handle_mcp_auth_changed(status);
        }
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
        if self.sessions.handle_out_event(event) {
            self.mark_dirty();
        }
    }
}
