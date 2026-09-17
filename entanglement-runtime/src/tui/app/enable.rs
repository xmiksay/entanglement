//! `App` surface for the `/enable`/`/disable` commands (#539, ADR-0149): folds
//! the `OutEvent::ToolOverlayChanged` reply to the `InMsg::SetToolOverlay` wire
//! op `enable_command` sends, tracks the per-session overlay list the commands
//! read to compute their full-replacement update, and owns the bare-`/enable`
//! session-tools checklist dialog. Confirmations (including a live bash
//! enablement, ADR-0163 #611) and parse errors render as a transcript status
//! line.

use entanglement_core::{SessionId, ToolOverlayEntry};
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

    /// Records how many currently-registered tools `pattern` matches
    /// (ADR-0199 part 4) — immediate feedback for a hand-typed glob, ahead
    /// of the `ToolOverlayChanged` confirmation toast. Counted against
    /// `tool_roster`, the same startup-built roster the `/agent` tools
    /// checklist and bare `/enable`'s session-tools dialog already read —
    /// a live-connected MCP server's tools discovered *after* startup won't
    /// show here (the same caveat those dialogs carry), never a panic or a
    /// hard error either way.
    pub fn record_enable_match_count(&mut self, pattern: &str) {
        let entry = entanglement_core::ToolOverlayEntry::ask(pattern.to_string());
        let count = self
            .tool_roster
            .iter()
            .filter(|name| entry.matches(name))
            .count();
        self.sessions.active_view_mut().record_status(
            "enable",
            format!("`{pattern}` matches {count} currently-registered tool(s)"),
        );
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
    /// every profile inherits every tool now (ADR-0207 moved authority off
    /// the profile and onto the session's independent permission mode, so
    /// there is no more per-profile mask to seed a default from), overridden
    /// by the live overlay's disposition (#539).
    ///
    /// A provider-bundled/`allowed` MCP server (#542) isn't in `tool_roster` —
    /// its tools don't exist until enabled — so it would otherwise be invisible
    /// in the very UI meant to discover it (#555). Each available server gets
    /// one extra row named `mcp__<server>__*`, the same whole-server pattern
    /// `/enable mcp <name>` and the `/mcp` panel's `e` key write; checking it
    /// lazily connects the server on submit (`enable_command::lazy_enable_entries`).
    pub fn open_session_tools_dialog(&mut self) {
        let rows = self.session_tool_rows(&[]);
        self.session_tools_dialog.show(rows);
        self.mark_dirty();
    }

    /// The checklist rows over the roster, every available server's
    /// whole-server pattern, and `extra` patterns (deduped) — shared with the
    /// `/set` dialog's Tools tab.
    pub(super) fn session_tool_rows(&self, extra: &[String]) -> Vec<SessionToolRow> {
        let session = self.active_session_id().clone();
        let overlay = self.overlay_entries(&session);
        let row = |name: &str| {
            // No profile carries a mask any more (ADR-0207) — every tool is a
            // session's default until its own overlay says otherwise.
            let profile_default = true;
            SessionToolRow {
                name: name.to_string(),
                profile_default,
                checked: ToolOverlayEntry::disposition(&overlay, name).unwrap_or(profile_default),
                allow: ToolOverlayEntry::find(&overlay, name)
                    .map(|e| e.allow)
                    .unwrap_or(false),
            }
        };
        let mut names: Vec<String> = self.tool_roster.clone();
        if let Some(handles) = self.mcp_handles() {
            names.extend(
                handles
                    .avail
                    .available_names()
                    .into_iter()
                    .map(|server| format!("mcp__{server}__*")),
            );
        }
        for pattern in extra {
            if !names.contains(pattern) {
                names.push(pattern.clone());
            }
        }
        names.iter().map(|n| row(n)).collect()
    }

    pub fn close_session_tools_dialog(&mut self) {
        self.session_tools_dialog.hide();
        self.mark_dirty();
    }

    /// Folds an `OutEvent::ToolOverlayChanged` (#539): updates the head-side
    /// mirror and surfaces a confirmation as a transient info-line toast.
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
        self.set_toast(message);
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
        assert!(
            app.toast().is_some_and(|t| t.contains("mcp__docs__*")),
            "expected a toast with the overlay"
        );
        // An empty replacement clears the tracked entry.
        app.handle_tool_overlay_changed(&session, Vec::new());
        assert!(app.overlay_entries(&session).is_empty());
    }

    #[test]
    fn record_enable_match_count_reports_the_pattern_hit_count() {
        // `App::new_for_test`'s roster: read, grep, glob, edit, write, bash.
        let mut app = App::new_for_test(SessionId::new("s1"));
        app.record_enable_match_count("bash");
        let text: String = app.transcript().iter().map(|e| format!("{e:?}")).collect();
        assert!(
            text.contains("`bash` matches 1 currently-registered tool"),
            "{text}"
        );

        app.record_enable_match_count("mcp__docs__*");
        let text: String = app.transcript().iter().map(|e| format!("{e:?}")).collect();
        assert!(
            text.contains("`mcp__docs__*` matches 0 currently-registered tool"),
            "{text}"
        );
    }

    #[test]
    fn deny_entries_render_as_off() {
        let mut app = App::new_for_test(SessionId::new("s1"));
        let session = app.active_session_id().clone();
        app.handle_tool_overlay_changed(&session, vec![ToolOverlayEntry::deny("bash")]);
        assert!(
            app.toast().is_some_and(|t| t.contains("bash (off)")),
            "expected the deny entry toasted as off"
        );
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

    /// #555: an `allowed` bundled/available MCP server has no tools yet, so it
    /// can never ride `tool_roster` — without a dedicated row it would be
    /// invisible in the very checklist meant to discover it (#542).
    #[test]
    fn session_tools_dialog_lists_an_available_server() {
        let mut app = App::new_for_test(SessionId::new("s1"));
        app.set_mcp_handles(test_handles_with_available_server("zread"));
        app.open_session_tools_dialog();
        let rows = app.session_tools_dialog().rows().to_vec();
        assert!(
            rows.iter().any(|r| r.name == "mcp__zread__*"),
            "expected an `mcp__zread__*` row for the available server, got {rows:?}"
        );
    }

    /// Builds `McpHandles` around a single ungated `allowed` server named
    /// `name`, for the row-listing test above (#555). Points at a URL, never
    /// actually dialed.
    fn test_handles_with_available_server(name: &str) -> crate::mcp::McpHandles {
        let mut user_mcp = std::collections::HashMap::new();
        user_mcp.insert(
            name.to_string(),
            crate::mcp::McpServerConfig {
                command: None,
                args: vec![],
                env: std::collections::HashMap::new(),
                url: Some("https://example.invalid/mcp".to_string()),
                headers: std::collections::HashMap::new(),
                disabled: false,
                capabilities: std::collections::HashMap::new(),
                oauth: None,
                state: Some(crate::mcp::McpServerState::Allowed),
            },
        );
        let catalog = entanglement_core::Catalog { providers: vec![] };
        let (_startup, avail) = crate::mcp::AvailableMcp::partition(&catalog, &user_mcp, vec![]);
        crate::mcp::McpHandles {
            avail: std::sync::Arc::new(avail),
            registry: std::sync::Arc::new(std::sync::RwLock::new(crate::ToolRegistry::new())),
            active: std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        }
    }
}
