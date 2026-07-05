use entanglement_core::{AgentState, Holly, OutEvent, SessionId, TaskItem};
use tracing::debug;

#[derive(Debug, Clone)]
pub enum TranscriptEntry {
    TextDelta { text: String },
    ToolRequest { tool: String, input: String },
    ToolOutput { output: String },
    Error { message: String },
    Done,
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
}

impl App {
    pub fn new(holly: Holly, session_id: SessionId) -> Self {
        Self {
            _holly: holly,
            session_id,
            dirty: true,
            agent: "default".to_string(),
            state: AgentState::Idle,
            transcript: Vec::new(),
            plan: None,
            task_list: None,
            last_seen_seq: 0,
            scroll_offset: 0,
            auto_follow: true,
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

    pub fn handle_out_event(&mut self, event: OutEvent) {
        debug!("App handling OutEvent: {:?}", event);

        match event {
            OutEvent::Status { session, state } => {
                if session == self.session_id {
                    self.state = state;
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
                tool,
                input,
                ..
            } => {
                if session == self.session_id && seq > self.last_seen_seq {
                    self.transcript
                        .push(TranscriptEntry::ToolRequest { tool, input });
                    self.last_seen_seq = seq;
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
}
