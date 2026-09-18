//! `/tools`'s read-heavy browser (#560 P9, ADR-0199 part 3): every tool the
//! session could reach, grouped by kind, filterable by free text plus a
//! category cycle. Unlike [`super::session_tools_dialog`] (a checklist that
//! *edits* the overlay), this is primarily a *lookup* surface — `Enter` on a
//! row still enables it (delegating to the same `/enable` path,
//! ADR-0199 part 4), but the point of the view is answering "what exists,
//! and can I already use it" before reaching for `/enable` by hand.
//!
//! Kept as its own dialog rather than folded into `SessionToolsDialog`: that
//! dialog's `to_entries` diff-against-profile submit shape has no analogue
//! here (this view never submits a batch), and its rows carry only a
//! name/checkbox — no room for a description, kind, or status without
//! reshaping a struct three other call sites already depend on. The
//! row-building logic it needs (name/description/kind) is *not*
//! reimplemented here — see `App::tools_view_rows` (`app/tools_view.rs`),
//! which calls into [`crate::discover::index_rows`], the same index
//! `explore` itself serves.

use ratatui::widgets::ListState;

/// One row: a describable tool/MCP-server-hint/skill/endpoint plus this
/// session's relationship to it. `status` and `grade` are best-effort —
/// `"n/a"`/`None` when the underlying state isn't cheaply available (no
/// live `AdvertisingState` handle, no matching agent profile) rather than a
/// guess.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolsViewRow {
    pub name: String,
    pub description: String,
    /// `"tool"` | `"mcp"` | `"skill"` | `"endpoint"` — matches
    /// `explore`'s own `kind` enum values byte-for-byte.
    pub kind: &'static str,
    /// `"kernel"` | `"advertised"` | `"discoverable"` | `"n/a"`.
    pub status: &'static str,
    /// Withheld by the session's own tool overlay — the same
    /// effective-availability read `session_tools_dialog` computes. No
    /// profile-borne mask exists any more (ADR-0207): every profile inherits
    /// every tool, so the overlay is the only thing that can mask a row.
    pub masked: bool,
    /// Always `None` now (ADR-0207 moved the permission grade off the
    /// profile and onto the session's mode, not wired into this view yet) —
    /// kept as a field so a later stage can populate it from the mode
    /// without another `ToolsViewRow` shape change.
    pub grade: Option<&'static str>,
}

/// The category cycle order `cycle_category` steps through — `None` (every
/// kind) first, then each kind in the same fixed order `explore`'s own
/// sections render in.
const CATEGORY_ORDER: [Option<&str>; 5] = [
    None,
    Some("tool"),
    Some("mcp"),
    Some("skill"),
    Some("endpoint"),
];

pub struct ToolsView {
    visible: bool,
    rows: Vec<ToolsViewRow>,
    filter: String,
    category: Option<&'static str>,
    state: ListState,
}

impl ToolsView {
    pub fn new() -> Self {
        Self {
            visible: false,
            rows: Vec::new(),
            filter: String::new(),
            category: None,
            state: ListState::default(),
        }
    }

    pub fn visible(&self) -> bool {
        self.visible
    }

    pub fn state(&mut self) -> &mut ListState {
        &mut self.state
    }

    pub fn filter_text(&self) -> &str {
        &self.filter
    }

    pub fn category(&self) -> Option<&'static str> {
        self.category
    }

    /// Open over a freshly-built row set (`App::tools_view_rows`) — filter
    /// and category cycle reset to "everything" each time the view opens.
    pub fn show(&mut self, rows: Vec<ToolsViewRow>) {
        self.rows = rows;
        self.filter.clear();
        self.category = None;
        self.state.select((!self.rows.is_empty()).then_some(0));
        self.visible = true;
    }

    pub fn hide(&mut self) {
        self.visible = false;
    }

    /// Rows surviving the current category + free-text filter, in the
    /// caller-provided (already kind-then-name sorted, see
    /// `App::tools_view_rows`) order.
    pub fn filtered(&self) -> Vec<&ToolsViewRow> {
        let needle = self.filter.to_ascii_lowercase();
        self.rows
            .iter()
            .filter(|r| self.category.is_none_or(|k| r.kind == k))
            .filter(|r| {
                needle.is_empty()
                    || r.name.to_ascii_lowercase().contains(&needle)
                    || r.description.to_ascii_lowercase().contains(&needle)
            })
            .collect()
    }

    pub fn selected(&self) -> Option<&ToolsViewRow> {
        let idx = self.state.selected()?;
        self.filtered().into_iter().nth(idx)
    }

    fn clamp_selection(&mut self) {
        let len = self.filtered().len();
        if len == 0 {
            self.state.select(None);
        } else {
            let current = self.state.selected().unwrap_or(0).min(len - 1);
            self.state.select(Some(current));
        }
    }

    pub fn push_filter_char(&mut self, c: char) {
        self.filter.push(c);
        self.clamp_selection();
    }

    pub fn backspace_filter(&mut self) {
        self.filter.pop();
        self.clamp_selection();
    }

    pub fn cycle_category(&mut self) {
        let idx = CATEGORY_ORDER
            .iter()
            .position(|c| *c == self.category)
            .unwrap_or(0);
        self.category = CATEGORY_ORDER[(idx + 1) % CATEGORY_ORDER.len()];
        self.clamp_selection();
    }

    pub fn select_next(&mut self) {
        let len = self.filtered().len();
        if len == 0 {
            return;
        }
        let current = self.state.selected().unwrap_or(0);
        self.state.select(Some((current + 1) % len));
    }

    pub fn select_prev(&mut self) {
        let len = self.filtered().len();
        if len == 0 {
            return;
        }
        let current = self.state.selected().unwrap_or(0);
        let prev = if current == 0 { len - 1 } else { current - 1 };
        self.state.select(Some(prev));
    }
}

impl Default for ToolsView {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str, kind: &'static str) -> ToolsViewRow {
        ToolsViewRow {
            name: name.to_string(),
            description: format!("{name} description"),
            kind,
            status: "n/a",
            masked: false,
            grade: None,
        }
    }

    fn view_with(rows: Vec<ToolsViewRow>) -> ToolsView {
        let mut v = ToolsView::new();
        v.show(rows);
        v
    }

    #[test]
    fn show_resets_filter_and_category_and_selects_first_row() {
        let mut v = view_with(vec![row("bash", "tool")]);
        v.push_filter_char('x');
        v.cycle_category();
        v.show(vec![row("read", "tool"), row("mcp__docs__search", "mcp")]);
        assert_eq!(v.filter_text(), "");
        assert_eq!(v.category(), None);
        assert_eq!(v.state().selected(), Some(0));
    }

    #[test]
    fn category_cycle_narrows_the_filtered_set() {
        let mut v = view_with(vec![
            row("bash", "tool"),
            row("mcp__docs__search", "mcp"),
            row("git", "skill"),
        ]);
        assert_eq!(v.filtered().len(), 3);
        v.cycle_category(); // -> tool
        assert_eq!(v.category(), Some("tool"));
        assert_eq!(v.filtered(), vec![&row("bash", "tool")]);
        v.cycle_category(); // -> mcp
        assert_eq!(v.filtered(), vec![&row("mcp__docs__search", "mcp")]);
        v.cycle_category(); // -> skill
        v.cycle_category(); // -> endpoint
        assert!(v.filtered().is_empty());
        v.cycle_category(); // wraps back to "all"
        assert_eq!(v.category(), None);
        assert_eq!(v.filtered().len(), 3);
    }

    #[test]
    fn free_text_filter_matches_name_or_description_case_insensitively() {
        let mut v = view_with(vec![row("bash", "tool"), row("read", "tool")]);
        v.push_filter_char('B');
        v.push_filter_char('A');
        assert_eq!(v.filtered(), vec![&row("bash", "tool")]);
        v.backspace_filter();
        v.backspace_filter();
        assert_eq!(v.filtered().len(), 2);
    }

    #[test]
    fn selection_clamps_when_the_filtered_set_shrinks() {
        let mut v = view_with(vec![row("bash", "tool"), row("mcp__docs__search", "mcp")]);
        v.select_next();
        assert_eq!(v.state().selected(), Some(1));
        // Narrowing to a one-row category must not leave a stale out-of-range
        // selection.
        v.cycle_category(); // -> tool (one row)
        assert_eq!(v.state().selected(), Some(0));
        assert_eq!(v.selected(), Some(&row("bash", "tool")));
    }

    #[test]
    fn selection_clears_when_nothing_matches() {
        let mut v = view_with(vec![row("bash", "tool")]);
        v.push_filter_char('z');
        assert_eq!(v.state().selected(), None);
        assert_eq!(v.selected(), None);
    }

    #[test]
    fn select_next_and_prev_wrap() {
        let mut v = view_with(vec![row("a", "tool"), row("b", "tool"), row("c", "tool")]);
        v.select_prev();
        assert_eq!(
            v.state().selected(),
            Some(2),
            "prev from 0 wraps to the end"
        );
        v.select_next();
        assert_eq!(
            v.state().selected(),
            Some(0),
            "next from the end wraps to 0"
        );
    }
}
