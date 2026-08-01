//! `App` surface for the `/enable`/`/disable` commands (#539, ADR-0149): folds
//! the `OutEvent::ToolOverlayChanged` reply to the `InMsg::SetToolOverlay` wire
//! op `enable_command` sends, tracks the per-session overlay list the commands
//! read to compute their full-replacement update, and owns the bare-`/enable`
//! session-tools checklist dialog. Confirmations and parse errors render as a
//! transcript status line, mirroring `App::record_bash_error`/`handle_bash_changed`.

use entanglement_core::{AgentProfile, SessionId, ToolOverlayEntry};
use ratatui::widgets::ListState;

use crate::tui::session_tools_dialog::{SessionToolRow, SessionToolsDialog};

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

    pub fn showing_session_tools_dialog(&self) -> bool {
        self.session_tools_dialog.visible()
    }

    pub fn session_tools_dialog(&self) -> &SessionToolsDialog {
        &self.session_tools_dialog
    }

    pub fn session_tools_dialog_mut(&mut self) -> &mut SessionToolsDialog {
        self.mark_dirty();
        &mut self.session_tools_dialog
    }

    pub fn session_tools_dialog_state(&mut self) -> &mut ListState {
        self.session_tools_dialog.state()
    }

    /// Open the bare-`/enable` checklist over the full advertised roster,
    /// seeding each row from the active session's *effective* availability:
    /// the profile's mask default overridden by the live overlay's disposition
    /// (#539). The profile is resolved from the active view's agent name; an
    /// unknown name (e.g. before the first `AgentChanged` lands) falls back to
    /// inherit-all, matching a `tools: None` profile.
    pub fn open_session_tools_dialog(&mut self) {
        let session = self.active_session_id().clone();
        let agent = self.sessions.active_view().agent().to_string();
        let profile = self.available_profiles.iter().find(|p| p.name == agent);
        let overlay = self.overlay_entries(&session);
        let rows: Vec<SessionToolRow> = self
            .tool_roster
            .iter()
            .map(|name| {
                let profile_default = profile
                    .map(|p| {
                        AgentProfile::mask_allows(p.tools.as_deref(), &p.disallowed_tools, name)
                    })
                    .unwrap_or(true);
                let checked =
                    ToolOverlayEntry::disposition(&overlay, name).unwrap_or(profile_default);
                let allow = ToolOverlayEntry::find(&overlay, name)
                    .map(|e| e.allow)
                    .unwrap_or(false);
                SessionToolRow {
                    name: name.clone(),
                    profile_default,
                    checked,
                    allow,
                }
            })
            .collect();
        self.session_tools_dialog.show(rows);
        self.mark_dirty();
    }

    pub fn close_session_tools_dialog(&mut self) {
        self.session_tools_dialog.hide();
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
        return "(none — profile defaults)".to_string();
    }
    entries
        .iter()
        .map(|e| {
            if e.deny {
                format!("{} (off)", e.pattern)
            } else if e.allow {
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

    #[test]
    fn deny_entries_render_as_off() {
        let mut app = App::new_for_test(SessionId::new("s1"));
        let session = app.active_session_id().clone();
        app.handle_tool_overlay_changed(&session, vec![ToolOverlayEntry::deny("bash")]);
        let rendered = app
            .transcript()
            .iter()
            .any(|e| format!("{e:?}").contains("bash (off)"));
        assert!(rendered, "expected the deny entry rendered as off");
    }

    #[test]
    fn session_tools_dialog_seeds_effective_availability() {
        let mut app = App::new_for_test(SessionId::new("s1"));
        let session = app.active_session_id().clone();
        // Overlay: enable one MCP tool, deny read.
        app.handle_tool_overlay_changed(
            &session,
            vec![
                ToolOverlayEntry::ask("mcp__docs__*"),
                ToolOverlayEntry::deny("read"),
            ],
        );
        app.open_session_tools_dialog();
        assert!(app.showing_session_tools_dialog());
        let rows = app.session_tools_dialog().rows().to_vec();
        // The test app has no profile registered for the view's agent, so
        // profile defaults are inherit-all; the overlay overrides per row.
        let get = |n: &str| rows.iter().find(|r| r.name == n).cloned();
        if let Some(r) = get("read") {
            assert!(!r.checked, "deny entry unchecks a default-on tool");
        }
        for r in &rows {
            if r.name.starts_with("mcp__docs__") {
                assert!(r.checked, "enable entry checks its matches");
            }
        }
    }
}
