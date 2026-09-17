//! `App` glue for bare `/set`'s settings dialog (`tui::settings_dialog`):
//! seeding it from the live session + managed stores, and running the
//! confirmed plan through [`LiveEffects`][super::settings_apply::LiveEffects].

use entanglement_core::{Holly, SessionId};
use entanglement_provider::{Catalog, Discovery, ToolAdvertising};
use ratatui::layout::Rect;

use super::settings_apply::LiveEffects;
use super::App;
use crate::config::aux_models::Purpose;
use crate::tool_advertising::Encoding;
use crate::tui::settings_dialog::{
    mode_names, model_options, run_plan, AdvertisingRows, AuxTab, Confirm, SessionTab,
    SettingsDialog, ToolsTab,
};

/// The dialog (`None` = closed, so cancelling discards by construction), the
/// catalog its model rows are built from, and its click geometry.
pub struct SettingsState {
    dialog: Option<SettingsDialog>,
    catalog: Catalog,
    tabs_rect: Rect,
    body_rect: Rect,
}

impl SettingsState {
    pub fn new(catalog: Catalog) -> Self {
        Self {
            dialog: None,
            catalog,
            tabs_rect: Rect::default(),
            body_rect: Rect::default(),
        }
    }
}

impl App {
    pub fn showing_settings_dialog(&self) -> bool {
        self.settings.dialog.is_some()
    }

    pub fn settings_dialog(&self) -> Option<&SettingsDialog> {
        self.settings.dialog.as_ref()
    }

    pub fn settings_dialog_mut(&mut self) -> Option<&mut SettingsDialog> {
        self.settings.dialog.as_mut()
    }

    pub fn close_settings_dialog(&mut self) {
        self.settings.dialog = None;
        self.set_settings_rects(Rect::default(), Rect::default());
        self.mark_dirty();
    }

    pub fn set_settings_rects(&mut self, tabs: Rect, body: Rect) {
        self.settings.tabs_rect = tabs;
        self.settings.body_rect = body;
    }

    pub fn settings_rects(&self) -> (Rect, Rect) {
        (self.settings.tabs_rect, self.settings.body_rect)
    }

    /// Open the dialog seeded from the active session: its agent and model,
    /// its last confirmed generation params (else the agent's persisted
    /// override), its effective tool availability, its pinned advertising
    /// facts, and the aux pins.
    pub fn open_settings_dialog(&mut self) {
        let session_id = self.active_session_id().clone();
        let agent = self.agent().to_string();
        let models = model_options(&self.settings.catalog);
        let aux_models = models
            .iter()
            .map(|m| (m.provider.clone(), m.model.clone()))
            .collect();
        let session = SessionTab::new(
            agent.clone(),
            models,
            (&self.active_provider, &self.model_info.id),
            mode_names(),
            self.mode(),
        );
        let current = self
            .sessions
            .active_view()
            .cost()
            .generation()
            .or_else(|| {
                let store = self.agent_generation.as_ref()?;
                store.lock().ok()?.get(&agent)
            })
            .unwrap_or_default();
        let tools = ToolsTab::new(
            self.session_tool_rows(&self.roster_server_patterns()),
            self.advertising_rows(&session_id),
        );
        let aux = AuxTab::new(aux_models, &self.aux_pins());
        self.settings.dialog = Some(SettingsDialog::new(session, current, tools, aux));
        self.mark_dirty();
    }

    /// `Enter` on the dialog: advance to the re-pin confirmation, close when
    /// nothing is pending, or run the plan and record one summary line.
    pub async fn confirm_settings_dialog(&mut self, holly: &Holly) {
        let Some(dialog) = self.settings.dialog.as_mut() else {
            return;
        };
        match dialog.confirm() {
            Confirm::Close => self.close_settings_dialog(),
            Confirm::NeedsRepinConfirmation => self.mark_dirty(),
            Confirm::Apply(plan) => {
                let notes = dialog.notes();
                self.close_settings_dialog();
                let report = run_plan(&plan, &mut LiveEffects { app: self, holly }).await;
                self.sessions
                    .active_view_mut()
                    .record_status("settings", report.summary(&notes));
                self.mark_dirty();
            }
        }
    }

    /// One whole-server row per MCP server whose tools are in the roster —
    /// `allowed` servers already get theirs from `session_tool_rows`.
    fn roster_server_patterns(&self) -> Vec<String> {
        let mut servers: Vec<String> = self
            .tool_roster
            .iter()
            .filter_map(|name| {
                let (server, _) = name.strip_prefix("mcp__")?.split_once("__")?;
                Some(format!("mcp__{server}__*"))
            })
            .collect();
        servers.sort();
        servers.dedup();
        servers
    }

    fn advertising_rows(&self, session: &SessionId) -> AdvertisingRows {
        match &self.advertising {
            Some(adv) => {
                // `session_*` include a re-pin still pending the first resolution.
                let mode = adv
                    .session_mode(session)
                    .unwrap_or_else(|| adv.mode(session));
                let discovery = adv.session_discovery(session).unwrap_or_default();
                let client_side = adv.encoding(session) == Encoding::ClientSide;
                AdvertisingRows::new(
                    true,
                    client_side,
                    mode,
                    discovery,
                    self.active_provider.clone(),
                )
            }
            None => AdvertisingRows::new(
                false,
                false,
                ToolAdvertising::default(),
                Discovery::default(),
                String::new(),
            ),
        }
    }

    fn aux_pins(&self) -> Vec<(Purpose, Option<(String, String)>)> {
        let Some(Ok(store)) = self.aux_models.as_ref().map(|s| s.lock()) else {
            return Vec::new();
        };
        [Purpose::Summarize, Purpose::SessionTitle, Purpose::Narrate]
            .into_iter()
            .map(|p| (p, store.get(p).map(|(a, b)| (a.to_string(), b.to_string()))))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::settings_dialog::{RowId, Tab};
    use entanglement_core::{EngineConfig, InMsg};

    fn focus(app: &mut App, id: RowId) {
        let d = app.settings_dialog_mut().expect("dialog open");
        let index = d
            .rows()
            .iter()
            .position(|r| r.id == id)
            .expect("row present");
        d.focus_row(index);
    }

    #[test]
    fn open_seeds_the_active_agent_and_model() {
        let mut app = App::new_for_test(SessionId::new("s1"));
        app.set_active_provider("zai".to_string());
        app.set_model_info(entanglement_provider::ModelInfo {
            id: "glm-5.3".to_string(),
            display_name: "GLM-5.3".to_string(),
            context_window: None,
        });
        app.open_settings_dialog();
        let d = app.settings_dialog().expect("dialog open");
        assert_eq!(d.tab(), Tab::Session);
        let rows = d.rows();
        // The agent row is read-only display now (ADR-0207 §9): a `Note`
        // naming the fixed agent, not a focusable `RowId`.
        assert!(
            rows.iter()
                .any(|r| r.id == RowId::Note && r.label.contains("agent: build")),
            "{rows:?}"
        );
        let value = |id| rows.iter().find(|r| r.id == id).map(|r| r.value.clone());
        assert_eq!(value(RowId::Model).as_deref(), Some("zai/glm-5.3"));
        // #560 P12, ADR-0207 §12: the mode row seeds from the session's
        // current mode ("build", `App::new_for_test`'s default).
        assert_eq!(value(RowId::PermMode).as_deref(), Some("build"));
        assert!(d.pending().is_empty());
    }

    #[test]
    fn cancel_discards_and_reopen_starts_fresh() {
        let mut app = App::new_for_test(SessionId::new("s1"));
        app.open_settings_dialog();
        focus(&mut app, RowId::Model);
        app.settings_dialog_mut().unwrap().activate(true);
        assert_eq!(app.settings_dialog().unwrap().pending().len(), 1);
        app.close_settings_dialog();
        assert!(!app.showing_settings_dialog());
        app.open_settings_dialog();
        assert!(app.settings_dialog().unwrap().pending().is_empty());
    }

    #[tokio::test]
    async fn confirm_sends_the_model_switch_and_records_one_summary() {
        let holly = Holly::spawn(EngineConfig::default());
        let mut inbound = holly.subscribe_inbound();
        let mut app = App::new_for_test(SessionId::new("s1"));
        app.open_settings_dialog();
        focus(&mut app, RowId::Model);
        app.settings_dialog_mut().unwrap().activate(true);
        let pending = app.settings_dialog().unwrap().pending();
        assert_eq!(pending.len(), 1, "{pending:?}");
        app.confirm_settings_dialog(&holly).await;

        assert!(!app.showing_settings_dialog());
        let msg = tokio::time::timeout(std::time::Duration::from_millis(500), inbound.recv())
            .await
            .expect("an inbound message")
            .expect("channel open");
        assert!(matches!(msg, InMsg::SetModel { .. }), "{msg:?}");
        let summaries = app
            .transcript()
            .iter()
            .filter(|e| format!("{e:?}").contains("applied: model → "))
            .count();
        assert_eq!(summaries, 1);
    }

    /// #560 P12, ADR-0207 §12: cycling the Session tab's mode row and
    /// confirming sends a live `InMsg::SetMode` for the active session —
    /// the natural slot the read-only agent row's retirement (ADR-0207 §9)
    /// left, unlike the Tools tab's unrelated tool-advertising `RowId::Mode`.
    #[tokio::test]
    async fn confirm_sends_the_mode_switch_and_records_one_summary() {
        let holly = Holly::spawn(EngineConfig::default());
        let mut inbound = holly.subscribe_inbound();
        let mut app = App::new_for_test(SessionId::new("s1"));
        app.open_settings_dialog();
        focus(&mut app, RowId::PermMode);
        app.settings_dialog_mut().unwrap().activate(true);
        let pending = app.settings_dialog().unwrap().pending();
        assert_eq!(pending.len(), 1, "{pending:?}");
        assert!(pending[0].starts_with("mode → "), "{pending:?}");
        app.confirm_settings_dialog(&holly).await;

        assert!(!app.showing_settings_dialog());
        let msg = tokio::time::timeout(std::time::Duration::from_millis(500), inbound.recv())
            .await
            .expect("an inbound message")
            .expect("channel open");
        assert!(matches!(msg, InMsg::SetMode { .. }), "{msg:?}");
        let summaries = app
            .transcript()
            .iter()
            .filter(|e| format!("{e:?}").contains("applied: mode → "))
            .count();
        assert_eq!(summaries, 1);
    }
}
