//! The engine actor. [`Holly`] owns a process-wide inbox (`mpsc<InMsg>`) and
//! outbox (`broadcast<OutEvent>`). The supervisor routes inbound messages to
//! per-session tasks (lazily spawned, one per [`SessionId`]).
//!
//! This is the ABI foundation: an embedder holds a (cheaply-cloned) `Holly`,
//! calls [`Holly::send`] with typed [`InMsg`]s and drains
//! [`Holly::subscribe`] for [`OutEvent`]s — no serialization. Every transport
//! (stdio, WS, TUI) is a thin adapter over these two methods.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::{broadcast, mpsc};

use crate::id_gen::IdGen;
use crate::protocol::{InMsg, OutEvent, SessionId};
use entanglement_provider::ContentPart;

mod config;
mod emit;
mod outlets;
mod routing;
mod supervisor;
mod unknown_session;

pub use config::{
    AgentCatalog, ConfigError, EngineConfig, SessionModel, SystemPromptResolver, ToolSpecResolver,
};
use outlets::Outlets;
use supervisor::supervisor;
pub use unknown_session::{SessionLoader, UnknownSession};

/// Per-session monotonic seq counters, shared between each session task (which
/// owns the same `Arc<AtomicU64>` as its `Session::seq`) and the supervisor +
/// runtime (#157). A runtime service authoring an event for a *parked* session —
/// an approval `ToolRequest`, a `Plan`/`TaskList` snapshot, a `FileChange` —
/// mints a fresh seq from this shared counter via [`Holly::emit_for_session`]
/// instead of reusing the parked `ToolExec` seq, so `(session, seq)` stays unique
/// across every authored event. The session task registers its counter on start
/// and removes it on exit; a `Mutex` (never held across an `.await`) orders the
/// map reads/writes.
pub(crate) type SeqRegistry = Arc<Mutex<HashMap<SessionId, Arc<AtomicU64>>>>;

/// Mint the next monotonic seq for `session` from `seqs`, or `0` when the session
/// has no live counter (already ended, never started, or a supervisor error for
/// an id that never spawned a session). Shared by [`Holly::emit_for_session`] and
/// the supervisor's [`routing::emit_supervisor_error`].
pub(crate) fn next_seq_for(seqs: &SeqRegistry, session: &SessionId) -> u64 {
    seqs.lock()
        .expect("seq registry mutex poisoned")
        .get(session)
        .map(|c| c.fetch_add(1, Ordering::Relaxed) + 1)
        .unwrap_or(0)
}

/// Per-session settledness, shared between each session task (which owns the
/// writes, mirroring [`SeqRegistry`]) and the supervisor's idle-TTL sweep
/// (#363). `None` while the session is mid-turn or parked on a tool/approval/
/// question result (`Session::turn.is_some()`); `Some(instant)` records the
/// [`tokio::time::Instant`] it last became settled (`Session::turn` went back
/// to `None`) — using tokio's own clock, not `std::time::Instant`, so the sweep
/// stays test-friendly under a paused/advanced runtime clock. A session absent
/// from the map (not yet reached its first idle point, or already gone) is
/// treated as unsettled — the conservative default that never hibernates a
/// session the sweep can't positively prove is idle.
pub(crate) type ActivityRegistry = Arc<Mutex<HashMap<SessionId, Option<tokio::time::Instant>>>>;

/// Why [`Holly::send_from_wire`] refused an untrusted wire frame (#155).
#[derive(Debug, thiserror::Error)]
pub enum WireError {
    /// A privileged, runtime-authored variant (`tool_result`/`spawn`/`resume`)
    /// arrived from a wire head, which may only forward the allowlist
    /// ([`InMsg::wire_allowed`]). The variant's `kind` tag is carried for
    /// diagnostics.
    #[error("privileged frame `{0}` refused from wire head (runtime-authored only)")]
    Privileged(&'static str),
    /// A `set_tool_overlay` frame carried at least one **enable** entry
    /// (#634, ADR-0177 amending ADR-0149): a wire head may submit a
    /// deny-only overlay (withdrawing tools the profile already advertises)
    /// but never an enable/`allow` entry, which would hand the model a
    /// tool — optionally with no approval prompt — with no authenticated
    /// origin behind the grant.
    #[error("tool-overlay enable entries refused from wire head (deny-only allowed, #634)")]
    OverlayEnable,
    /// The engine inbox is closed (the actor stopped).
    #[error("engine inbox closed")]
    Closed,
}

const INBOX_CAPACITY: usize = 256;
const OUTBOX_CAPACITY: usize = 1024;
/// Bound on a per-session command channel (also the supervisor's routing cap).
const SESSION_CMD_CAPACITY: usize = 64;
/// Bound on the session→supervisor fork channel (ADR-0205). A session forks at
/// most once (it is retired immediately after), so this only ever holds frames
/// from distinct sessions compacting at the same moment.
const FORK_CAPACITY: usize = 64;
/// How many non-blocking `try_send` attempts the supervisor makes before it
/// sheds a command destined for a saturated session (ADR-0028). Yielding
/// between attempts lets a merely-behind session drain; a genuinely stalled one
/// sheds after the last attempt rather than blocking routing to other sessions.
const ROUTE_ATTEMPTS: usize = 8;
/// Profile a new session starts under (ADR-0207 stage 6a collapsed the
/// three-persona-plus-two roster to `general`/`plan`/`debug`; `general` is
/// the default worker persona, formerly named `build`). Public so a head
/// synthesizing its own trusted `Spawn` (the authenticated wire head,
/// ADR-0174) names the same profile the lazy-`Prompt` path resolves, instead
/// of a hardcoded string that could drift.
pub const DEFAULT_AGENT: &str = "general";
/// Permission mode a new session starts under (ADR-0207): authority is a
/// second, independent axis from the agent, so this deliberately shares no
/// definition with [`DEFAULT_AGENT`] — it only happens to share a spelling
/// with the *former* default agent name because `skutter`'s four-mode table
/// names its ordinary interactive posture `build` too. Core carries this name
/// opaquely; it validates against nothing, since the mode table lives in the
/// runtime.
pub const DEFAULT_MODE: &str = "build";

/// Handle to the running engine. Cheap to clone; the actor task lives until
/// [`shutdown`][Holly::shutdown] or until all clones drop (the inbox closes).
#[derive(Clone)]
pub struct Holly {
    inbox: mpsc::Sender<InMsg>,
    /// The outbox plus the fan-out of every inbound [`InMsg`] (lets e.g. the
    /// tool executor observe `Approve`/`Reject`/`Stop`, ADR-0010, #59).
    outlets: Arc<Outlets>,
    /// Shared per-session seq counters (#157) — see [`SeqRegistry`].
    seqs: SeqRegistry,
    /// The engine's configured id generator (ADR-0164), cloned out of
    /// [`EngineConfig::id_gen`] at spawn time so an in-process embedder/head
    /// can mint a fresh [`SessionId`]/job id/request id through the *same*
    /// generator the engine itself would use, instead of reaching for a
    /// hardcoded default.
    id_gen: Arc<dyn IdGen>,
}

impl Holly {
    /// Spawn the engine actor with `cfg` and return a handle.
    pub fn spawn(cfg: EngineConfig) -> Self {
        let (inbox, rx) = mpsc::channel::<InMsg>(INBOX_CAPACITY);
        let (events, _) = broadcast::channel::<OutEvent>(OUTBOX_CAPACITY);
        let (inbound, _) = broadcast::channel::<InMsg>(INBOX_CAPACITY);
        let seqs: SeqRegistry = Arc::new(Mutex::new(HashMap::new()));
        let activity: ActivityRegistry = Arc::new(Mutex::new(HashMap::new()));
        let id_gen = cfg.id_gen.clone();
        let (supervisor_events, supervisor_inbound) = (events.clone(), inbound.clone());
        let supervisor_seqs = seqs.clone();
        let (outlets, shutdown) = Outlets::new(events, inbound);
        outlets.set_supervisor(tokio::spawn(async move {
            supervisor(
                rx,
                supervisor_events,
                supervisor_inbound,
                supervisor_seqs,
                activity,
                cfg,
                shutdown,
            )
            .await
        }));
        Self {
            inbox,
            outlets: Arc::new(outlets),
            seqs,
            id_gen,
        }
    }

    /// Stop every session and the supervisor, closing both broadcasts however
    /// many clones are still alive (#700). Afterwards `send` fails and a fresh
    /// subscription is already closed. Idempotent.
    pub async fn shutdown(&self) {
        if let Some(supervisor) = self.outlets.close() {
            let _ = supervisor.await;
        }
    }

    /// Mint a fresh id of `kind` through the engine's configured
    /// [`IdGen`][crate::id_gen::IdGen] (ADR-0164) — the sanctioned way for a
    /// head/embedder to mint a new [`SessionId`], background-job id, or
    /// runtime request/correlation id so it goes through the same generator
    /// (and, for an embedder that overrides it, the same *policy*) the engine
    /// itself uses internally.
    pub fn next_id(&self, kind: crate::id_gen::IdKind) -> String {
        self.id_gen.next(kind)
    }

    /// Push an [`InMsg`] into the engine — the **privileged in-process** entry
    /// point. An in-process embedder (a head, the runtime tool executor) holds a
    /// `Holly` and is trusted to author any frame, including the runtime-only
    /// [`ToolResult`][InMsg::ToolResult]/[`Spawn`][InMsg::Spawn]. A head relaying
    /// **untrusted wire bytes** must use [`send_from_wire`][Self::send_from_wire]
    /// instead, which enforces the [`InMsg::wire_allowed`] allowlist (#155).
    // The raw tokio `SendError<InMsg>` is the deliberate return type: it carries
    // the rejected frame back to an embedder that may want to re-route it.
    // Boxing it (clippy 1.98's `result_large_err` suggestion) would churn the
    // public API of every head for a size no caller ever moves in bulk.
    #[allow(clippy::result_large_err)]
    pub async fn send(&self, msg: InMsg) -> Result<(), mpsc::error::SendError<InMsg>> {
        self.inbox.send(msg).await
    }

    /// Relay an [`InMsg`] **deserialized from an untrusted wire head** (stdio
    /// `pipe`, WebSocket `serve`), enforcing the trusted/untrusted frame
    /// split (#155). A privileged, runtime-authored variant
    /// ([`ToolResult`][InMsg::ToolResult]/[`Spawn`][InMsg::Spawn]/
    /// [`Resume`][InMsg::Resume]) is **refused**, not routed — a forged
    /// `ToolResult` would resolve a parked turn on `request_id` alone (bypassing
    /// execution + permission) and a forged `Spawn` would bypass the tool path's
    /// spawn-refusal gate. A [`SetToolOverlay`][InMsg::SetToolOverlay] frame is
    /// refused only if it carries an enable entry — a deny-only overlay passes
    /// (#634, [`InMsg::wire_allowed`] is content-aware for this one variant).
    /// Head-authored frames pass through to [`send`][Self::send]. The runtime's
    /// own executor never calls this; it holds the privileged handle and uses
    /// [`submit_tool_result`][Self::submit_tool_result].
    pub async fn send_from_wire(&self, msg: InMsg) -> Result<(), WireError> {
        if !msg.wire_allowed() {
            let err = if matches!(&msg, InMsg::SetToolOverlay { .. }) {
                WireError::OverlayEnable
            } else {
                WireError::Privileged(msg.variant_name())
            };
            tracing::warn!(variant = msg.variant_name(), %err, "refused InMsg from wire head");
            return Err(err);
        }
        self.inbox.send(msg).await.map_err(|_| WireError::Closed)
    }

    /// Submit a tool result over the **privileged in-process handle** (#155). This
    /// is the sanctioned path for the runtime tool executor to fold a completed
    /// `ToolExec` round-trip back into the parked turn — kept distinct from the
    /// untrusted [`send_from_wire`][Self::send_from_wire] path so a `ToolResult`
    /// is never forgeable off the wire. A thin wrapper over the privileged
    /// [`send`][Self::send]; the executor holds a `Holly`, so it is trusted.
    /// `is_error`/`duration_ms` (#636, ADR-0176) are the structured side channel
    /// — whether the call was denied/masked/refused/errored, and how long
    /// execution took — carried alongside `content`'s text. `exit_code` (#681,
    /// ADR-0186) extends it with a process-running tool's numeric exit status
    /// (`None` for every non-process tool and for a signal-killed process).
    #[allow(clippy::result_large_err)] // see `send` — the raw SendError is deliberate
    pub async fn submit_tool_result(
        &self,
        session: SessionId,
        request_id: String,
        content: Vec<ContentPart>,
        is_error: bool,
        duration_ms: Option<u64>,
        exit_code: Option<i32>,
    ) -> Result<(), mpsc::error::SendError<InMsg>> {
        self.send(InMsg::ToolResult {
            session,
            request_id,
            content,
            is_error,
            duration_ms,
            exit_code,
        })
        .await
    }

    /// Whether `session` has a running task: `true` from its
    /// [`SessionStarted`][OutEvent::SessionStarted] through its
    /// [`SessionEnded`][OutEvent::SessionEnded]/[`SessionHibernated`][OutEvent::SessionHibernated].
    /// A cheap synchronous read of the engine's own registry, so an embedder
    /// need not mirror liveness by folding the broadcast. Advisory only: a
    /// message already in the inbox may change the answer before it is routed —
    /// use [`EngineConfig::unknown_session`] to decide what a message to a
    /// non-live id does, rather than check-then-send (ADR-0210).
    pub fn is_live(&self, session: &SessionId) -> bool {
        self.seqs
            .lock()
            .expect("seq registry mutex poisoned")
            .contains_key(session)
    }

    /// Subscribe to the outbound event stream (every session, fan-out).
    pub fn subscribe(&self) -> broadcast::Receiver<OutEvent> {
        self.outlets.subscribe()
    }

    /// Mint the next seq for `session` from the shared registry, or `0` when the
    /// session has no live counter (ended / never started). Shared by
    /// [`emit_for_session`][Self::emit_for_session] and the supervisor's
    /// error-emission path.
    pub(crate) fn next_seq(&self, session: &SessionId) -> u64 {
        next_seq_for(&self.seqs, session)
    }

    /// Subscribe to the inbound [`InMsg`] fan-out. Every message sent through
    /// [`send`][Self::send] is cloned here before the supervisor routes it, so a
    /// runtime service (e.g. the tool executor) can react to `Approve`/`Reject`/
    /// `Stop` without the engine having to interpret them.
    pub fn subscribe_inbound(&self) -> broadcast::Receiver<InMsg> {
        self.outlets.subscribe_inbound()
    }

    /// Resume `root_id` from its replayed `(Option<InMsg>, OutEvent)` log
    /// records: the supervisor rebuilds the session state and spawns a task
    /// seeded from it. Returns `root_id`.
    #[allow(clippy::result_large_err)] // see `send` — the raw SendError is deliberate
    pub async fn resume(
        &self,
        root_id: SessionId,
        records: Vec<(Option<InMsg>, OutEvent)>,
    ) -> Result<SessionId, mpsc::error::SendError<InMsg>> {
        self.inbox
            .send(InMsg::Resume {
                session: root_id.clone(),
                records,
            })
            .await?;
        Ok(root_id)
    }

    /// Hibernate a session: evict its in-memory state without tombstoning the id
    /// (#318, ADR-0077). Tears down the session task and its sub-tree, releasing
    /// each [`Context`][crate::context::Context], but leaves the id **resumable**
    /// — a later [`resume`][Self::resume] rebuilds it from the embedder's event
    /// log. The **privileged in-process** control for a long-lived embedder to cap
    /// memory across many sessions; it is trusted-only (not wire-allowed), so a
    /// wire head cannot evict another session. A thin wrapper over the privileged
    /// [`send`][Self::send]. Emits [`OutEvent::SessionHibernated`]; an unknown id
    /// is a no-op.
    #[allow(clippy::result_large_err)] // see `send` — the raw SendError is deliberate
    pub async fn hibernate(&self, session: SessionId) -> Result<(), mpsc::error::SendError<InMsg>> {
        self.send(InMsg::HibernateSession { session }).await
    }
}
