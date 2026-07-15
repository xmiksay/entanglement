//! In-session inspection overlay state (#214, drill-down #331): a three-tab
//! read-only view over the **active session's** resolved prompt / agent registry
//! / skill registry — the same data `skutter inspect prompt|agents|skills`
//! exposes on the CLI, but reachable mid-session via `/inspect` without leaving
//! the TUI.
//!
//! The views are resolved engine-lessly (`crate::inspect::tui_reports`) from the
//! working directory + the live agent when the overlay opens. The Prompt tab is
//! a single scroll-only document; the Agents and Skills tabs are **two-level**
//! (#331): a selectable list (`j`/`k`/arrows move the highlight, `Enter` opens
//! the per-item detail pane rendered by the same code path the CLI uses,
//! `Esc`/`Backspace` returns to the list, `Esc` from the list closes the
//! overlay). Tab switching works from either level.

use crate::inspect::{InspectItem, InspectListTab};

use super::App;

/// The three inspection tabs, in display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InspectTab {
    #[default]
    Prompt,
    Agents,
    Skills,
}

impl InspectTab {
    const ORDER: [InspectTab; 3] = [InspectTab::Prompt, InspectTab::Agents, InspectTab::Skills];

    pub fn title(self) -> &'static str {
        match self {
            InspectTab::Prompt => "Prompt",
            InspectTab::Agents => "Agents",
            InspectTab::Skills => "Skills",
        }
    }

    fn index(self) -> usize {
        Self::ORDER.iter().position(|t| *t == self).unwrap_or(0)
    }

    fn next(self) -> Self {
        Self::ORDER[(self.index() + 1) % Self::ORDER.len()]
    }

    fn prev(self) -> Self {
        Self::ORDER[(self.index() + Self::ORDER.len() - 1) % Self::ORDER.len()]
    }

    /// The two-level list tab (`Agents`/`Skills`); `None` for `Prompt`, which is
    /// a single scroll-only document with no list level (#331). Public so the
    /// overlay renderer can pick the level-appropriate hint line.
    pub fn list_tab(self) -> Option<InspectListTab> {
        match self {
            InspectTab::Prompt => None,
            InspectTab::Agents => Some(InspectListTab::Agents),
            InspectTab::Skills => Some(InspectListTab::Skills),
        }
    }
}

/// Which level of a two-level tab is showing (#331): the list or the per-item
/// detail. The Prompt tab has no list level, so it is always treated as `List`
/// and its detail entry is unreachable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InspectLevel {
    #[default]
    List,
    Detail,
}

/// The overlay's state: whether it is open, the selected tab, its vertical scroll,
/// the pre-rendered text of each tab (resolved once, on open), and — for the
/// two-level Agents/Skills tabs (#331) — the selectable list rows, the
/// highlighted row, the current level, and the per-item detail string.
#[derive(Default)]
pub struct InspectState {
    visible: bool,
    tab: InspectTab,
    scroll: u16,
    prompt: String,
    agents: String,
    skills: String,
    /// Two-level list rows per tab (#331). Empty for the Prompt tab.
    agent_items: Vec<InspectItem>,
    skill_items: Vec<InspectItem>,
    /// Highlighted row within the current two-level tab's list.
    selected: usize,
    /// Current level: list or per-item detail (#331).
    level: InspectLevel,
    /// The resolved per-item detail string, set on `Enter`. Cleared on back.
    detail: Option<String>,
}

impl InspectState {
    fn content(&self) -> &str {
        // The detail pane overrides the flat content while a two-level tab is
        // drilled into (#331); otherwise the tab's flat summary shows.
        if let Some(detail) = &self.detail {
            return detail;
        }
        match self.tab {
            InspectTab::Prompt => &self.prompt,
            InspectTab::Agents => &self.agents,
            InspectTab::Skills => &self.skills,
        }
    }

    fn items(&self) -> &[InspectItem] {
        match self.tab {
            InspectTab::Prompt => &[],
            InspectTab::Agents => &self.agent_items,
            InspectTab::Skills => &self.skill_items,
        }
    }
}

impl App {
    pub fn showing_inspect(&self) -> bool {
        self.inspect.visible
    }

    pub fn inspect_tab(&self) -> InspectTab {
        self.inspect.tab
    }

    pub fn inspect_scroll(&self) -> u16 {
        self.inspect.scroll
    }

    pub fn inspect_content(&self) -> &str {
        self.inspect.content()
    }

    /// Whether the current tab is showing its list level (#331) — i.e. the
    /// overlay should render the selectable list rather than a scroll-only
    /// document. The Prompt tab is never a list.
    pub fn inspect_showing_list(&self) -> bool {
        self.inspect.tab.list_tab().is_some() && self.inspect.level == InspectLevel::List
    }

    /// The selectable rows of the current two-level tab (#331), for the list
    /// renderer. Empty when the Prompt tab is active or the level is Detail.
    pub fn inspect_items(&self) -> &[InspectItem] {
        if self.inspect_showing_list() {
            self.inspect.items()
        } else {
            &[]
        }
    }

    /// The highlighted row index in the current two-level list (#331).
    pub fn inspect_selected(&self) -> usize {
        self.inspect.selected
    }

    /// Open (resolving the three views from the live cwd + agent) or close the
    /// overlay. Re-resolving on every open keeps the views fresh across mid-session
    /// edits to agent/skill definitions.
    pub fn toggle_inspect(&mut self) {
        if self.inspect.visible {
            self.close_inspect();
        } else {
            let reports = crate::inspect::tui_reports(self.root(), self.agent());
            self.inspect.prompt = reports.prompt;
            self.inspect.agents = reports.agents;
            self.inspect.skills = reports.skills;
            self.inspect.agent_items = reports.agent_items;
            self.inspect.skill_items = reports.skill_items;
            self.inspect.tab = InspectTab::Prompt;
            self.inspect.scroll = 0;
            self.inspect.selected = 0;
            self.inspect.level = InspectLevel::List;
            self.inspect.detail = None;
            self.inspect.visible = true;
            self.mark_dirty();
        }
    }

    pub fn close_inspect(&mut self) {
        self.inspect.visible = false;
        // Reset two-level state so a reopen starts fresh.
        self.inspect.level = InspectLevel::List;
        self.inspect.detail = None;
        self.inspect.selected = 0;
        self.mark_dirty();
    }

    pub fn inspect_next_tab(&mut self) {
        self.inspect.tab = self.inspect.tab.next();
        self.reset_tab_view();
        self.mark_dirty();
    }

    pub fn inspect_prev_tab(&mut self) {
        self.inspect.tab = self.inspect.tab.prev();
        self.reset_tab_view();
        self.mark_dirty();
    }

    /// `Tab`/`BackTab` keep switching tabs from either level (#331): switching
    /// away from a drilled-into detail returns to the new tab's list level and
    /// clamps the selection to that tab's row count.
    fn reset_tab_view(&mut self) {
        self.inspect.scroll = 0;
        self.inspect.level = InspectLevel::List;
        self.inspect.detail = None;
        // Clamp to the new tab's list length; a Prompt tab has no rows so the
        // selection is harmless but reset to 0 for sanity.
        let len = self.inspect.items().len();
        if self.inspect.selected >= len {
            self.inspect.selected = len.saturating_sub(1);
        }
    }

    /// Move the list highlight down `n` rows (clamped) on the list level (#331).
    pub fn inspect_list_down(&mut self, n: usize) {
        if !self.inspect_showing_list() {
            return;
        }
        let len = self.inspect.items().len();
        if len == 0 {
            return;
        }
        let max = len - 1;
        self.inspect.selected = (self.inspect.selected.saturating_add(n)).min(max);
        self.mark_dirty();
    }

    /// Move the list highlight up `n` rows (clamped) on the list level (#331).
    pub fn inspect_list_up(&mut self, n: usize) {
        if !self.inspect_showing_list() {
            return;
        }
        self.inspect.selected = self.inspect.selected.saturating_sub(n);
        self.mark_dirty();
    }

    /// Open the detail pane for the highlighted row (#331). Renders the per-item
    /// detail via the same code path the CLI uses (`inspect agents <name>` /
    /// `inspect skills <name>`). A no-op on the Prompt tab (no list level) and
    /// when already in the detail pane.
    pub fn inspect_open_detail(&mut self) {
        let Some(list_tab) = self.inspect.tab.list_tab() else {
            return;
        };
        if self.inspect.level != InspectLevel::List {
            return;
        }
        let Some(item) = self.inspect.items().get(self.inspect.selected).cloned() else {
            return;
        };
        let detail = match list_tab {
            InspectListTab::Agents => crate::inspect::agent_detail(self.root(), &item.name),
            InspectListTab::Skills => crate::inspect::skill_detail(self.root(), &item.name),
        };
        self.inspect.detail = detail.or_else(|| Some(format!("(no detail for `{}`)", item.name)));
        self.inspect.level = InspectLevel::Detail;
        self.inspect.scroll = 0;
        self.mark_dirty();
    }

    /// Return from the detail pane to the list level (#331). A no-op when already
    /// on the list (the caller closes the overlay on that `Esc` instead).
    pub fn inspect_back_to_list(&mut self) {
        if self.inspect.level != InspectLevel::Detail {
            return;
        }
        self.inspect.level = InspectLevel::List;
        self.inspect.detail = None;
        self.inspect.scroll = 0;
        self.mark_dirty();
    }

    pub fn inspect_scroll_down(&mut self, n: u16) {
        // Clamp to the last line so the pane can't scroll past its content.
        let max = self.inspect.content().lines().count().saturating_sub(1) as u16;
        self.inspect.scroll = self.inspect.scroll.saturating_add(n).min(max);
        self.mark_dirty();
    }

    pub fn inspect_scroll_up(&mut self, n: u16) {
        self.inspect.scroll = self.inspect.scroll.saturating_sub(n);
        self.mark_dirty();
    }
}

#[cfg(test)]
mod tests;
