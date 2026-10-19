use entanglement_core::{AgentState, CompactionMode, OutEvent, SessionId};
use ratatui::text::Line;
use std::collections::{HashMap, HashSet, VecDeque};

use crate::tui::markdown::MarkdownRenderer;
use crate::tui::theme::{RoleColors, Theme};
use crate::tui::transcript::cache::RenderCache;

mod approval;
mod cost;
mod generation;
mod question;
mod reducer;
mod scroll;
#[cfg(test)]
mod tests;
mod usage;

pub use approval::PendingRequest;
pub use cost::CostLedger;
pub use question::PendingQuestion;
pub use usage::RoundUsage;

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
        /// Folded from `OutEvent::ToolOutput::is_error` (#672, ADR-0176) when
        /// the paired output arrives; stays `false` while running and for a
        /// head-side passthrough, which never carries the field.
        is_error: bool,
        /// A `rhai` op's live nested-call lines (ADR-0233), folded in by
        /// `parent_request_id` so they stay under their own script — and
        /// collapse with it. Empty for every other op, and after a resume.
        nested: Vec<NestedLine>,
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
    /// A nested-call line whose `rhai` op is not in the transcript (ADR-0233)
    /// — kept standalone rather than dropped, like an unmatched `ToolOutput`.
    NestedCall(NestedLine),
    Done,
}

/// One compact line of a `rhai` script's nested-call activity (ADR-0233): a
/// call, or the closing "+K more" summary. Live only — never in a resumed
/// transcript or an export.
#[derive(Debug, Clone, PartialEq, Hash)]
pub struct NestedLine {
    pub line: String,
    pub is_error: bool,
    /// How many calls the line stands for: 1 for a call, `suppressed` for a
    /// summary — so an op's header counts calls, not lines.
    pub calls: u64,
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
    /// The session's live permission mode (ADR-0207), folded from
    /// `OutEvent::ModeChanged` the same way `agent` folds `AgentChanged` —
    /// every session gets one unconditionally at start (`session.rs` sends it
    /// right after `AgentChanged`). Needed by `/allow` (#634): a `SessionDir`
    /// grant is mode-scoped, so recording one has to know the mode it's
    /// earned under.
    mode: String,
    /// The session's working directory (#764), from `WorkdirChanged`.
    workdir: Option<String>,
    state: AgentState,
    transcript: Vec<TranscriptEntry>,
    /// Some `User` entry may still be `pending` (dimmed). Lets
    /// `clear_pending_user` — called on every streamed delta — return at once
    /// instead of scanning back to the last prompt each time: a resumed log
    /// with a million deltas and few prompts made that scan quadratic and
    /// froze the TUI for minutes.
    user_pending: bool,
    plan: Option<String>,
    /// Root-relative location of the session's bound plan file (#513), from
    /// the same `OutEvent::Plan` snapshot as `plan` — empty-string logs
    /// pre-513 replay carry no real path, so `plan_path()` treats `""` as
    /// `None` too.
    plan_path: Option<String>,
    task_list: Option<String>,
    last_seen_seq: u64,
    /// Top-anchored vertical offset (line index of the first visible row).
    /// Only meaningful while frozen; when `auto_follow` is set the view is
    /// pinned to the bottom at draw time and this value is ignored.
    scroll_offset: usize,
    auto_follow: bool,
    /// Rendered line count and viewport height cached from the last draw so
    /// scroll math can clamp/anchor without re-deriving them — only `draw_body`
    /// knows the wrapped line count and `area.height`.
    last_content_height: usize,
    last_viewport_height: usize,
    approval_mode: ApprovalMode,
    /// FIFO of parked approval cards. Core batch-emits a
    /// turn's tool calls (#270, ADR-0061), so several `ToolRequest`s can be
    /// outstanding at once; only the front is prompted, and resolving it
    /// promotes the next (#273). `approval_mode` always refers to the front.
    pending_tool_requests: VecDeque<PendingRequest>,
    /// FIFO of parked `ask_user` questions — same batch rationale as approvals.
    pending_questions: VecDeque<PendingQuestion>,
    /// Snippet of the session's first user prompt — the live "what is this
    /// session about" label the sidebar shows under the session row. Engine-
    /// spawned children never record a user message head-side, so they simply
    /// have none; the resume path replays `Prompt` records through
    /// `record_user_message`, so a restored view gets it for free.
    first_prompt: Option<String>,
    /// Display name set via `SetSessionMeta` (folded from
    /// `SessionMetaChanged`): the sidebar/modal title, preferred over the
    /// short id when set.
    name: Option<String>,
    /// Current action ("what the agent is doing now"), same fold; the sidebar
    /// description line prefers it over `first_prompt` when set.
    action: Option<String>,
    parent: Option<SessionId>,
    /// The session this one is a `/fork` of (#786), for the lineage label.
    forked_from: Option<SessionId>,
    /// The session this one is the compaction successor of (ADR-0205): a
    /// moved card leaves its view (ADR-0238).
    predecessor: Option<SessionId>,
    /// Wall-clock (ms since epoch) the session started / ended, from
    /// `SessionStarted` / `SessionEnded`. Drives the live spawn-duration shown
    /// for sub-agent (child) sessions in the sessions list (#89, ADR-0026).
    started_ms: Option<u64>,
    ended_ms: Option<u64>,
    /// Collapsible blocks the user has expanded, keyed by the transcript index
    /// that mints the block's stable id: a reasoning run's first `ReasoningDelta`
    /// or a tool op's `ToolCall` (#340). Absent = collapsed (the default).
    expanded_blocks: HashSet<usize>,
    /// Token/cost accounting folded from `OutEvent::Usage` (#192, #560),
    /// per session and held on the view (not head-global) so a resumed
    /// session restores it — the resume path replays persisted records
    /// through `apply_event`.
    cost: CostLedger,
    /// In-progress streamed tool calls (#194): `request_id → transcript index`
    /// of the `ToolCall` entry whose `input` is growing as `ToolCallDelta`
    /// fragments arrive. The assembled `ToolCall` finalizes and removes the
    /// entry; a terminal status drops any that never finished.
    streaming_tool_calls: HashMap<String, usize>,
    /// Per-block render memo (#342): keyed on content hash so an idle redraw
    /// re-parses zero markdown and only the block that changed re-renders.
    render_cache: RenderCache,
}

impl SessionView {
    pub fn new() -> Self {
        Self {
            agent: "build".to_string(),
            // Mirrors `entanglement_core::holly::DEFAULT_MODE` — overwritten
            // by the real `ModeChanged` every session emits unconditionally
            // at start; this is only ever observed by a view that hasn't
            // folded its first event yet.
            mode: "build".to_string(),
            workdir: None,
            state: AgentState::Idle,
            transcript: Vec::new(),
            user_pending: false,
            plan: None,
            plan_path: None,
            task_list: None,
            last_seen_seq: 0,
            scroll_offset: 0,
            auto_follow: true,
            last_content_height: 0,
            last_viewport_height: 0,
            approval_mode: ApprovalMode::Normal,
            pending_tool_requests: VecDeque::new(),
            pending_questions: VecDeque::new(),
            first_prompt: None,
            name: None,
            action: None,
            parent: None,
            forked_from: None,
            predecessor: None,
            started_ms: None,
            ended_ms: None,
            expanded_blocks: HashSet::new(),
            streaming_tool_calls: HashMap::new(),
            render_cache: RenderCache::new(),
            cost: CostLedger::default(),
        }
    }

    /// Renders the transcript body through the per-block render cache (#342):
    /// only blocks whose content hash changed re-render; the rest are cloned
    /// from the memo. Returns the owned lines plus per-line block provenance.
    pub(crate) fn render_body(
        &mut self,
        md: &MarkdownRenderer,
        theme: Theme,
        user: RoleColors,
        width: u16,
    ) -> (Vec<Line<'static>>, Vec<Option<usize>>) {
        let Self {
            transcript,
            expanded_blocks,
            render_cache,
            ..
        } = self;
        render_cache.render(
            transcript,
            |id| expanded_blocks.contains(&id),
            md,
            theme,
            user,
            width,
        )
    }

    /// Blocks re-rendered on the last [`Self::render_body`] pass (a test hook;
    /// `0` when an unchanged redraw reused every cached block).
    #[cfg(test)]
    pub(crate) fn last_render_rebuilt(&self) -> usize {
        self.render_cache.last_rebuilt()
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

    pub fn mode(&self) -> &str {
        &self.mode
    }

    pub fn state(&self) -> AgentState {
        self.state
    }

    pub fn transcript(&self) -> &[TranscriptEntry] {
        &self.transcript
    }

    /// The highest content-event seq this view has folded — the seq-dedupe
    /// watermark. Used by the compaction-fork interceptor (ADR-0101) to tell a
    /// live `Compacted` from a replayed duplicate.
    pub fn last_seen_seq(&self) -> u64 {
        self.last_seen_seq
    }

    pub fn plan(&self) -> Option<&String> {
        self.plan.as_ref()
    }

    /// The bound plan file's root-relative location (#513), or `None` when no
    /// plan has been proposed yet (or a pre-513 log carried no path).
    pub fn plan_path(&self) -> Option<&str> {
        self.plan_path.as_deref().filter(|p| !p.is_empty())
    }

    pub fn task_list(&self) -> Option<&String> {
        self.task_list.as_ref()
    }

    /// Whole billed prompt volume for this session (uncached + cache-read +
    /// cache-write, every purpose) — the "in" figure a rollup sums (#192).
    pub fn input_tokens(&self) -> u64 {
        self.cost.prompt_total()
    }

    pub fn output_tokens(&self) -> u64 {
        self.cost.output_tokens()
    }

    /// Accumulated session cost in USD, summed from `OutEvent::Usage`.
    pub fn cost_usd(&self) -> f64 {
        self.cost.cost_usd()
    }

    /// Whether [`Self::cost_usd`] reflects real catalog pricing (`true`) or is
    /// just the zero default because no round so far carried pricing
    /// (`false`) — see [`CostLedger`] (#560).
    pub fn cost_known(&self) -> bool {
        self.cost.cost_known()
    }

    /// Cumulative cache-read input tokens (#560).
    pub fn cached_input_tokens(&self) -> u64 {
        self.cost.cached_tokens()
    }

    /// The full per-purpose / per-model ledger behind the accessors above —
    /// what the status bar and `/cost` render from.
    pub fn cost(&self) -> &CostLedger {
        &self.cost
    }

    pub fn scroll_offset(&self) -> usize {
        self.scroll_offset
    }

    pub fn auto_follow(&self) -> bool {
        self.auto_follow
    }

    pub fn approval_mode(&self) -> &ApprovalMode {
        &self.approval_mode
    }

    /// The session's first-prompt snippet, if a user message was recorded.
    pub fn first_prompt(&self) -> Option<&str> {
        self.first_prompt.as_deref()
    }

    /// Display name set via `/name`/`SetSessionMeta`, if any.
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// Current action ("what the agent is doing now"), if set.
    pub fn action(&self) -> Option<&str> {
        self.action.as_deref()
    }

    pub fn parent(&self) -> Option<&SessionId> {
        self.parent.as_ref()
    }

    pub fn forked_from(&self) -> Option<&SessionId> {
        self.forked_from.as_ref()
    }

    pub fn predecessor(&self) -> Option<&SessionId> {
        self.predecessor.as_ref()
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
