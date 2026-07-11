use entanglement_core::{AgentState, OutEvent, SessionId};
use entanglement_provider::{Catalog, ModelInfo};
use ratatui::layout::Rect;
use ratatui::widgets::ListState;
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::session_store::{list_sessions, LogRecord, SessionMeta};
use crate::tui::commands::{Command, CommandPalette};
use crate::tui::input::SimpleInput;
use crate::tui::keybindings::{Action, LeaderKeyHandler};
use crate::tui::markdown::MarkdownRenderer;
use crate::tui::mention::{FileIndex, MentionPopup};
use crate::tui::session_view::{ApprovalMode, PendingQuestion, TranscriptEntry};
use crate::tui::sessions::SessionRegistry;
use crate::tui::theme::Theme;

const HISTORY_CAPACITY: usize = 100;

/// A deferred, terminal-owning side effect a command/action requests but cannot
/// perform itself: the `App` has no `Terminal`, so it records the intent here
/// and the event loop (which does) runs it via `tui::editor::run_effect`
/// (ADR-0029).
#[derive(Debug, Clone, PartialEq)]
pub enum UiEffect {
    /// Suspend the TUI, edit the input draft in `$EDITOR`, read it back.
    OpenEditor,
    /// Export the transcript to Markdown and open it in `$EDITOR`.
    Export,
}

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

    // Chat click hit-testing: geometry + per-rendered-line block provenance
    // captured at draw time so a later mouse click maps back to a transcript
    // block. `chat_scroll_offset` is the resolved vertical offset the paragraph
    // was drawn at.
    chat_area: Rect,
    chat_scroll_offset: usize,
    chat_line_blocks: Vec<Option<usize>>,

    // Deferred terminal-owning effect (editor / export) for the event loop to run.
    pending_effect: Option<UiEffect>,

    // `@file` mention completion + `!bash` passthrough (ADR-0030). `root` is the
    // working directory both the file index and `!bash` execution are rooted at.
    root: PathBuf,
    bash_enabled: bool,
    mention: MentionPopup,
}

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
            root: PathBuf::from("."),
            bash_enabled: false,
            mention: MentionPopup::new(FileIndex::default()),
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

    /// Adopt an externally-minted session id (create its view if absent + switch)
    /// — the `propose_plan` handoff mints a fresh root `build` session (#141).
    pub fn adopt_session(&mut self, id: SessionId) {
        self.sessions.adopt(id);
        self.mark_dirty();
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

    pub fn task_list(&self) -> Option<&String> {
        self.sessions.active_view().task_list()
    }

    pub fn scroll_offset(&self) -> usize {
        self.sessions.active_view().scroll_offset()
    }

    pub fn scroll_offset_x(&self) -> usize {
        self.sessions.active_view().scroll_offset_x()
    }

    pub fn auto_follow(&self) -> bool {
        self.sessions.active_view().auto_follow()
    }

    /// Feeds the metrics `draw_body` measured back to the active session so the
    /// next scroll can clamp and follow can re-arm.
    pub fn set_viewport_metrics(&mut self, content_height: usize, viewport_height: usize) {
        self.sessions
            .active_view_mut()
            .set_viewport_metrics(content_height, viewport_height);
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

    /// Whether the reasoning run `id` (transcript index of its first delta) is
    /// expanded in the active session.
    pub fn reasoning_expanded(&self, id: usize) -> bool {
        self.sessions.active_view().reasoning_expanded(id)
    }

    /// Flips a reasoning run between collapsed and expanded.
    pub fn toggle_reasoning_block(&mut self, id: usize) {
        self.sessions.active_view_mut().toggle_reasoning(id);
        self.mark_dirty();
    }

    /// Stores the chat viewport geometry + line provenance captured this frame
    /// so a later mouse click can be mapped back to a transcript block.
    pub fn set_chat_hit_test(
        &mut self,
        area: Rect,
        scroll_offset: usize,
        line_blocks: Vec<Option<usize>>,
    ) {
        self.chat_area = area;
        self.chat_scroll_offset = scroll_offset;
        self.chat_line_blocks = line_blocks;
    }

    /// Maps a terminal click at `(col, row)` to the reasoning block under it, or
    /// `None` when the click lands outside the chat area or on a non-block line.
    pub fn reasoning_block_at(&self, col: u16, row: u16) -> Option<usize> {
        let area = self.chat_area;
        if area.width == 0 || area.height == 0 {
            return None;
        }
        if col < area.x || col >= area.x + area.width {
            return None;
        }
        if row < area.y || row >= area.y + area.height {
            return None;
        }
        let line_idx = (row - area.y) as usize + self.chat_scroll_offset;
        self.chat_line_blocks.get(line_idx).copied().flatten()
    }

    /// Keyboard fallback for the click: toggles the most recent reasoning run.
    pub fn toggle_last_reasoning_block(&mut self) {
        if let Some(id) = self.last_reasoning_block_id() {
            self.toggle_reasoning_block(id);
        }
    }

    /// Transcript index of the first delta of the last coalesced reasoning run.
    fn last_reasoning_block_id(&self) -> Option<usize> {
        let mut last = None;
        let mut prev_was_reasoning = false;
        for (idx, entry) in self.transcript().iter().enumerate() {
            if matches!(entry, TranscriptEntry::ReasoningDelta { .. }) {
                if !prev_was_reasoning {
                    last = Some(idx);
                }
                prev_was_reasoning = true;
            } else {
                prev_was_reasoning = false;
            }
        }
        last
    }

    pub fn input(&mut self) -> &mut SimpleInput {
        &mut self.input
    }

    pub fn input_text(&self) -> String {
        self.input.lines().join("\n")
    }

    /// Replaces the input buffer wholesale (used after an `$EDITOR` round-trip).
    pub fn set_input_text(&mut self, text: String) {
        self.input = SimpleInput::default();
        self.input.insert_str(&text);
        self.mark_dirty();
    }

    /// Wire the working directory into the head features that need it: builds
    /// the `@file` completion index and records whether `!bash` passthrough is
    /// allowed (ADR-0030). Called once by the event loop at startup.
    pub fn init_head_context(&mut self, root: PathBuf, bash_enabled: bool) {
        self.mention = MentionPopup::new(FileIndex::build(&root));
        self.root = root;
        self.bash_enabled = bash_enabled;
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn bash_enabled(&self) -> bool {
        self.bash_enabled
    }

    pub fn mention(&self) -> &MentionPopup {
        &self.mention
    }

    pub fn mention_mut(&mut self) -> &mut MentionPopup {
        &mut self.mention
    }

    pub fn mention_visible(&self) -> bool {
        self.mention.visible()
    }

    /// Recompute the `@file` popup from the current input line. Call after any
    /// key that changes the input text or cursor position.
    pub fn update_mention(&mut self) {
        let before = self.input.current_line_before_cursor().to_string();
        self.mention.update(&before);
        self.mark_dirty();
    }

    pub fn hide_mention(&mut self) {
        self.mention.hide();
        self.mark_dirty();
    }

    pub fn mention_select_next(&mut self) {
        self.mention.select_next();
        self.mark_dirty();
    }

    pub fn mention_select_prev(&mut self) {
        self.mention.select_prev();
        self.mark_dirty();
    }

    /// Swap the active `@query` token for the highlighted path (`@path `).
    /// Returns false (no-op) when the popup has no selection.
    pub fn accept_mention(&mut self) -> bool {
        let Some(path) = self.mention.selected().cloned() else {
            return false;
        };
        let before = self.input.current_line_before_cursor().to_string();
        if let Some(range) = crate::tui::mention::active_mention_range(&before) {
            self.input
                .replace_on_cursor_line(range.start, range.end, &format!("@{path} "));
        }
        self.mention.hide();
        self.mark_dirty();
        true
    }

    /// Record a `!bash` passthrough round-trip in the transcript (ADR-0030):
    /// the command and its captured output, rendered like a tool call/output.
    pub fn record_bash_passthrough(&mut self, command: String, output: String) {
        self.sessions
            .active_view_mut()
            .record_bash_passthrough(command, output);
        self.scroll_to_bottom();
        self.mark_dirty();
    }

    /// Records a deferred terminal-owning effect for the event loop to run.
    pub fn request_effect(&mut self, effect: UiEffect) {
        self.pending_effect = Some(effect);
    }

    /// Takes the pending terminal-owning effect, if any (event loop drains it).
    pub fn take_pending_effect(&mut self) -> Option<UiEffect> {
        self.pending_effect.take()
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

    pub fn pending_question(&self) -> Option<&PendingQuestion> {
        self.sessions.active_view().pending_question()
    }

    pub fn is_asking(&self) -> bool {
        self.sessions.active_view().is_asking()
    }

    pub fn question_move(&mut self, delta: isize) {
        self.sessions.active_view_mut().question_move(delta);
        self.mark_dirty();
    }

    pub fn question_begin_free_form(&mut self) {
        self.sessions.active_view_mut().question_begin_free_form();
        self.mark_dirty();
    }

    pub fn question_cancel_free_form(&mut self) {
        self.sessions.active_view_mut().question_cancel_free_form();
        self.mark_dirty();
    }

    pub fn clear_question(&mut self) {
        self.sessions.active_view_mut().clear_question();
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

    /// Set the active model, carrying the resolved `ModelInfo` (id, display
    /// name, context window) verbatim. The context window is already resolved on
    /// the incoming `ModelInfo` — re-deriving it from the catalog by id here
    /// would drop it (the id isn't always a catalog key), so we store as-is.
    pub fn set_model_info(&mut self, model_info: ModelInfo) {
        self.model_info = model_info;
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
            Command::Editor => {
                self.request_effect(UiEffect::OpenEditor);
                false
            }
            Command::Export => {
                self.request_effect(UiEffect::Export);
                false
            }
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
            Action::OpenEditor => {
                self.request_effect(UiEffect::OpenEditor);
                false
            }
            Action::Export => {
                self.request_effect(UiEffect::Export);
                false
            }
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
            Action::ToggleReasoning => {
                self.toggle_last_reasoning_block();
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

    pub fn showing_resume_modal(&self) -> bool {
        self.showing_resume_modal
    }

    pub fn resume_state(&mut self) -> &mut ListState {
        &mut self.resume_state
    }

    pub fn toggle_resume_modal(&mut self) {
        self.showing_resume_modal = !self.showing_resume_modal;
        if self.showing_resume_modal {
            if let Ok(mut sessions) = list_sessions(&std::env::current_dir().unwrap_or_default()) {
                // Only root sessions are independently resumable; spawned
                // children live inside their root's file. Most-recent first.
                sessions.retain(|s| s.root);
                sessions.sort_by_key(|s| std::cmp::Reverse(s.last_active));
                self.available_sessions = sessions;
            }
            self.resume_state
                .select(if self.available_sessions.is_empty() {
                    None
                } else {
                    Some(0)
                });
        }
        self.mark_dirty();
    }

    pub fn close_resume_modal(&mut self) {
        self.showing_resume_modal = false;
        self.mark_dirty();
    }

    /// Rebuilds and switches to a session's view from persisted records,
    /// restoring its full visible transcript.
    pub fn restore_session(&mut self, id: SessionId, records: &[LogRecord]) {
        self.sessions.restore_from_records(id, records);
        self.mark_dirty();
    }

    pub fn resume_next(&mut self) {
        if self.available_sessions.is_empty() {
            return;
        }
        if let Some(selected) = self.resume_state.selected() {
            let next = (selected + 1) % self.available_sessions.len();
            self.resume_state.select(Some(next));
        }
    }

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

    pub fn available_sessions(&self) -> &[SessionMeta] {
        &self.available_sessions
    }

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
    fn history_up_down_navigates_and_restores_draft() {
        let mut app = App::new_for_test(SessionId::new("test"));
        app.input.insert_str("first");
        assert_eq!(app.take_input_text(), "first");
        app.input.insert_str("second");
        assert_eq!(app.take_input_text(), "second");

        // A draft is preserved as the search term and restored on the way down.
        app.input.insert_str("draft");
        app.history_up();
        assert_eq!(app.input_text(), "second");
        app.history_up();
        assert_eq!(app.input_text(), "first");
        app.history_up(); // clamps at the oldest entry
        assert_eq!(app.input_text(), "first");
        app.history_down();
        assert_eq!(app.input_text(), "second");
        app.history_down(); // past the newest → restore the draft
        assert_eq!(app.input_text(), "draft");
    }

    #[test]
    fn history_navigation_is_a_noop_with_empty_history() {
        let mut app = App::new_for_test(SessionId::new("test"));
        app.history_up();
        app.history_down();
        assert_eq!(app.input_text(), "");
    }

    #[test]
    fn history_up_preserves_multibyte_entry() {
        let mut app = App::new_for_test(SessionId::new("test"));
        app.input.insert_str("héllo 🚀");
        assert_eq!(app.take_input_text(), "héllo 🚀");
        app.history_up();
        assert_eq!(app.input_text(), "héllo 🚀");
    }

    #[test]
    fn test_profile_color_for_hash() {
        let sid = SessionId::new("test");
        let app = App::new_for_test(sid);
        let color1 = app.profile_color_for("build");
        let color2 = app.profile_color_for("plan");
        let color3 = app.profile_color_for("build");

        assert_eq!(color1, color3);
        assert_ne!(color1, color2);
    }

    #[test]
    fn test_profile_color_for_override() {
        let sid = SessionId::new("test");
        let mut app = App::new_for_test(sid);
        let hash_color = app.profile_color_for("build");

        app.profile_colors
            .insert("build".to_string(), ratatui::style::Color::Magenta);
        let override_color = app.profile_color_for("build");

        assert_ne!(hash_color, override_color);
        assert_eq!(override_color, ratatui::style::Color::Magenta);
    }

    #[test]
    fn reasoning_block_at_maps_row_plus_offset_to_block() {
        let sid = SessionId::new("test");
        let mut app = App::new_for_test(sid);
        // Chat area at (x=2, y=1), 10 wide, 4 tall, scrolled down by 3 lines.
        let area = Rect::new(2, 1, 10, 4);
        // Rendered lines: only indices 3 and 5 belong to reasoning block 7.
        let line_blocks = vec![None, None, None, Some(7), None, Some(7), None];
        app.set_chat_hit_test(area, 3, line_blocks);

        // Top row of the area (row 1) + offset 3 → line index 3 → block 7.
        assert_eq!(app.reasoning_block_at(3, 1), Some(7));
        // row 3 + offset 3 → line 5 → block 7.
        assert_eq!(app.reasoning_block_at(5, 3), Some(7));
        // row 2 + offset 3 → line 4 → padding line, no block.
        assert_eq!(app.reasoning_block_at(5, 2), None);
    }

    #[test]
    fn reasoning_block_at_rejects_clicks_outside_chat_rect() {
        let sid = SessionId::new("test");
        let mut app = App::new_for_test(sid);
        let area = Rect::new(2, 1, 10, 4);
        app.set_chat_hit_test(area, 3, vec![None, None, None, Some(7)]);

        assert_eq!(app.reasoning_block_at(1, 1), None, "left of area");
        assert_eq!(app.reasoning_block_at(12, 1), None, "right of area");
        assert_eq!(app.reasoning_block_at(3, 0), None, "above area");
        assert_eq!(app.reasoning_block_at(3, 5), None, "below area");
    }

    #[test]
    fn reasoning_block_at_is_empty_before_first_draw() {
        let sid = SessionId::new("test");
        let app = App::new_for_test(sid);
        assert_eq!(app.reasoning_block_at(0, 0), None);
    }

    #[test]
    fn test_thinking_state_tracking() {
        let sid = SessionId::new("test");
        let mut app = App::new_for_test(sid.clone());

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

    #[test]
    fn accept_mention_replaces_at_token_with_path() {
        let mut app = App::new_for_test(SessionId::new("test"));
        app.mention = MentionPopup::new(FileIndex::from_paths(vec!["src/tui/app.rs".to_string()]));
        app.input.insert_str("explain @app");

        app.update_mention();
        assert!(app.mention_visible());

        assert!(app.accept_mention());
        assert_eq!(app.input_text(), "explain @src/tui/app.rs ");
        assert!(!app.mention_visible());
    }

    #[test]
    fn record_bash_passthrough_appends_tool_call_and_output() {
        let mut app = App::new_for_test(SessionId::new("test"));
        app.record_bash_passthrough("echo hi".to_string(), "[exit 0]\nhi\n".to_string());

        let entries = app.transcript();
        assert!(matches!(
            &entries[entries.len() - 2],
            TranscriptEntry::ToolCall { tool, input } if tool == "!bash" && input == "echo hi"
        ));
        assert!(matches!(
            &entries[entries.len() - 1],
            TranscriptEntry::ToolOutput { tool: Some(t), output } if t == "!bash" && output.contains("hi")
        ));
    }

    #[test]
    fn set_model_info_preserves_resolved_context_window() {
        // Regression (issue #103): the resolved `ModelInfo` — context window
        // included — must be carried verbatim. Re-deriving it from the catalog
        // by id would drop the window for ids that aren't catalog keys.
        let mut app = App::new_for_test(SessionId::new("test"));
        app.set_model_info(ModelInfo {
            id: "claude-sonnet-4-5".to_string(),
            display_name: "Claude Sonnet 4.5".to_string(),
            context_window: Some(200_000),
        });

        let info = app.model_info();
        assert_eq!(info.id, "claude-sonnet-4-5");
        assert_eq!(info.display_name, "Claude Sonnet 4.5");
        assert_eq!(info.context_window, Some(200_000));
    }
}
