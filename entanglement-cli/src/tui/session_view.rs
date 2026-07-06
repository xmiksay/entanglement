use entanglement_core::{AgentState, OutEvent, TaskItem};

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
    auto_follow: bool,
    approval_mode: ApprovalMode,
    pending_tool_request: Option<(String, String, String)>,
    /// Set when we send `InMsg::Stop` for this session; the engine destroys
    /// the session task and a later `Prompt` lazily recreates it with
    /// `seq` starting back at 0. Cleared (and `last_seen_seq` reset) the
    /// next time we send a `Prompt`, so the dedupe guard doesn't discard
    /// every event from the fresh session.
    stopped: bool,
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
            auto_follow: true,
            approval_mode: ApprovalMode::Normal,
            pending_tool_request: None,
            stopped: false,
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

    pub fn scroll_down(&mut self, lines: usize) {
        self.scroll_offset = self.scroll_offset.saturating_add(lines);
        self.auto_follow = false;
    }

    pub fn scroll_up(&mut self, lines: usize) {
        self.scroll_offset = self.scroll_offset.saturating_sub(lines);
        self.auto_follow = false;
    }

    pub fn scroll_to_bottom(&mut self) {
        self.scroll_offset = 0;
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

    /// Marks this session stopped so the next `Prompt` resets the seq dedupe
    /// guard against the engine recreating the session at `seq = 0`.
    pub fn note_stop_sent(&mut self) {
        self.stopped = true;
        self.clear_approval();
    }

    /// Call right before sending a `Prompt` for this session.
    pub fn note_prompt_sent(&mut self) {
        if self.stopped {
            self.stopped = false;
            self.last_seen_seq = 0;
        }
    }

    /// Applies an `OutEvent` already routed to this session. Returns `true`
    /// if it changed anything the UI needs to redraw for.
    pub fn apply_event(&mut self, event: OutEvent) -> bool {
        match event {
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
                    self.transcript.push(TranscriptEntry::TextDelta { text });
                    self.last_seen_seq = seq;
                    if self.auto_follow {
                        self.scroll_offset = 0;
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
                    self.transcript.push(TranscriptEntry::ToolRequest {
                        tool: tool.clone(),
                        input: input.clone(),
                        request_id: request_id.clone(),
                    });
                    self.last_seen_seq = seq;
                    self.pending_tool_request = Some((request_id.clone(), tool, input));
                    self.approval_mode = ApprovalMode::WaitingForApproval { request_id };
                    if self.auto_follow {
                        self.scroll_offset = 0;
                    }
                    true
                } else {
                    false
                }
            }
            OutEvent::ToolOutput { seq, output, .. } => {
                if seq > self.last_seen_seq {
                    self.transcript.push(TranscriptEntry::ToolOutput { output });
                    self.last_seen_seq = seq;
                    if self.auto_follow {
                        self.scroll_offset = 0;
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
                    }
                    true
                } else {
                    false
                }
            }
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
    fn stop_then_prompt_resets_seq_guard() {
        let mut v = SessionView::new();
        v.apply_event(OutEvent::TextDelta {
            session: sid(),
            seq: 9,
            text: "a".into(),
        });
        v.note_stop_sent();
        v.note_prompt_sent();
        assert!(v.apply_event(OutEvent::TextDelta {
            session: sid(),
            seq: 1,
            text: "fresh".into(),
        }));
    }
}
