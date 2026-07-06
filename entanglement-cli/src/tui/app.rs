use entanglement_core::{AgentState, OutEvent, SessionId, TaskItem};
use ratatui::widgets::ListState;
use std::collections::VecDeque;
use tui_textarea::{CursorMove, TextArea};

use crate::tui::commands::{Command, CommandPalette};
use crate::tui::keybindings::{Action, LeaderKeyHandler};
use crate::tui::session_view::{ApprovalMode, TranscriptEntry};
use crate::tui::sessions::SessionRegistry;

const HISTORY_CAPACITY: usize = 100;

#[derive(Clone)]
pub struct ModelInfo {
    pub provider: String,
    pub model: String,
}

#[derive(Clone)]
pub struct ProfileInfo {
    pub name: String,
    pub description: String,
}

pub struct App {
    sessions: SessionRegistry,
    dirty: bool,

    // Input state — one keyboard shared across sessions (shell-history semantics).
    input: TextArea<'static>,
    history: VecDeque<String>,
    history_index: Option<usize>,
    history_search_term: Option<String>,

    // Profile picker state — catalog is global, selection acts on the active session.
    showing_profile_picker: bool,
    profile_picker_state: ListState,
    available_profiles: Vec<ProfileInfo>,
    primary_profile_order: Vec<String>,

    // Model picker state — catalog is global, selection is display-only (requires restart)
    showing_model_picker: bool,
    model_picker_state: ListState,
    available_models: Vec<(String, Vec<String>)>,
    model_info: ModelInfo,

    // Leader key state
    leader_handler: LeaderKeyHandler,
    showing_help: bool,

    // Command palette state
    command_palette: CommandPalette,

    // Sidebar state
    sidebar_visible: bool,
    sidebar_width: u16,
}

impl App {
    pub fn new(initial_session: SessionId) -> Self {
        let mut input = TextArea::default();
        input.set_placeholder_text("Type a message... (Enter to send, Shift+Enter for newline)");

        let available_profiles = vec![
            ProfileInfo {
                name: "build".to_string(),
                description: "Coding agent - implements changes using available tools".to_string(),
            },
            ProfileInfo {
                name: "plan".to_string(),
                description: "Planning agent - produces plans without making changes".to_string(),
            },
            ProfileInfo {
                name: "explore".to_string(),
                description: "Read-only exploration agent - answers questions about codebase"
                    .to_string(),
            },
        ];

        let primary_profile_order = vec![
            "build".to_string(),
            "plan".to_string(),
            "explore".to_string(),
        ];

        let mut profile_picker_state = ListState::default();
        profile_picker_state.select(Some(0));

        let available_models = vec![
            (
                "zai".to_string(),
                vec!["glm-5.2".to_string(), "glm-4.7".to_string()],
            ),
            (
                "openai".to_string(),
                vec![
                    "gpt-4o".to_string(),
                    "gpt-4-turbo".to_string(),
                    "gpt-3.5-turbo".to_string(),
                ],
            ),
            (
                "ollama".to_string(),
                vec![
                    "llama3.1".to_string(),
                    "llama3".to_string(),
                    "mistral".to_string(),
                ],
            ),
            (
                "anthropic".to_string(),
                vec![
                    "claude-sonnet-4-5".to_string(),
                    "claude-3-5-sonnet-20241022".to_string(),
                ],
            ),
        ];

        let mut model_picker_state = ListState::default();
        model_picker_state.select(Some(0));

        Self {
            sessions: SessionRegistry::new(initial_session),
            dirty: true,
            input,
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
                provider: "dummy".to_string(),
                model: "dummy".to_string(),
            },
            leader_handler: LeaderKeyHandler::new(),
            showing_help: false,
            command_palette: CommandPalette::new(),
            sidebar_visible: false,
            sidebar_width: 24,
        }
    }

    pub fn active_session_id(&self) -> &SessionId {
        self.sessions.active_id()
    }

    pub fn create_session(&mut self) -> SessionId {
        let id = self.sessions.create();
        self.mark_dirty();
        id
    }

    pub fn sessions(&self) -> Vec<(&SessionId, &crate::tui::session_view::SessionView)> {
        self.sessions.all()
    }

    pub fn clear_dirty(&mut self) {
        self.dirty = false;
    }

    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    pub fn agent(&self) -> &str {
        self.sessions.active_view().agent()
    }

    pub fn state(&self) -> AgentState {
        self.sessions.active_view().state()
    }

    pub fn transcript(&self) -> &[TranscriptEntry] {
        self.sessions.active_view().transcript()
    }

    pub fn plan(&self) -> Option<&String> {
        self.sessions.active_view().plan()
    }

    pub fn task_list(&self) -> Option<&[TaskItem]> {
        self.sessions.active_view().task_list()
    }

    pub fn scroll_offset(&self) -> usize {
        self.sessions.active_view().scroll_offset()
    }

    pub fn scroll_down(&mut self, lines: usize) {
        self.sessions.active_view_mut().scroll_down(lines);
        self.mark_dirty();
    }

    pub fn scroll_up(&mut self, lines: usize) {
        self.sessions.active_view_mut().scroll_up(lines);
        self.mark_dirty();
    }

    pub fn scroll_to_bottom(&mut self) {
        self.sessions.active_view_mut().scroll_to_bottom();
        self.mark_dirty();
    }

    pub fn input(&mut self) -> &mut TextArea<'static> {
        &mut self.input
    }

    pub fn history_index(&self) -> Option<usize> {
        self.history_index
    }

    pub fn take_input_text(&mut self) -> String {
        let text = self.input.lines().join("\n");
        if !text.is_empty() {
            self.history.push_back(text.clone());
            if self.history.len() > HISTORY_CAPACITY {
                self.history.pop_front();
            }
            self.history_index = None;
            self.history_search_term = None;
        }
        self.input = TextArea::default();
        self.input
            .set_placeholder_text("Type a message... (Enter to send, Shift+Enter for newline)");
        self.mark_dirty();
        text
    }

    pub fn history_up(&mut self) {
        if self.history.is_empty() {
            return;
        }

        let current_text = self.input.lines().join("\n");

        if self.history_index.is_none() {
            if !current_text.is_empty() {
                self.history_search_term = Some(current_text);
            }
            self.history_index = Some(self.history.len().saturating_sub(1));
        } else if let Some(idx) = self.history_index {
            if idx > 0 {
                self.history_index = Some(idx - 1);
            }
        }

        if let Some(idx) = self.history_index {
            if let Some(text) = self.history.get(idx) {
                self.input = TextArea::from(text.lines().collect::<Vec<&str>>());
                self.mark_dirty();
            }
        }
    }

    pub fn history_down(&mut self) {
        if self.history.is_empty() {
            return;
        }

        if let Some(idx) = self.history_index {
            if idx < self.history.len().saturating_sub(1) {
                self.history_index = Some(idx + 1);
                if let Some(text) = self.history.get(idx + 1) {
                    self.input = TextArea::from(text.lines().collect::<Vec<_>>());
                    self.mark_dirty();
                }
            } else {
                self.history_index = None;
                let search_term = self.history_search_term.take().unwrap_or_default();
                self.input = TextArea::from(search_term.lines().collect::<Vec<&str>>());
                self.mark_dirty();
            }
        }
    }

    pub fn handle_readline_key(&mut self, c: char) -> bool {
        match c {
            'a' => {
                self.input.move_cursor(CursorMove::Head);
                true
            }
            'e' => {
                self.input.move_cursor(CursorMove::End);
                true
            }
            'k' => {
                self.input.delete_line_by_end();
                true
            }
            'u' => {
                self.input.delete_line_by_head();
                true
            }
            'w' => {
                self.input.delete_word();
                true
            }
            _ => false,
        }
    }

    pub fn approval_mode(&self) -> &ApprovalMode {
        self.sessions.active_view().approval_mode()
    }

    pub fn pending_tool_request(&self) -> Option<&(String, String, String)> {
        self.sessions.active_view().pending_tool_request()
    }

    pub fn set_approval_mode(&mut self, mode: ApprovalMode) {
        self.sessions.active_view_mut().set_approval_mode(mode);
        self.mark_dirty();
    }

    pub fn clear_approval(&mut self) {
        self.sessions.active_view_mut().clear_approval();
        self.mark_dirty();
    }

    /// Call right after sending `InMsg::Stop` for the active session.
    pub fn note_stop_sent(&mut self) {
        self.sessions.active_view_mut().note_stop_sent();
    }

    /// Call right before sending `InMsg::Prompt` for the active session.
    pub fn note_prompt_sent(&mut self) {
        self.sessions.active_view_mut().note_prompt_sent();
    }

    pub fn handle_out_event(&mut self, event: OutEvent) {
        tracing::debug!("App handling OutEvent: {:?}", event);
        if self.sessions.handle_out_event(event) {
            self.mark_dirty();
        }
    }

    pub fn showing_profile_picker(&self) -> bool {
        self.showing_profile_picker
    }

    pub fn profile_picker_state(&mut self) -> &mut ListState {
        &mut self.profile_picker_state
    }

    pub fn available_profiles(&self) -> &[ProfileInfo] {
        &self.available_profiles
    }

    pub fn toggle_profile_picker(&mut self) {
        self.showing_profile_picker = !self.showing_profile_picker;
        if self.showing_profile_picker {
            let agent = self.sessions.active_view().agent().to_string();
            let current_index = self
                .available_profiles
                .iter()
                .position(|p| p.name == agent)
                .unwrap_or(0);
            self.profile_picker_state.select(Some(current_index));
        }
        self.mark_dirty();
    }

    pub fn close_profile_picker(&mut self) {
        self.showing_profile_picker = false;
        self.mark_dirty();
    }

    pub fn select_profile_picker(&mut self) -> Option<String> {
        if let Some(selected) = self.profile_picker_state.selected() {
            if selected < self.available_profiles.len() {
                let profile_name = self.available_profiles[selected].name.clone();
                self.showing_profile_picker = false;
                self.mark_dirty();
                return Some(profile_name);
            }
        }
        None
    }

    pub fn profile_picker_next(&mut self) {
        if let Some(selected) = self.profile_picker_state.selected() {
            let next = (selected + 1) % self.available_profiles.len();
            self.profile_picker_state.select(Some(next));
            self.mark_dirty();
        }
    }

    pub fn profile_picker_prev(&mut self) {
        if let Some(selected) = self.profile_picker_state.selected() {
            let prev = if selected == 0 {
                self.available_profiles.len() - 1
            } else {
                selected - 1
            };
            self.profile_picker_state.select(Some(prev));
            self.mark_dirty();
        }
    }

    pub fn cycle_primary_profile(&mut self) -> Option<String> {
        let current = self.sessions.active_view().agent().to_string();
        let current_index = self
            .primary_profile_order
            .iter()
            .position(|name| name == &current)
            .unwrap_or(0);
        let next_index = (current_index + 1) % self.primary_profile_order.len();
        let new_agent = self.primary_profile_order[next_index].clone();
        self.sessions.active_view_mut().set_agent(new_agent.clone());
        self.mark_dirty();
        Some(new_agent)
    }

    pub fn showing_sessions_modal(&self) -> bool {
        self.sessions.showing_modal()
    }

    pub fn toggle_sessions_modal(&mut self) {
        self.sessions.toggle_modal();
        self.mark_dirty();
    }

    pub fn close_sessions_modal(&mut self) {
        self.sessions.close_modal();
        self.mark_dirty();
    }

    pub fn sessions_modal_state(&mut self) -> &mut ListState {
        self.sessions.modal_state()
    }

    pub fn sessions_modal_next(&mut self) {
        self.sessions.modal_next();
        self.mark_dirty();
    }

    pub fn sessions_modal_prev(&mut self) {
        self.sessions.modal_prev();
        self.mark_dirty();
    }

    pub fn select_session_from_modal(&mut self) {
        self.sessions.select_from_modal();
        self.mark_dirty();
    }

    pub fn leader_handler(&mut self) -> &mut LeaderKeyHandler {
        &mut self.leader_handler
    }

    pub fn showing_help(&self) -> bool {
        self.showing_help
    }

    pub fn toggle_help(&mut self) {
        self.showing_help = !self.showing_help;
        self.mark_dirty();
    }

    pub fn close_help(&mut self) {
        self.showing_help = false;
        self.mark_dirty();
    }

    pub fn showing_command_palette(&self) -> bool {
        self.command_palette.visible()
    }

    pub fn toggle_command_palette(&mut self) {
        if self.command_palette.visible() {
            self.command_palette.hide();
        } else {
            self.command_palette.show();
        }
        self.mark_dirty();
    }

    pub fn close_command_palette(&mut self) {
        self.command_palette.hide();
        self.mark_dirty();
    }

    pub fn command_palette(&mut self) -> &mut CommandPalette {
        &mut self.command_palette
    }

    pub fn showing_sidebar(&self) -> bool {
        self.sidebar_visible
    }

    pub fn sidebar_width(&self) -> u16 {
        self.sidebar_width
    }

    pub fn toggle_sidebar(&mut self) {
        self.sidebar_visible = !self.sidebar_visible;
        self.mark_dirty();
    }

    pub fn showing_model_picker(&self) -> bool {
        self.showing_model_picker
    }

    pub fn model_picker_state(&mut self) -> &mut ListState {
        &mut self.model_picker_state
    }

    pub fn available_models(&self) -> &[(String, Vec<String>)] {
        &self.available_models
    }

    pub fn model_info(&self) -> &ModelInfo {
        &self.model_info
    }

    pub fn set_model_info(&mut self, provider: String, model: String) {
        self.model_info = ModelInfo { provider, model };
        self.mark_dirty();
    }

    pub fn toggle_model_picker(&mut self) {
        self.showing_model_picker = !self.showing_model_picker;
        if self.showing_model_picker {
            self.model_picker_state.select(Some(0));
        }
        self.mark_dirty();
    }

    pub fn close_model_picker(&mut self) {
        self.showing_model_picker = false;
        self.mark_dirty();
    }

    pub fn model_picker_next(&mut self) {
        let total_models: usize = self
            .available_models
            .iter()
            .map(|(_, models)| models.len())
            .sum();
        if let Some(selected) = self.model_picker_state.selected() {
            let next = (selected + 1) % total_models;
            self.model_picker_state.select(Some(next));
            self.mark_dirty();
        }
    }

    pub fn model_picker_prev(&mut self) {
        let total_models: usize = self
            .available_models
            .iter()
            .map(|(_, models)| models.len())
            .sum();
        if let Some(selected) = self.model_picker_state.selected() {
            let prev = if selected == 0 {
                total_models - 1
            } else {
                selected - 1
            };
            self.model_picker_state.select(Some(prev));
            self.mark_dirty();
        }
    }

    pub fn execute_command(&mut self, command: Command) -> bool {
        match command {
            Command::Help => {
                self.toggle_help();
                false
            }
            Command::New => {
                self.create_session();
                false
            }
            Command::Exit => true,
            Command::Agent => {
                self.toggle_profile_picker();
                false
            }
            Command::Model => false,
            Command::Plan => false,
            Command::Tasks => false,
            Command::Editor => false,
            Command::Export => false,
        }
    }

    pub fn dispatch_action(&mut self, action: Action) -> bool {
        match action {
            Action::Quit => true,
            Action::NewSession => {
                self.create_session();
                false
            }
            Action::ListSessions => {
                self.toggle_sessions_modal();
                false
            }
            Action::PickAgent => {
                self.toggle_profile_picker();
                false
            }
            Action::CycleAgent => {
                self.cycle_primary_profile();
                false
            }
            Action::PickModel => {
                self.toggle_model_picker();
                false
            }
            Action::ToggleSidebar => {
                self.toggle_sidebar();
                false
            }
            Action::OpenEditor => false,
            Action::Export => false,
            Action::Interrupt => false,
            Action::ScrollUp => {
                self.scroll_up(5);
                false
            }
            Action::ScrollDown => {
                self.scroll_down(5);
                false
            }
            Action::ShowHelp => {
                self.toggle_help();
                false
            }
            Action::CommandPalette => {
                self.toggle_command_palette();
                false
            }
        }
    }
}
