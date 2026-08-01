//! `App` surface for the `/enable`/`/disable` commands (#539, ADR-0149): folds
//! the `OutEvent::ToolOverlayChanged` reply to the `InMsg::SetToolOverlay` wire
//! op `enable_command` sends, and tracks the per-session overlay list the
//! commands read to compute their full-replacement update. Confirmations and
//! parse errors render as a transcript status line, mirroring
//! `App::record_bash_error`/`handle_bash_changed`.

use entanglement_core::{SessionId, ToolOverlayEntry};

use super::App;

impl App {
    /// The active session's current live tool overlay — the base
    /// `/enable`/`/disable` mutate before sending the full replacement.
    pub fn overlay_entries(&self, session: &SessionId) -> Vec<ToolOverlayEntry> {
        self.tool_overlays.get(session).cloned().unwrap_or_default()
    }

    /// Records a `/enable`/`/disable` parse error as a transcript status line —
    /// no engine traffic, so nothing else to fold.
    pub fn record_enable_error(&mut self, message: String) {
        self.sessions
            .active_view_mut()
            .record_status("enable", format!("error: {message}"));
        self.mark_dirty();
    }

    /// Renders a bare `/enable` status: the session's current overlay plus the
    /// available tool roster (the `/mcp` panel lists servers + their tools).
    pub fn render_overlay_status(&mut self) {
        let session = self.active_session_id().clone();
        let overlay = render_entries(&self.overlay_entries(&session));
        let roster = self.tool_roster.join(", ");
        self.sessions.active_view_mut().record_status(
            "enable",
            format!(
                "session tools: {overlay} — available: {roster} \
                 (see /mcp for servers; /enable mcp <server> | /enable tool <name>)"
            ),
        );
        self.mark_dirty();
    }

    /// Folds an `OutEvent::ToolOverlayChanged` (#539): updates the head-side
    /// mirror and renders a confirmation status line on the session's view.
    pub(super) fn handle_tool_overlay_changed(
        &mut self,
        session: &SessionId,
        entries: Vec<ToolOverlayEntry>,
    ) {
        let message = format!("session tools: {}", render_entries(&entries));
        if entries.is_empty() {
            self.tool_overlays.remove(session);
        } else {
            self.tool_overlays.insert(session.clone(), entries);
        }
        if let Some(view) = self.sessions.view_for_mut(session) {
            view.record_status("enable", message);
        }
        self.mark_dirty();
    }
}

fn render_entries(entries: &[ToolOverlayEntry]) -> String {
    if entries.is_empty() {
        return "(none)".to_string();
    }
    entries
        .iter()
        .map(|e| {
            if e.allow {
                format!("{} (allow)", e.pattern)
            } else {
                e.pattern.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlay_changed_tracks_and_renders() {
        let mut app = App::new_for_test(SessionId::new("s1"));
        let session = app.active_session_id().clone();
        app.handle_tool_overlay_changed(&session, vec![ToolOverlayEntry::ask("mcp__docs__*")]);
        assert_eq!(app.overlay_entries(&session).len(), 1);
        let rendered = app
            .transcript()
            .iter()
            .any(|e| format!("{e:?}").contains("mcp__docs__*"));
        assert!(rendered, "expected a transcript entry with the overlay");
        // An empty replacement clears the tracked entry.
        app.handle_tool_overlay_changed(&session, Vec::new());
        assert!(app.overlay_entries(&session).is_empty());
    }
}
