use entanglement_core::{AgentState, OutEvent, QuestionOption, SessionId, TaskItem};
use std::collections::HashSet;

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

/// A model-driven `ask_user` question awaiting the user's answer (ADR-0027).
/// Distinct from [`ApprovalMode`]: approval is binary, this carries labelled
/// choices plus an optional free-text "Other" escape.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingQuestion {
    pub request_id: String,
    pub question: String,
    pub options: Vec<QuestionOption>,
    pub allow_free_form: bool,
    /// Highlighted choice. Indices `0..options.len()` are the options; when
    /// `allow_free_form`, index `options.len()` is the "Other" entry.
    pub selected: usize,
    /// True while the "Other" free-text field is being typed into (answer flows
    /// through the shared input box, like a reject reason).
    pub entering_free_form: bool,
}

impl PendingQuestion {
    /// Total selectable choices, including the "Other" entry when allowed.
    pub fn choice_count(&self) -> usize {
        self.options.len() + usize::from(self.allow_free_form)
    }

    /// Whether the highlighted choice is the free-text "Other" entry.
    pub fn free_form_selected(&self) -> bool {
        self.allow_free_form && self.selected == self.options.len()
    }
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
    /// Top-anchored vertical offset (line index of the first visible row).
    /// Only meaningful while frozen; when `auto_follow` is set the view is
    /// pinned to the bottom at draw time and this value is ignored.
    scroll_offset: usize,
    scroll_offset_x: usize,
    auto_follow: bool,
    /// Rendered line count and viewport height cached from the last draw so
    /// scroll math can clamp/anchor without re-deriving them — only `draw_body`
    /// knows the wrapped line count and `area.height`.
    last_content_height: usize,
    last_viewport_height: usize,
    approval_mode: ApprovalMode,
    pending_tool_request: Option<(String, String, String)>,
    pending_question: Option<PendingQuestion>,
    parent: Option<SessionId>,
    /// Wall-clock (ms since epoch) the session started / ended, from
    /// `SessionStarted` / `SessionEnded`. Drives the live spawn-duration shown
    /// for sub-agent (child) sessions in the sessions list (#89, ADR-0026).
    started_ms: Option<u64>,
    ended_ms: Option<u64>,
    /// Reasoning runs the user has expanded, keyed by the transcript index of
    /// the run's first `ReasoningDelta` (a stable id — runs are coalesced from
    /// consecutive deltas at render time). Absent = collapsed (the default).
    expanded_reasoning: HashSet<usize>,
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
            last_content_height: 0,
            last_viewport_height: 0,
            approval_mode: ApprovalMode::Normal,
            pending_tool_request: None,
            pending_question: None,
            parent: None,
            started_ms: None,
            ended_ms: None,
            expanded_reasoning: HashSet::new(),
        }
    }

    /// Whether the reasoning run identified by `id` (transcript index of its
    /// first `ReasoningDelta`) is currently expanded.
    pub fn reasoning_expanded(&self, id: usize) -> bool {
        self.expanded_reasoning.contains(&id)
    }

    /// Flips a reasoning run between collapsed and expanded.
    pub fn toggle_reasoning(&mut self, id: usize) {
        if !self.expanded_reasoning.remove(&id) {
            self.expanded_reasoning.insert(id);
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

    pub fn auto_follow(&self) -> bool {
        self.auto_follow
    }

    /// Largest valid top-anchored offset for the last-drawn content: the line
    /// index at which the final line sits on the bottom row of the viewport.
    fn max_offset(&self) -> usize {
        self.last_content_height
            .saturating_sub(self.last_viewport_height)
    }

    /// The offset the view should actually render at, resolving auto-follow
    /// (pinned to the bottom) and clamping a frozen offset to the content.
    pub fn effective_scroll_offset(&self) -> usize {
        if self.auto_follow {
            self.max_offset()
        } else {
            self.scroll_offset.min(self.max_offset())
        }
    }

    /// Caches the metrics `draw_body` measured this frame. A resize/shrink that
    /// leaves a frozen view sitting at the bottom re-arms follow.
    pub fn set_viewport_metrics(&mut self, content_height: usize, viewport_height: usize) {
        self.last_content_height = content_height;
        self.last_viewport_height = viewport_height;
        if !self.auto_follow && self.scroll_offset >= self.max_offset() {
            self.auto_follow = true;
        }
    }

    pub fn scroll_down(&mut self, lines: usize) {
        let max = self.max_offset();
        let next = (self.effective_scroll_offset() + lines).min(max);
        self.scroll_offset = next;
        // Reaching the last line re-arms follow; otherwise stay frozen.
        self.auto_follow = next >= max;
    }

    pub fn scroll_up(&mut self, lines: usize) {
        // Anchor at the currently displayed position before moving: while
        // auto-following the stored offset is stale (draw uses `max_offset`).
        self.scroll_offset = self.effective_scroll_offset().saturating_sub(lines);
        self.auto_follow = false;
    }

    pub fn scroll_right(&mut self, cols: usize) {
        self.scroll_offset_x = self.scroll_offset_x.saturating_add(cols);
    }

    pub fn scroll_left(&mut self, cols: usize) {
        self.scroll_offset_x = self.scroll_offset_x.saturating_sub(cols);
    }

    pub fn scroll_to_bottom(&mut self) {
        self.scroll_offset = self.max_offset();
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

    pub fn pending_question(&self) -> Option<&PendingQuestion> {
        self.pending_question.as_ref()
    }

    /// Whether an `ask_user` question is awaiting an answer.
    pub fn is_asking(&self) -> bool {
        self.pending_question.is_some()
    }

    /// Move the highlighted choice by `delta`, wrapping around all choices
    /// (options plus the "Other" entry when allowed).
    pub fn question_move(&mut self, delta: isize) {
        if let Some(q) = &mut self.pending_question {
            let count = q.choice_count() as isize;
            if count > 0 {
                q.selected = (q.selected as isize + delta).rem_euclid(count) as usize;
            }
        }
    }

    /// Enter free-text mode for the "Other" entry (no-op without free-form).
    pub fn question_begin_free_form(&mut self) {
        if let Some(q) = &mut self.pending_question {
            if q.allow_free_form {
                q.selected = q.options.len();
                q.entering_free_form = true;
            }
        }
    }

    /// Leave free-text mode, returning to choice selection.
    pub fn question_cancel_free_form(&mut self) {
        if let Some(q) = &mut self.pending_question {
            q.entering_free_form = false;
        }
    }

    pub fn clear_question(&mut self) {
        self.pending_question = None;
    }

    pub fn parent(&self) -> Option<&SessionId> {
        self.parent.as_ref()
    }

    /// Elapsed run time in whole seconds given the current wall clock (`now_ms`,
    /// ms since epoch): the span from `SessionStarted` to `SessionEnded`, or to
    /// `now_ms` while still running. `None` until the session's start is known.
    /// Used to surface a sub-agent's spawn duration in the sessions list (#89).
    pub fn elapsed_secs(&self, now_ms: u64) -> Option<u64> {
        let started = self.started_ms?;
        let end = self.ended_ms.unwrap_or(now_ms).max(started);
        Some((end - started) / 1000)
    }

    /// Whether the session has ended (its final duration is now fixed).
    pub fn has_ended(&self) -> bool {
        self.ended_ms.is_some()
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
    }

    /// Records a head-side `!bash` passthrough (ADR-0030): the command and its
    /// captured output, reusing the tool call/output entries so it renders like
    /// any other tool run. Local only — not sent to the engine or the model.
    pub fn record_bash_passthrough(&mut self, command: String, output: String) {
        self.transcript.push(TranscriptEntry::ToolCall {
            tool: "!bash".to_string(),
            input: command,
        });
        self.transcript.push(TranscriptEntry::ToolOutput {
            tool: Some("!bash".to_string()),
            output,
        });
    }

    /// Applies an `OutEvent` already routed to this session. Returns `true`
    /// if it changed anything the UI needs to redraw for.
    pub fn apply_event(&mut self, event: OutEvent) -> bool {
        match event {
            OutEvent::SessionStarted { parent, ts, .. } => {
                self.parent = parent;
                self.started_ms = Some(ts);
                true
            }
            OutEvent::SessionEnded { ts, .. } => {
                self.ended_ms = Some(ts);
                true
            }
            // Supervisor-global enumeration reply (ADR-0028): not a per-session
            // view update — the app handles it, if at all.
            OutEvent::SessionList { .. } => false,
            OutEvent::Status { state, .. } => {
                self.state = state;
                if state == AgentState::Idle
                    || state == AgentState::Done
                    || state == AgentState::Error
                {
                    self.clear_approval();
                    self.clear_question();
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
                    true
                } else {
                    false
                }
            }
            OutEvent::UserQuestion {
                seq,
                request_id,
                question,
                options,
                allow_free_form,
                ..
            } => {
                if seq > self.last_seen_seq {
                    self.last_seen_seq = seq;
                    self.pending_question = Some(PendingQuestion {
                        request_id,
                        question,
                        options,
                        allow_free_form,
                        selected: 0,
                        entering_free_form: false,
                    });
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
                    true
                } else {
                    false
                }
            }
            OutEvent::Done { seq, .. } => {
                if seq > self.last_seen_seq {
                    self.transcript.push(TranscriptEntry::Done);
                    self.last_seen_seq = seq;
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
    fn user_question_sets_pending_then_status_clears() {
        use entanglement_core::QuestionOption;
        let mut v = SessionView::new();
        v.apply_event(OutEvent::UserQuestion {
            session: sid(),
            seq: 1,
            request_id: "q1".into(),
            question: "Which?".into(),
            options: vec![
                QuestionOption {
                    label: "A".into(),
                    description: None,
                },
                QuestionOption {
                    label: "B".into(),
                    description: None,
                },
            ],
            allow_free_form: true,
        });
        assert!(v.is_asking());
        let q = v.pending_question().unwrap();
        // 2 options + the "Other" entry = 3 choices.
        assert_eq!(q.choice_count(), 3);
        assert_eq!(q.selected, 0);

        // Wrap past the last option onto "Other", then back to the top.
        v.question_move(-1);
        assert!(v.pending_question().unwrap().free_form_selected());
        v.question_move(1);
        assert_eq!(v.pending_question().unwrap().selected, 0);

        // A terminal status clears the pending question.
        v.apply_event(OutEvent::Status {
            session: sid(),
            state: AgentState::Done,
        });
        assert!(!v.is_asking());
    }

    #[test]
    fn elapsed_tracks_running_then_freezes_on_end() {
        let mut v = SessionView::new();
        // Unknown until the session start is seen.
        assert_eq!(v.elapsed_secs(10_000), None);

        v.apply_event(OutEvent::SessionStarted {
            session: sid(),
            parent: Some(SessionId::new("root")),
            profile: "explore".into(),
            model: None,
            root: false,
            ts: 1_000,
        });
        // Running: measured against the current wall clock.
        assert_eq!(v.elapsed_secs(4_000), Some(3));
        assert!(!v.has_ended());

        v.apply_event(OutEvent::SessionEnded {
            session: sid(),
            ts: 6_500,
        });
        // Ended: fixed span regardless of the clock advancing further.
        assert!(v.has_ended());
        assert_eq!(v.elapsed_secs(999_999), Some(5));
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
