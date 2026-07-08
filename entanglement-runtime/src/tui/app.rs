use entanglement_core::{AgentState, OutEvent, SessionId, TaskItem};
use entanglement_provider::{models_for, ModelInfo};
use ratatui::widgets::ListState;
use std::collections::{HashMap, VecDeque};
use std::time::Instant;

use crate::session_store::{list_sessions, SessionMeta};
use crate::tui::commands::{Command, CommandPalette};
use crate::tui::keybindings::{Action, LeaderKeyHandler};
use crate::tui::markdown::MarkdownRenderer;
use crate::tui::session_view::{ApprovalMode, TranscriptEntry};
use crate::tui::sessions::SessionRegistry;
use crate::tui::theme::Theme;

#[derive(Debug, Clone, Default)]
pub struct SimpleInput {
    lines: Vec<String>,
    cursor_row: usize,
    cursor_col: usize,
    scroll_offset: u16,
}

impl SimpleInput {
    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    pub fn cursor(&self) -> (usize, usize) {
        (self.cursor_row, self.cursor_col)
    }

    #[allow(dead_code)]
    pub fn cursor_row(&self) -> usize {
        self.cursor_row
    }

    pub fn cursor_col(&self) -> usize {
        self.cursor_col
    }

    pub fn insert_char(&mut self, c: char) {
        if self.cursor_row >= self.lines.len() {
            self.lines.resize(self.cursor_row + 1, String::new());
        }
        let line = &mut self.lines[self.cursor_row];
        if self.cursor_col > line.len() {
            line.extend(std::iter::repeat_n(' ', self.cursor_col - line.len()));
        }
        line.insert(self.cursor_col, c);
        self.cursor_col += 1;
    }

    pub fn insert_str(&mut self, s: &str) {
        for c in s.chars() {
            self.insert_char(c);
        }
    }

    pub fn insert_newline(&mut self) {
        let current_line = self.lines.get(self.cursor_row).cloned().unwrap_or_default();
        let (before, after) = current_line.split_at(self.cursor_col);
        self.lines[self.cursor_row] = before.to_string();
        self.lines.insert(self.cursor_row + 1, after.to_string());
        self.cursor_row += 1;
        self.cursor_col = 0;
    }

    pub fn delete_char(&mut self) {
        if self.cursor_col > 0 {
            let line = &mut self.lines[self.cursor_row];
            if self.cursor_col <= line.len() {
                line.remove(self.cursor_col - 1);
            }
            self.cursor_col -= 1;
        } else if self.cursor_row > 0 {
            let current_line = self.lines.remove(self.cursor_row);
            let prev_line = &mut self.lines[self.cursor_row - 1];
            self.cursor_col = prev_line.len();
            prev_line.push_str(&current_line);
            self.cursor_row -= 1;
        }
    }

    pub fn delete_line_by_end(&mut self) {
        let line = &mut self.lines[self.cursor_row];
        line.truncate(self.cursor_col);
    }

    pub fn delete_line_by_head(&mut self) {
        let line = &mut self.lines[self.cursor_row];
        let after = line.split_off(self.cursor_col);
        *line = after;
        self.cursor_col = 0;
    }

    pub fn delete_word(&mut self) {
        let line = &mut self.lines[self.cursor_row];
        if self.cursor_col > 0 {
            let before = &line[..self.cursor_col];
            let after = &line[self.cursor_col..];
            let new_before = before.trim_end();
            let removed = before.len() - new_before.len();
            if removed > 0 {
                *line = format!("{}{}", new_before, after);
                self.cursor_col -= removed;
            }
        }
    }

    #[allow(dead_code)]
    pub fn clear(&mut self) {
        self.lines = vec![String::new()];
        self.cursor_row = 0;
        self.cursor_col = 0;
        self.scroll_offset = 0;
    }

    pub fn move_cursor_up(&mut self) {
        if self.cursor_row > 0 {
            self.cursor_row -= 1;
            let line_len = self
                .lines
                .get(self.cursor_row)
                .map(|l| l.len())
                .unwrap_or(0);
            self.cursor_col = self.cursor_col.min(line_len);
        }
    }

    pub fn move_cursor_down(&mut self) {
        if self.cursor_row < self.lines.len().saturating_sub(1) {
            self.cursor_row += 1;
            let line_len = self
                .lines
                .get(self.cursor_row)
                .map(|l| l.len())
                .unwrap_or(0);
            self.cursor_col = self.cursor_col.min(line_len);
        }
    }

    pub fn move_cursor_left(&mut self) {
        if self.cursor_col > 0 {
            self.cursor_col -= 1;
        }
    }

    pub fn move_cursor_right(&mut self) {
        let line_len = self
            .lines
            .get(self.cursor_row)
            .map(|l| l.len())
            .unwrap_or(0);
        if self.cursor_col < line_len {
            self.cursor_col += 1;
        }
    }

    pub fn move_cursor_to_head(&mut self) {
        self.cursor_col = 0;
    }

    pub fn move_cursor_to_end(&mut self) {
        let line_len = self
            .lines
            .get(self.cursor_row)
            .map(|l| l.len())
            .unwrap_or(0);
        self.cursor_col = line_len;
    }

    #[allow(dead_code)]
    pub fn set_scroll_offset(&mut self, offset: u16) {
        self.scroll_offset = offset;
    }

    pub fn scroll_offset(&self) -> u16 {
        self.scroll_offset
    }
}

const HISTORY_CAPACITY: usize = 100;

#[derive(Clone)]
pub struct ProfileInfo {
    pub name: String,
    pub description: String,
}

pub struct App {
    sessions: SessionRegistry,
    dirty: bool,
    markdown_renderer: MarkdownRenderer,

    input: SimpleInput,
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

    // Theme and rendering state
    theme: Theme,
    profile_colors: HashMap<String, ratatui::style::Color>,
    thinking_since: Option<Instant>,

    // Token usage tracking
    input_tokens: u64,
    output_tokens: u64,

    // Input state
    input_multiline: bool,
    help_text: String,

    // Resume session modal state
    showing_resume_modal: bool,
    resume_state: ListState,
    available_sessions: Vec<SessionMeta>,
}

impl App {
    pub fn new(initial_session: SessionId) -> Self {
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

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn markdown_renderer(&self) -> &MarkdownRenderer {
        &self.markdown_renderer
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

    pub fn scroll_offset_x(&self) -> usize {
        self.sessions.active_view().scroll_offset_x()
    }

    pub fn scroll_down(&mut self, lines: usize) {
        self.sessions.active_view_mut().scroll_down(lines);
        self.mark_dirty();
    }

    pub fn scroll_up(&mut self, lines: usize) {
        self.sessions.active_view_mut().scroll_up(lines);
        self.mark_dirty();
    }

    pub fn scroll_right(&mut self, cols: usize) {
        self.sessions.active_view_mut().scroll_right(cols);
        self.mark_dirty();
    }

    pub fn scroll_left(&mut self, cols: usize) {
        self.sessions.active_view_mut().scroll_left(cols);
        self.mark_dirty();
    }

    pub fn scroll_to_bottom(&mut self) {
        self.sessions.active_view_mut().scroll_to_bottom();
        self.mark_dirty();
    }

    pub fn input(&mut self) -> &mut SimpleInput {
        &mut self.input
    }

    pub fn input_text(&self) -> String {
        self.input.lines().join("\n")
    }

    #[allow(dead_code)]
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
        self.input = SimpleInput::default();
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
                self.input = SimpleInput::default();
                self.input.insert_str(text);
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
                    self.input = SimpleInput::default();
                    self.input.insert_str(text);
                    self.mark_dirty();
                }
            } else {
                self.history_index = None;
                let search_term = self.history_search_term.take().unwrap_or_default();
                self.input = SimpleInput::default();
                self.input.insert_str(&search_term);
                self.mark_dirty();
            }
        }
    }

    pub fn handle_readline_key(&mut self, c: char) -> bool {
        match c {
            'a' => {
                self.input.move_cursor_to_head();
                true
            }
            'e' => {
                self.input.move_cursor_to_end();
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

    /// Records the user's prompt into the active session's transcript so the
    /// chat scrollback mirrors a real conversation (the engine never echoes
    /// `InMsg::Prompt` back as an `OutEvent`).
    pub fn record_user_message(&mut self, text: String) {
        self.sessions.active_view_mut().record_user_message(text);
        self.mark_dirty();
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
        let display_name = model.clone();
        let context_window = models_for(&provider)
            .into_iter()
            .find(|m| m.id == model)
            .and_then(|m| m.context_window);
        self.model_info = ModelInfo {
            id: model,
            display_name,
            context_window,
        };
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
            Command::Model => {
                self.toggle_model_picker();
                false
            }
            Command::Plan => false,
            Command::Tasks => false,
            Command::Editor => false,
            Command::Export => false,
            Command::Resume => {
                self.toggle_resume_modal();
                false
            }
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

    pub fn theme(&self) -> Theme {
        self.theme
    }

    pub fn profile_color_for(&self, name: &str) -> ratatui::style::Color {
        self.profile_colors
            .get(name)
            .copied()
            .unwrap_or_else(|| crate::tui::theme::hash_profile_color(name))
    }

    pub fn thinking_since(&self) -> Option<Instant> {
        self.thinking_since
    }

    pub fn tick_thinking(&mut self) {
        let is_thinking = matches!(self.state(), AgentState::Thinking);
        match (self.thinking_since, is_thinking) {
            (None, true) => {
                self.thinking_since = Some(Instant::now());
                self.mark_dirty();
            }
            (Some(_), false) => {
                self.thinking_since = None;
                self.mark_dirty();
            }
            (Some(_), true) => {
                self.mark_dirty();
            }
            _ => {}
        }
    }

    pub fn input_tokens(&self) -> u64 {
        self.input_tokens
    }

    pub fn output_tokens(&self) -> u64 {
        self.output_tokens
    }

    #[allow(dead_code)]
    pub fn add_input_tokens(&mut self, tokens: u64) {
        self.input_tokens += tokens;
    }

    #[allow(dead_code)]
    pub fn add_output_tokens(&mut self, tokens: u64) {
        self.output_tokens += tokens;
    }

    #[allow(dead_code)]
    pub fn is_input_multiline(&self) -> bool {
        self.input_multiline
    }

    #[allow(dead_code)]
    pub fn toggle_input_multiline(&mut self) {
        self.input_multiline = !self.input_multiline;
        self.mark_dirty();
    }

    pub fn set_input_multiline(&mut self, multiline: bool) {
        self.input_multiline = multiline;
        self.mark_dirty();
    }

    pub fn help_text(&self) -> &str {
        &self.help_text
    }

    #[allow(dead_code)]
    pub fn showing_resume_modal(&self) -> bool {
        self.showing_resume_modal
    }

    #[allow(dead_code)]
    pub fn resume_state(&mut self) -> &mut ListState {
        &mut self.resume_state
    }

    #[allow(dead_code)]
    pub fn toggle_resume_modal(&mut self) {
        self.showing_resume_modal = !self.showing_resume_modal;
        if self.showing_resume_modal {
            if let Ok(sessions) = list_sessions(&std::env::current_dir().unwrap_or_default()) {
                self.available_sessions = sessions;
                if !self.available_sessions.is_empty() {
                    self.resume_state.select(Some(0));
                }
            }
        }
        self.mark_dirty();
    }

    #[allow(dead_code)]
    pub fn close_resume_modal(&mut self) {
        self.showing_resume_modal = false;
        self.mark_dirty();
    }

    #[allow(dead_code)]
    pub fn resume_next(&mut self) {
        if self.available_sessions.is_empty() {
            return;
        }
        if let Some(selected) = self.resume_state.selected() {
            let next = (selected + 1) % self.available_sessions.len();
            self.resume_state.select(Some(next));
        }
    }

    #[allow(dead_code)]
    pub fn resume_prev(&mut self) {
        if self.available_sessions.is_empty() {
            return;
        }
        if let Some(selected) = self.resume_state.selected() {
            let prev = if selected == 0 {
                self.available_sessions.len() - 1
            } else {
                selected - 1
            };
            self.resume_state.select(Some(prev));
        }
    }

    #[allow(dead_code)]
    pub fn available_sessions(&self) -> &[SessionMeta] {
        &self.available_sessions
    }

    #[allow(dead_code)]
    pub fn selected_resume_session(&self) -> Option<SessionMeta> {
        self.resume_state
            .selected()
            .and_then(|i| self.available_sessions.get(i).cloned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use entanglement_core::OutEvent;

    #[test]
    fn test_profile_color_for_hash() {
        let sid = SessionId::new("test");
        let app = App::new(sid);
        let color1 = app.profile_color_for("build");
        let color2 = app.profile_color_for("plan");
        let color3 = app.profile_color_for("build");

        assert_eq!(color1, color3);
        assert_ne!(color1, color2);
    }

    #[test]
    fn test_profile_color_for_override() {
        let sid = SessionId::new("test");
        let mut app = App::new(sid);
        let hash_color = app.profile_color_for("build");

        app.profile_colors
            .insert("build".to_string(), ratatui::style::Color::Magenta);
        let override_color = app.profile_color_for("build");

        assert_ne!(hash_color, override_color);
        assert_eq!(override_color, ratatui::style::Color::Magenta);
    }

    #[test]
    fn test_thinking_state_tracking() {
        let sid = SessionId::new("test");
        let mut app = App::new(sid.clone());

        app.handle_out_event(OutEvent::Status {
            session: sid.clone(),
            state: AgentState::Thinking,
        });
        app.tick_thinking();

        assert!(app.thinking_since().is_some());
        assert!(matches!(app.state(), AgentState::Thinking));

        app.handle_out_event(OutEvent::Status {
            session: sid.clone(),
            state: AgentState::Done,
        });
        app.tick_thinking();

        assert!(app.thinking_since().is_none());
    }
}
