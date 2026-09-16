//! The live side effects of the `/set` dialog's plan. Each step goes through
//! the path its single-purpose command already uses: `SetAgent`/`SetModel`/
//! `SetGeneration` with the ADR-0081/0095 persist-on-confirmation pendings,
//! `/enable`'s lazy MCP connect + `SetToolOverlay`, the ADR-0083 allowlist
//! materializer, the shared advertising state's live re-pin, and
//! `/aux-model`'s store write.

use entanglement_core::{Holly, InMsg, SessionId};

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

    async fn apply_tools(
        &mut self,
        session: SessionId,
        change: &ToolsChange,
    ) -> Result<(), String> {
        for server in &change.enable_servers {
            let pattern = format!("mcp__{server}__*");
            crate::tui::enable_command::lazy_enable_available(self.app, &pattern).await?;
        }
        if let Some(handles) = self.app.mcp_handles() {
            for server in &change.disable_servers {
                handles.avail.mark_disabled(server, &session);
            }
        }
        if let Some(entries) = &change.entries {
            let entries = entries.clone();
            self.send(InMsg::SetToolOverlay { session, entries })
                .await?;
        }
        if let Some((agent, allowlist)) = &change.persist {
            crate::agents::save_tools_override(&self.app.root, agent, allowlist.as_deref())
                .map_err(|e| format!("saving the allowlist for '{agent}': {e:#}"))?;
        }
        Ok(())
    }
}

impl SettingsEffects for LiveEffects<'_> {
    async fn apply(&mut self, step: &ApplyStep) -> Result<(), String> {
        let session = self.app.active_session_id().clone();
        match step {
            ApplyStep::Agent(agent) => {
                let agent = agent.clone();
                self.send(InMsg::SetAgent { session, agent }).await
            }
            ApplyStep::Model {
                provider,
                model,
                persist_for,
            } => {
                // Named explicitly: the view's agent still shows the old
                // profile until the `AgentChanged` for a same-plan switch lands.
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
            ApplyStep::Tools(change) => self.apply_tools(session, change).await,
            ApplyStep::Repin { mode, discovery } => {
                let adv =
                    self.app.advertising.clone().ok_or_else(|| {
                        "advertising state isn't wired into this head".to_string()
                    })?;
                adv.repin(&session, *mode, *discovery);
                Ok(())
            }
            ApplyStep::PersistAdvertising {
                mode,
                provider,
                discovery,
            } => crate::config::write_key::save_advertising_defaults(
                Some(*mode),
                provider,
                *discovery,
            )
            .map_err(|e| format!("persist failed: {e:#}")),
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
