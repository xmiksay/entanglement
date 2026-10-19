//! Wire protocol — the single set of typed messages shared by *every* head
//! (in-process ABI, stdio NDJSON, WebSocket, future TUI). Because the same
//! `InMsg`/`OutEvent` cross all transports, the ABI is just "use these types
//! directly without serializing."
//!
//! Every frame is session-scoped: one transport connection multiplexes many
//! sessions, routed by [`SessionId`] (the same model as the `agent` reference's
//! `task_id`). Content events carry a monotonic per-session `seq` so a head can
//! dedupe/order against replayed history; lifecycle frames
//! ([`Status`][OutEvent::Status], [`AgentChanged`][OutEvent::AgentChanged])
//! are point-in-time and carry no `seq`.
//!
//! `(session, seq)` is unique across every authored content event (#157): the
//! seq is drawn from one per-session counter shared by the core session task and
//! the runtime (via [`Holly::emit_for_session`][crate::Holly]), so a
//! runtime-authored event minted while the session is parked — an approval
//! [`ToolRequest`][OutEvent::ToolRequest]/[`UserQuestion`][OutEvent::UserQuestion],
//! a [`Plan`][OutEvent::Plan]/[`TaskList`][OutEvent::TaskList] snapshot, a
//! [`FileChange`][OutEvent::FileChange] — gets a fresh seq rather than reusing
//! the parked [`ToolExec`][OutEvent::ToolExec] seq. A retired (hibernated, ended
//! or closed) session keeps minting above its last seq (ADR-0239). The one
//! exemption is an event for an id with no sequence — one that never started,
//! or whose retired entry was evicted past the cap (#891), e.g. a supervisor
//! lifecycle [`Error`][OutEvent::Error] refusing a spawn: it carries
//! `seq == 0` — a value core never mints — which a head renders unconditionally
//! instead of dropping under a `seq > last` dedupe.

use entanglement_provider::UserId;
use serde::{Deserialize, Serialize};

mod approvals;
mod in_msg;
mod mcp;
mod model_choice;
mod out_event;
mod overlay;
mod permission;
mod questions;
mod retired;
mod revert;

pub use approvals::{ApprovalKind, ApprovalOutcome, CardReason, PendingApproval};
pub use in_msg::InMsg;
pub use mcp::{McpAction, McpAuthAction, McpAuthStatus, McpServerSpec, McpServerStatus};
pub use model_choice::ModelChoice;
pub use out_event::{CompactionMode, OutEvent, ToolEnvelope, UsagePurpose};
pub use overlay::ToolOverlayEntry;
pub use permission::{ApprovalScope, Permission, PermissionProfile};
pub use questions::{PendingQuestion, Question, QuestionOption, Questions};
pub use retired::{RETIRED_IN_MSG_KINDS, RETIRED_OUT_EVENT_KINDS};
pub use revert::{FileRevertReport, RevertSkip};

/// Stable identifier for a conversation session. Serialized transparently as a
/// plain string on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(pub String);

impl SessionId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Mint a fresh session id via the default [`IdGen`][crate::id_gen::IdGen]
    /// (ADR-0164: `s-<epoch-seconds hex><salt><counter>`, 15 chars). Kept name
    /// for call-site compatibility even though the scheme is no longer a
    /// UUID — every caller treats the result as an opaque string, and legacy
    /// UUID-form ids coexist indefinitely (the two shapes cannot collide). A
    /// `Holly` handle in scope should prefer
    /// [`Holly::next_id`][crate::Holly::next_id] instead, which goes through
    /// the engine's *configured* generator rather than always the default.
    pub fn new_uuid() -> Self {
        use crate::id_gen::{DefaultIdGen, IdContext, IdGen, IdKind};
        Self(DefaultIdGen::new().next(IdKind::Session, &IdContext::default()))
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

/// Lifecycle state of a session, surfaced via [`OutEvent::Status`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    /// Session is live but idle, waiting for the next prompt. Only the
    /// genuinely-never-run-yet state: emitted once at session start, and the
    /// resting state everywhere else is [`Done`][AgentState::Done] (ADR-0139).
    Idle,
    /// Engine is actively reasoning / calling the model.
    Thinking,
    /// A tool batch is parked (after `ToolExec` for all calls, before
    /// `ToolResult`s resolve) — distinguishable from model-generation
    /// [`Thinking`][AgentState::Thinking] so a head can show "running a
    /// command" vs "LLM is generating" (ADR-0139).
    Working,
    /// Parked on a sub-agent result — the blocking `agent` tool. A
    /// long-running child otherwise looks identical to the parent agent
    /// staring at the wall; this state makes the park visible (ADR-0139).
    WaitingAgent,
    /// Engine emitted a tool request and is parked waiting for approval
    /// (`Approve`/`Reject`).
    WaitingApproval,
    /// Parked on a model-driven `ask_user` question, waiting for the user to pick
    /// an option or type a free-form answer (`AnswerQuestion`). Distinct from
    /// [`WaitingApproval`][AgentState::WaitingApproval] (#160): a question is not
    /// a permission decision, and heads render the two differently.
    WaitingAnswer,
    /// Explicitly held by `InMsg::PauseSession` (#516, ADR-0208) — distinct from
    /// every wait state above, which are all waiting on *something specific*
    /// ([`WaitingApproval`][AgentState::WaitingApproval]/
    /// [`WaitingAnswer`][AgentState::WaitingAnswer]/
    /// [`WaitingAgent`][AgentState::WaitingAgent] resolve themselves once their
    /// event arrives). `Paused` never self-resolves: a new turn is deferred (an
    /// idle session) and a parked batch's already-arrived results still fold
    /// into `Context`, but the turn does not continue past the drained batch,
    /// until `InMsg::ResumeSession` lifts the hold. A session mid-stream when
    /// `PauseSession` arrives is not interrupted — the pause takes effect at the
    /// next round boundary (turn end or tool-call park), the same "deferred until
    /// safe" mechanism `SetModel` already uses mid-stream — so this
    /// state is never observed while actively streaming; send `Stop` for an
    /// immediate interrupt.
    Paused,
    /// Last turn finished cleanly. The resting state after a turn ends or is
    /// cancelled by `Stop` (ADR-0139) — the user prefers it over `Idle` so a
    /// completed interaction is never rendered the same as never-run-yet.
    Done,
    /// Last turn ended with an error.
    Error,
}

/// Kind of file change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileChangeKind {
    Edit,
    /// Multi-hunk unified-diff apply, produced by the runtime's `apply_patch`
    /// host tool (#455) — beside `edit`'s single exact-string replace.
    ApplyDiff,
    Create,
}

/// A live session's identity + lineage, as reported in an
/// [`OutEvent::SessionList`] enumeration snapshot (ADR-0028). Mirrors the fields
/// a head would otherwise have to reconstruct by folding the `SessionStarted` /
/// `SessionEnded` broadcast itself. `agent` is the session's agent — fixed
/// for its whole life (ADR-0207 §9: an agent is chosen when a session starts,
/// never switched) — as announced by [`OutEvent::AgentChanged`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub session: SessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<SessionId>,
    /// The session this one succeeds — a compaction fork's source
    /// (ADR-0205); `None` for an ordinary session. Mirrors
    /// [`OutEvent::SessionStarted`]'s field (ADR-0211).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub predecessor: Option<SessionId>,
    /// Spelled `profile` before ADR-0207; the alias keeps 0.6.0 frames loading (#720).
    #[serde(alias = "profile")]
    pub agent: String,
    pub root: bool,
    /// The session's owning user in a multi-user deployment (#522). `None` in
    /// single-user mode (the default) or for a session an embedder spawned
    /// without a `user`. Set once at spawn, inherited by every child — see
    /// [`InMsg::Spawn`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<UserId>,
}

/// Kind of pending operation in an [`OutEvent::OperationList`] snapshot (#607,
/// ADR-0161 §6): a background `bash`/`call` job, a launched sub-agent, or a
/// background `rhai` script (#637, ADR-0185).
/// Lifetimes differ — a job is an OS process owned by this engine process and
/// cannot outlive it, a script is an in-process task with the same lifetime,
/// while an agent handle is itself a session, so it persists across
/// hibernation/resume however the session bookkeeping already does — the
/// listing surfaces which kind an entry is precisely so a head can render
/// that distinction instead of implying every entry behaves the same.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationKind {
    Job,
    Agent,
    Script,
}

/// Terminal or in-progress state of a pending operation (#607). Mirrors the
/// running/complete vocabulary `poll`'s single-handle join already reports.
/// Unlike a finished job (evicted after a TTL), a completed agent stays
/// listed until the model polls it by handle — a `Complete` entry here is an
/// unclaimed answer, not stale data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationStatus {
    Running,
    Complete,
}

/// One outstanding background job or launched sub-agent, as reported in an
/// [`OutEvent::OperationList`] snapshot (#607, ADR-0161 §6) — the "what do I
/// still have running" answer, the no-handle counterpart to `poll`'s
/// single-handle "how is this going." `session` is the operation's owner — a
/// snapshot spanning every session (an unfiltered [`InMsg::ListOperations`])
/// needs each entry to name its own, mirroring [`PendingQuestion`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationInfo {
    pub session: SessionId,
    pub kind: OperationKind,
    pub handle: String,
    /// What launched it: a job's command line, or an agent's profile name.
    pub launched_by: String,
    pub elapsed_secs: u64,
    pub status: OperationStatus,
}

/// A bundle of identity — system prompt, model/provider pin — that defines who
/// a session is. Chosen once, when the session starts (`--agent`, `config.yml`'s
/// `agent:`, or the `agent` tool's own argument for a spawned child), and fixed
/// for that session's whole life: there is no live "switch profile" message
/// (ADR-0207 §9 retired `SetAgent` — a persona is text already sent, so
/// delegating to a different one is a fresh spawn, not a rewrite of this
/// session's own system prompt). Mirrors opencode's agent concept.
///
/// **Authority is not here.** ADR-0207 moved every permission fact — the tool
/// mask (`tools`/`disallowed_tools`), the `permission` rules, spawn control
/// (`can_spawn`/`spawnable_agents`), sandbox confinement, and the
/// primary/subagent/all `mode` distinction — off the profile and onto the
/// session's independent permission **mode** axis (or, for spawn bounds, the
/// mode's `max_depth`/`max_agents`). A profile no longer says what a session
/// may do or where it may run — only who it is. Any agent may be a session
/// root or a spawn target (ADR-0207 §4/§6).
///
/// Profiles are **file-defined** in the runtime (markdown + YAML frontmatter,
/// ADR-0034): `name`/`model` come from the frontmatter and `system_prompt` is
/// the file body. `description` drives delegation matching — it is the one
/// field disclosed to a spawning model (via the `agent` tool description).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Agent {
    pub name: String,
    /// One-line summary; disclosed to a spawning model for delegation matching.
    #[serde(default)]
    pub description: String,
    pub system_prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Provider this profile pins its [`model`][Self::model] to (#323, ADR-0081).
    /// A profile with **both** `provider` and `model` set forms a *model pin*
    /// ([`model_pin`][Self::model_pin]): the runtime re-binds the session's
    /// backend to `(provider, model)` at session start, so a profile carries its
    /// own endpoint, not just a model id within the startup provider. `model`
    /// without `provider` keeps today's request-level fallback (no rebind) — the
    /// legacy behaviour. Back-compat: `#[serde(default)]`, so logs/frames written
    /// before #323 deserialize with `provider: None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// The permission mode a fresh session under this agent *starts* in when
    /// neither the head nor config names one (#763, ADR-0218). A starting-mode
    /// hint, not authority: it grants nothing beyond that mode's own rules,
    /// never starts a root wider than `DEFAULT_MODE`, `/mode` still switches
    /// it, a spawned child never widens past its parent's mode with it, and a
    /// resumed session keeps its replayed mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_mode: Option<String>,
}

impl Agent {
    /// The profile's model pin (#323, ADR-0081): `Some((provider, model))` only
    /// when **both** [`provider`][Self::provider] and [`model`][Self::model] are
    /// set, so the runtime can re-bind the session's backend to that endpoint at
    /// session start. A `model`-only profile returns `None` — it keeps
    /// the legacy request-level model fallback and triggers no rebind.
    pub fn model_pin(&self) -> Option<(&str, &str)> {
        match (self.provider.as_deref(), self.model.as_deref()) {
            (Some(provider), Some(model)) => Some((provider, model)),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_id_new_uuid_generates_unique_ids() {
        // ADR-0164: `new_uuid` mints the short kind-tagged scheme, not a UUID
        // — this is the sole migration point the ADR calls out. No consumer
        // in the tree parses a session id as a UUID (`SessionId` is an opaque
        // string), so only this assertion needed to change.
        let id1 = SessionId::new_uuid();
        let id2 = SessionId::new_uuid();
        let id3 = SessionId::new_uuid();

        assert_ne!(id1, id2, "ids should be unique");
        assert_ne!(id2, id3, "ids should be unique");
        assert_ne!(id1, id3, "ids should be unique");

        for id in [&id1, &id2, &id3] {
            assert!(
                id.0.starts_with("s-"),
                "session id should be `s-` tagged: {id}"
            );
            assert_eq!(id.0.len(), 15, "session id should be 15 chars: {id}");
        }
    }

    #[test]
    fn waiting_answer_state_serializes_snake_case() {
        let ev = OutEvent::Status {
            session: SessionId::new("s1"),
            state: AgentState::WaitingAnswer,
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains(r#""state":"waiting_answer""#), "{json}");
        assert_eq!(serde_json::from_str::<OutEvent>(&json).unwrap(), ev);
    }
}
