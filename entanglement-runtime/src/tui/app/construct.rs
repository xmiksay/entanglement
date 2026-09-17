use entanglement_provider::{Catalog, ModelInfo};
use ratatui::widgets::ListState;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;

use crate::tui::commands::CommandPalette;
use crate::tui::input::SimpleInput;
use crate::tui::keybindings::LeaderKeyHandler;
use crate::tui::markdown::MarkdownRenderer;
use crate::tui::mention::{FileIndex, MentionPopup};
use crate::tui::sessions::SessionRegistry;
use crate::tui::theme::Theme;
use entanglement_core::SessionId;
use ratatui::layout::Rect;

use super::{App, ModalClickAreas, ProfileInfo, HISTORY_CAPACITY};

impl App {
    /// Test constructor: builds an `App` over the embedded default catalog with a
    /// hardcoded primary-profile roster.
    #[cfg(test)]
    pub(crate) fn new_for_test(initial_session: SessionId) -> Self {
        Self::new(
            initial_session,
            Catalog::builtin(),
            vec![
                ProfileInfo {
                    name: "build".to_string(),
                    description: "Coding agent".to_string(),
                },
                ProfileInfo {
                    name: "plan".to_string(),
                    description: "Planning agent".to_string(),
                },
            ],
            vec![
                "read".to_string(),
                "grep".to_string(),
                "glob".to_string(),
                "edit".to_string(),
                "write".to_string(),
                "bash".to_string(),
            ],
        )
    }

    /// `entry_profiles` are every registered agent (ADR-0207 §4 retires the
    /// old `mode ∈ {primary, all}` filter — any agent may be a session root)
    /// the (read-only, ADR-0207 §9) `/agent` picker lists, in the order the
    /// caller (the runtime head) loaded them from the `ProfileRegistry`.
    /// `tool_roster` is the full advertised tool-name roster (#330) `/tools`
    /// and the bare `/enable` checklist offer.
    pub fn new(
        initial_session: SessionId,
        catalog: Catalog,
        entry_profiles: Vec<ProfileInfo>,
        tool_roster: Vec<String>,
    ) -> Self {
        // Fall back to `general` if a custom registry somehow exposed no entry
        // agent, so the picker is never empty (it indexes unconditionally).
        let available_profiles = if entry_profiles.is_empty() {
            vec![ProfileInfo {
                name: "general".to_string(),
                description: "Coding agent".to_string(),
            }]
        } else {
            entry_profiles
        };

        let mut profile_picker_state = ListState::default();
        profile_picker_state.select(Some(0));

        // Model picker groups: one (provider, [model ids]) pair per catalog
        // provider, in catalog order.
        let available_models: Vec<(String, Vec<String>)> = catalog
            .providers
            .iter()
            .map(|p| {
                (
                    p.name.clone(),
                    p.models.iter().map(|m| m.id.clone()).collect(),
                )
            })
            .collect();

        let mut model_picker_state = ListState::default();
        model_picker_state.select(Some(0));

        // `/key` dialog offers only *keyed* providers (a keyless Ollama has no key
        // env to set), carrying each provider's key env var (#304).
        let key_providers: Vec<crate::tui::key_dialog::KeyProvider> = catalog
            .providers
            .iter()
            .filter_map(|p| {
                p.key_env
                    .clone()
                    .map(|key_env| crate::tui::key_dialog::KeyProvider {
                        name: p.name.clone(),
                        key_env,
                    })
            })
            .collect();

        let mut resume_state = ListState::default();
        resume_state.select(Some(0));
        let available_sessions = Vec::new();

        Self {
            sessions: SessionRegistry::new(initial_session),
            dirty: true,
            markdown_renderer: MarkdownRenderer::new(),
            input: SimpleInput::default(),
            history: VecDeque::with_capacity(HISTORY_CAPACITY),
            history_index: None,
            history_search_term: None,
            showing_profile_picker: false,
            profile_picker_state,
            available_profiles,
            showing_model_picker: false,
            model_picker_state,
            available_models,
            agent_models: None,
            pending_model_persist: None,
            agent_generation: None,
            pending_generation_persist: None,
            aux_models: None,
            http_client: None,
            grants: None,
            configured_editor: None,
            key_dialog: crate::tui::key_dialog::KeyDialog::new(key_providers),
            mcp_panel: crate::tui::mcp_panel::McpPanel::default(),
            mcp_handles: None,
            tool_overlays: HashMap::new(),
            session_tools_dialog: crate::tui::session_tools_dialog::SessionToolsDialog::new(),
            tool_roster,
            advertising: None,
            tools_view: crate::tui::tools_view::ToolsView::new(),
            model_info: ModelInfo {
                id: "dummy".to_string(),
                display_name: "dummy".to_string(),
                context_window: None,
            },
            active_provider: String::new(),
            leader_handler: LeaderKeyHandler::new(),
            showing_help: false,
            help_scroll: 0,
            mcp_scroll: 0,
            command_palette: CommandPalette::new(),
            sidebar_visible: true,
            sidebar_width: 0,
            theme: Theme::default(),
            profile_colors: HashMap::new(),
            thinking_since: None,
            input_multiline: false,
            showing_resume_modal: false,
            resume_state,
            available_sessions,
            chat_area: Rect::default(),
            chat_scroll_offset: 0,
            chat_line_blocks: Vec::new(),
            chat_line_text: Vec::new(),
            selection: None,
            sidebar_sessions_area: Rect::default(),
            sidebar_rows: Vec::new(),
            attention_area: Rect::default(),
            modal_click: ModalClickAreas::default(),
            input_area: Rect::default(),
            pending_effect: None,
            compaction_seeds: HashMap::new(),
            inspect: Default::default(),
            approval_pager: Default::default(),
            root: PathBuf::from("."),
            mention: MentionPopup::new(FileIndex::default()),
            slash: crate::tui::slash_popup::SlashPopup::new(),
            quit_pending: false,
            quit_pending_at: None,
            toast: None,
            settings: super::settings::SettingsState::new(catalog),
        }
    }
}
