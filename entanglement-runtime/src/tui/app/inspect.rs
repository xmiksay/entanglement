//! In-session inspection overlay state (#214): a read-only three-tab view over
//! the **active session's** resolved prompt / agent registry / skill registry —
//! the same data `skutter inspect prompt|agents|skills` exposes on the CLI, but
//! reachable mid-session via `/inspect` without leaving the TUI.
//!
//! The three views are resolved engine-lessly (`crate::inspect::tui_reports`)
//! from the working directory + the live agent when the overlay opens, then held
//! as plain scrollable text — a presentation layer over the CLI's own renderers.

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
}

/// The overlay's state: whether it is open, the selected tab, its vertical scroll,
/// and the pre-rendered text of each tab (resolved once, on open).
#[derive(Default)]
pub struct InspectState {
    visible: bool,
    tab: InspectTab,
    scroll: u16,
    prompt: String,
    agents: String,
    skills: String,
}

impl InspectState {
    fn content(&self) -> &str {
        match self.tab {
            InspectTab::Prompt => &self.prompt,
            InspectTab::Agents => &self.agents,
            InspectTab::Skills => &self.skills,
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
            self.inspect.tab = InspectTab::Prompt;
            self.inspect.scroll = 0;
            self.inspect.visible = true;
            self.mark_dirty();
        }
    }

    pub fn close_inspect(&mut self) {
        self.inspect.visible = false;
        self.mark_dirty();
    }

    pub fn inspect_next_tab(&mut self) {
        self.inspect.tab = self.inspect.tab.next();
        self.inspect.scroll = 0;
        self.mark_dirty();
    }

    pub fn inspect_prev_tab(&mut self) {
        self.inspect.tab = self.inspect.tab.prev();
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
mod tests {
    use super::*;

    #[test]
    fn tab_cycles_forward_and_back() {
        assert_eq!(InspectTab::Prompt.next(), InspectTab::Agents);
        assert_eq!(InspectTab::Agents.next(), InspectTab::Skills);
        assert_eq!(InspectTab::Skills.next(), InspectTab::Prompt);
        assert_eq!(InspectTab::Prompt.prev(), InspectTab::Skills);
    }

    #[test]
    fn toggle_opens_populated_overlay_and_tabs_cycle() {
        use entanglement_core::SessionId;

        let mut app = App::new_for_test(SessionId::new("s1"));
        assert!(!app.showing_inspect());

        // Opening resolves all three views from the built-in registries (always
        // present via `include_str!`, so this is cwd-independent).
        app.toggle_inspect();
        assert!(app.showing_inspect());
        assert_eq!(app.inspect_tab(), InspectTab::Prompt);
        assert!(!app.inspect_content().is_empty());

        app.inspect_next_tab();
        assert_eq!(app.inspect_tab(), InspectTab::Agents);
        // The agents view lists the built-in roster.
        assert!(app.inspect_content().contains("build"));
        assert_eq!(app.inspect_scroll(), 0, "tab switch resets scroll");

        app.inspect_scroll_down(2);
        assert!(app.inspect_scroll() > 0);
        app.inspect_prev_tab();
        assert_eq!(app.inspect_scroll(), 0, "tab switch resets scroll");

        app.toggle_inspect();
        assert!(!app.showing_inspect());
    }

    #[test]
    fn scroll_clamps_at_top_and_bottom() {
        let mut st = InspectState {
            prompt: "a\nb\nc".to_string(),
            ..Default::default()
        };
        // Up from 0 stays at 0.
        st.scroll = 0;
        st.scroll = st.scroll.saturating_sub(5);
        assert_eq!(st.scroll, 0);
        // Bottom clamps to last line (3 lines → max offset 2).
        let max = st.content().lines().count().saturating_sub(1) as u16;
        st.scroll = 99u16.min(max);
        assert_eq!(st.scroll, 2);
    }
}
