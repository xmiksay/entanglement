//! Outbound events ([`OutEvent`]). The enum is too large for one file, so its
//! variants are split by family: this file holds the lifecycle/point-in-time
//! variants inside the `out_event_enum!` definition, and `content.rs` invokes
//! it with the seq-bearing content variants — the expanded type is one plain
//! `enum`.

use serde::{Deserialize, Serialize};

use crate::protocol::*;

/// Which request an [`OutEvent::Usage`] round was. A head totals every
/// round's spend, but only a `Turn` round's prompt size is the session's
/// context size — a `Compaction` round prices the summarizer's own request
/// (the head being summarized plus the instruction), not the live context.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsagePurpose {
    #[default]
    Turn,
    Compaction,
}

/// How a [`Compacted`][OutEvent::Compacted] compaction reclaimed room
/// ([ADR-0205](../../docs/adr/0205-every-compaction-forks-a-successor-session.md)).
/// Orthogonal to that event's `auto` flag, which says *who asked*: `auto`
/// alone cannot tell the overflow guard's two fallbacks apart, and a head
/// renders them differently — one carries a real summary, the other does not.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionMode {
    /// An LLM-authored summary of the history's head. The default, so every
    /// pre-ADR-0205 record on the wire keeps its meaning.
    #[default]
    Summary,
    /// The prune-only fallback: no LLM was available (disabled, guard tripped,
    /// or its result still didn't fit), so the oldest tool outputs were
    /// replaced with a short placeholder and the pruned history itself seeds
    /// the successor. `summary` then holds that pruned transcript.
    Prune,
}

/// An `invoke {name, args}` call as the model emitted it (ADR-0204): the outer
/// tool name and the raw argument string, verbatim. Carried on
/// [`ToolCall`][OutEvent::ToolCall]/[`ToolExec`][OutEvent::ToolExec]/
/// [`ToolOutput`][OutEvent::ToolOutput] when core unwrapped the call, so heads,
/// gates and hooks see the inner tool while replay can rebuild the emitted
/// call byte for byte — the model-facing history is never rewritten.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolEnvelope {
    pub tool: String,
    pub input: String,
}

/// Defines [`OutEvent`] from its lifecycle variants below plus the
/// content variants passed in by `content.rs` (variant order preserved).
macro_rules! out_event_enum {
    ($($rest:tt)*) => {
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
            $($rest)*
        }
    };
}

mod content;
#[cfg(test)]
mod tests;

pub use content::OutEvent;

impl OutEvent {
    /// The session this event belongs to, or `None` for a supervisor-global query
    /// reply that names no single session — [`SessionList`][OutEvent::SessionList]
    /// (#160), [`McpList`][OutEvent::McpList] and
    /// [`McpChanged`][OutEvent::McpChanged] (#375; MCP config is engine-global).
    /// [`History`][OutEvent::History] does name a session (the one whose
    /// history it carries), so it returns `Some`.
    pub fn session(&self) -> Option<&SessionId> {
        match self {
            OutEvent::SessionStarted { session, .. }
            | OutEvent::SessionEnded { session, .. }
            | OutEvent::SessionHibernated { session, .. }
            | OutEvent::History { session, .. }
            | OutEvent::Status { session, .. }
            | OutEvent::AgentChanged { session, .. }
            | OutEvent::ModeChanged { session, .. }
            | OutEvent::ModelChanged { session, .. }
            | OutEvent::GenerationChanged { session, .. }
            | OutEvent::SessionMetaChanged { session, .. }
            | OutEvent::ToolOverlayChanged { session, .. }
            | OutEvent::Plan { session, .. }
            | OutEvent::TextDelta { session, .. }
            | OutEvent::ReasoningDelta { session, .. }
            | OutEvent::ToolCallDelta { session, .. }
            | OutEvent::ToolCall { session, .. }
            | OutEvent::ToolRequest { session, .. }
            | OutEvent::ToolExec { session, .. }
            | OutEvent::UserQuestion { session, .. }
            | OutEvent::ToolOutput { session, .. }
            | OutEvent::TaskList { session, .. }
            | OutEvent::Usage { session, .. }
            | OutEvent::Error { session, .. }
            | OutEvent::Done { session, .. }
            | OutEvent::Compacted { session, .. }
            | OutEvent::FileChange { session, .. }
            | OutEvent::PlanChanged { session, .. }
            | OutEvent::SkillActive { session, .. }
            | OutEvent::AmbiguousRetry { session, .. }
            | OutEvent::SearchResult { session, .. }
            | OutEvent::ReasoningBlock { session, .. } => Some(session),
            OutEvent::SessionList { .. }
            | OutEvent::QuestionList { .. }
            | OutEvent::OperationList { .. }
            | OutEvent::McpList { .. }
            | OutEvent::McpChanged { .. }
            | OutEvent::McpAuthChanged { .. }
            | OutEvent::Throttle { .. } => None,
        }
    }

    /// The monotonic per-session sequence number for a **content** event, or
    /// `None` for a point-in-time lifecycle/query event that carries no `seq`
    /// (`SessionStarted`, `SessionEnded`, `SessionList`, `QuestionList`,
    /// `OperationList`, `History`, `Status`, `AgentChanged`, `ModelChanged`,
    /// `GenerationChanged`, `SessionMetaChanged`, `ModeChanged`). Returning `Option`
    /// instead of a fake `0`
    /// (#160, ADR-0072) lets a head tell "seq 0" apart from "no seq" — the
    /// supervisor-shed `Error` sentinel (seq `0`) is a real `Some(0)`, distinct
    /// from a lifecycle event's `None`.
    pub fn seq(&self) -> Option<u64> {
        match self {
            OutEvent::SessionStarted { .. }
            | OutEvent::SessionEnded { .. }
            | OutEvent::SessionHibernated { .. }
            | OutEvent::SessionList { .. }
            | OutEvent::QuestionList { .. }
            | OutEvent::OperationList { .. }
            | OutEvent::McpList { .. }
            | OutEvent::McpChanged { .. }
            | OutEvent::McpAuthChanged { .. }
            | OutEvent::Throttle { .. }
            | OutEvent::History { .. }
            | OutEvent::Status { .. }
            | OutEvent::AgentChanged { .. }
            | OutEvent::ModeChanged { .. }
            | OutEvent::ModelChanged { .. }
            | OutEvent::GenerationChanged { .. }
            | OutEvent::SessionMetaChanged { .. }
            | OutEvent::ToolOverlayChanged { .. } => None,
            OutEvent::Plan { seq, .. }
            | OutEvent::TextDelta { seq, .. }
            | OutEvent::ReasoningDelta { seq, .. }
            | OutEvent::ToolCallDelta { seq, .. }
            | OutEvent::ToolCall { seq, .. }
            | OutEvent::ToolRequest { seq, .. }
            | OutEvent::ToolExec { seq, .. }
            | OutEvent::UserQuestion { seq, .. }
            | OutEvent::ToolOutput { seq, .. }
            | OutEvent::TaskList { seq, .. }
            | OutEvent::Usage { seq, .. }
            | OutEvent::Error { seq, .. }
            | OutEvent::Done { seq, .. }
            | OutEvent::Compacted { seq, .. }
            | OutEvent::FileChange { seq, .. }
            | OutEvent::PlanChanged { seq, .. }
            | OutEvent::SkillActive { seq, .. }
            | OutEvent::AmbiguousRetry { seq, .. }
            | OutEvent::SearchResult { seq, .. }
            | OutEvent::ReasoningBlock { seq, .. } => Some(*seq),
        }
    }
}
