//! `/mcp list` result panel (#373): a popup listing connected MCP servers,
//! their transport, status, and tools. Mirrors [`crate::tui::key_dialog::KeyDialog`]'s
//! show/hide shape. Selectable since #539: `Up`/`Down` move a server
//! selection so `e`/`d` can enable/disable the highlighted server for the
//! active session (a `mcp__<name>__*` tool-overlay entry).

use entanglement_core::McpServerStatus;

#[derive(Default)]
pub struct McpPanel {
    visible: bool,
    servers: Vec<McpServerStatus>,
    /// Correlation id of the outstanding `McpList` query, if any (#375): guards
    /// against a stale/foreign `OutEvent::McpList` opening the panel with the
    /// wrong snapshot (e.g. a reply to another head sharing the same engine).
    pending: Option<String>,
    /// The highlighted server row (#539) the `e`/`d` keys act on.
    selected: usize,
}

impl McpPanel {
    pub fn visible(&self) -> bool {
        self.visible
    }

    pub fn servers(&self) -> &[McpServerStatus] {
        &self.servers
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    /// The highlighted server's name, if any server is listed.
    pub fn selected_server(&self) -> Option<&str> {
        self.servers.get(self.selected).map(|s| s.name.as_str())
    }

    pub fn select_next(&mut self) {
        if !self.servers.is_empty() {
            self.selected = (self.selected + 1) % self.servers.len();
        }
    }

    pub fn select_prev(&mut self) {
        if !self.servers.is_empty() {
            self.selected = self
                .selected
                .checked_sub(1)
                .unwrap_or(self.servers.len() - 1);
        }
    }

    pub fn hide(&mut self) {
        self.visible = false;
    }

    /// Records the correlation id of a just-sent `McpList` query.
    pub fn request(&mut self, correlation_id: String) {
        self.pending = Some(correlation_id);
    }

    /// Applies an `OutEvent::McpList` reply. Opens the panel with the snapshot
    /// only when it answers our own outstanding query; returns whether it did.
    pub fn apply_list(&mut self, correlation_id: &str, servers: Vec<McpServerStatus>) -> bool {
        if self.pending.as_deref() != Some(correlation_id) {
            return false;
        }
        self.pending = None;
        self.selected = 0;
        self.servers = servers;
        self.visible = true;
        true
    }
}

#[cfg(test)]
mod tests {
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
    fn apply_list_opens_the_panel_when_it_matches_the_pending_request() {
        let mut panel = McpPanel::default();
        panel.request("c1".to_string());
        assert!(panel.apply_list("c1", vec![status("srv")]));
        assert!(panel.visible());
        assert_eq!(panel.servers().len(), 1);
    }

    #[test]
    fn apply_list_ignores_a_reply_for_a_different_query() {
        let mut panel = McpPanel::default();
        panel.request("c1".to_string());
        assert!(!panel.apply_list("stray", vec![status("srv")]));
        assert!(!panel.visible());
        assert!(panel.servers().is_empty());
    }

    #[test]
    fn apply_list_ignores_a_reply_with_no_outstanding_request() {
        let mut panel = McpPanel::default();
        assert!(!panel.apply_list("c1", vec![status("srv")]));
        assert!(!panel.visible());
    }

    #[test]
    fn hide_clears_visibility_but_keeps_the_last_snapshot() {
        let mut panel = McpPanel::default();
        panel.request("c1".to_string());
        panel.apply_list("c1", vec![status("srv")]);
        panel.hide();
        assert!(!panel.visible());
        assert_eq!(panel.servers().len(), 1);
    }
}
