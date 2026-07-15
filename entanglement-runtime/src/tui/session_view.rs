use entanglement_core::{AgentState, OutEvent, QuestionOption, SessionId};
use std::collections::{HashMap, HashSet, VecDeque};

mod reducer;
mod scroll;
#[cfg(test)]
mod tests;

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
        /// `None` for head-side passthroughs (`!bash`) that never round-trip
        /// through the engine; `Some` for engine tool calls, keyed so the paired
        /// `ToolOutput` folds into this same entry (#340).
        request_id: Option<String>,
        tool: String,
        input: String,
        /// `None` until the paired `ToolOutput` arrives (still running).
        output: Option<String>,
    },
    /// Standalone out-of-band notices only (`record_status`, or a `ToolOutput`
    /// with no matching call); a tool op's real output folds into its
    /// [`TranscriptEntry::ToolCall`] (#340).
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
    task_list: Option<String>,
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
    /// FIFO of parked `(request_id, tool, input)` approvals. Core batch-emits a
    /// turn's tool calls (#270, ADR-0061), so several `ToolRequest`s can be
    /// outstanding at once; only the front is prompted, and resolving it
    /// promotes the next (#273). `approval_mode` always refers to the front.
    pending_tool_requests: VecDeque<(String, String, String)>,
    /// FIFO of parked `ask_user` questions — same batch rationale as approvals.
    pending_questions: VecDeque<PendingQuestion>,
    parent: Option<SessionId>,
    /// Wall-clock (ms since epoch) the session started / ended, from
    /// `SessionStarted` / `SessionEnded`. Drives the live spawn-duration shown
    /// for sub-agent (child) sessions in the sessions list (#89, ADR-0026).
    started_ms: Option<u64>,
    ended_ms: Option<u64>,
    /// Collapsible blocks the user has expanded, keyed by the transcript index
    /// that mints the block's stable id: a reasoning run's first `ReasoningDelta`
    /// or a tool op's `ToolCall` (#340). Absent = collapsed (the default).
    expanded_blocks: HashSet<usize>,
    /// In-progress streamed tool calls (#194): `request_id → transcript index`
    /// of the `ToolCall` entry whose `input` is growing as `ToolCallDelta`
    /// fragments arrive. The assembled `ToolCall` finalizes and removes the
    /// entry; a terminal status drops any that never finished.
    streaming_tool_calls: HashMap<String, usize>,
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
            pending_tool_requests: VecDeque::new(),
            pending_questions: VecDeque::new(),
            parent: None,
            started_ms: None,
            ended_ms: None,
            expanded_blocks: HashSet::new(),
            streaming_tool_calls: HashMap::new(),
        }
    }

    /// Whether the collapsible block identified by `id` (a reasoning run's or a
    /// tool op's minting transcript index) is currently expanded.
    pub fn block_expanded(&self, id: usize) -> bool {
        self.expanded_blocks.contains(&id)
    }

    /// Flips a collapsible block (reasoning run or tool op) between collapsed
    /// and expanded.
    pub fn toggle_block(&mut self, id: usize) {
        if !self.expanded_blocks.remove(&id) {
            self.expanded_blocks.insert(id);
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

    pub fn task_list(&self) -> Option<&String> {
        self.task_list.as_ref()
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

    pub fn approval_mode(&self) -> &ApprovalMode {
        &self.approval_mode
    }

    /// The front of the approval queue — the request currently prompted.
    pub fn pending_tool_request(&self) -> Option<&(String, String, String)> {
        self.pending_tool_requests.front()
    }

    /// Approvals parked behind the prompted one (#273).
    pub fn queued_approvals(&self) -> usize {
        self.pending_tool_requests.len().saturating_sub(1)
    }

    pub fn set_approval_mode(&mut self, mode: ApprovalMode) {
        self.approval_mode = mode;
    }

    /// Pops the front approval (just answered) and promotes the next queued
    /// request, if any, so its prompt surfaces immediately (#273).
    pub fn advance_approval(&mut self) {
        self.pending_tool_requests.pop_front();
        self.approval_mode = match self.pending_tool_requests.front() {
            Some((request_id, ..)) => ApprovalMode::WaitingForApproval {
                request_id: request_id.clone(),
            },
            None => ApprovalMode::Normal,
        };
    }

    /// Drops the whole approval queue — a turn interrupt or terminal status
    /// invalidates every parked request, not just the prompted one.
    pub fn clear_approval(&mut self) {
        self.approval_mode = ApprovalMode::Normal;
        self.pending_tool_requests.clear();
    }

    pub fn is_waiting_approval(&self) -> bool {
        matches!(
            self.approval_mode,
            ApprovalMode::WaitingForApproval { .. } | ApprovalMode::EnteringRejectReason { .. }
        )
    }

    /// The front of the question queue — the question currently prompted.
    pub fn pending_question(&self) -> Option<&PendingQuestion> {
        self.pending_questions.front()
    }

    /// Whether an `ask_user` question is awaiting an answer.
    pub fn is_asking(&self) -> bool {
        !self.pending_questions.is_empty()
    }

    /// Move the highlighted choice by `delta`, wrapping around all choices
    /// (options plus the "Other" entry when allowed).
    pub fn question_move(&mut self, delta: isize) {
        if let Some(q) = self.pending_questions.front_mut() {
            let count = q.choice_count() as isize;
            if count > 0 {
                q.selected = (q.selected as isize + delta).rem_euclid(count) as usize;
            }
        }
    }

    /// Enter free-text mode for the "Other" entry (no-op without free-form).
    pub fn question_begin_free_form(&mut self) {
        if let Some(q) = self.pending_questions.front_mut() {
            if q.allow_free_form {
                q.selected = q.options.len();
                q.entering_free_form = true;
            }
        }
    }

    /// Leave free-text mode, returning to choice selection.
    pub fn question_cancel_free_form(&mut self) {
        if let Some(q) = self.pending_questions.front_mut() {
            q.entering_free_form = false;
        }
    }

    /// Pops the front question (just answered) so the next queued one, if any,
    /// surfaces with its own fresh selection state (#273).
    pub fn advance_question(&mut self) {
        self.pending_questions.pop_front();
    }

    /// Drops the whole question queue (turn interrupt / terminal status).
    pub fn clear_question(&mut self) {
        self.pending_questions.clear();
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
}

impl Default for SessionView {
    fn default() -> Self {
        Self::new()
    }
}
