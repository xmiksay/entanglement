use entanglement_core::{AgentState, Holly, OutEvent, SessionId, TaskItem};
use ratatui::widgets::ListState;
use std::collections::VecDeque;
use tracing::debug;
use tui_textarea::{CursorMove, TextArea};

#[derive(Debug, Clone)]
pub enum TranscriptEntry {
    TextDelta {
        text: String,
    },
    ToolRequest {
        tool: String,
        input: String,
        #[allow(dead_code)]
        request_id: String,
    },
    ToolOutput {
        output: String,
    },
    Error {
        message: String,
    },
    Done,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ApprovalMode {
    Normal,
    WaitingForApproval { request_id: String },
    EnteringRejectReason { request_id: String },
}

const HISTORY_CAPACITY: usize = 100;

#[derive(Clone)]
pub struct ProfileInfo {
    pub name: String,
    pub description: String,
}

pub struct App {
    _holly: Holly,
    session_id: SessionId,
    dirty: bool,

    // Status bar state
    agent: String,
    state: AgentState,

    // Content state
    transcript: Vec<TranscriptEntry>,
    plan: Option<String>,
    task_list: Option<Vec<TaskItem>>,

    // Per-session last-seen seq (for deduping)
    last_seen_seq: u64,

    // Scrolling state
    scroll_offset: usize,
    auto_follow: bool,

    // Input state
    input: TextArea<'static>,
    history: VecDeque<String>,
    history_index: Option<usize>,
    history_search_term: Option<String>,

    // Approval state
    approval_mode: ApprovalMode,
    pending_tool_request: Option<(String, String, String)>,

    // Profile picker state
    showing_profile_picker: bool,
    profile_picker_state: ListState,
    available_profiles: Vec<ProfileInfo>,
    primary_profile_order: Vec<String>,
}

impl App {
    pub fn new(holly: Holly, session_id: SessionId) -> Self {
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

        Self {
            _holly: holly,
            session_id,
            dirty: true,
            agent: "build".to_string(),
            state: AgentState::Idle,
            transcript: Vec::new(),
            plan: None,
            task_list: None,
            last_seen_seq: 0,
            scroll_offset: 0,
            auto_follow: true,
            input,
            history: VecDeque::with_capacity(HISTORY_CAPACITY),
            history_index: None,
            history_search_term: None,
            approval_mode: ApprovalMode::Normal,
            pending_tool_request: None,
            showing_profile_picker: false,
            profile_picker_state,
            available_profiles,
            primary_profile_order,
        }
    }

    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    pub fn clear_dirty(&mut self) {
        self.dirty = false;
    }

    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    pub fn agent(&self) -> &str {
        &self.agent
    }

    pub fn state(&self) -> AgentState {
        self.state
    }

    pub fn transcript(&self) -> &[TranscriptEntry] {
        &self.transcript
    }

    pub fn plan(&self) -> Option<&String> {
        self.plan.as_ref()
    }

    pub fn task_list(&self) -> Option<&[TaskItem]> {
        self.task_list.as_deref()
    }

    pub fn scroll_offset(&self) -> usize {
        self.scroll_offset
    }

    pub fn scroll_down(&mut self, lines: usize) {
        self.scroll_offset = self.scroll_offset.saturating_add(lines);
        self.auto_follow = false;
        self.mark_dirty();
    }

    pub fn scroll_up(&mut self, lines: usize) {
        self.scroll_offset = self.scroll_offset.saturating_sub(lines);
        self.auto_follow = false;
        self.mark_dirty();
    }

    pub fn scroll_to_bottom(&mut self) {
        self.scroll_offset = 0;
        self.auto_follow = true;
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
        &self.approval_mode
    }

    pub fn pending_tool_request(&self) -> Option<&(String, String, String)> {
        self.pending_tool_request.as_ref()
    }

    pub fn set_approval_mode(&mut self, mode: ApprovalMode) {
        self.approval_mode = mode;
        self.mark_dirty();
    }

    pub fn clear_approval(&mut self) {
        self.approval_mode = ApprovalMode::Normal;
        self.pending_tool_request = None;
        self.mark_dirty();
    }

    pub fn handle_out_event(&mut self, event: OutEvent) {
        debug!("App handling OutEvent: {:?}", event);

        match event {
            OutEvent::Status { session, state } => {
                if session == self.session_id {
                    self.state = state;
                    if state == AgentState::Idle
                        || state == AgentState::Done
                        || state == AgentState::Error
                    {
                        self.clear_approval();
                    }
                    self.mark_dirty();
                }
            }
            OutEvent::AgentChanged { session, agent } => {
                if session == self.session_id {
                    self.agent = agent;
                    self.mark_dirty();
                }
            }
            OutEvent::Plan {
                session,
                seq,
                content,
            } => {
                if session == self.session_id && seq > self.last_seen_seq {
                    self.plan = Some(content);
                    self.last_seen_seq = seq;
                    self.mark_dirty();
                }
            }
            OutEvent::TextDelta { session, seq, text } => {
                if session == self.session_id && seq > self.last_seen_seq {
                    self.transcript.push(TranscriptEntry::TextDelta { text });
                    self.last_seen_seq = seq;
                    if self.auto_follow {
                        self.scroll_offset = 0;
                    }
                    self.mark_dirty();
                }
            }
            OutEvent::ToolRequest {
                session,
                seq,
                request_id,
                tool,
                input,
            } => {
                if session == self.session_id && seq > self.last_seen_seq {
                    self.transcript.push(TranscriptEntry::ToolRequest {
                        tool: tool.clone(),
                        input: input.clone(),
                        request_id: request_id.clone(),
                    });
                    self.last_seen_seq = seq;
                    self.pending_tool_request = Some((request_id.clone(), tool, input));
                    self.approval_mode = ApprovalMode::WaitingForApproval {
                        request_id: request_id.clone(),
                    };
                    if self.auto_follow {
                        self.scroll_offset = 0;
                    }
                    self.mark_dirty();
                }
            }
            OutEvent::ToolOutput {
                session,
                seq,
                output,
                ..
            } => {
                if session == self.session_id && seq > self.last_seen_seq {
                    self.transcript.push(TranscriptEntry::ToolOutput { output });
                    self.last_seen_seq = seq;
                    if self.auto_follow {
                        self.scroll_offset = 0;
                    }
                    self.mark_dirty();
                }
            }
            OutEvent::TaskList {
                session,
                seq,
                tasks,
            } => {
                if session == self.session_id && seq > self.last_seen_seq {
                    self.task_list = Some(tasks);
                    self.last_seen_seq = seq;
                    self.mark_dirty();
                }
            }
            OutEvent::Error {
                session,
                seq,
                message,
            } => {
                if session == self.session_id && seq > self.last_seen_seq {
                    self.transcript.push(TranscriptEntry::Error { message });
                    self.last_seen_seq = seq;
                    if self.auto_follow {
                        self.scroll_offset = 0;
                    }
                    self.mark_dirty();
                }
            }
            OutEvent::Done { session, seq } => {
                if session == self.session_id && seq > self.last_seen_seq {
                    self.transcript.push(TranscriptEntry::Done);
                    self.last_seen_seq = seq;
                    if self.auto_follow {
                        self.scroll_offset = 0;
                    }
                    self.mark_dirty();
                }
            }
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
            let current_index = self
                .available_profiles
                .iter()
                .position(|p| p.name == self.agent)
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
        let current_index = self
            .primary_profile_order
            .iter()
            .position(|name| name == &self.agent)
            .unwrap_or(0);
        let next_index = (current_index + 1) % self.primary_profile_order.len();
        let new_agent = self.primary_profile_order[next_index].clone();
        self.agent = new_agent.clone();
        self.mark_dirty();
        Some(new_agent)
    }
}
