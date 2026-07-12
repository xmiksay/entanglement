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

use super::{App, ProfileInfo, HISTORY_CAPACITY};

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
        )
    }

    /// `entry_profiles` are the registry-driven entry agents (`mode ∈
    /// {primary, all}`, #119) the `/agent` picker and Tab-cycle offer — a
    /// `subagent` leaf like `explore` is never a manual entry agent. The caller
    /// (the runtime head) filters and orders them from the loaded
    /// `ProfileRegistry`.
    pub fn new(
        initial_session: SessionId,
        catalog: Catalog,
        entry_profiles: Vec<ProfileInfo>,
    ) -> Self {
        // Fall back to `build` if a custom registry somehow exposed no entry
        // agent, so the picker/cycle is never empty (it indexes unconditionally).
        let available_profiles = if entry_profiles.is_empty() {
            vec![ProfileInfo {
                name: "build".to_string(),
                description: "Coding agent".to_string(),
            }]
        } else {
            entry_profiles
        };

        let primary_profile_order: Vec<String> =
            available_profiles.iter().map(|p| p.name.clone()).collect();

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
            primary_profile_order,
            showing_model_picker: false,
            model_picker_state,
            available_models,
            model_info: ModelInfo {
                id: "dummy".to_string(),
                display_name: "dummy".to_string(),
                context_window: None,
            },
            leader_handler: LeaderKeyHandler::new(),
            showing_help: false,
            command_palette: CommandPalette::new(),
            sidebar_visible: false,
            sidebar_width: 24,
            theme: Theme::default(),
            profile_colors: HashMap::new(),
            thinking_since: None,
            input_tokens: 0,
            output_tokens: 0,
            input_multiline: false,
            help_text: "Enter: send | Shift+Enter: newline | Ctrl+A: agent picker | Ctrl+L: sessions | Ctrl+P: command palette | ?: help".to_string(),
            showing_resume_modal: false,
            resume_state,
            available_sessions,
            chat_area: Rect::default(),
            chat_scroll_offset: 0,
            chat_line_blocks: Vec::new(),
            pending_effect: None,
            inspect: Default::default(),
            root: PathBuf::from("."),
            bash_enabled: false,
            mention: MentionPopup::new(FileIndex::default()),
        }
    }
}
