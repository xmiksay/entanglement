//! `App` surface for the `/mcp` command (#373): folds the
//! `OutEvent::McpList`/`McpChanged` replies to the wire ops `event_loop`/
//! `modal_events` send. `list` opens the read-only panel
//! ([`crate::tui::mcp_panel::McpPanel`]); `add`/`remove` confirmations render as
//! a transcript status line, mirroring `/key`'s save notice.

use entanglement_core::{McpAction, McpServerStatus};

use super::App;

impl App {
    /// Wire the per-session enablement handles for `allowed` MCP servers
    /// (#542) — set once at TUI startup, `None` in tests.
    pub fn set_mcp_handles(&mut self, handles: crate::mcp::McpHandles) {
        self.mcp_handles = Some(handles);
    }

    /// The enablement handles, for `/enable`/`/disable`'s lazy-connect path.
    pub(in crate::tui) fn mcp_handles(&self) -> Option<&crate::mcp::McpHandles> {
        self.mcp_handles.as_ref()
    }

    pub fn showing_mcp_panel(&self) -> bool {
        self.mcp_panel.visible()
    }

    pub fn mcp_servers(&self) -> &[McpServerStatus] {
        self.mcp_panel.servers()
    }

    pub fn mcp_scroll(&self) -> u16 {
        self.mcp_scroll
    }

    /// Set the MCP panel scroll offset (clamped at 0; the event loop clamps
    /// the upper bound against the rendered line count).
    pub fn set_mcp_scroll(&mut self, scroll: u16) {
        self.mcp_scroll = scroll;
        self.mark_dirty();
    }

    pub fn close_mcp_panel(&mut self) {
        self.mcp_panel.hide();
        self.mark_dirty();
    }

    /// The highlighted server row index (#539) the panel's `e`/`d` keys act on.
    pub fn mcp_selected(&self) -> usize {
        self.mcp_panel.selected()
    }

    /// The highlighted server's name, if any.
    pub fn mcp_selected_server(&self) -> Option<String> {
        self.mcp_panel.selected_server().map(str::to_string)
    }

    /// Move the panel selection and keep it in view: each server renders as
    /// two lines, so the scroll follows the selection with a small margin.
    pub fn mcp_select_next(&mut self) {
        self.mcp_panel.select_next();
        self.mcp_scroll = (self.mcp_panel.selected() as u16 * 2).saturating_sub(4);
        self.mark_dirty();
    }

    pub fn mcp_select_prev(&mut self) {
        self.mcp_panel.select_prev();
        self.mcp_scroll = (self.mcp_panel.selected() as u16 * 2).saturating_sub(4);
        self.mark_dirty();
    }

    /// Records the correlation id of a just-sent `/mcp list` query so the
    /// matching (and only the matching) `OutEvent::McpList` opens the panel.
    pub fn record_pending_mcp_list(&mut self, correlation_id: String) {
        self.mcp_panel.request(correlation_id);
    }

    /// Records a `/mcp` parse error (unknown subcommand, malformed add/remove
    /// args) as a transcript status line — no engine traffic, so nothing else
    /// to fold; mirrors `App::record_set_error`.
    pub fn record_mcp_error(&mut self, message: String) {
        self.sessions
            .active_view_mut()
            .record_status("mcp", format!("error: {message}"));
        self.mark_dirty();
    }

    /// Folds an `OutEvent::McpList` reply (#375): only opens the panel when it
    /// answers our own outstanding query.
    pub(super) fn handle_mcp_list(&mut self, correlation_id: &str, servers: Vec<McpServerStatus>) {
        if self.mcp_panel.apply_list(correlation_id, servers) {
            self.mcp_scroll = 0;
            self.mark_dirty();
        }
    }

    /// Folds an `OutEvent::McpChanged` reply (#375) as a transient info-line
    /// toast — a state-change confirmation, not transcript content.
    pub(super) fn handle_mcp_changed(&mut self, name: &str, action: McpAction) {
        let verb = match action {
            McpAction::Added => "added",
            McpAction::Removed => "removed",
        };
        self.set_toast(format!("MCP server '{name}' {verb}"));
    }
}

#[cfg(test)]
mod tests {
    use entanglement_core::SessionId;

    use super::*;

    fn status(name: &str) -> McpServerStatus {
        McpServerStatus {
            name: name.to_string(),
            transport: "stdio".to_string(),
            connected: true,
            tools: vec!["mcp__srv__tool".to_string()],
            error: None,
            state: None,
        }
    }

    #[test]
    fn mcp_list_opens_the_panel_on_a_matching_reply() {
        let mut app = App::new_for_test(SessionId::new("s1"));
        app.record_pending_mcp_list("c1".to_string());
        app.handle_mcp_list("c1", vec![status("srv")]);
        assert!(app.showing_mcp_panel());
        assert_eq!(app.mcp_servers().len(), 1);
    }

    #[test]
    fn mcp_list_ignores_a_reply_for_a_different_query() {
        let mut app = App::new_for_test(SessionId::new("s1"));
        app.record_pending_mcp_list("c1".to_string());
        app.handle_mcp_list("stray", vec![status("srv")]);
        assert!(!app.showing_mcp_panel());
    }

    #[test]
    fn mcp_changed_toasts_the_confirmation() {
        let mut app = App::new_for_test(SessionId::new("s1"));
        app.handle_mcp_changed("srv", McpAction::Added);
        assert_eq!(app.toast(), Some("MCP server 'srv' added"));
        assert!(
            app.transcript().is_empty(),
            "a state-change confirmation must not become transcript content"
        );
    }

    #[test]
    fn mcp_error_records_a_transcript_status_line() {
        let mut app = App::new_for_test(SessionId::new("s1"));
        app.record_mcp_error("unknown /mcp subcommand: bogus".to_string());
        let rendered = app
            .transcript()
            .iter()
            .any(|e| format!("{e:?}").contains("unknown /mcp subcommand"));
        assert!(rendered, "expected the parse error in the transcript");
    }
}
