//! The dialog's row model: [`RowId`]/[`RowView`] and the per-tab row lists
//! that render, keys and mouse all read. Split out of
//! `settings_dialog.rs` (#745).

use super::{aux, GenField, SettingsDialog, Tab, AUX_PERSIST_REASON};
use crate::config::aux_models::Purpose;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowId {
    Persist,
    Model,
    /// Permission mode (#560 P12, ADR-0207 §12), Session tab.
    PermMode,
    Gen(GenField),
    Tool(usize),
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

pub(super) fn row(id: RowId, label: &str, value: String) -> RowView {
    RowView {
        id,
        label: label.to_string(),
        value,
        changed: false,
        disabled: None,
    }
}

pub(super) fn note(text: &str) -> RowView {
    row(RowId::Note, text, String::new())
}

pub(super) fn checkbox(on: bool) -> &'static str {
    if on {
        "[x]"
    } else {
        "[ ]"
    }
}

impl SettingsDialog {
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
                // Read-only (ADR-0207 §9): the agent is fixed for the
                // session's whole life, so this is display, not a `RowId` —
                // never focusable, never part of the plan.
                note(&format!("agent: {agent} (fixed for this session)")),
                RowView {
                    changed: self.session.model_change().is_some(),
                    ..row(RowId::Model, "model", self.session.model_label())
                },
                note("PgUp/PgDn: jump provider"),
                RowView {
                    changed: self.session.mode_change().is_some(),
                    ..row(
                        RowId::PermMode,
                        "mode",
                        self.session.final_mode().to_string(),
                    )
                },
            ],
            Tab::Generation => self.generation_rows(agent),
            Tab::Tools => self.tools_rows(),
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
        let model = self.session.final_model();
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

    pub(super) fn tools_rows(&self) -> Vec<RowView> {
        let t = &self.tools;
        let mut rows = vec![note(
            "── tools this session · Space: toggle, a: auto-allow ──",
        )];
        for (i, r) in t.rows().iter().enumerate() {
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
        rows
    }
}
