//! `App` surface for `/tools` (#560 P9, ADR-0199 part 3): builds a fresh row
//! set on open (never cached, mirroring `explore`'s own pull-only posture),
//! owns the view's open/close/navigation, and enables the highlighted row on
//! `Enter` via the same path a typed `/enable` takes (ADR-0199 part 4).
//!
//! Row building reuses [`crate::discover::index_rows`] — the exact live
//! index `explore`/`tool_search` already serve — for name/description/kind,
//! then layers this session's own overlay/advertising status on top. No
//! profile-borne mask or permission grade exists to layer any more (ADR-0207
//! moved both onto the session's independent permission mode), so `masked`
//! reflects the overlay alone and `grade` has nothing local left to report.
//! Nothing here re-walks the registry/MCP/skill state independently.

use std::sync::Arc;

use entanglement_core::{Holly, ToolAdvertising, ToolOverlayEntry};
use ratatui::widgets::ListState;

use crate::discover::IndexRow;
use crate::tool_advertising::AdvertisingState;
use crate::tool_names;
use crate::tui::tools_view::{ToolsView, ToolsViewRow};

use super::App;

impl App {
    /// Install the shared ADR-0196 §2-3 pinned-mode/discovered-set handle —
    /// the same `Arc` the tool executor and the resolver closures in
    /// `main.rs` read/write. `/tools`' status column is read-only against it.
    pub fn set_advertising_state(&mut self, advertising: Arc<AdvertisingState>) {
        self.advertising = Some(advertising);
    }

    pub fn showing_tools_view(&self) -> bool {
        self.tools_view.visible()
    }

    pub fn tools_view(&self) -> &ToolsView {
        &self.tools_view
    }

    pub fn tools_view_state(&mut self) -> &mut ListState {
        self.tools_view.state()
    }

    pub fn open_tools_view(&mut self) {
        let rows = self.tools_view_rows();
        self.tools_view.show(rows);
        self.mark_dirty();
    }

    pub fn close_tools_view(&mut self) {
        self.tools_view.hide();
        self.mark_dirty();
    }

    pub fn tools_view_select_next(&mut self) {
        self.tools_view.select_next();
        self.mark_dirty();
    }

    pub fn tools_view_select_prev(&mut self) {
        self.tools_view.select_prev();
        self.mark_dirty();
    }

    pub fn tools_view_cycle_category(&mut self) {
        self.tools_view.cycle_category();
        self.mark_dirty();
    }

    pub fn tools_view_push_filter_char(&mut self, c: char) {
        self.tools_view.push_filter_char(c);
        self.mark_dirty();
    }

    pub fn tools_view_backspace_filter(&mut self) {
        self.tools_view.backspace_filter();
        self.mark_dirty();
    }

    /// `Enter` on a `/tools` row (ADR-0199 part 4): the exact enable path a
    /// typed `/enable tool <name>` takes, applied to the highlighted row's
    /// name — a flat `Ask`-grade enable (no `--allow`, matching the typed
    /// command's own default). Closes the view on success so the effect is
    /// visible in the transcript toast right away.
    pub async fn enable_selected_tools_view_row(&mut self, holly: &Holly) {
        let Some(pattern) = self.tools_view.selected().map(|r| r.name.clone()) else {
            return;
        };
        crate::tui::enable_command::upsert_enable(self, holly, pattern, false, None).await;
        self.close_tools_view();
    }

    /// The pure row-building step behind [`open_tools_view`][Self::open_tools_view].
    /// A missing `mcp_handles`/`advertising` handle (tests, or a caller that
    /// never wired them) degrades gracefully — an empty roster or an `"n/a"`
    /// status column, never a panic.
    fn tools_view_rows(&self) -> Vec<ToolsViewRow> {
        let session = self.active_session_id().clone();
        let overlay = self.overlay_entries(&session);

        let index: Vec<IndexRow> = match self.mcp_handles() {
            Some(handles) => {
                let registry = handles
                    .registry
                    .read()
                    .expect("tool registry lock poisoned")
                    .clone();
                // Freshly loaded, not the live per-session-activated registry
                // (that state lives inside the tool executor's own loop and
                // isn't threaded out to the TUI) — mirrors `skutter inspect`'s
                // own posture of re-running discovery with no engine.
                let skills = crate::skills::load_registry(self.root()).unwrap_or_default();
                crate::discover::index_rows(
                    &registry,
                    &handles.avail,
                    &handles.active,
                    &skills,
                    &session,
                )
            }
            None => Vec::new(),
        };

        let mode = self.advertising.as_ref().map(|a| a.mode(&session));
        let discovered = self
            .advertising
            .as_ref()
            .map(|a| {
                a.discovered
                    .lock()
                    .expect("discovered-tool mutex poisoned")
                    .names(&session)
            })
            .unwrap_or_default();

        let mut rows: Vec<ToolsViewRow> = index
            .into_iter()
            .map(|r| build_row(r, &overlay, mode, &discovered))
            .collect();
        rows.sort_by(|a, b| a.kind.cmp(b.kind).then_with(|| a.name.cmp(&b.name)));
        rows
    }
}

fn build_row(
    row: IndexRow,
    overlay: &[ToolOverlayEntry],
    mode: Option<ToolAdvertising>,
    discovered: &[String],
) -> ToolsViewRow {
    // Every profile inherits every tool now (ADR-0207) — the session's own
    // overlay is the only thing left that can mask a row here.
    let effective = ToolOverlayEntry::disposition(overlay, &row.name).unwrap_or(true);
    // No profile-borne permission grade exists any more either (ADR-0207
    // moved it onto the session's mode, not wired into this view yet) — the
    // column has nothing local left to report.
    let grade = None;
    ToolsViewRow {
        status: status_for(&row.name, mode, discovered),
        name: row.name,
        description: row.description,
        kind: row.kind,
        masked: !effective,
        grade,
    }
}

/// A name's advertising status: `Full` mode is universal (ADR-0192), so
/// every row reads `"advertised"`; `ToolSearch` mode distinguishes the fixed
/// lean kernel from a name already in this session's discovered tail
/// (ADR-0196 §2-3) from one still only reachable via `explore`/`describe`.
fn status_for(name: &str, mode: Option<ToolAdvertising>, discovered: &[String]) -> &'static str {
    match mode {
        None => "n/a",
        Some(ToolAdvertising::Full) => "advertised",
        Some(ToolAdvertising::ToolSearch) => {
            if is_kernel(name) {
                "kernel"
            } else if discovered.iter().any(|n| n == name) {
                "advertised"
            } else {
                "discoverable"
            }
        }
    }
}

/// Kernel membership per ADR-0196 §2: the fixed `TOOL_SEARCH_KERNEL` list,
/// the discovery pair itself (non-maskable, `explore`/`describe`), plus the
/// profile-defining specs threaded in separately from that list
/// (`propose_plan`, the spawn enum) — the same carve-out `tool_names.rs`
/// documents on `TOOL_SEARCH_KERNEL`.
fn is_kernel(name: &str) -> bool {
    tool_names::TOOL_SEARCH_KERNEL.contains(&name)
        || tool_names::is_non_maskable(name)
        || matches!(name, "propose_plan" | "agent" | "agent_send")
}

#[cfg(test)]
mod tests {
    use super::*;
    use entanglement_core::SessionId;

    fn index_row(name: &str, kind: &'static str) -> IndexRow {
        IndexRow {
            name: name.to_string(),
            description: format!("{name} description"),
            source: "built-in".to_string(),
            kind,
        }
    }

    #[test]
    fn full_mode_reports_every_row_advertised() {
        let row = build_row(
            index_row("bash", "tool"),
            &[],
            Some(ToolAdvertising::Full),
            &[],
        );
        assert_eq!(row.status, "advertised");
    }

    #[test]
    fn tool_search_mode_distinguishes_kernel_discoverable_and_discovered() {
        let kernel = build_row(
            index_row("bash", "tool"),
            &[],
            Some(ToolAdvertising::ToolSearch),
            &[],
        );
        assert_eq!(kernel.status, "kernel");

        let undiscovered = build_row(
            index_row("mcp__docs__search", "mcp"),
            &[],
            Some(ToolAdvertising::ToolSearch),
            &[],
        );
        assert_eq!(undiscovered.status, "discoverable");

        let discovered = build_row(
            index_row("mcp__docs__search", "mcp"),
            &[],
            Some(ToolAdvertising::ToolSearch),
            &["mcp__docs__search".to_string()],
        );
        assert_eq!(discovered.status, "advertised");
    }

    #[test]
    fn no_advertising_handle_reports_n_a() {
        let row = build_row(index_row("bash", "tool"), &[], None, &[]);
        assert_eq!(row.status, "n/a");
    }

    #[test]
    fn masked_reflects_the_overlay_since_no_profile_mask_exists_any_more() {
        // ADR-0207: every profile inherits every tool, so only the session's
        // own overlay can mask a row here.
        let unmasked = build_row(index_row("bash", "tool"), &[], None, &[]);
        assert!(!unmasked.masked);

        let overlay = vec![ToolOverlayEntry::deny("bash")];
        let masked = build_row(index_row("bash", "tool"), &overlay, None, &[]);
        assert!(masked.masked, "a deny overlay entry masks the row");
    }

    #[test]
    fn grade_has_no_profile_local_source_left() {
        let row = build_row(index_row("read", "tool"), &[], None, &[]);
        assert_eq!(row.grade, None);
    }

    #[test]
    fn is_kernel_covers_the_fixed_list_and_the_profile_defining_specs() {
        assert!(is_kernel("bash"));
        assert!(is_kernel("explore"));
        assert!(is_kernel("describe"));
        assert!(is_kernel("propose_plan"));
        assert!(is_kernel("agent"));
        assert!(is_kernel("agent_send"));
        assert!(!is_kernel("mcp__docs__search"));
        assert!(!is_kernel("call"));
    }

    #[tokio::test]
    async fn open_tools_view_degrades_to_an_empty_roster_with_no_handles() {
        let mut app = App::new_for_test(SessionId::new("s1"));
        app.open_tools_view();
        assert!(app.showing_tools_view());
        assert!(app.tools_view().filtered().is_empty());
    }
}
