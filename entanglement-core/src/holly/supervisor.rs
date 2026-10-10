//! The supervisor actor: routes inbound messages to per-session tasks and owns
//! the session directory (liveness, spawn tree, tombstones, mode cache). The
//! `Spawn`/lazy-root, `Resume`, unknown-session policy (ADR-0210) and
//! sub-tree/idle-sweep paths live in the child modules.

use std::collections::{HashMap, HashSet};

use tokio::sync::{broadcast, mpsc};

use super::outlets;
use super::routing::{msg_to_cmd, route_to_session};
use super::{ActivityRegistry, EngineConfig, SeqRegistry, FORK_CAPACITY};
use crate::protocol::{InMsg, OutEvent, SessionId, SessionInfo};
use crate::session::SessionCmd;

mod resume;
mod spawn;
mod subtree;
mod unknown;

use subtree::{
    collect_subtree, hibernate_subtree, successor_of, sweep_idle_sessions, sweep_period,
};

/// The supervisor loop's state, threaded through the per-message handlers.
struct Supervisor {
    sessions: HashMap<SessionId, mpsc::Sender<SessionCmd>>,
    // source → successor (ADR-0205), so a tool result still addressed to a
    // session that compacted away resolves against the session that took over
    // its turn instead of hitting the closed-id refusal.
    successors: HashMap<SessionId, SessionId>,
    // Live-session directory, kept in lockstep with `sessions`, so `ListSessions`
    // can answer without folding the outbound broadcast (ADR-0028). A session
    // task only exits when its channel is dropped (CloseSession / shutdown), so
    // `sessions` is the liveness source of truth and this never drifts.
    session_meta: HashMap<SessionId, SessionInfo>,
    // Tombstone set of session ids retired by `CloseSession`. Ids are single-use
    // (ADR-0028): once closed, no path — lazy prompt, `Resume`, or `Spawn` — may
    // resurrect the id under a fresh, blank session (issue #105). A head that
    // already rendered `SessionEnded` must never see a second `SessionStarted`.
    closed: HashSet<SessionId>,
    // child → parent. Populated on `Spawn` (#60) so a child's `SessionStarted`
    // (and the tree-walk helpers that read it) reflect the real hierarchy;
    // previously nothing ever inserted here, so every session was a root.
    parent_links: HashMap<SessionId, Option<SessionId>>,
    // session → its live mode (ADR-0207 §6), the supervisor's own cache — not
    // the wire `SessionInfo`, which stays a spawn-time snapshot like its other
    // fields. Written on every `SetMode` cascade and on `Spawn`/resume, read
    // only to seed a fresh child's `initial_mode` from its parent. Sparse: an
    // absent entry means `DEFAULT_MODE`, so a session never explicitly set
    // never needs an entry. Cleared on `CloseSession` (the id is retired for
    // good); left in place across a hibernate/resume cycle, since resume
    // re-derives it from the replayed truth (`spawn_resumed`) and nothing
    // consults it while the session is hibernated.
    modes: HashMap<SessionId, String>,
    events: broadcast::Sender<OutEvent>,
    seqs: SeqRegistry,
    activity: ActivityRegistry,
    cfg: EngineConfig,
    // Session→supervisor channel for the two frames a compaction fork needs
    // (ADR-0205): a session task cannot mint a session, so it asks here. These
    // are ordinary `InMsg`s handled by the loop below exactly like inbox ones,
    // fan-out included — which is what lets the persistence tap synthesize the
    // successor's seed prompt from its `Spawn` (ADR-0113) and so replay it.
    //
    // Deliberately *not* the inbox: the supervisor holding an inbox sender
    // would keep the actor alive after every `Holly` handle dropped. This
    // channel dies with the supervisor instead.
    fork_tx: mpsc::Sender<InMsg>,
    // Ids with a `SessionLoader` call in flight (ADR-0210) → the routable
    // messages that arrived for them meanwhile, in arrival order. Presence is
    // the "one load per id" guard; the entry is drained on completion.
    pending_loads: HashMap<SessionId, Vec<InMsg>>,
    // Load completions flow back here rather than through the inbox, for the
    // same keep-the-actor-alive reason as `fork_tx`.
    load_tx: mpsc::Sender<unknown::LoadDone>,
}

/// Route inbound messages to per-session tasks, lazily spawning one per new
/// [`SessionId`]. Exits (stopping all sessions) when the inbox closes or
/// [`Holly::shutdown`][super::Holly::shutdown] is requested.
pub(super) async fn supervisor(
    mut rx: mpsc::Receiver<InMsg>,
    events: broadcast::Sender<OutEvent>,
    inbound: broadcast::Sender<InMsg>,
    seqs: SeqRegistry,
    activity: ActivityRegistry,
    cfg: EngineConfig,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let (fork_tx, mut fork_rx) = mpsc::channel::<InMsg>(FORK_CAPACITY);
    let (load_tx, mut load_rx) = mpsc::channel::<unknown::LoadDone>(FORK_CAPACITY);
    // Idle-TTL auto-hibernation sweep (#363). `None` (the default) leaves the
    // tick branch pending forever, so with no `idle_ttl` configured the loop
    // only ever wakes for messages. A coarse interval, not a per-session
    // timer: sweeping is O(sessions) and cheap, and a quarter of the TTL (floored
    // at 30s) is plenty precise for an eviction policy, not a scheduler.
    let ttl = cfg.idle_ttl;
    let mut sweep = ttl.map(|ttl| tokio::time::interval(sweep_period(ttl)));
    let mut sup = Supervisor {
        sessions: HashMap::new(),
        successors: HashMap::new(),
        session_meta: HashMap::new(),
        closed: HashSet::new(),
        parent_links: HashMap::new(),
        modes: HashMap::new(),
        events,
        seqs,
        activity,
        cfg,
        fork_tx,
        pending_loads: HashMap::new(),
        load_tx,
    };

    loop {
        let tick = async {
            match sweep.as_mut() {
                Some(timer) => {
                    timer.tick().await;
                }
                None => std::future::pending::<()>().await,
            }
        };
        let msg = tokio::select! {
            biased;
            _ = outlets::requested(&mut shutdown) => None,
            m = rx.recv() => m,
            m = fork_rx.recv() => m,
            Some(done) = load_rx.recv() => {
                sup.finish_load(done).await;
                continue;
            }
            _ = tick => {
                if let Some(ttl) = ttl {
                    sweep_idle_sessions(ttl, &mut sup.sessions, &mut sup.session_meta, &mut sup.parent_links, &sup.activity).await;
                }
                continue;
            }
        };
        let Some(msg) = msg else { break };
        // Fan the message out to inbound subscribers (runtime services) before
        // routing it. A closed/lagging subscriber is not fatal to routing.
        let _ = inbound.send(msg.clone());
        sup.handle(msg).await;
    }
    // Inbox closed or shutdown: stop every session and drop its channel;
    // `try_send`, as a saturated session must not stall shutdown (#700).
    for (_, tx) in sup.sessions.drain() {
        let _ = tx.try_send(SessionCmd::Stop);
    }
}

impl Supervisor {
    /// Handle one inbound (or fork-channel) message.
    async fn handle(&mut self, msg: InMsg) {
        // Runtime-consumed off the inbound fan-out above, never routed to a
        // session task: approval decisions (`Approve`/`Reject`, #59),
        // `AnswerQuestion`/`RetractQuestion`/`ReplaceQuestion` for `ask_user`
        // (ADR-0027, #515), and the `ReplayFrom` history query answered by the
        // runtime's log-owning responder (#160).
        if matches!(
            msg,
            InMsg::Approve { .. }
                | InMsg::Reject { .. }
                | InMsg::AnswerQuestion { .. }
                | InMsg::RetractQuestion { .. }
                | InMsg::ReplaceQuestion { .. }
                | InMsg::ReplayFrom { .. }
        ) {
            return;
        }

        // Supervisor-global lifecycle queries (ADR-0028): answered here, never
        // routed to a session task. `correlation_id` is an opaque token echoed
        // on the reply (#160) — not an overloaded session id.
        if let InMsg::ListSessions { correlation_id } = &msg {
            let mut list: Vec<SessionInfo> = self.session_meta.values().cloned().collect();
            list.sort_by(|a, b| a.session.0.cmp(&b.session.0));
            let _ = self.events.send(OutEvent::SessionList {
                correlation_id: correlation_id.clone(),
                sessions: list,
            });
            return;
        }

        // Every remaining variant is session-scoped except `ListSessions`
        // (handled above), `ListQuestions` (#515), `ListOperations` (#607),
        // and the MCP ops `McpList`/`McpAdd`/`McpRemove` (#375): each carries
        // no session either (`ListQuestions`/`ListOperations`'s optional
        // filter is a data field, not a routing target) — they are answered
        // by a runtime service off the `inbound` fan-out above, never routed
        // here, so they simply fall through to this `return`.
        let Some(session_id) = msg.session().cloned() else {
            return;
        };
        // A tool result for a session that compacted away belongs to the
        // successor that inherited its turn (ADR-0205). Without this the frame
        // would hit the closed-id refusal below and surface a spurious error
        // for work the user never cancelled. Only `ToolResult` is redirected:
        // every other frame naming a retired id is a genuine mistake worth
        // reporting, and only a still-unknown id can be a fork hand-off.
        let session_id = if matches!(msg, InMsg::ToolResult { .. })
            && !self.sessions.contains_key(&session_id)
        {
            successor_of(&self.successors, &session_id).unwrap_or(session_id)
        } else {
            session_id
        };
        if let InMsg::CloseSession { session } = &msg {
            self.close(session).await;
            return;
        }

        if let InMsg::HibernateSession { session } = &msg {
            // Memory eviction, not termination (#318, ADR-0077). Cascades over
            // the spawn sub-tree and tombstones nothing — see `hibernate_subtree`.
            // Shared with the idle-TTL sweep (#363), which reaches the same code
            // when it decides a settled root has been idle past `idle_ttl`.
            hibernate_subtree(
                session,
                &mut self.sessions,
                &mut self.session_meta,
                &mut self.parent_links,
            )
            .await;
            return;
        }

        if let InMsg::Resume { records, .. } = &msg {
            self.resume(&session_id, records).await;
            return;
        }

        if let InMsg::Spawn {
            session: child,
            parent,
            predecessor,
            agent,
            prompt,
            user,
        } = &msg
        {
            self.spawn(child, parent, predecessor, agent, prompt, user)
                .await;
            return;
        }

        // Every variant still here is routable to a session task. A non-live
        // target — or one whose queue is still draining behind a load —
        // goes through the unknown-session policy (ADR-0210).
        let msg = if !self.sessions.contains_key(&session_id)
            || self.pending_loads.contains_key(&session_id)
        {
            match self.admit_unknown(&session_id, msg) {
                Some(msg) => msg,
                None => return,
            }
        } else {
            msg
        };

        if let InMsg::SetMode { session, mode } = &msg {
            // Only cascades onto the target's *other* descendants; the target
            // itself takes the ordinary routing below. Runs after the policy
            // above so a `SetMode` to a not-yet-live id pre-seeds its mode the
            // same way any other session-scoped `InMsg` creates it.
            self.cascade_mode(session, mode).await;
        }

        // Non-routable variants are all handled/returned above; a stray one
        // maps to `None` and is dropped rather than crashing the supervisor
        // (#160 — the former `unreachable!` would have taken down every session).
        let Some(cmd) = msg_to_cmd(msg.clone()) else {
            tracing::warn!(
                variant = msg.variant_name(),
                "non-routable message reached session routing; dropped"
            );
            return;
        };

        if let Some(tx) = self.sessions.get(&session_id) {
            route_to_session(tx, cmd, &session_id, &self.events, &self.seqs).await;
        }
    }

    /// `CloseSession`: retire `session` and its whole spawn sub-tree.
    async fn close(&mut self, session: &SessionId) {
        // Cascade (#180): closing a session retires its whole sub-tree, not
        // just the target. `parent_links` is child→parent, so a spawned
        // descendant left running has no consumer for its answers and keeps
        // burning provider tokens. Walk the tree and close every descendant
        // alongside the target.
        // The closed sub-tree's root has a parent *outside* the closed set
        // (descendants' parents are themselves closing) — capture it before
        // the links are torn down so the still-live parent's `children`
        // mirror can drop the retired child.
        let root_parent = self.parent_links.get(session).cloned().flatten();
        for victim in collect_subtree(session, &self.parent_links) {
            // Dropping the command channel makes the task's `rx.recv()`
            // return `None`; it emits `SessionEnded` and exits. Unknown
            // id → no-op.
            if self.sessions.remove(&victim).is_some() {
                self.session_meta.remove(&victim);
            }
            self.parent_links.remove(&victim);
            self.modes.remove(&victim);
            // A load still in flight (ADR-0210) is abandoned: its queued
            // messages are refused like any frame racing a close, and the
            // completion finds no queue and applies nothing.
            if let Some(queued) = self.pending_loads.remove(&victim) {
                self.refuse_queued(&victim, queued.len(), unknown::CLOSED_ID);
            }
            // Tombstone the id regardless of liveness: it is spent
            // (ADR-0028), so a `Prompt` queued behind this `CloseSession`
            // can't respawn it blank.
            self.closed.insert(victim);
        }
        if let Some(parent) = root_parent {
            if let Some(ptx) = self.sessions.get(&parent) {
                let _ = ptx.send(SessionCmd::ChildClosed(session.clone())).await;
            }
        }
    }

    /// `SetMode`: cascade the new mode onto `session`'s live descendants.
    async fn cascade_mode(&mut self, session: &SessionId, mode: &str) {
        // ADR-0207 §6: mode applies to the whole spawn sub-tree, not just
        // the target — reusing `collect_subtree`, the exact tree-walk
        // `CloseSession`/`HibernateSession` already cascade over, so a
        // switch on a session with live children reaches them too. Each
        // live descendant gets its own `SessionCmd::SetMode`, so it
        // applies and announces through the same per-session path a
        // direct switch would (immediately, turn live or not — a mode is
        // a label the runtime grades the *next* call against, never
        // deferred like `SetAgent` used to be) — no session-loop
        // special-casing needed for the cascade to be correct. `modes` is
        // updated for every id in the subtree, live or not, so a hibernated
        // descendant's cache entry is fixed up defensively too (it plays no
        // functional role until that id resumes and re-derives the truth
        // from its own replay anyway).
        self.modes.insert(session.clone(), mode.to_string());
        for descendant in collect_subtree(session, &self.parent_links) {
            if &descendant == session {
                continue;
            }
            self.modes.insert(descendant.clone(), mode.to_string());
            if let Some(tx) = self.sessions.get(&descendant) {
                route_to_session(
                    tx,
                    SessionCmd::SetMode(mode.to_string()),
                    &descendant,
                    &self.events,
                    &self.seqs,
                )
                .await;
            }
        }
    }
}
