//! `App` surface for `/tools` (#560 P9, ADR-0199 part 3): builds a fresh row
//! set on open (never cached), owns the view's open/close/navigation, and
//! enables the highlighted row on `Enter` via the same path a typed
//! `/enable` takes (ADR-0199 part 4).
//!
//! Row building reuses [`crate::discover::index_rows`] — the runtime's one
//! live tool index — for name/description/kind, then layers this session's
//! own overlay and advertised-surface status on top (ADR-0226). No
//! profile-borne mask or permission grade exists to layer any more (ADR-0207
//! moved both onto the session's independent permission mode), so `masked`
//! reflects the overlay alone and `grade` has nothing local left to report.
//! Nothing here re-walks the registry/MCP/skill state independently.

use std::sync::Arc;

use entanglement_core::{Holly, ToolOverlayEntry};
use ratatui::widgets::ListState;

use crate::discover::IndexRow;
use crate::tool_advertising::AdvertisingState;
use crate::tui::tools_view::{ToolsView, ToolsViewRow};

use super::App;

impl App {
    /// Install the shared advertising state — the same `Arc` the tool
    /// executor and the resolver closures in `boot.rs` read/write. `/tools`'
    /// status column is read-only against it.
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
                let skills = crate::skills::load_registry_in(self.root(), &self.inspect.user_layer)
                    .unwrap_or_default();
                crate::discover::index_rows(&registry, &handles.avail, &handles.active, &skills)
            }
            None => Vec::new(),
        };

        let advertised = self
            .advertising
            .as_ref()
            .and_then(|a| a.advertised_names(&session));

        let mut rows: Vec<ToolsViewRow> = index
            .into_iter()
            .map(|r| build_row(r, &overlay, advertised.as_deref()))
            .collect();
        rows.sort_by(|a, b| a.kind.cmp(b.kind).then_with(|| a.name.cmp(&b.name)));
        rows
    }
}

fn build_row(
    row: IndexRow,
    overlay: &[ToolOverlayEntry],
    advertised: Option<&[String]>,
) -> ToolsViewRow {
    // Every profile inherits every tool now (ADR-0207) — the session's own
    // overlay is the only thing left that can mask a row here.
    let effective = ToolOverlayEntry::disposition(overlay, &row.name).unwrap_or(true);
    ToolsViewRow {
        status: status_for(&row.name, advertised),
        name: row.name,
        description: row.description,
        kind: row.kind,
        masked: !effective,
        // No profile-borne permission grade exists any more (ADR-0207 moved
        // it onto the session's mode, not wired into this view yet).
        grade: None,
    }
}

/// A name's advertising status against the session's surface snapshot
/// (ADR-0226): in it ⇒ "advertised"; otherwise "hidden" — a script-only
/// tool or an MCP tool reached through `mcp`. No snapshot yet (the session
/// hasn't run a round) or no handle ⇒ "n/a" rather than a guess.
fn status_for(name: &str, advertised: Option<&[String]>) -> &'static str {
    match advertised {
        None => "n/a",
        Some(names) if names.iter().any(|n| n == name) => "advertised",
        Some(_) => "hidden",
    }
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
    fn status_follows_the_session_surface_snapshot() {
        let surface = vec!["bash".to_string(), "mcp".to_string()];
        let shown = build_row(index_row("bash", "tool"), &[], Some(&surface));
        assert_eq!(shown.status, "advertised");
        let hidden = build_row(index_row("mcp__docs__search", "mcp"), &[], Some(&surface));
        assert_eq!(hidden.status, "hidden");
        let script_only = build_row(index_row("glob", "tool"), &[], Some(&surface));
        assert_eq!(script_only.status, "hidden");
    }

    #[test]
    fn no_snapshot_reports_n_a() {
        let row = build_row(index_row("bash", "tool"), &[], None);
        assert_eq!(row.status, "n/a");
    }

    #[test]
    fn masked_reflects_the_overlay_since_no_profile_mask_exists_any_more() {
        // ADR-0207: every profile inherits every tool, so only the session's
        // own overlay can mask a row here.
        let unmasked = build_row(index_row("bash", "tool"), &[], None);
        assert!(!unmasked.masked);

        let overlay = vec![ToolOverlayEntry::deny("bash")];
        let masked = build_row(index_row("bash", "tool"), &overlay, None);
        assert!(masked.masked, "a deny overlay entry masks the row");
    }

    #[test]
    fn grade_has_no_profile_local_source_left() {
        let row = build_row(index_row("read", "tool"), &[], None);
        assert_eq!(row.grade, None);
    }

    #[tokio::test]
    async fn open_tools_view_degrades_to_an_empty_roster_with_no_handles() {
        let mut app = App::new_for_test(SessionId::new("s1"));
        app.open_tools_view();
        assert!(app.showing_tools_view());
        assert!(app.tools_view().filtered().is_empty());
    }
}
