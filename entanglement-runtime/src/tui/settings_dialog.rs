//! Bare `/set`'s tabbed session-settings dialog: Session (agent + model),
//! Generation (only the knobs the selected model accepts), Tools (the
//! per-session overlay) and Aux (per-purpose pins). Pure state —
//! `app/settings.rs` seeds it and runs the confirmed plan, `modals/settings.rs`
//! draws it, `settings_events.rs` routes keys and clicks.
//!
//! Every row of the active tab is a [`RowView`]; navigation moves a focus over
//! the focusable ones and `←/→/Space` act on the focused row, so render, keys
//! and mouse all read one row list and can't disagree.

mod apply;
mod aux;
mod generation;
mod rows;
mod session;
mod tools;

#[cfg(test)]
mod tests;

pub use apply::{run_plan, ApplyStep, SettingsEffects, ToolsChange};
pub use aux::AuxTab;
pub use generation::GenField;
pub use rows::{RowId, RowView};
pub use session::{mode_names, model_options, SessionTab};
pub use tools::ToolsTab;

use entanglement_provider::GenerationParams;
use ratatui::widgets::ListState;

use generation::GenerationTab;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Session,
    Generation,
    Tools,
    Aux,
}

impl Tab {
    pub const ALL: [Tab; 4] = [Tab::Session, Tab::Generation, Tab::Tools, Tab::Aux];

    pub fn title(self) -> &'static str {
        match self {
            Tab::Session => "Session",
            Tab::Generation => "Generation",
            Tab::Tools => "Tools",
            Tab::Aux => "Aux",
        }
    }

    fn index(self) -> usize {
        Tab::ALL.iter().position(|t| *t == self).unwrap_or(0)
    }
}

pub enum Confirm {
    /// Nothing pending — just close.
    Close,
    Apply(Vec<ApplyStep>),
}

pub const AUX_PERSIST_REASON: &str = "aux pins are process-wide and always persisted";

pub struct SettingsDialog {
    tab: Tab,
    /// Focus per tab, as an index into that tab's focusable rows.
    focus: [usize; 4],
    list: ListState,
    /// "Save as default" for Session, Generation; Aux is always on and Tools
    /// has nothing left to save as default (ADR-0207).
    persist: [bool; 2],
    session: SessionTab,
    generation: GenerationTab,
    tools: ToolsTab,
    aux: AuxTab,
}

impl SettingsDialog {
    pub fn new(
        session: SessionTab,
        current: GenerationParams,
        tools: ToolsTab,
        aux: AuxTab,
    ) -> Self {
        let caps = session.final_model().caps;
        Self {
            tab: Tab::Session,
            focus: [0; 4],
            list: ListState::default(),
            persist: [false; 2],
            session,
            generation: GenerationTab::new(current, caps),
            tools,
            aux,
        }
    }

    pub fn tab(&self) -> Tab {
        self.tab
    }

    pub fn list_state(&mut self) -> &mut ListState {
        &mut self.list
    }

    pub fn set_tab(&mut self, tab: Tab) {
        self.tab = tab;
    }

    pub fn next_tab(&mut self) {
        self.set_tab(Tab::ALL[(self.tab.index() + 1) % Tab::ALL.len()]);
    }

    pub fn prev_tab(&mut self) {
        self.set_tab(Tab::ALL[(self.tab.index() + Tab::ALL.len() - 1) % Tab::ALL.len()]);
    }

    /// Whether `tab`'s "save as default" is on.
    #[cfg(test)]
    pub fn persist(&self, tab: Tab) -> bool {
        match tab {
            Tab::Aux => true,
            Tab::Tools => false,
            Tab::Session => self.persist[0],
            Tab::Generation => self.persist[1],
        }
    }

    fn focusable(&self) -> Vec<usize> {
        self.rows()
            .iter()
            .enumerate()
            .filter(|(_, r)| r.id != RowId::Note)
            .map(|(i, _)| i)
            .collect()
    }

    /// The focused row as an index into [`rows`][Self::rows].
    pub fn focused_row(&self) -> usize {
        let focusable = self.focusable();
        let f = self.focus[self.tab.index()].min(focusable.len().saturating_sub(1));
        focusable.get(f).copied().unwrap_or(0)
    }

    pub fn move_focus(&mut self, delta: isize) {
        let len = self.focusable().len();
        if len == 0 {
            return;
        }
        let slot = &mut self.focus[self.tab.index()];
        let current = (*slot).min(len - 1) as isize;
        *slot = (current + delta).clamp(0, len as isize - 1) as usize;
    }

    /// Focus the row at `index` into [`rows`][Self::rows] (a mouse click);
    /// a non-focusable row is ignored.
    pub fn focus_row(&mut self, index: usize) {
        if let Some(f) = self.focusable().iter().position(|i| *i == index) {
            self.focus[self.tab.index()] = f;
        }
    }

    fn focused_id(&self) -> RowId {
        self.rows()
            .get(self.focused_row())
            .map(|r| r.id)
            .unwrap_or(RowId::Note)
    }

    /// `←`/`→`/`Space` on the focused row.
    pub fn activate(&mut self, forward: bool) {
        match self.focused_id() {
            // Session (0) and Generation (1) are the only tabs with a
            // `Persist` row now (Aux is always-on, Tools has nothing left to
            // save as default, ADR-0207) — `self.tab.index()` is always a
            // valid `persist` slot whenever this row is actually focused.
            RowId::Persist if matches!(self.tab, Tab::Session | Tab::Generation) => {
                let i = self.tab.index();
                self.persist[i] = !self.persist[i];
            }
            RowId::Model => {
                self.session.cycle_model(forward);
                self.refresh_caps();
            }
            RowId::PermMode => self.session.cycle_mode(forward),
            RowId::Gen(f) => self.generation.cycle(f, forward),
            RowId::Tool(i) => self.tools.toggle(i),
            RowId::Aux(p) => self.aux.cycle(p, forward),
            RowId::Persist | RowId::Note => {}
        }
    }

    pub fn toggle_allow(&mut self) {
        if let RowId::Tool(i) = self.focused_id() {
            self.tools.toggle_allow(i);
        }
    }

    /// `PgUp`/`PgDn`: jump provider on the model row, else page the focus.
    pub fn page(&mut self, forward: bool) {
        if self.focused_id() == RowId::Model {
            self.session.jump_provider(forward);
            self.refresh_caps();
        } else {
            self.move_focus(if forward { 8 } else { -8 });
        }
    }

    /// The Generation tab tracks the model the session will end up on.
    fn refresh_caps(&mut self) {
        self.generation.set_caps(self.session.final_model().caps);
    }

    pub fn pending(&self) -> Vec<String> {
        let mut out = self.session.pending();
        out.extend(self.generation.pending());
        out.extend(self.tools.pending());
        out.extend(self.aux.pending());
        out
    }

    /// Changes the dialog shows but can't send, for the summary line.
    pub fn notes(&self) -> Vec<String> {
        self.generation
            .overrides()
            .1
            .into_iter()
            .map(|f| {
                format!(
                    "{}: no catalog default to reset to, left unchanged",
                    f.label()
                )
            })
            .collect()
    }

    /// `Enter`: the plan of every pending change, or close when there is none.
    pub fn confirm(&mut self) -> Confirm {
        let plan = self.plan();
        if plan.is_empty() {
            Confirm::Close
        } else {
            Confirm::Apply(plan)
        }
    }

    fn plan(&self) -> Vec<ApplyStep> {
        let agent = self.session.final_agent().to_string();
        let keep = |i: usize| self.persist[i].then(|| agent.clone());
        let mut plan = Vec::new();
        if let Some((provider, model)) = self.session.model_change() {
            plan.push(ApplyStep::Model {
                provider,
                model,
                persist_for: keep(0),
            });
        }
        if let Some(mode) = self.session.mode_change() {
            plan.push(ApplyStep::PermMode { mode });
        }
        let (overrides, _) = self.generation.overrides();
        if overrides != GenerationParams::default() {
            plan.push(ApplyStep::Generation {
                overrides,
                persist_for: keep(1),
            });
        }
        plan.extend(self.tools.overlay_step());
        plan.extend(
            self.aux
                .changes()
                .into_iter()
                .map(|(purpose, provider, model)| ApplyStep::Aux {
                    purpose,
                    provider,
                    model,
                }),
        );
        plan
    }
}
