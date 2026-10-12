//! The live side effects of the `/set` dialog's plan. Each step goes through
//! the path its single-purpose command already uses: `SetModel`/
//! `SetGeneration` with the ADR-0081/0095 persist-on-confirmation pendings,
//! `/enable`'s `SetToolOverlay`, and `/aux-model`'s store write. The agent row is
//! read-only display now (ADR-0207 §9) — there is no `ApplyStep::Agent` any
//! more.

use entanglement_core::{Holly, InMsg};

use super::App;
use crate::tui::settings_dialog::{ApplyStep, SettingsEffects, ToolsChange};

pub(super) struct LiveEffects<'a> {
    pub app: &'a mut App,
    pub holly: &'a Holly,
}

impl LiveEffects<'_> {
    async fn send(&self, msg: InMsg) -> Result<(), String> {
        self.holly
            .send(msg)
            .await
            .map_err(|_| "the engine is no longer accepting messages".to_string())
    }
}

impl SettingsEffects for LiveEffects<'_> {
    async fn apply(&mut self, step: &ApplyStep) -> Result<(), String> {
        let session = self.app.active_session_id().clone();
        match step {
            ApplyStep::Model {
                provider,
                model,
                persist_for,
            } => {
                // Named explicitly, since `persist_for` is the *agent* the
                // pin is saved for — the session's own agent (`self.app.agent()`)
                // is fixed for its whole life (ADR-0207 §9) and would be a
                // pointless roundabout way to say the same thing here.
                if let Some(agent) = persist_for {
                    self.app.pending_model_persist =
                        Some((agent.clone(), provider.clone(), model.clone()));
                }
                let (provider, model) = (provider.clone(), model.clone());
                self.send(InMsg::SetModel {
                    session,
                    provider,
                    model,
                })
                .await
            }
            ApplyStep::PermMode { mode } => {
                let mode = mode.clone();
                self.send(InMsg::SetMode { session, mode }).await
            }
            ApplyStep::Generation {
                overrides,
                persist_for,
            } => {
                if let Some(agent) = persist_for {
                    self.app.pending_generation_persist = Some((agent.clone(), *overrides));
                }
                let overrides = *overrides;
                self.send(InMsg::SetGeneration { session, overrides }).await
            }
            ApplyStep::Tools(ToolsChange { entries }) => {
                let entries = entries.clone();
                self.send(InMsg::SetToolOverlay { session, entries }).await
            }
            ApplyStep::Aux {
                purpose,
                provider,
                model,
            } => {
                let store = self.app.aux_models.clone().ok_or_else(|| {
                    "no config directory for the managed aux-models file".to_string()
                })?;
                let mut store = store
                    .lock()
                    .map_err(|_| "the aux-models store lock is poisoned".to_string())?;
                store
                    .set(*purpose, provider, model)
                    .map_err(|e| format!("persist failed: {e:#}"))
            }
        }
    }
}
