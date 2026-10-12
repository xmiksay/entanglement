//! ADR-0201 classification for [`super::AvailableMcp`]: whether an
//! `mcp__<server>__*` name's `server` is available (eligible for a lazy
//! connect), explicitly `disabled`, or genuinely unknown — read live from the
//! roster with **no connect attempt** — plus the dispatch-time connect-failure
//! cooldown `available_enable.rs`'s `try_lazy_connect` consults before it
//! *does* attempt one. Split out of `available.rs` along the 400-line cap.

use std::time::{Duration, Instant};

use super::AvailableMcp;

/// What a `mcp__<server>__*` name's `server` resolves to, read live from the
/// roster with **no connect attempt** (ADR-0201). Never confuse this with a
/// *connect* outcome — `LazyConnectOutcome` (`available_enable.rs`) is that.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpTier {
    /// Registered and not disabled (ADR-0226: registered = available).
    Available,
    /// Explicitly configured `disabled` — a **known** name, deliberately off.
    Disabled,
    /// No configured or bundled server under this name — genuinely unknown.
    Unknown,
}

/// Parse `mcp__<server>__<tool>` into `server` — `None` for a name that
/// isn't namespaced this way (a host tool, or malformed).
pub fn server_name_of(tool_name: &str) -> Option<&str> {
    tool_name.strip_prefix("mcp__")?.split("__").next()
}

/// The decline for an MCP name matching no registered server.
pub fn unknown_server_decline(server: &str) -> String {
    format!("no MCP server named `{server}` — list_mcp shows the available ones")
}

/// The decline for a registered `server` that has no tool `tool` (a
/// registered `mcp__<server>__<tool>` name).
pub fn missing_tool_decline(server: &str, tool: &str) -> String {
    let short = tool
        .strip_prefix(&super::super::tool::namespaced_tool_name(server, ""))
        .unwrap_or(tool);
    format!(
        "mcp server `{server}` has no tool named `{short}` — \
         mcp_tools {{\"name\": \"{server}\"}} lists its tools"
    )
}

/// The truthful, attributed decline for a [`McpTier::Disabled`] server
/// (ADR-0201) — shared by every call site that reports it.
pub fn disabled_decline(server: &str) -> String {
    format!("mcp server `{server}` is disabled by configuration (`disabled: true`)")
}

impl AvailableMcp {
    /// Classify `server` per [`McpTier`] — a pure roster read.
    pub fn tier_of(&self, server: &str) -> McpTier {
        if self.get(server).is_some() {
            McpTier::Available
        } else if self.disabled_names.contains(server) {
            McpTier::Disabled
        } else {
            McpTier::Unknown
        }
    }

    /// The registered server — available or disabled — whose sanitized
    /// `mcp__<server>__` prefix `tool` carries.
    pub fn server_for_tool(&self, tool: &str) -> Option<String> {
        let available = self.available_names();
        let names = available
            .iter()
            .map(String::as_str)
            .chain(self.disabled_names.iter().map(String::as_str));
        super::super::tool::owning_server(names, tool).map(str::to_string)
    }

    /// Whether `server` failed a dispatch-time lazy connect within `cooldown`
    /// (ADR-0201) — the stampede guard a caller consults before retrying.
    pub(super) fn recently_failed_connect(&self, server: &str, cooldown: Duration) -> bool {
        self.recent_connect_failures
            .lock()
            .expect("MCP connect-failure mutex poisoned")
            .get(server)
            .is_some_and(|at| at.elapsed() < cooldown)
    }

    pub(super) fn record_connect_failure(&self, server: &str) {
        self.recent_connect_failures
            .lock()
            .expect("MCP connect-failure mutex poisoned")
            .insert(server.to_string(), Instant::now());
    }

    pub(crate) fn clear_connect_failure(&self, server: &str) {
        self.recent_connect_failures
            .lock()
            .expect("MCP connect-failure mutex poisoned")
            .remove(server);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_name_of_parses_the_namespaced_form_only() {
        assert_eq!(
            server_name_of("mcp__testserver__search"),
            Some("testserver")
        );
        assert_eq!(server_name_of("mcp__testserver__"), Some("testserver"));
        assert_eq!(server_name_of("bash"), None);
        assert_eq!(server_name_of(""), None);
    }

    #[test]
    fn tier_of_classifies_available_disabled_and_unknown() {
        let mut avail = AvailableMcp::default();
        avail.disabled_names.insert("off".to_string());
        assert_eq!(avail.tier_of("off"), McpTier::Disabled);
        assert_eq!(avail.tier_of("nope"), McpTier::Unknown);
        avail.insert_user("kb", crate::mcp::available::tests::stdio("true"));
        assert_eq!(avail.tier_of("kb"), McpTier::Available);
    }

    #[test]
    fn server_for_tool_resolves_by_registered_prefix() {
        let mut avail = AvailableMcp::default();
        avail.insert_user("my.docs", crate::mcp::available::tests::stdio("true"));
        avail.disabled_names.insert("off".to_string());
        assert_eq!(
            avail.server_for_tool("mcp__my_docs__read").as_deref(),
            Some("my.docs")
        );
        assert_eq!(avail.server_for_tool("mcp__off__x").as_deref(), Some("off"));
        assert_eq!(avail.server_for_tool("mcp__nope__x"), None);
    }

    #[test]
    fn cooldown_guard_expires() {
        let avail = AvailableMcp::default();
        assert!(!avail.recently_failed_connect("srv", Duration::from_secs(30)));
        avail.record_connect_failure("srv");
        assert!(avail.recently_failed_connect("srv", Duration::from_secs(30)));
        assert!(!avail.recently_failed_connect("srv", Duration::from_secs(0)));
        avail.clear_connect_failure("srv");
        assert!(!avail.recently_failed_connect("srv", Duration::from_secs(30)));
    }
}
