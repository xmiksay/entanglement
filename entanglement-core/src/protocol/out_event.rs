//! Outbound events: the [`OutEvent`] wire enum. A serde-tagged wire enum
//! cannot be split across files without macros, so it stays whole here and
//! this file holds nothing else (allowlisted over the file cap). Its small
//! companion types live in `parts.rs`; the `session`/`seq` accessors in
//! `accessors.rs`.

use entanglement_provider::{ContentPart, GenerationParams, Message, UserId};
use serde::{Deserialize, Serialize};

#[cfg(doc)]
use super::InMsg;
use super::{
    AgentState, FileChangeKind, McpAction, McpAuthStatus, McpServerStatus, OperationInfo,
    PendingApproval, PendingQuestion, Questions, RevertSkip, SessionId, SessionInfo,
    ToolOverlayEntry,
};

mod accessors;
mod parts;

pub use parts::{CompactionMode, ToolEnvelope, UsagePurpose};

/// Outbound: engine → harness. Cloned through a `broadcast` channel, so every
/// variant must be `Clone`.
///
/// Content variants ([`Plan`][OutEvent::Plan],
/// [`TextDelta`][OutEvent::TextDelta],
/// [`ToolRequest`][OutEvent::ToolRequest], [`ToolOutput`][OutEvent::ToolOutput],
/// [`TaskList`][OutEvent::TaskList], [`Usage`][OutEvent::Usage],
/// [`Error`][OutEvent::Error], [`Done`][OutEvent::Done]) carry a monotonic
/// per-session `seq`.
///
/// Not `Eq`: [`Usage::cost_usd`][OutEvent::Usage] is a floating-point dollar
/// amount, so the enum is `PartialEq` only (#192).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OutEvent {
    /// Session started (lifecycle event, no `seq`). Emits when a session is spawned.
    SessionStarted {
        session: SessionId,
        parent: Option<SessionId>,
        /// The session this one succeeds (a `/compact` fork, ADR-0110); `None`
        /// for an ordinary session. Non-live lineage — the predecessor's
        /// interactive session is closed once the successor starts.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        predecessor: Option<SessionId>,
        /// Spelled `profile` before ADR-0207; the alias keeps 0.6.0 logs loading (#720).
        #[serde(alias = "profile")]
        agent: String,
        model: Option<String>,
        root: bool,
        ts: u64,
        /// Owning user in a multi-user deployment (#522), resolved from
        /// [`InMsg::Spawn`]'s `user` (root spawn) or inherited from the
        /// parent/predecessor. `None` in single-user mode. Persisted so replay
        /// reconstructs [`Session::user`][crate::session::Session] without an
        /// embedder re-supplying it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        user: Option<UserId>,
    },
    /// Session ended (lifecycle event, no `seq`). Emits when a session exits.
    SessionEnded { session: SessionId, ts: u64 },
    /// Session evicted from memory but **not** tombstoned (lifecycle event, no
    /// `seq`), in reply to [`InMsg::HibernateSession`] (#318, ADR-0077). The
    /// session task tore down and released its [`Context`][crate::context::Context],
    /// but the id stays resumable — distinct from
    /// [`SessionEnded`][OutEvent::SessionEnded], whose `CloseSession` origin
    /// tombstones the id. A head renders it like an end; a persistence tap can
    /// observe the eviction. `Holly::resume` on the id rebuilds it from the log.
    SessionHibernated { session: SessionId, ts: u64 },
    /// Snapshot of every currently-live session (lifecycle event, no `seq`),
    /// sent in reply to [`InMsg::ListSessions`] (ADR-0028). `correlation_id`
    /// echoes the requester's opaque token so a multiplexed head can pair the
    /// reply with its request (#160, ADR-0072) — no longer an overloaded
    /// [`SessionId`], so this event names no session and [`session`][OutEvent::session]
    /// is `None` for it.
    SessionList {
        correlation_id: String,
        sessions: Vec<SessionInfo>,
    },
    /// Snapshot of every currently-open `ask_user` question (lifecycle event,
    /// no `seq`), in reply to [`InMsg::ListQuestions`] (#515) — same
    /// "session-less enumeration" shape as
    /// [`SessionList`][OutEvent::SessionList]. Answered by the runtime
    /// service that owns the open-question registry `ask_user` populates and
    /// clears (`entanglement_runtime::questions::OpenQuestions`), not core.
    QuestionList {
        correlation_id: String,
        questions: Vec<PendingQuestion>,
    },
    /// Snapshot of every currently-outstanding background job / launched
    /// sub-agent (lifecycle event, no `seq`), in reply to
    /// [`InMsg::ListOperations`] (#607, ADR-0161 §6) — same "session-less
    /// enumeration" shape as [`QuestionList`][OutEvent::QuestionList].
    /// Answered by the runtime service that owns the job/agent registries,
    /// not core.
    OperationList {
        correlation_id: String,
        operations: Vec<OperationInfo>,
    },
    /// Snapshot of every tool call currently parked awaiting approval
    /// (lifecycle event, no `seq`), in reply to [`InMsg::ListPending`] (#726)
    /// — same "session-less enumeration" shape as
    /// [`QuestionList`][OutEvent::QuestionList]. Answered by the runtime
    /// service that owns the parked-decision registry, not core.
    PendingList {
        correlation_id: String,
        approvals: Vec<PendingApproval>,
    },
    /// Snapshot of every currently-attached MCP server (lifecycle event, no
    /// `seq`), in reply to [`InMsg::McpList`] (#375). Answered by the runtime
    /// service that owns the live server connections — same "engine-global, not
    /// core's business" shape as [`SessionList`][OutEvent::SessionList].
    McpList {
        correlation_id: String,
        servers: Vec<McpServerStatus>,
    },
    /// An MCP server was hot-added or removed (lifecycle event, no `seq`), in
    /// reply to [`InMsg::McpAdd`]/[`InMsg::McpRemove`] (#375).
    McpChanged { name: String, action: McpAction },
    /// Engine-global (no `session`, no `seq`), in reply to
    /// [`InMsg::McpAuth`] (ADR-0153). A `Connect` emits this twice: once
    /// carrying `authorize_url` while the browser consent is pending, then
    /// again with the terminal `state` or `error`.
    McpAuthChanged { status: McpAuthStatus },
    /// An LLM endpoint's throttle posture changed (lifecycle event, no `seq`,
    /// #517). The provider's per-endpoint resilience pool (`HttpClient`, ADR-0050/
    /// ADR-0111) is **engine-global, not per-session** — many sessions can share
    /// one endpoint, and one throttled endpoint never blocks another — so this
    /// mirrors [`McpChanged`][OutEvent::McpChanged]'s
    /// shape rather than naming a session. Emitted by the runtime's throttle
    /// responder polling `HttpClient::throttle_statuses()` (it alone owns the
    /// `HttpClient`), **only on a transition** — entering/leaving a 429 cool-down,
    /// the adaptive pacing gate penalizing/relaxing, or clearing back to rest —
    /// not on every poll tick, so a remote (stdio/WS) head isn't spammed while a
    /// stall holds steady. `throttled` is `false` exactly when every other field
    /// but `endpoint`/`in_flight`/`cap` is at rest — a head clears its indicator
    /// on that transition. `retry_in_ms`/`pacing_in_ms` mirror
    /// [`ThrottleStatus`][entanglement_provider::ThrottleStatus]'s
    /// `backoff_remaining`/`next_request_in` (milliseconds, so the wire carries
    /// no `serde`-dependent `Duration` representation); at most one is ever
    /// `Some` at a time (a 429 cool-down always wins over pacing, ADR-0141).
    /// `retry_in_ms` already folds in a peer process's shared cool-down, not
    /// just this process's own (#552). `waiters`/`shared_leases` surface the
    /// two other gaps #552 closed: how many callers are queued behind this
    /// endpoint's own permit (#517), and how many
    /// cross-process leases (#523) are live against it — `shared_leases` is
    /// `None` when cross-process sharing is disabled or unreadable. Both are
    /// `#[serde(default)]` so a pre-#552 persisted log still replays.
    Throttle {
        endpoint: String,
        throttled: bool,
        in_flight: usize,
        cap: usize,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        retry_in_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pacing_in_ms: Option<u64>,
        #[serde(default)]
        waiters: usize,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        shared_leases: Option<usize>,
    },
    /// The runtime's persistence tap could not append to `root`'s session log
    /// (#767): the session keeps running, but its log has stopped. Emitted
    /// once, on the **first** failure of an outage — the transition, like
    /// [`Throttle`][OutEvent::Throttle] (ADR-0141), never per lost record.
    /// `root` is data, not routing: like `Throttle` this is engine-global (no
    /// `session`, no `seq`), so it is never persisted or replayed — which is
    /// also what keeps the tap from trying to write it into the very sink that
    /// is failing. `error` is the sink's error summary.
    PersistenceDegraded { root: SessionId, error: String },
    /// `root`'s session log is writable again (#767): the gap tombstone for
    /// the outage landed, recording `lost` dropped records (the in-tree
    /// resume refuses a gapped log; an embedder's `resumable_prefix` cuts
    /// there). Paired one-to-one with a prior
    /// [`PersistenceDegraded`][OutEvent::PersistenceDegraded]; an outage the
    /// process never recovers from ends with no `PersistenceRecovered`.
    PersistenceRecovered { root: SessionId, lost: u64 },
    /// A session's persisted content history from a requested `after_seq`, in
    /// reply to [`InMsg::ReplayFrom`] (#160, ADR-0072). Answered by the runtime's
    /// history responder — which owns the event log — not the core supervisor.
    /// `events` holds every content [`OutEvent`] whose `seq` exceeds the requested
    /// `after_seq`, in log order; `correlation_id` echoes the request so a
    /// multiplexed head can pair the reply. A lifecycle query reply (no `seq`); it
    /// is neither persisted nor folded on replay.
    History {
        correlation_id: String,
        session: SessionId,
        events: Vec<OutEvent>,
    },
    /// Lifecycle state change (point-in-time, no `seq`).
    Status {
        session: SessionId,
        state: AgentState,
    },
    /// The session switched agent profiles (point-in-time, no `seq`). ADR-0207
    /// moved every permission fact off the profile and onto the session's
    /// independent permission mode axis (see [`ModeChanged`][OutEvent::ModeChanged]),
    /// so this carries only the new profile's name.
    AgentChanged { session: SessionId, agent: String },
    /// The session switched permission mode (point-in-time, no `seq`), in reply
    /// to [`InMsg::SetMode`] (ADR-0207) — and once more at session start so the
    /// log records the mode a session began in (needed by replay and by the
    /// appended in-conversation notice, see [`Session::mode`][crate::session::Session]).
    /// Carries only the opaque `mode` name; core carries no rule table to
    /// resolve it against — the runtime owns what a mode means. Folded on
    /// replay by overwrite (last write wins),
    /// the same shape as [`GenerationChanged`][OutEvent::GenerationChanged]/
    /// [`ToolOverlayChanged`][OutEvent::ToolOverlayChanged].
    ModeChanged { session: SessionId, mode: String },
    /// The session switched to a different model/provider mid-run (point-in-time,
    /// no `seq`), in reply to [`InMsg::SetModel`] (#218). Carries the resolved
    /// `provider`/`model` and the new `context_window` (tokens) so a head can
    /// update its context bar / model display without re-reading the catalog.
    /// Replay re-applies it to re-bind a resumed session to the switched model.
    ModelChanged {
        session: SessionId,
        provider: String,
        model: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context_window: Option<usize>,
    },
    /// The session's live generation knobs changed (point-in-time, no `seq`),
    /// in reply to [`InMsg::SetGeneration`] (#374, ADR-0094) or an implicit
    /// overlay applied at session start. Carries the **full** resolved
    /// [`GenerationParams`] — not just what changed — so a head can render the
    /// effective state directly and so replay can restore it verbatim by simply
    /// overwriting [`Session::generation`][crate::session::Session]. Also folded
    /// into `Session::generation_by_agent` on replay, so a resumed session's live
    /// override survives the session-start default re-application.
    GenerationChanged {
        session: SessionId,
        generation: GenerationParams,
    },
    /// The session's live tool overlay changed (point-in-time, no `seq`), in
    /// reply to [`InMsg::SetToolOverlay`] (#539, ADR-0149). Carries the **full**
    /// effective entry list — not a delta — so a head can render/track the
    /// overlay directly and replay restores it by overwriting
    /// [`Session::tool_overlay`][crate::session::Session] verbatim (the
    /// `GenerationChanged` pattern). The runtime's tool executor also folds it
    /// to honor the overlay at dispatch (mask + grade override).
    ToolOverlayChanged {
        session: SessionId,
        entries: Vec<ToolOverlayEntry>,
    },
    /// Ack to [`InMsg::SetSessionMeta`] carrying the session's full merged
    /// display metadata (both fields as stored, not just the ones the set
    /// touched). Lifecycle-shaped like
    /// [`GenerationChanged`][OutEvent::GenerationChanged]: seq-less, persisted,
    /// folded on replay by overwrite (last write wins). Heads fold it into
    /// their session views (the `AgentChanged` pattern) — it is deliberately
    /// **not** mirrored into [`SessionInfo`]/[`SessionList`][OutEvent::SessionList],
    /// whose supervisor directory records creation-time facts only.
    SessionMetaChanged {
        session: SessionId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        action: Option<String>,
    },
    /// The agent's strategy plan (markdown prose), full snapshot on every change.
    /// Emitted by the runtime when it resolves a `propose_plan` call (#513,
    /// ADR-0145, superseding ADR-0049's `update_plan` half); the engine never
    /// stores or consumes the plan. `path` is the plan file's location
    /// root-relative to the project (e.g. `.entanglement/plans/<id>.md`) — the
    /// runtime always resolves one, since #513 makes a plan a file. `#[serde(default)]`
    /// so a pre-#513 persisted log (no `path` field) still deserializes, with an
    /// empty string standing in for "unknown".
    Plan {
        session: SessionId,
        seq: u64,
        content: String,
        #[serde(default)]
        path: String,
    },
    /// Incremental assistant text.
    TextDelta {
        session: SessionId,
        seq: u64,
        text: String,
    },
    /// Incremental reasoning/thinking text.
    ReasoningDelta {
        session: SessionId,
        seq: u64,
        text: String,
    },
    /// Incremental tool-call argument fragment (#194), correlated to the
    /// eventual [`ToolCall`][OutEvent::ToolCall]/[`ToolExec`][OutEvent::ToolExec]
    /// by `request_id`. Display-only: lets a head render file-sized `edit`/
    /// `write` arguments as they stream, before the assembled `ToolCall` (which
    /// core emits once at round end, ADR-0061) arrives. `tool` labels the stream;
    /// `delta` is a raw JSON-argument substring — fragments concatenated in
    /// arrival order rebuild the call `input`. Additive: a head that ignores it
    /// still gets the full `ToolCall`, so replay reconstructs state from that
    /// (this variant is skipped on the replay fold).
    ToolCallDelta {
        session: SessionId,
        seq: u64,
        request_id: String,
        tool: String,
        delta: String,
    },
    /// A tool is being called or about to be approved (display-only). Emitted
    /// for every tool call, before execution, so heads can show what's being called
    /// (tool name + input arguments). Separate from `ToolRequest` which handles
    /// approval mode.
    ToolCall {
        session: SessionId,
        seq: u64,
        request_id: String,
        tool: String,
        input: String,
        /// The call's opaque provider metadata (e.g. a Gemini thought
        /// signature), so replay rebuilds the call the provider signed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_meta: Option<serde_json::Value>,
        /// The call exactly as the model emitted it, when core unwrapped an
        /// `invoke` envelope (ADR-0204): `tool`/`input` then name the inner
        /// call. Absent for every ordinary call.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        envelope: Option<ToolEnvelope>,
    },
    /// Engine wants to run a host tool (permission `Ask`) and is pausing for approval.
    ToolRequest {
        session: SessionId,
        seq: u64,
        request_id: String,
        tool: String,
        input: String,
    },
    /// Core asks the runtime to execute a host tool that is cleared to run
    /// (permission `Allow`, or `Ask` after approval). The runtime executes it
    /// and replies with [`InMsg::ToolResult`]. Distinct from
    /// [`ToolRequest`][OutEvent::ToolRequest] (human approval) and
    /// [`ToolCall`][OutEvent::ToolCall] (display-only): only `ToolExec` drives
    /// execution, so a denied tool never runs (ADR-0006/0010).
    ///
    /// `agent` carries the emitting session's active profile name (#156): the
    /// runtime executor resolves permission/mask against it *authoritatively*,
    /// self-healing its per-session profile map from this field instead of the
    /// lossy `SessionStarted`/`AgentChanged` broadcast fold — so a dropped
    /// lifecycle event under burst can no longer silently downgrade a restricted
    /// session to allow-all/unmasked. `#[serde(default)]` keeps pre-#156 logs
    /// deserializable (empty ⇒ the executor falls back to its folded state).
    ToolExec {
        session: SessionId,
        seq: u64,
        request_id: String,
        tool: String,
        input: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        agent: String,
        /// The call exactly as the model emitted it, when core unwrapped an
        /// `invoke` envelope (ADR-0204): `tool`/`input` then name the inner
        /// call. Absent for every ordinary call.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        envelope: Option<ToolEnvelope>,
    },
    /// The model asked the user one or more decision questions in a single
    /// `ask_user` call (#488, supersedes parts of ADR-0027: one event now
    /// carries a `questions` array, and a free-text answer is always
    /// available rather than gated by a per-call `allow_free_form`). A head
    /// walks `questions` in order and replies once with
    /// [`InMsg::AnswerQuestion`]; the runtime folds every answer back as the
    /// one tool call's output. Dedicated (not
    /// [`ToolRequest`][OutEvent::ToolRequest]) so choices render cleanly.
    /// `questions` flattens onto the wire via [`Questions`] so a legacy
    /// single-question log still deserializes.
    UserQuestion {
        session: SessionId,
        seq: u64,
        request_id: String,
        #[serde(flatten)]
        questions: Questions,
    },
    /// Result of an executed tool, a denied tool, or a built-in tool. `output` is
    /// the text rendering heads display (for an image result, a short
    /// placeholder). `content` carries the full multimodal result (#221) — empty
    /// for the common text-only case (heads read `output`), populated with the
    /// image block(s) when `read` opens an image so **replay** reconstructs the
    /// model's view faithfully instead of degrading it to the placeholder.
    ///
    /// `is_error`/`duration_ms` (#636, ADR-0176) mirror the same fields on
    /// [`InMsg::ToolResult`] — the display-side half of the structured side
    /// channel, so a head can style a failed call (denied/masked/refused, or the
    /// tool itself erroring) without pattern-matching `output`'s text. Both
    /// default off so a pre-#636 log replays unchanged.
    ///
    /// `exit_code` (#681, ADR-0186) mirrors [`InMsg::ToolResult`]'s field: the
    /// numeric exit status when the call ran a process (`bash`/`call`, or a
    /// `poll` observing a job exit), `None` otherwise — so a wire consumer can
    /// branch on the code without parsing the `[exit N]` text.
    ToolOutput {
        session: SessionId,
        seq: u64,
        request_id: String,
        tool: String,
        output: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        content: Vec<ContentPart>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        is_error: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
        /// The call exactly as the model emitted it, when core unwrapped an
        /// `invoke` envelope (ADR-0204): `tool`/`input` then name the inner
        /// call. Absent for every ordinary call.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        envelope: Option<ToolEnvelope>,
    },
    /// Full snapshot of the session's task outline (sent on every change).
    /// Markdown, typically a `- [ ]`/`- [x]` checklist — displayed to the user
    /// as progress info, never parsed by the engine (mirrors
    /// [`Plan`][OutEvent::Plan]). Emitted by the runtime on an `update_tasks`
    /// state tool call (#231, ADR-0049).
    TaskList {
        session: SessionId,
        seq: u64,
        content: String,
    },
    /// Token usage + cost for one model round-trip, folded from the provider's
    /// `LlmEvent::Finish` (#192, ADR-0054). Counts are the normalized per-round-trip
    /// deltas (not cumulative); a head accumulates them for a session total.
    /// `input_tokens` is the *uncached* prompt portion — the whole prompt as
    /// billed is `input_tokens + cached_input_tokens + cache_write_tokens`.
    /// `cost_usd` is `None` when no catalog pricing covers the active model.
    /// `purpose` says which request the round was (a turn round, or a
    /// compaction summary) so a head can total spend while reading context
    /// size only off turn rounds; absent on the wire ⇒ `turn`.
    Usage {
        session: SessionId,
        seq: u64,
        input_tokens: u64,
        output_tokens: u64,
        cached_input_tokens: u64,
        cache_write_tokens: u64,
        cost_usd: Option<f64>,
        #[serde(default)]
        purpose: UsagePurpose,
    },
    /// Recoverable error surfaced to the UI; the engine stays alive.
    Error {
        session: SessionId,
        seq: u64,
        message: String,
    },
    /// Turn finished cleanly. Heads waiting on a one-shot turn exit on this.
    Done { session: SessionId, seq: u64 },
    /// Session compaction ran (#324, ADR-0082 → ADR-0101/0103/0205): this
    /// session was compacted into a **successor session**, and this event is
    /// the announcement. Since [ADR-0205](../../docs/adr/0205-every-compaction-forks-a-successor-session.md)
    /// **every** compaction forks — the source session's `Context` is never
    /// mutated, on any path — so this is always a *report*, never a
    /// confirmation of mutation, and `Session::replay`'s fold is always a
    /// **no-op**: there is nothing to reconstruct, because nothing was
    /// rewritten behind the log.
    ///
    /// The successor itself is announced separately by its own
    /// [`SessionStarted`][OutEvent::SessionStarted] carrying
    /// `predecessor: Some(this session)` — that pre-existing lineage field,
    /// not a field here, is how a head learns the successor's id and follows
    /// it. The source is retired (`CloseSession`) right after, so its log
    /// ends exactly at this event.
    ///
    /// `auto` and `mode` together name which of the three compaction paths ran:
    ///
    /// - `auto: false`, `mode: Summary` (both defaults) — **manual
    ///   `/compact`** (`InMsg::Oneshot`, ADR-0101/0110). Every pre-#398 record
    ///   on the wire is this case.
    /// - `auto: true`, `mode: Summary` — **auto-summarize on context
    ///   overflow** (#398, ADR-0103, amended by ADR-0205: it forks now rather
    ///   than mutating the live context).
    /// - `auto: true`, `mode: Prune` — the **prune-only fallback**, which
    ///   placeholder-prunes the oldest tool outputs when no LLM summary is
    ///   available. `summary` is then the pruned transcript that seeds the
    ///   successor, not an LLM-authored summary. ADR-0121's silence for this
    ///   path is retired by ADR-0205: a head must follow the fork, so it has
    ///   to be told.
    ///
    /// A persisted, seq-bearing content event in every case (persistence is
    /// variant-agnostic; seq-bearing ⇒ folded into `ReplayFrom` history).
    /// `kept` is how many trailing messages — clamped to the nearest safe
    /// turn boundary (#397, ADR-0102) — ride verbatim inside `summary`,
    /// appended after the LLM-generated summary of everything before them;
    /// `0` (the default) means the whole history was summarized with no
    /// verbatim tail, matching every pre-#397 record.
    Compacted {
        session: SessionId,
        seq: u64,
        summary: String,
        #[serde(default)]
        kept: u64,
        #[serde(default)]
        auto: bool,
        #[serde(default)]
        mode: CompactionMode,
    },
    /// File change record (audit log entry). Emitted by the runtime's tool
    /// executor after each successful `edit`/`write`/`apply_patch` (#202, #455).
    /// The record carries
    /// the `path`, `change_kind`, and a content **hash** (lowercase hex SHA-256
    /// of the after-content) — not the whole-file bytes: the event fans out to
    /// every subscriber, so a large edit must not clone its contents per head.
    FileChange {
        session: SessionId,
        seq: u64,
        path: String,
        change_kind: FileChangeKind,
        hash: String,
    },
    /// A bound plan file changed on disk **out of band** — i.e. not through this
    /// session's own `edit`/`write`/`apply_patch` (#627, ADR-0145
    /// "Consequences"). `propose_plan`'s staleness guard already refuses a stale
    /// `path` resubmit at the *next* `propose_plan` call; this is the same
    /// detection surfaced live, off a dedicated plans-folder watch, so a head
    /// doesn't have to wait for the next tool call to learn the bound file moved
    /// under it. `path` is the plan file's root-relative location; `hash` is its
    /// new content hash — informational only, mirroring `FileChange`'s "hash not
    /// bytes" fan-out discipline. No core replay-fold semantics, like
    /// `SkillActive`: a head just renders a notice and moves on.
    PlanChanged {
        session: SessionId,
        seq: u64,
        path: String,
        hash: String,
    },
    /// The outcome of an [`InMsg::RevertFiles`] (#731, ADR-0212): the session's
    /// tracked file edits after turn `to_turn` were rolled back. `restored`
    /// lists the paths put back (rewritten, or deleted when the agent had
    /// created them); `skipped` the paths left alone and why — most often
    /// because the file changed since the agent's edit, which a revert never
    /// clobbers. Emitted by the session itself (seq-bearing, persisted), so
    /// `Session::replay` re-appends the same user-role revert note to
    /// `Context` the live session did — and only that: replay never touches
    /// the filesystem, so a log containing a revert never re-reverts.
    FilesReverted {
        session: SessionId,
        seq: u64,
        to_turn: u32,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        restored: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        skipped: Vec<RevertSkip>,
    },
    /// This session is a non-compacting fork of `forked_from` (#786,
    /// ADR-0223), in reply to [`InMsg::ForkSession`]: emitted once by the
    /// fork itself, right after its start-up lifecycle events and before
    /// anything else it logs. `context` is the verbatim history it starts
    /// from — the source's whole `Context`, or turns `1..=at_turn` of it —
    /// so `Session::replay` reseeds it exactly and a head can render the
    /// inherited transcript. Distinct from `SessionStarted.predecessor`
    /// (ADR-0205), whose source is *retired*: here the source stays live.
    SessionForked {
        session: SessionId,
        seq: u64,
        forked_from: SessionId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        at_turn: Option<u32>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        context: Vec<Message>,
    },
    /// The session's active-skill posture changed (#400, ADR-0106). Emitted by
    /// the runtime's tool executor: `Some(skill_id)` when a `load_skill` call
    /// activates a skill, `None` when the skill's scope ends — the current turn's
    /// `Done`, or the session ending. `allowed_tools` is **vestigial** since
    /// ADR-0194 (skills are additive-only, never a tool-set restriction): still
    /// populated from the skill's frontmatter and serialized when present, for
    /// wire/log-replay compatibility, but never interpreted as an enforced mask —
    /// by core (which never did, ADR-0037) or the runtime. Wire-facing posture
    /// only; a head renders it as a notice. Mirrors `FileChange`: a fresh
    /// per-session seq (#157), no core replay-fold semantics (a head just tracks
    /// the latest value).
    SkillActive {
        session: SessionId,
        seq: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        skill_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        allowed_tools: Option<Vec<String>>,
    },
    /// An ambiguous LLM stop (ADR-0118) triggered a bounded in-place retry: the
    /// round ended with no tool calls and no confident finish signal, so core
    /// committed whatever partial text streamed, injected `nudge` as a synthetic
    /// user-role steering message, and re-queried the model within the same
    /// turn. Persisted + seq-bearing (like `Compacted`) so `Session::replay`
    /// reconstructs the exact round boundary — flushing the partial assistant
    /// message and pushing the nudge — instead of merging every retry round's
    /// `TextDelta`s into one assistant message and dropping the nudge, which
    /// would resume a session from a history the live model never saw. Its
    /// non-delta arrival also delimits the re-streamed partial text so a head
    /// (the TUI transcript, a sub-agent answer collector) starts a fresh
    /// segment rather than concatenating consecutive rounds' text. Heads render
    /// it as a one-line "retrying" notice.
    AmbiguousRetry {
        session: SessionId,
        seq: u64,
        nudge: String,
    },
    /// A provider-side web-search result block was minted this round (#481,
    /// follow-up to #305/ADR-0075's "not persisted" MVP limitation). Persisted
    /// and seq-bearing (like `AmbiguousRetry`) so `Session::replay` reconstructs
    /// the assistant message's content verbatim — including the provider-native
    /// `data` payload `part` carries, needed to replay the search back to the
    /// same provider on a later turn (mirrors `ToolCall.provider_meta`'s opaque
    /// round-trip contract). A different provider's converter (and any
    /// renderer) reads only `part`'s `summary`, never its opaque `data`. Heads
    /// render `summary` as a one-line notice, mirroring how `ReasoningDelta`
    /// already renders the live query/source lines for this same search.
    SearchResult {
        session: SessionId,
        seq: u64,
        part: ContentPart,
    },
    /// A model's extended-thinking block was captured this round. Persisted and
    /// seq-bearing for the same reason as `SearchResult`: `Session::replay` must
    /// reconstruct the assistant message's content verbatim, including the
    /// provider-native `data` payload, or a resumed parked turn cannot present
    /// the block the provider requires on the next request.
    ///
    /// Distinct from `ReasoningDelta`, which carries the same reasoning as live
    /// *display* text and is deliberately not folded into `Context`. This event
    /// is the persistence rail; the delta is the rendering one, and both are
    /// emitted for the same thinking.
    ///
    /// Capture — and therefore this event — is unconditional. Whether the block
    /// is actually sent back is a separate, catalog-level decision
    /// (`ModelEntry::replay_thinking`), so toggling replay never rewrites a
    /// session log or changes what a head renders.
    ReasoningBlock {
        session: SessionId,
        seq: u64,
        part: ContentPart,
    },
}

#[cfg(test)]
mod tests;
