//! Bare `/set`'s tabbed session-settings dialog: Session (agent + model),
//! Generation (only the knobs the selected model accepts), Tools (overlay,
//! MCP servers, advertising re-pin) and Aux (per-purpose pins). Pure state —
//! `app/settings.rs` seeds it and runs the confirmed plan, `modals/settings.rs`
//! draws it, `settings_events.rs` routes keys and clicks.
//!
//! Every row of the active tab is a [`RowView`]; navigation moves a focus over
//! the focusable ones and `←/→/Space` act on the focused row, so render, keys
//! and mouse all read one row list and can't disagree.

mod apply;
mod aux;
mod generation;
mod session;
mod tools;

#[cfg(test)]
mod tests;

pub use apply::{run_plan, ApplyStep, SettingsEffects, ToolsChange};
pub use aux::AuxTab;
pub use generation::GenField;
pub use session::{model_options, SessionTab};
pub use tools::{discovery_label, server_of, AdvertisingRows, ToolsTab};

use entanglement_provider::GenerationParams;
use ratatui::widgets::ListState;

use crate::config::aux_models::Purpose;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Editing,
    /// The second step a pending advertising/discovery re-pin requires.
    ConfirmRepin,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowId {
    Persist,
    Agent,
    Model,
    Gen(GenField),
    Tool(usize),
    Mode,
    Discovery,
    /// "Save mode/discovery as default" — its own flag, since the Tools tab's
    /// other persist checkbox saves the overlay as an agent allowlist.
    AdvertisingPersist,
    Aux(Purpose),
    /// Headers, hints and disabled read-only lines — never focusable.
    Note,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RowView {
    pub id: RowId,
    pub label: String,
    pub value: String,
    pub changed: bool,
    pub disabled: Option<&'static str>,
}

fn row(id: RowId, label: &str, value: String) -> RowView {
    RowView {
        id,
        label: label.to_string(),
        value,
        changed: false,
        disabled: None,
    }
}

fn note(text: &str) -> RowView {
    row(RowId::Note, text, String::new())
}

fn checkbox(on: bool) -> &'static str {
    if on {
        "[x]"
    } else {
        "[ ]"
    }
}

pub enum Confirm {
    /// Nothing pending — just close.
    Close,
    NeedsRepinConfirmation,
    Apply(Vec<ApplyStep>),
}

pub const AUX_PERSIST_REASON: &str = "aux pins are process-wide and always persisted";
pub const ADVERTISING_PERSIST_REASON: &str = "advertising state isn't wired into this head";

pub struct SettingsDialog {
    tab: Tab,
    stage: Stage,
    /// Focus per tab, as an index into that tab's focusable rows.
    focus: [usize; 4],
    list: ListState,
    /// "Save as default" for Session, Generation, Tools; Aux is always on.
    persist: [bool; 3],
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
        let caps = session.final_model().0.caps;
        Self {
            tab: Tab::Session,
            stage: Stage::Editing,
            focus: [0; 4],
            list: ListState::default(),
            persist: [false; 3],
            session,
            generation: GenerationTab::new(current, caps),
            tools,
            aux,
        }
    }

    pub fn tab(&self) -> Tab {
        self.tab
    }

    pub fn stage(&self) -> Stage {
        self.stage
    }

    pub fn list_state(&mut self) -> &mut ListState {
        &mut self.list
    }

    pub fn set_tab(&mut self, tab: Tab) {
        self.tab = tab;
        self.stage = Stage::Editing;
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
            other => self.persist[other.index()],
        }
    }

    pub fn rows(&self) -> Vec<RowView> {
        let agent = self.session.final_agent();
        match self.tab {
            Tab::Session => vec![
                row(
                    RowId::Persist,
                    "save as default",
                    format!(
                        "{} model pin for agent '{agent}'",
                        checkbox(self.persist[0])
                    ),
                ),
                RowView {
                    changed: self.session.agent_change().is_some(),
                    ..row(RowId::Agent, "agent", agent.to_string())
                },
                RowView {
                    changed: self.session.model_change().is_some(),
                    ..row(RowId::Model, "model", self.session.model_label())
                },
                note("agent choice applies to this session only · PgUp/PgDn: jump provider"),
            ],
            Tab::Generation => self.generation_rows(agent),
            Tab::Tools => self.tools_rows(agent),
            Tab::Aux => {
                let mut rows = vec![RowView {
                    disabled: Some(AUX_PERSIST_REASON),
                    ..row(RowId::Persist, "save as default", "[x] always".to_string())
                }];
                rows.extend(aux::PURPOSES.iter().map(|p| RowView {
                    changed: self.aux.changed(*p),
                    ..row(RowId::Aux(*p), p.as_str(), self.aux.value_label(*p))
                }));
                rows
            }
        }
    }

    fn generation_rows(&self, agent: &str) -> Vec<RowView> {
        let (model, _) = self.session.final_model();
        let mut rows = vec![
            row(
                RowId::Persist,
                "save as default",
                format!(
                    "{} for agent '{agent}' (once the engine confirms)",
                    checkbox(self.persist[1])
                ),
            ),
            note(&format!("for model {}/{}", model.provider, model.model)),
        ];
        rows.extend(
            self.generation
                .caps()
                .visible_fields()
                .into_iter()
                .map(|f| RowView {
                    changed: self.generation.changed(f),
                    ..row(RowId::Gen(f), f.label(), self.generation.value_label(f))
                }),
        );
        rows.push(note(
            "changing effort or thinking rebuilds the prompt cache",
        ));
        if self.generation.caps().thinking_required {
            rows.push(note("this model always thinks (thinking required)"));
        }
        rows
    }

    fn tools_rows(&self, agent: &str) -> Vec<RowView> {
        let t = &self.tools;
        let mut rows = vec![
            row(
                RowId::Persist,
                "save as default",
                format!(
                    "{} overlay as agent '{agent}' allowlist (next restart)",
                    checkbox(self.persist[2])
                ),
            ),
            note("── (a) tools this session · Space: toggle, a: auto-allow ──"),
        ];
        for i in t.indices(false) {
            let r = &t.rows()[i];
            let tag = match (r.checked, r.profile_default, r.allow) {
                (true, false, true) => "  +session (allow)",
                (true, false, false) => "  +session",
                (false, true, _) => "  -session",
                _ => "",
            };
            rows.push(RowView {
                changed: t.row_changed(i),
                ..row(
                    RowId::Tool(i),
                    &r.name,
                    format!("{}{tag}", checkbox(r.checked)),
                )
            });
        }
        rows.push(note("── (b) MCP servers ──"));
        for i in t.indices(true) {
            let r = &t.rows()[i];
            let name = server_of(&r.name).unwrap_or(&r.name);
            rows.push(RowView {
                changed: t.row_changed(i),
                ..row(RowId::Tool(i), name, checkbox(r.checked).to_string())
            });
        }
        rows.push(note(
            "── (c) advertising · re-pins live, asks to confirm ──",
        ));
        rows.push(RowView {
            changed: t.adv.mode_changed(),
            disabled: t.adv.mode_disabled(),
            ..row(
                RowId::Mode,
                "advertising mode",
                t.adv.mode().label().to_string(),
            )
        });
        rows.push(RowView {
            changed: t.adv.discovery_changed(),
            disabled: t.adv.discovery_disabled(),
            ..row(
                RowId::Discovery,
                "discovery",
                discovery_label(t.adv.discovery()).to_string(),
            )
        });
        rows.push(RowView {
            changed: t.adv.persist(),
            disabled: t.adv.persist_disabled(),
            ..row(
                RowId::AdvertisingPersist,
                "save mode/discovery as default",
                format!(
                    "{} for new sessions (config.yml)",
                    checkbox(t.adv.persist())
                ),
            )
        });
        rows
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
            RowId::Persist if self.tab != Tab::Aux => {
                let i = self.tab.index();
                self.persist[i] = !self.persist[i];
            }
            RowId::Agent => {
                self.session.cycle_agent(forward);
                self.refresh_caps();
            }
            RowId::Model => {
                self.session.cycle_model(forward);
                self.refresh_caps();
            }
            RowId::Gen(f) => self.generation.cycle(f, forward),
            RowId::Tool(i) => self.tools.toggle(i),
            RowId::Mode => self.tools.adv.cycle_mode(),
            RowId::AdvertisingPersist => self.tools.adv.toggle_persist(),
            RowId::Discovery => self.tools.adv.cycle_discovery(forward),
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
        self.generation.set_caps(self.session.final_model().0.caps);
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

    /// `Enter`. A pending re-pin first moves to [`Stage::ConfirmRepin`]; only
    /// confirming there yields a plan containing it.
    pub fn confirm(&mut self) -> Confirm {
        if self.stage == Stage::Editing && self.tools.adv.repin().is_some() {
            self.stage = Stage::ConfirmRepin;
            return Confirm::NeedsRepinConfirmation;
        }
        let plan = self.plan();
        if plan.is_empty() {
            Confirm::Close
        } else {
            Confirm::Apply(plan)
        }
    }

    /// Leave the re-pin confirmation without applying anything.
    pub fn back(&mut self) {
        self.stage = Stage::Editing;
    }

    fn plan(&self) -> Vec<ApplyStep> {
        let agent = self.session.final_agent().to_string();
        let keep = |i: usize| self.persist[i].then(|| agent.clone());
        let mut plan = Vec::new();
        if let Some(a) = self.session.agent_change() {
            plan.push(ApplyStep::Agent(a));
        }
        if let Some((provider, model)) = self.session.model_change() {
            plan.push(ApplyStep::Model {
                provider,
                model,
                persist_for: keep(0),
            });
        }
        let (overrides, _) = self.generation.overrides();
        if overrides != GenerationParams::default() {
            plan.push(ApplyStep::Generation {
                overrides,
                persist_for: keep(1),
            });
        }
        plan.extend(self.tools.overlay_step(keep(2)));
        plan.extend(self.tools.adv.repin());
        plan.extend(self.tools.adv.persist_step());
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
