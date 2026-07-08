use entanglement_core::{AgentState, OutEvent, SessionId, TaskItem};

#[derive(Debug, Clone, PartialEq)]
pub enum TranscriptEntry {
    User {
        text: String,
        pending: bool,
    },
    TextDelta {
        text: String,
    },
    ReasoningDelta {
        text: String,
    },
    ToolCall {
        tool: String,
        input: String,
    },
    ToolOutput {
        tool: Option<String>,
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

/// All state scoped to a single `SessionId`: the engine spawns one task per
/// session with its own history/profile/seq, so the head mirrors that split
/// to keep transcripts, approvals, and scroll position from bleeding across
/// sessions when the user switches the active one.
pub struct SessionView {
    agent: String,
    state: AgentState,
    transcript: Vec<TranscriptEntry>,
    plan: Option<String>,
    task_list: Option<Vec<TaskItem>>,
    last_seen_seq: u64,
    scroll_offset: usize,
    scroll_offset_x: usize,
    auto_follow: bool,
    approval_mode: ApprovalMode,
    pending_tool_request: Option<(String, String, String)>,
    parent: Option<SessionId>,
}

impl SessionView {
    pub fn new() -> Self {
        Self {
            agent: "build".to_string(),
            state: AgentState::Idle,
            transcript: Vec::new(),
            plan: None,
            task_list: None,
            last_seen_seq: 0,
            scroll_offset: 0,
            scroll_offset_x: 0,
            auto_follow: true,
            approval_mode: ApprovalMode::Normal,
            pending_tool_request: None,
            parent: None,
        }
    }

    pub fn agent(&self) -> &str {
        &self.agent
    }

    pub fn set_agent(&mut self, agent: String) {
        self.agent = agent;
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

    pub fn scroll_offset_x(&self) -> usize {
        self.scroll_offset_x
    }

    pub fn scroll_down(&mut self, lines: usize) {
        self.scroll_offset = self.scroll_offset.saturating_add(lines);
        self.auto_follow = false;
    }

    pub fn scroll_up(&mut self, lines: usize) {
        self.scroll_offset = self.scroll_offset.saturating_sub(lines);
        self.auto_follow = false;
    }

    pub fn scroll_right(&mut self, cols: usize) {
        self.scroll_offset_x = self.scroll_offset_x.saturating_add(cols);
        self.auto_follow = false;
    }

    pub fn scroll_left(&mut self, cols: usize) {
        self.scroll_offset_x = self.scroll_offset_x.saturating_sub(cols);
        self.auto_follow = false;
    }

    pub fn scroll_to_bottom(&mut self) {
        self.scroll_offset = 0;
        self.scroll_offset_x = 0;
        self.auto_follow = true;
    }

    pub fn approval_mode(&self) -> &ApprovalMode {
        &self.approval_mode
    }

    pub fn pending_tool_request(&self) -> Option<&(String, String, String)> {
        self.pending_tool_request.as_ref()
    }

    pub fn set_approval_mode(&mut self, mode: ApprovalMode) {
        self.approval_mode = mode;
    }

    pub fn clear_approval(&mut self) {
        self.approval_mode = ApprovalMode::Normal;
        self.pending_tool_request = None;
    }

    pub fn is_waiting_approval(&self) -> bool {
        matches!(
            self.approval_mode,
            ApprovalMode::WaitingForApproval { .. } | ApprovalMode::EnteringRejectReason { .. }
        )
    }

    pub fn parent(&self) -> Option<&SessionId> {
        self.parent.as_ref()
    }

    /// Records the user's outgoing prompt into the transcript so it shows up
    /// in the chat scrollback. Unlike engine `OutEvent`s, user prompts carry
    /// no `seq` and bypass the dedupe guard — they originate here, not the
    /// engine broadcast.
    pub fn record_user_message(&mut self, text: String) {
        self.transcript.push(TranscriptEntry::User {
            text,
            pending: true,
        });
        if self.auto_follow {
            self.scroll_offset = 0;
            self.scroll_offset_x = 0;
        }
    }

    /// Applies an `OutEvent` already routed to this session. Returns `true`
    /// if it changed anything the UI needs to redraw for.
    pub fn apply_event(&mut self, event: OutEvent) -> bool {
        match event {
            OutEvent::SessionStarted { parent, .. } => {
                self.parent = parent;
                true
            }
            OutEvent::SessionEnded { .. } => true,
            OutEvent::Status { state, .. } => {
                self.state = state;
                if state == AgentState::Idle
                    || state == AgentState::Done
                    || state == AgentState::Error
                {
                    self.clear_approval();
                }
                true
            }
            OutEvent::AgentChanged { agent, .. } => {
                self.agent = agent;
                true
            }
            OutEvent::Plan { seq, content, .. } => {
                if seq > self.last_seen_seq {
                    self.plan = Some(content);
                    self.last_seen_seq = seq;
                    true
                } else {
                    false
                }
            }
            OutEvent::TextDelta { seq, text, .. } => {
                if seq > self.last_seen_seq {
                    for entry in self.transcript.iter_mut().rev() {
                        if let TranscriptEntry::User { pending, .. } = entry {
                            *pending = false;
                            break;
                        }
                    }
                    self.transcript.push(TranscriptEntry::TextDelta { text });
                    self.last_seen_seq = seq;
                    if self.auto_follow {
                        self.scroll_offset = 0;
                        self.scroll_offset_x = 0;
                    }
                    true
                } else {
                    false
                }
            }
            OutEvent::ReasoningDelta { seq, text, .. } => {
                if seq > self.last_seen_seq {
                    self.transcript
                        .push(TranscriptEntry::ReasoningDelta { text });
                    self.last_seen_seq = seq;
                    if self.auto_follow {
                        self.scroll_offset = 0;
                        self.scroll_offset_x = 0;
                    }
                    true
                } else {
                    false
                }
            }
            OutEvent::ToolCall {
                seq, tool, input, ..
            } => {
                if seq > self.last_seen_seq {
                    self.transcript.push(TranscriptEntry::ToolCall {
                        tool: tool.clone(),
                        input: input.clone(),
                    });
                    self.last_seen_seq = seq;
                    if self.auto_follow {
                        self.scroll_offset = 0;
                        self.scroll_offset_x = 0;
                    }
                    true
                } else {
                    false
                }
            }
            OutEvent::ToolRequest {
                seq,
                request_id,
                tool,
                input,
                ..
            } => {
                if seq > self.last_seen_seq {
                    self.last_seen_seq = seq;
                    self.pending_tool_request = Some((request_id.clone(), tool, input));
                    self.approval_mode = ApprovalMode::WaitingForApproval { request_id };
                    if self.auto_follow {
                        self.scroll_offset = 0;
                        self.scroll_offset_x = 0;
                    }
                    true
                } else {
                    false
                }
            }
            // Runtime plumbing (#58): the tool executor answers this, not the
            // UI. The call is already shown via `ToolCall`.
            OutEvent::ToolExec { .. } => false,
            OutEvent::ToolOutput {
                seq, tool, output, ..
            } => {
                if seq > self.last_seen_seq {
                    self.transcript.push(TranscriptEntry::ToolOutput {
                        tool: Some(tool.clone()),
                        output,
                    });
                    self.last_seen_seq = seq;
                    if self.auto_follow {
                        self.scroll_offset = 0;
                        self.scroll_offset_x = 0;
                    }
                    true
                } else {
                    false
                }
            }
            OutEvent::TaskList { seq, tasks, .. } => {
                if seq > self.last_seen_seq {
                    self.task_list = Some(tasks);
                    self.last_seen_seq = seq;
                    true
                } else {
                    false
                }
            }
            OutEvent::Error { seq, message, .. } => {
                if seq > self.last_seen_seq {
                    self.transcript.push(TranscriptEntry::Error { message });
                    self.last_seen_seq = seq;
                    if self.auto_follow {
                        self.scroll_offset = 0;
                        self.scroll_offset_x = 0;
                    }
                    true
                } else {
                    false
                }
            }
            OutEvent::Done { seq, .. } => {
                if seq > self.last_seen_seq {
                    self.transcript.push(TranscriptEntry::Done);
                    self.last_seen_seq = seq;
                    if self.auto_follow {
                        self.scroll_offset = 0;
                        self.scroll_offset_x = 0;
                    }
                    true
                } else {
                    false
                }
            }
            OutEvent::FileChange {
                path: _,
                change_kind: _,
                ..
            } => true,
        }
    }
}

impl Default for SessionView {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use entanglement_core::SessionId;

    fn sid() -> SessionId {
        SessionId::new("s1")
    }

    #[test]
    fn seq_dedupe_drops_replay() {
        let mut v = SessionView::new();
        assert!(v.apply_event(OutEvent::TextDelta {
            session: sid(),
            seq: 1,
            text: "a".into(),
        }));
        assert!(!v.apply_event(OutEvent::TextDelta {
            session: sid(),
            seq: 1,
            text: "replay".into(),
        }));
        assert!(v.apply_event(OutEvent::TextDelta {
            session: sid(),
            seq: 2,
            text: "b".into(),
        }));
        assert_eq!(v.transcript().len(), 2);
    }

    #[test]
    fn tool_request_sets_waiting_then_status_clears() {
        let mut v = SessionView::new();
        v.apply_event(OutEvent::ToolRequest {
            session: sid(),
            seq: 1,
            request_id: "t1".into(),
            tool: "read".into(),
            input: "{}".into(),
        });
        assert!(v.is_waiting_approval());
        assert_eq!(
            v.pending_tool_request().map(|(id, ..)| id.as_str()),
            Some("t1")
        );

        v.apply_event(OutEvent::Status {
            session: sid(),
            state: AgentState::Idle,
        });
        assert!(!v.is_waiting_approval());
        assert!(v.pending_tool_request().is_none());
    }

    #[test]
    fn record_user_message_appears_before_streamed_reply() {
        // Regression for "user messages don't show in chat": recording a
        // prompt must insert a `User` entry into the transcript (and it must
        // not be subject to the seq dedupe guard, which only covers engine
        // `OutEvent`s).
        let mut v = SessionView::new();
        v.record_user_message("hello?".into());
        v.apply_event(OutEvent::TextDelta {
            session: sid(),
            seq: 1,
            text: "hi!".into(),
        });

        let entries = v.transcript();
        assert!(matches!(entries[0], TranscriptEntry::User { ref text, .. } if text == "hello?"));
        assert!(matches!(entries[1], TranscriptEntry::TextDelta { .. }));
        assert_eq!(entries.len(), 2);
    }
}
