//! Session-control, query and MCP-management [`InMsg`] variants; this file
//! is where the full enum is expanded (see `in_msg.rs`).

use entanglement_provider::{ContentPart, GenerationParams, UserId};
use serde::{Deserialize, Serialize};

use super::de_prompt_content;
use crate::protocol::*;

in_msg_enum! {
    /// Enumerate the engine's currently-live sessions. The supervisor answers
    /// with a single [`OutEvent::SessionList`] snapshot (ADR-0028); this message
    /// is supervisor-global, not routed to a session task. `correlation_id` is an
    /// opaque token the head mints and the reply echoes back, so a multiplexed
    /// head can pair the snapshot to its request without overloading a
    /// [`SessionId`] as a correlation key (#160, ADR-0072).
    ListSessions { correlation_id: String },
    /// Enumerate every currently-open `ask_user` question (#515) — the
    /// `ask_user`/`UserQuestion` counterpart to
    /// [`ListSessions`][InMsg::ListSessions]: a reconnecting head or a
    /// multi-pane UI has no other authoritative source, since a parked
    /// [`OutEvent::UserQuestion`] it missed leaves no trace elsewhere.
    /// `session`, when set, filters to that session's own open questions;
    /// `None` lists every session's (the common case, mirroring
    /// `ListSessions`'s whole-engine scope). Like `ListSessions` this is
    /// supervisor-global — [`session`][InMsg::session] returns `None` for it
    /// regardless of the filter, since it names no *routing* target — but
    /// answered by the same runtime service that owns `ask_user`'s open-call
    /// registry, off the inbound fan-out (mirrors
    /// [`McpList`][InMsg::McpList]), not the core supervisor.
    /// `correlation_id` pairs the reply
    /// ([`OutEvent::QuestionList`][crate::protocol::OutEvent::QuestionList])
    /// to this query, exactly like `ListSessions`/`McpList`.
    ListQuestions {
        correlation_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<SessionId>,
    },
    /// Enumerate this engine's outstanding background `bash`/`call` jobs and
    /// launched sub-agents (#607, ADR-0161 §6) — the head-facing counterpart
    /// to `poll`'s no-handle model surface: "what do I still have running,"
    /// answered by attributing every job/agent handle to the session that
    /// launched it. `session`, when set, filters to that session's own
    /// pending operations; `None` lists every session's — same optional-filter
    /// convention as [`ListQuestions`][InMsg::ListQuestions]. Supervisor-global:
    /// [`session`][InMsg::session] returns `None` for it regardless of the
    /// filter, since it names no *routing* target — answered by the runtime
    /// service that owns the job/agent registries, off the inbound fan-out,
    /// not core. Wire-allowed: a read of the caller's own outstanding work,
    /// mutating nothing. `correlation_id` pairs the reply
    /// ([`OutEvent::OperationList`][crate::protocol::OutEvent::OperationList])
    /// to this query, exactly like `ListSessions`/`ListQuestions`.
    ListOperations {
        correlation_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<SessionId>,
    },
    /// Enumerate the engine's currently-attached MCP servers (#375). MCP config
    /// is global (not per-session), so — like [`ListSessions`][InMsg::ListSessions]
    /// — this is supervisor-global: core routes it to no session task, and it is
    /// answered by a runtime service that owns the tool registry + live server
    /// connections, off the inbound fan-out (mirrors how
    /// [`ReplayFrom`][InMsg::ReplayFrom] is answered by the runtime's history
    /// responder). `correlation_id` pairs the reply
    /// ([`OutEvent::McpList`][crate::protocol::OutEvent::McpList]) to this query.
    McpList { correlation_id: String },
    /// Hot-connect an MCP server in the running process and persist it to
    /// `config.yml` so it survives a restart (#375). Best-effort like startup
    /// connect: a failed connect/handshake is logged, not surfaced as a session
    /// error (there is no session to attach one to). On success emits
    /// [`OutEvent::McpChanged`] with [`McpAction::Added`]. **Trusted-only**
    /// (#472, ADR-0124): a stdio `config` spawns an arbitrary local subprocess
    /// with no approval prompt, so this must never arrive over an untrusted
    /// wire — see [`wire_allowed`][InMsg::wire_allowed].
    McpAdd { name: String, config: McpServerSpec },
    /// Disconnect an MCP server (killing its subprocess / closing its HTTP
    /// session), drop its tools from the registry, and persist the removal
    /// (#375). Unknown name is a no-op (logged). On success emits
    /// [`OutEvent::McpChanged`] with [`McpAction::Removed`]. **Trusted-only**
    /// (#472, ADR-0124) like [`McpAdd`][InMsg::McpAdd]: it mutates engine-global
    /// config and tears down live tools.
    McpRemove { name: String },
    /// Manage a server's OAuth authorization (ADR-0153): sign in, validate the
    /// stored credential, or drop it. Engine-global like
    /// [`McpAdd`][InMsg::McpAdd] — MCP config and its credentials are
    /// process-wide, not per-session — and answered by the runtime's MCP
    /// responder, which alone holds the server configs and the token store.
    /// Emits [`OutEvent::McpAuthChanged`].
    ///
    /// **Trusted-only** (#472, ADR-0124, the `McpAdd` rationale sharpened): a
    /// [`Connect`][McpAuthAction::Connect] opens a browser window and mints a
    /// durable credential on the user's behalf, and a
    /// [`Disconnect`][McpAuthAction::Disconnect] destroys one — neither may be
    /// driven by an untrusted wire peer.
    McpAuth { name: String, action: McpAuthAction },
    /// Fetch a session's persisted content history from `after_seq` onward, for a
    /// head that subscribed late and missed the live broadcast (#160, ADR-0072).
    /// Answered out-of-core by the runtime's history responder — which owns the
    /// event log — with a single [`OutEvent::History`] snapshot carrying every
    /// content event whose `seq` exceeds `after_seq`, `correlation_id` echoed so
    /// the requester can pair the reply. `after_seq == 0` requests the whole
    /// content history. A head-authored query, so it is wire-allowed.
    ReplayFrom {
        session: SessionId,
        correlation_id: String,
        after_seq: u64,
    },
    /// Explicitly terminate a live session: the supervisor drops its command
    /// channel and the session task exits, emitting [`OutEvent::SessionEnded`]
    /// (ADR-0028). Distinct from [`Stop`][InMsg::Stop], which only cancels the
    /// in-flight turn and leaves the session alive (ADR-0017) — `CloseSession`
    /// is the lifecycle destroy Stop no longer performs. Unknown / already-closed
    /// ids are a no-op. Session ids are single-use: mint a fresh one
    /// ([`SessionId::new_uuid`]) rather than reusing a closed id.
    CloseSession { session: SessionId },
    /// Evict a live session from memory **without** tombstoning its id (#318,
    /// ADR-0077). The supervisor tears down the session task and drops its
    /// in-memory [`Context`][crate::context::Context]/history — cascading over the
    /// spawn sub-tree like [`CloseSession`][InMsg::CloseSession] — but records no
    /// tombstone, so the id stays **resumable**: a later
    /// [`Holly::resume`][crate::Holly::resume] rebuilds it from the embedder's
    /// event log exactly like the restart path, re-offering any pending
    /// `ToolExec` for a turn parked on approval (ADR-0061/0071). Distinct from
    /// `CloseSession` (terminal, tombstoned) and [`Stop`][InMsg::Stop] (cancels a
    /// turn, keeps the session live). Emits [`OutEvent::SessionHibernated`].
    /// A mid-stream turn is torn down (stop-then-hibernate): the uncommitted
    /// round is discarded exactly as replay drops a text-only tail, so resume is
    /// lossless w.r.t. the embedder's log. **Embedder-initiated and trusted-only**
    /// (like [`Resume`][InMsg::Resume]): it is *not* wire-allowed — a wire head
    /// cannot evict another session's memory. Unknown ids are a no-op.
    HibernateSession { session: SessionId },
    /// Switch the session's permission **mode** by name (ADR-0207). Authority
    /// is a second axis, independent of the agent: core carries `mode` as an
    /// **opaque name it never evaluates** — the runtime owns the mode table
    /// (`research`/`plan`/`build`/`auto` for `skutter`) and every rule that
    /// name resolves to. Applied **immediately**, turn live or not — unlike
    /// [`SetModel`][InMsg::SetModel]/[`SetGeneration`][InMsg::SetGeneration]/
    /// [`SetToolOverlay`][InMsg::SetToolOverlay], which defer because they
    /// touch something a live round is actively using (the backend, the
    /// in-flight request's generation knobs, the advertised tool array). A
    /// mode is a label consulted only when the runtime next grades a tool
    /// call, so there is nothing for a live turn to protect by waiting —
    /// deferring it instead left a `propose_plan` approval's very next tool
    /// call graded under the mode the plan was *written* in, not the one the
    /// approver just switched to (#560). Always
    /// succeeds and confirms with [`OutEvent::ModeChanged`] — core has no
    /// table to validate `mode` against, so there is nothing to fail here; an
    /// unresolvable name is the runtime's problem to reject at the point it is
    /// evaluated. **Trusted-only**: a mode carries real authority (unlike the
    /// agent, which is identity fixed at session start and never switched,
    /// ADR-0207 §9), so it is refused from an untrusted wire head (see
    /// [`wire_allowed`][InMsg::wire_allowed]).
    SetMode { session: SessionId, mode: String },
    /// Switch the session's live model/provider without restarting the engine
    /// (#218). The runtime re-resolves `(provider, model)` against the catalog +
    /// user config (via [`EngineConfig::model_resolver`][crate::EngineConfig]),
    /// rebuilds the session's `Box<dyn Llm>`, and retargets generation + the
    /// context-window budget. Both fields are catalog-qualified — a head's model
    /// picker yields the provider alongside the model — so this covers a
    /// same-provider model change and a full provider switch uniformly. On
    /// success the session emits [`OutEvent::ModelChanged`]; an unknown
    /// provider / missing key surfaces [`OutEvent::Error`]. Applied once the live
    /// turn ends when one is running (stash replay) — rebuilding the backend
    /// mid-round would be incoherent. Unlike [`SetMode`][InMsg::SetMode]
    /// (applied immediately — a mode is a label, not something a live round
    /// is using).
    SetModel {
        session: SessionId,
        provider: String,
        model: String,
    },
    /// Live-adjust generation knobs (temperature / max-output / thinking budget /
    /// reasoning effort) without restarting the engine (#374, ADR-0094). `overrides`
    /// is **partial**: a `None` field means "leave unchanged" — only the fields the
    /// caller sets are merged onto the session's current
    /// [`Session::generation`][crate::session::Session] via
    /// [`GenerationParams::apply_overrides`]. Unlike [`SetModel`][InMsg::SetModel],
    /// there is no resolver to fail against, so this always succeeds and always
    /// emits [`OutEvent::GenerationChanged`] with the merged, full effective
    /// params — even when every override happens to match the current value — so a
    /// head can rely on the reply to confirm the write landed. Applied once the
    /// live turn ends when one is running (stash replay), like
    /// [`SetModel`][InMsg::SetModel] (unlike [`SetMode`][InMsg::SetMode],
    /// which applies immediately — see its own doc for why). The merged
    /// result is also recorded in `Session::generation_by_agent`, so a resumed
    /// session's replay-reconstructed live override isn't clobbered by the
    /// persisted default `EngineConfig::generation_resolver` would otherwise
    /// re-apply at session start (#374, ADR-0094).
    SetGeneration {
        session: SessionId,
        overrides: GenerationParams,
    },
    /// Set this session's display **metadata**: a human-readable `name`
    /// (a title, e.g. derived from the first prompt) and/or the current
    /// `action` (what the agent is doing right now). A `None` field leaves the
    /// stored value untouched; `Some("")` clears it. Unlike
    /// [`SetGeneration`][InMsg::SetGeneration] it is applied **immediately,
    /// never stashed behind a live turn** — `action`'s whole purpose is to
    /// change mid-turn — and it always acks with
    /// [`OutEvent::SessionMetaChanged`] carrying the full merged values.
    /// Wire-allowed: cosmetic, session-scoped, no privilege.
    ///
    /// `if_unset` (#553): when `true`, `name` is applied only if the session
    /// has no name yet — a no-op (still acked, with the unchanged state) once
    /// the session already carries one, whether set by an earlier `/name` or
    /// restored by resume. Never clears an existing name. The auto session-title
    /// generator (`entanglement-runtime::session_title`) always sets this so a
    /// late-arriving generated title can never race a user-set `/name`; a
    /// user-authored rename leaves it `false` (default) so it always wins.
    SetSessionMeta {
        session: SessionId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        action: Option<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        if_unset: bool,
    },
    /// Replace this session's live **tool overlay** (#539, ADR-0149): the full
    /// list of [`ToolOverlayEntry`] patterns whose matching tools exist for
    /// this session *regardless* of the active profile's #116 mask — the
    /// per-session injection a head uses to enable an MCP server/tool (or any
    /// registered tool) without editing agent definitions. Full replacement,
    /// not a merge (an empty list clears the overlay); the head computes the
    /// new list from the previous [`OutEvent::ToolOverlayChanged`] it holds.
    /// Always succeeds and always emits `ToolOverlayChanged` with the full
    /// effective list (mirroring [`SetGeneration`][InMsg::SetGeneration]);
    /// deferred while a turn is live (stash replay), like `SetModel` — a
    /// mid-round edit would change the advertised tool array under a request
    /// already in flight. Unlike [`SetMode`][InMsg::SetMode], which applies
    /// immediately since it changes no array and busts no cache.
    /// **Trusted-only** (not wire-allowed, #472, ADR-0124): it can hand the
    /// model tools with no restart and — with `allow: true` — no approval
    /// prompt.
    SetToolOverlay {
        session: SessionId,
        entries: Vec<ToolOverlayEntry>,
    },
    /// Run a single out-of-band LLM op outside the turn loop (#324, ADR-0082).
    /// The generic surface is the **wire shape** — an opaque `op` string plus
    /// `args` — not a plugin registry: `session::ops::run_oneshot` matches on
    /// `op` (`"compact"` today; an unknown op emits a recoverable `Error`).
    /// Mutates only the caller's own `Context`, so it is wire-allowed. Deferred
    /// while a turn is live (stash replay), like `SetModel`.
    /// `"compact"`'s `args`: `instructions` (optional free-text steer) and
    /// `kept` (optional `u64`, default `0` — a keep-tail request, #397/
    /// ADR-0102, clamped to the nearest safe turn boundary).
    Oneshot {
        session: SessionId,
        op: String,
        #[serde(default)]
        args: serde_json::Value,
    },
    /// Spawn a session running `prompt` beneath the named `agent` profile (#60,
    /// ADR-0021). `session` is the new session's id. With `parent = Some(p)` it is
    /// a **child** sub-agent under `p` (the common case): the supervisor records
    /// the parent link (populating the spawn tree the tree-walk helpers read) so a
    /// `CloseSession`/hibernate cascade and the permission ancestor clamp cover it.
    /// With `parent = None` it is a **root** — used by the `/compact` successor
    /// fork (ADR-0110), which sets `predecessor = Some(source)` to record the
    /// session it succeeds *without* joining the source's spawn sub-tree (so
    /// closing the source doesn't cascade onto the successor). The runtime's
    /// `agent` tool (blocking by default, `background: true` for the detached
    /// form) issues the child form, then relays the child's final answer back
    /// to the parent as a tool result — core needs no notion of "child
    /// session" in its loop.
    Spawn {
        session: SessionId,
        #[serde(default)]
        parent: Option<SessionId>,
        /// The session this one succeeds (a `/compact` fork, ADR-0110). Non-live
        /// lineage only — never a spawn edge.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        predecessor: Option<SessionId>,
        agent: String,
        prompt: String,
        /// Owning user for a multi-user deployment (#522, ADR-0147). Only
        /// consulted for a true root spawn (`parent` and `predecessor` both
        /// `None`) — the multi-user embedder's session-creation entry point. A
        /// child spawn (`parent: Some(_)`) or a compaction successor
        /// (`predecessor: Some(_)`) always inherits its parent's/predecessor's
        /// user instead, so this field is ignored there (an embedder need not
        /// re-specify it). `None` (single-user mode) is the default.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        user: Option<UserId>,
    },
    /// Resume a session from replayed log records (internal, not serialized).
    #[serde(skip)]
    Resume {
        session: SessionId,
        records: Vec<(Option<InMsg>, OutEvent)>,
    },
}
