//! The engine actor. [`Holly`] owns a process-wide inbox (`mpsc<InMsg>`) and
//! outbox (`broadcast<OutEvent>`). The supervisor routes inbound messages to
//! per-session tasks (lazily spawned, one per [`SessionId`]).
//!
//! This is the ABI foundation: an embedder holds a (cheaply-cloned) `Holly`,
//! calls [`Holly::send`] with typed [`InMsg`]s and drains
//! [`Holly::subscribe`] for [`OutEvent`]s — no serialization. Every transport
//! (stdio, WS, TUI) is a thin adapter over these two methods.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{broadcast, mpsc};

use crate::protocol::{AgentState, InMsg, OutEvent, SessionId, SessionInfo};
use crate::session::{session_loop, Session, SessionCmd};
use entanglement_provider::ContentPart;

mod config;
mod routing;

pub use config::{
    ConfigError, EngineConfig, ProfileRegistry, SystemPromptResolver, ToolSpecResolver,
};
use routing::{emit_supervisor_error, msg_to_cmd, resume_meta, route_to_session};

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
    /// The engine inbox is closed (the actor stopped).
    #[error("engine inbox closed")]
    Closed,
}

const INBOX_CAPACITY: usize = 256;
const OUTBOX_CAPACITY: usize = 1024;
/// Bound on a per-session command channel (also the supervisor's routing cap).
const SESSION_CMD_CAPACITY: usize = 64;
/// How many non-blocking `try_send` attempts the supervisor makes before it
/// sheds a command destined for a saturated session (ADR-0028). Yielding
/// between attempts lets a merely-behind session drain; a genuinely stalled one
/// sheds after the last attempt rather than blocking routing to other sessions.
const ROUTE_ATTEMPTS: usize = 8;
/// Profile a new session starts under (opencode-style: `build` is the default).
const DEFAULT_PROFILE: &str = "build";

/// Handle to the running engine. Cheap to clone; the actor task lives until all
/// clones drop (the inbox closes) or every session stops.
#[derive(Clone)]
pub struct Holly {
    inbox: mpsc::Sender<InMsg>,
    events: broadcast::Sender<OutEvent>,
    /// Fan-out of every inbound [`InMsg`] (cloned before routing). Lets a
    /// runtime-side service observe protocol messages it doesn't route itself —
    /// e.g. the tool executor watching `Approve`/`Reject`/`Stop` while it owns
    /// permission dispatch + approval (ADR-0010, #59).
    inbound: broadcast::Sender<InMsg>,
    /// Shared per-session seq counters (#157) — see [`SeqRegistry`].
    seqs: SeqRegistry,
}

impl Holly {
    /// Spawn the engine actor with `cfg` and return a handle.
    pub fn spawn(cfg: EngineConfig) -> Self {
        let (inbox, rx) = mpsc::channel::<InMsg>(INBOX_CAPACITY);
        let (events, _) = broadcast::channel::<OutEvent>(OUTBOX_CAPACITY);
        let (inbound, _) = broadcast::channel::<InMsg>(INBOX_CAPACITY);
        let seqs: SeqRegistry = Arc::new(Mutex::new(HashMap::new()));
        let activity: ActivityRegistry = Arc::new(Mutex::new(HashMap::new()));
        let supervisor_events = events.clone();
        let supervisor_inbound = inbound.clone();
        let supervisor_seqs = seqs.clone();
        tokio::spawn(async move {
            supervisor(
                rx,
                supervisor_events,
                supervisor_inbound,
                supervisor_seqs,
                activity,
                cfg,
            )
            .await
        });
        Self {
            inbox,
            events,
            inbound,
            seqs,
        }
    }

    /// Push an [`InMsg`] into the engine — the **privileged in-process** entry
    /// point. An in-process embedder (a head, the runtime tool executor) holds a
    /// `Holly` and is trusted to author any frame, including the runtime-only
    /// [`ToolResult`][InMsg::ToolResult]/[`Spawn`][InMsg::Spawn]. A head relaying
    /// **untrusted wire bytes** must use [`send_from_wire`][Self::send_from_wire]
    /// instead, which enforces the [`InMsg::wire_allowed`] allowlist (#155).
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
    /// spawn-refusal gate. Head-authored frames pass through to
    /// [`send`][Self::send]. The runtime's own executor never calls this; it holds
    /// the privileged handle and uses [`submit_tool_result`][Self::submit_tool_result].
    pub async fn send_from_wire(&self, msg: InMsg) -> Result<(), WireError> {
        if !msg.wire_allowed() {
            tracing::warn!(
                variant = msg.variant_name(),
                "refused privileged InMsg from wire head (runtime-authored only)"
            );
            return Err(WireError::Privileged(msg.variant_name()));
        }
        self.inbox.send(msg).await.map_err(|_| WireError::Closed)
    }

    /// Submit a tool result over the **privileged in-process handle** (#155). This
    /// is the sanctioned path for the runtime tool executor to fold a completed
    /// `ToolExec` round-trip back into the parked turn — kept distinct from the
    /// untrusted [`send_from_wire`][Self::send_from_wire] path so a `ToolResult`
    /// is never forgeable off the wire. A thin wrapper over the privileged
    /// [`send`][Self::send]; the executor holds a `Holly`, so it is trusted.
    pub async fn submit_tool_result(
        &self,
        session: SessionId,
        request_id: String,
        content: Vec<ContentPart>,
    ) -> Result<(), mpsc::error::SendError<InMsg>> {
        self.send(InMsg::ToolResult {
            session,
            request_id,
            content,
        })
        .await
    }

    /// Subscribe to the outbound event stream (every session, fan-out).
    pub fn subscribe(&self) -> broadcast::Receiver<OutEvent> {
        self.events.subscribe()
    }

    /// Mint a fresh monotonic per-session `seq` and broadcast a runtime-authored
    /// content event for `session` (#157). The sanctioned way for a runtime
    /// service to emit a seq-bearing [`OutEvent`] while a session is parked (an
    /// approval `ToolRequest`/`UserQuestion`, a `Plan`/`TaskList` snapshot, a
    /// `FileChange`): the seq is drawn from the session's shared counter — the
    /// *same* sequence the session task uses — so `(session, seq)` stays unique
    /// instead of reusing the parked `ToolExec` seq. `make` receives the minted
    /// seq and builds the event. A session with no live counter (already ended,
    /// or never started) mints seq `0` — harmless, as there is no live content
    /// stream to collide with.
    pub fn emit_for_session(&self, session: &SessionId, make: impl FnOnce(u64) -> OutEvent) {
        let seq = self.next_seq(session);
        let _ = self.events.send(make(seq));
    }

    /// Broadcast a point-in-time lifecycle [`OutEvent::Status`] for `session`.
    /// Status carries no `seq` (it's not content, so it's exempt from the seq
    /// contract), so no counter is touched — this is the seq-less sibling of
    /// [`emit_for_session`][Self::emit_for_session] for the runtime's `Thinking`/
    /// `WaitingApproval` transitions around a parked tool call.
    pub fn emit_status(&self, session: &SessionId, state: AgentState) {
        let _ = self.events.send(OutEvent::Status {
            session: session.clone(),
            state,
        });
    }

    /// Broadcast a runtime-authored [`OutEvent::History`] reply to an
    /// [`InMsg::ReplayFrom`] query (#160, ADR-0072). Like
    /// [`emit_status`][Self::emit_status] it carries no `seq` — a supervisor-global
    /// query reply, not session content — so no counter is touched. The sanctioned
    /// way for the runtime's log-owning history responder to answer a late
    /// subscriber without the raw outbound sender being exposed.
    pub fn emit_history(&self, correlation_id: String, session: SessionId, events: Vec<OutEvent>) {
        let _ = self.events.send(OutEvent::History {
            correlation_id,
            session,
            events,
        });
    }

    /// Broadcast a runtime-authored [`OutEvent::McpList`] reply to an
    /// [`InMsg::McpList`] query (#375). Engine-global like
    /// [`emit_history`][Self::emit_history]: no `seq`, no counter touched.
    pub fn emit_mcp_list(
        &self,
        correlation_id: String,
        servers: Vec<crate::protocol::McpServerStatus>,
    ) {
        let _ = self.events.send(OutEvent::McpList {
            correlation_id,
            servers,
        });
    }

    /// Broadcast a runtime-authored [`OutEvent::McpChanged`] reply to
    /// [`InMsg::McpAdd`]/[`InMsg::McpRemove`] (#375). No `seq` — a point-in-time
    /// engine-global lifecycle event, not session content.
    pub fn emit_mcp_changed(&self, name: String, action: crate::protocol::McpAction) {
        let _ = self.events.send(OutEvent::McpChanged { name, action });
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
        self.inbound.subscribe()
    }

    /// Resume a session from replayed log records.
    ///
    /// This reconstructs the session state from the provided records and spawns
    /// a session task seeded from that state. Returns the session ID.
    ///
    /// # Parameters
    ///
    /// - `root_id`: The session ID to resume
    /// - `records`: A slice of `(Option<InMsg>, OutEvent)` tuples representing the log
    ///
    /// # Returns
    ///
    /// The session ID of the resumed session.
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
    pub async fn hibernate(&self, session: SessionId) -> Result<(), mpsc::error::SendError<InMsg>> {
        self.send(InMsg::HibernateSession { session }).await
    }
}

/// Route inbound messages to per-session tasks, lazily spawning one per new
/// [`SessionId`]. Exits (stopping all sessions) when the inbox closes.
async fn supervisor(
    mut rx: mpsc::Receiver<InMsg>,
    events: broadcast::Sender<OutEvent>,
    inbound: broadcast::Sender<InMsg>,
    seqs: SeqRegistry,
    activity: ActivityRegistry,
    cfg: EngineConfig,
) {
    let mut sessions: HashMap<SessionId, mpsc::Sender<SessionCmd>> = HashMap::new();
    // Live-session directory, kept in lockstep with `sessions`, so `ListSessions`
    // can answer without folding the outbound broadcast (ADR-0028). A session
    // task only exits when its channel is dropped (CloseSession / shutdown), so
    // `sessions` is the liveness source of truth and this never drifts.
    let mut session_meta: HashMap<SessionId, SessionInfo> = HashMap::new();
    // Tombstone set of session ids retired by `CloseSession`. Ids are single-use
    // (ADR-0028): once closed, no path — lazy prompt, `Resume`, or `Spawn` — may
    // resurrect the id under a fresh, blank session (issue #105). A head that
    // already rendered `SessionEnded` must never see a second `SessionStarted`.
    let mut closed: HashSet<SessionId> = HashSet::new();
    // child → parent. Populated on `Spawn` (#60) so a child's `SessionStarted`
    // (and the tree-walk helpers that read it) reflect the real hierarchy;
    // previously nothing ever inserted here, so every session was a root.
    let mut parent_links: HashMap<SessionId, Option<SessionId>> = HashMap::new();
    // Idle-TTL auto-hibernation sweep (#363). `None` (the default) makes this
    // branch never armed, so a `select!` with no `idle_ttl` configured takes the
    // exact same path as the old bare `rx.recv().await` — byte-identical
    // behavior when the feature is off. A coarse interval, not a per-session
    // timer: sweeping is O(sessions) and cheap, and a quarter of the TTL (floored
    // at 30s) is plenty precise for an eviction policy, not a scheduler.
    let mut sweep = cfg
        .idle_ttl
        .map(|ttl| (ttl, tokio::time::interval(sweep_period(ttl))));

    loop {
        let msg = match sweep.as_mut() {
            Some((ttl, timer)) => {
                tokio::select! {
                    biased;
                    m = rx.recv() => m,
                    _ = timer.tick() => {
                        sweep_idle_sessions(*ttl, &mut sessions, &mut session_meta, &mut parent_links, &activity).await;
                        continue;
                    }
                }
            }
            None => rx.recv().await,
        };
        let Some(msg) = msg else { break };
        // Fan the message out to inbound subscribers (runtime services) before
        // routing it. A closed/lagging subscriber is not fatal to routing.
        let _ = inbound.send(msg.clone());

        // Runtime-consumed off the inbound fan-out above, never routed to a
        // session task: approval decisions (`Approve`/`Reject`, #59),
        // `AnswerQuestion` for `ask_user` (ADR-0027), and the `ReplayFrom`
        // history query answered by the runtime's log-owning responder (#160).
        if matches!(
            msg,
            InMsg::Approve { .. }
                | InMsg::Reject { .. }
                | InMsg::AnswerQuestion { .. }
                | InMsg::ReplayFrom { .. }
        ) {
            continue;
        }

        // Supervisor-global lifecycle queries (ADR-0028): answered here, never
        // routed to a session task. `correlation_id` is an opaque token echoed
        // on the reply (#160) — not an overloaded session id.
        if let InMsg::ListSessions { correlation_id } = &msg {
            let mut list: Vec<SessionInfo> = session_meta.values().cloned().collect();
            list.sort_by(|a, b| a.session.0.cmp(&b.session.0));
            let _ = events.send(OutEvent::SessionList {
                correlation_id: correlation_id.clone(),
                sessions: list,
            });
            continue;
        }

        // Every remaining variant is session-scoped except `ListSessions`
        // (handled above) and the MCP ops `McpList`/`McpAdd`/`McpRemove` (#375):
        // MCP config is engine-global, so they carry no session either — they
        // are answered by a runtime service off the `inbound` fan-out above,
        // never routed here, so they simply fall through to this `continue`.
        let Some(session_id) = msg.session().cloned() else {
            continue;
        };
        if let InMsg::CloseSession { session } = &msg {
            // Cascade (#180): closing a session retires its whole sub-tree, not
            // just the target. `parent_links` is child→parent, so a spawned
            // descendant left running has no consumer for its answers and keeps
            // burning provider tokens. Walk the tree and close every descendant
            // alongside the target.
            // The closed sub-tree's root has a parent *outside* the closed set
            // (descendants' parents are themselves closing) — capture it before
            // the links are torn down so the still-live parent's `children`
            // mirror can drop the retired child.
            let root_parent = parent_links.get(session).cloned().flatten();
            for victim in collect_subtree(session, &parent_links) {
                // Dropping the command channel makes the task's `rx.recv()`
                // return `None`; it emits `SessionEnded` and exits. Unknown
                // id → no-op.
                if sessions.remove(&victim).is_some() {
                    session_meta.remove(&victim);
                }
                parent_links.remove(&victim);
                // Tombstone the id regardless of liveness: it is spent
                // (ADR-0028), so a `Prompt` queued behind this `CloseSession`
                // can't respawn it blank.
                closed.insert(victim);
            }
            if let Some(parent) = root_parent {
                if let Some(ptx) = sessions.get(&parent) {
                    let _ = ptx.send(SessionCmd::ChildClosed(session.clone())).await;
                }
            }
            continue;
        }

        if let InMsg::HibernateSession { session } = &msg {
            // Memory eviction, not termination (#318, ADR-0077). Cascades over
            // the spawn sub-tree and tombstones nothing — see `hibernate_subtree`.
            // Shared with the idle-TTL sweep (#363), which reaches the same code
            // when it decides a settled root has been idle past `idle_ttl`.
            hibernate_subtree(session, &mut sessions, &mut session_meta, &mut parent_links).await;
            continue;
        }

        if let InMsg::Resume { records, .. } = &msg {
            // A retired id is single-use; refuse rather than resurrect (ADR-0028).
            if closed.contains(&session_id) {
                emit_supervisor_error(
                    &events,
                    &seqs,
                    &session_id,
                    "cannot resume a closed session id (ids are single-use)",
                );
                continue;
            }
            // Resuming a live id would overwrite its sender and orphan the running
            // task (it sees its channel close mid-turn). Refuse, like `Spawn`.
            if sessions.contains_key(&session_id) {
                emit_supervisor_error(
                    &events,
                    &seqs,
                    &session_id,
                    "cannot resume an already-live session id",
                );
                continue;
            }
            // Replay *before* registering the session. A failed replay used to
            // still insert the sender while its task returned early, leaving a
            // dead id that showed in `ListSessions` and silently swallowed every
            // routed `Prompt` (issue #105). Register only on success; on failure
            // surface an `Error` and leave the id unclaimed.
            let initial_session = match Session::replay(records, &cfg) {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!("Failed to replay session {}: {}", session_id, e);
                    emit_supervisor_error(
                        &events,
                        &seqs,
                        &session_id,
                        &format!("failed to resume session: {e}"),
                    );
                    continue;
                }
            };
            // Enrich the replay-derived meta with the resolved posture (#189): the
            // log preserves only the profile name, but the replayed session holds
            // the full profile, so a reconnecting head sees the live posture.
            let mut meta = resume_meta(&session_id, records);
            meta.profile_detail = Some(initial_session.profile.detail());
            session_meta.insert(session_id.clone(), meta);
            let (stx, srx) = mpsc::channel::<SessionCmd>(SESSION_CMD_CAPACITY);
            let ev = events.clone();
            let cfg2 = cfg.clone();
            let sid = session_id.clone();
            let profile = initial_session.profile.clone();
            let parent = initial_session.parent.clone();
            let seqs2 = seqs.clone();
            let activity2 = activity.clone();
            tokio::spawn(async move {
                session_loop(
                    sid,
                    srx,
                    ev,
                    cfg2,
                    profile,
                    Some(initial_session),
                    parent,
                    // Resume reconstructs `predecessor` from the log (replay);
                    // pass `None` so it isn't overwritten.
                    None,
                    seqs2,
                    activity2,
                )
                .await;
            });
            sessions.insert(session_id.clone(), stx);
            continue;
        }

        if let InMsg::Spawn {
            session: child,
            parent,
            predecessor,
            agent,
            prompt,
        } = &msg
        {
            // A duplicate spawn for a live child is a no-op (the child already runs).
            if sessions.contains_key(child) {
                continue;
            }
            // A retired id is single-use; never respawn it (ADR-0028, issue #105).
            if closed.contains(child) {
                emit_supervisor_error(
                    &events,
                    &seqs,
                    child,
                    "cannot spawn a closed session id (ids are single-use)",
                );
                continue;
            }
            // An unknown spawn target must not silently escalate to `build` (the
            // most-privileged default): `resolve` would fall back there, so a
            // typo'd `Spawn` would launch a full coding agent. `get` + a
            // supervisor error refuses instead (#119). The lazy-Prompt path below
            // still uses `resolve` — that fallback is a blank user session, not a
            // model-chosen spawn target.
            let profile = match cfg.profiles.get(agent) {
                Some(p) => p.clone(),
                None => {
                    emit_supervisor_error(
                        &events,
                        &seqs,
                        child,
                        &format!("cannot spawn unknown agent profile `{agent}`"),
                    );
                    continue;
                }
            };
            // `parent = None` is a root spawn (the `/compact` successor fork,
            // ADR-0108): it records `predecessor` for lineage but joins no spawn
            // sub-tree, so a `CloseSession` on the source never cascades onto it.
            let parent = parent.clone();
            let is_root = parent.is_none();
            // Record the parent link *before* spawning so it's in place for any
            // later lazy path, and so the child starts under the requested profile.
            parent_links.insert(child.clone(), parent.clone());
            session_meta.insert(
                child.clone(),
                SessionInfo {
                    session: child.clone(),
                    parent: parent.clone(),
                    profile: profile.name.clone(),
                    root: is_root,
                    profile_detail: Some(profile.detail()),
                },
            );
            let (stx, srx) = mpsc::channel::<SessionCmd>(SESSION_CMD_CAPACITY);
            let ev = events.clone();
            let cfg2 = cfg.clone();
            let sid = child.clone();
            let predecessor = predecessor.clone();
            let parent_for_loop = parent.clone();
            let seqs2 = seqs.clone();
            let activity2 = activity.clone();
            tokio::spawn(async move {
                session_loop(
                    sid,
                    srx,
                    ev,
                    cfg2,
                    profile,
                    None,
                    parent_for_loop,
                    predecessor,
                    seqs2,
                    activity2,
                )
                .await
            });
            // Queue the initial prompt; the child drains it after its lifecycle
            // events. Spawn prompts are text-only (#197).
            let content = vec![entanglement_provider::ContentPart::text(prompt.clone())];
            let _ = stx.send(SessionCmd::Prompt(content)).await;
            sessions.insert(child.clone(), stx);
            // Mirror the child→parent edge onto the parent's live `children`
            // list. Best-effort: if the parent task already exited, the edge
            // still lives in `parent_links`; the mirror is a convenience view.
            if let Some(parent_id) = parent.as_ref() {
                if let Some(ptx) = sessions.get(parent_id) {
                    let _ = ptx.send(SessionCmd::ChildSpawned(child.clone())).await;
                }
            }
            continue;
        }

        // Non-routable variants are all handled/continued above; a stray one
        // maps to `None` and is dropped rather than crashing the supervisor
        // (#160 — the former `unreachable!` would have taken down every session).
        let Some(cmd) = msg_to_cmd(msg.clone()) else {
            tracing::warn!(
                variant = msg.variant_name(),
                "non-routable message reached session routing; dropped"
            );
            continue;
        };

        if !sessions.contains_key(&session_id) {
            // A closed id is spent (ADR-0028): a `Prompt` that raced behind its
            // `CloseSession` must not lazily respawn a blank session under it
            // (issue #105). Refuse with feedback instead of silently resurrecting.
            if closed.contains(&session_id) {
                emit_supervisor_error(
                    &events,
                    &seqs,
                    &session_id,
                    "session id is closed (ids are single-use); mint a fresh session id",
                );
                continue;
            }
            let profile = cfg.profiles.resolve(DEFAULT_PROFILE);
            let parent = parent_links.get(&session_id).cloned().flatten();
            session_meta.insert(
                session_id.clone(),
                SessionInfo {
                    session: session_id.clone(),
                    parent: parent.clone(),
                    profile: profile.name.clone(),
                    root: parent.is_none(),
                    profile_detail: Some(profile.detail()),
                },
            );
            let (stx, srx) = mpsc::channel::<SessionCmd>(SESSION_CMD_CAPACITY);
            let ev = events.clone();
            let cfg2 = cfg.clone();
            let sid = session_id.clone();
            let seqs2 = seqs.clone();
            let activity2 = activity.clone();
            tokio::spawn(async move {
                session_loop(
                    sid, srx, ev, cfg2, profile, None, parent, None, seqs2, activity2,
                )
                .await
            });
            sessions.insert(session_id.clone(), stx);
        }

        if let Some(tx) = sessions.get(&session_id) {
            route_to_session(tx, cmd, &session_id, &events, &seqs).await;
        }
    }
    // Inbox closed: signal every session to stop. Their tasks return on receipt.
    for (_, tx) in sessions.drain() {
        let _ = tx.send(SessionCmd::Stop).await;
    }
}

/// Collect `root` plus every transitive descendant from the child→parent
/// `parent_links` map (breadth-first). Backs the `CloseSession` cascade (#180):
/// closing a session must retire its whole sub-tree so no spawned sub-agent is
/// left orphaned. Small session counts make the O(n²) scan a non-issue.
fn collect_subtree(
    root: &SessionId,
    parent_links: &HashMap<SessionId, Option<SessionId>>,
) -> Vec<SessionId> {
    let mut subtree = vec![root.clone()];
    let mut cursor = 0;
    while cursor < subtree.len() {
        let current = subtree[cursor].clone();
        for (child, parent) in parent_links {
            if parent.as_ref() == Some(&current) && !subtree.contains(child) {
                subtree.push(child.clone());
            }
        }
        cursor += 1;
    }
    subtree
}

/// Evict `root` plus its whole spawn sub-tree: tear down each live task, drop
/// its meta/parent-link entries, and record **no** tombstone so every id stays
/// resumable (#318, ADR-0077). Shared by the `HibernateSession` handler and the
/// idle-TTL sweep (#363) — the only two paths that decide a session should be
/// evicted.
async fn hibernate_subtree(
    root: &SessionId,
    sessions: &mut HashMap<SessionId, mpsc::Sender<SessionCmd>>,
    session_meta: &mut HashMap<SessionId, SessionInfo>,
    parent_links: &mut HashMap<SessionId, Option<SessionId>>,
) {
    for victim in collect_subtree(root, parent_links) {
        if let Some(tx) = sessions.remove(&victim) {
            // Deliver `Hibernate`, then drop the sender: a buffered command is
            // received before the channel-closed `None`, so the task tears down
            // via `SessionHibernated`. The drop also unblocks a turn parked
            // mid-stream — its `rx.recv()` returns `None` (cancel), then the
            // stashed `Hibernate` pops when idle (stop-then-hibernate). Awaiting
            // is safe: the session drains its own channel independently of this
            // loop.
            let _ = tx.send(SessionCmd::Hibernate).await;
        }
        session_meta.remove(&victim);
        parent_links.remove(&victim);
        // Deliberately NOT inserted into `closed`: the id is evictable and
        // rebuildable, never spent.
    }
}

/// Coarse polling period for the idle-TTL sweep (#363): a quarter of the TTL,
/// floored at 30s so a short TTL doesn't spin the supervisor loop. This is an
/// eviction policy, not a scheduler — a session can sit idle up to one extra
/// sweep period past `idle_ttl` before it's noticed.
fn sweep_period(ttl: Duration) -> Duration {
    (ttl / 4).max(Duration::from_secs(30))
}

/// One idle-TTL sweep tick (#363): auto-hibernate every **settled** root whose
/// whole spawn sub-tree has been continuously idle for at least `ttl`.
/// Judged per root, strictly — a live turn or a single parked child anywhere in
/// the sub-tree pins the whole tree live, matching manual `HibernateSession`'s
/// stricter-than-`Stop` posture but going further: unlike a manual hibernate
/// (stop-then-hibernate), a timer must never cancel live work, so this sweep
/// only ever touches a session already at rest. The idle clock for a sub-tree
/// starts at the *latest* member's settle time — a recently-active child resets
/// the whole tree's window, not just its own.
async fn sweep_idle_sessions(
    ttl: Duration,
    sessions: &mut HashMap<SessionId, mpsc::Sender<SessionCmd>>,
    session_meta: &mut HashMap<SessionId, SessionInfo>,
    parent_links: &mut HashMap<SessionId, Option<SessionId>>,
    activity: &ActivityRegistry,
) {
    let now = tokio::time::Instant::now();
    let roots: Vec<SessionId> = session_meta
        .values()
        .filter(|m| m.root)
        .map(|m| m.session.clone())
        .collect();
    for root in roots {
        if !sessions.contains_key(&root) {
            continue;
        }
        let subtree = collect_subtree(&root, parent_links);
        let settled_since = {
            let idle = activity.lock().expect("activity registry mutex poisoned");
            let mut latest: Option<tokio::time::Instant> = None;
            let mut all_settled = true;
            for member in &subtree {
                match idle.get(member) {
                    Some(Some(t)) => latest = Some(latest.map_or(*t, |l| l.max(*t))),
                    _ => {
                        all_settled = false;
                        break;
                    }
                }
            }
            all_settled.then_some(latest).flatten()
        };
        let Some(since) = settled_since else { continue };
        if now.saturating_duration_since(since) >= ttl {
            hibernate_subtree(&root, sessions, session_meta, parent_links).await;
        }
    }
}
