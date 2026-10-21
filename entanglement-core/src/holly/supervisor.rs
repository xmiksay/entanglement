//! The supervisor actor: routes inbound messages to per-session tasks and owns
//! the session directory (liveness, spawn tree, tombstones, mode cache). The
//! `Spawn`/lazy-root, `Resume`, unknown-session policy (ADR-0210) and
//! sub-tree/idle-sweep paths live in the child modules.

use std::collections::{HashMap, HashSet};

use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;

use super::outlets;
use super::routing::{msg_to_cmd, route_to_session};
use super::{ActivityRegistry, EngineConfig, SeqRegistry, SessionDirectory, FORK_CAPACITY};
use crate::protocol::{InMsg, OutEvent, SessionId};
use crate::session::{Ending, ExitSignal, ForkFrame, Inherited, SessionCmd};

mod ancestry;
mod relink;
mod resume;
mod spawn;
mod starting_mode;
mod subtree;
mod unknown;
mod workdir;

use subtree::{collect_subtree, sweep_period};

/// The supervisor loop's state, threaded through the per-message handlers.
struct Supervisor {
    sessions: HashMap<SessionId, mpsc::Sender<SessionCmd>>,
    // Each live task's join handle, beside its `sessions` sender. A hibernate
    // moves it to a watcher; until the task has exited the id's exit signal
    // sits in `retiring`, and a task started for the id meanwhile takes it
    // and waits on it first, so two tasks never run for one id (#892,
    // ADR-0244). The watcher reports the exit on `retired_tx`.
    tasks: HashMap<SessionId, JoinHandle<Ending>>,
    retiring: HashMap<SessionId, ExitSignal>,
    retired_tx: mpsc::Sender<(SessionId, ExitSignal)>,
    // Live-session directory, kept in lockstep with `sessions`, so `ListSessions`
    // can answer without folding the outbound broadcast (ADR-0028). A session
    // task exits when its channel is dropped (CloseSession, a hibernate, or
    // shutdown), and its `sessions` entry goes in the same step, so `sessions`
    // is the liveness source of truth and this never drifts.
    // Shared with `Holly::session_info` for synchronous reads (ADR-0211).
    // Also holds the source → successor chain (ADR-0205), so a tool result
    // still addressed to a session that compacted away resolves against the
    // session that took over its turn, and the runtime can follow a
    // compacted sub-agent losslessly (ADR-0234).
    session_meta: SessionDirectory,
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
    // the wire `SessionInfo`, whose fields stay spawn-time facts (only its
    // `parent` follows a re-link, ADR-0234). Written on every `SetMode` cascade and on `Spawn`/resume, read
    // to seed a fresh child's `initial_mode` from its parent and a resumed
    // descendant's ceiling from its nearest live ancestor (ADR-0246). Sparse:
    // an absent entry means `DEFAULT_MODE`, so a session never explicitly set
    // never needs an entry. Cleared on `CloseSession` (the id is retired for
    // good); left in place across a hibernate/resume cycle, holding the mode
    // the task last ran in (a cascade only ever narrows a non-live entry), which a
    // resume reads while that task still tears down; otherwise resume
    // re-derives it from the replayed log and the ancestors (`spawn_resumed`).
    modes: HashMap<SessionId, String>,
    // session → its latest validated workdir (#764), the base a relative
    // `SetWorkdir` resolves against and what a new child inherits. Same
    // lifecycle as `modes`; absent means the project root.
    workdirs: HashMap<SessionId, String>,
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
    fork_tx: mpsc::Sender<ForkFrame>,
    // successor → the source state its fork's `Spawn` carried (#768), parked
    // here between the fork frame's arrival and `spawn` consuming it, since
    // the `InMsg` it rides beside is handled through the ordinary path.
    inherited: HashMap<SessionId, Inherited>,
    // Ids with a `SessionLoader` call in flight (ADR-0210) → the routable
    // messages that arrived for them meanwhile, in arrival order, and the
    // ids hibernated meanwhile (ADR-0246). Presence is the "one load per id"
    // guard; the entry is drained on completion.
    pending_loads: HashMap<SessionId, unknown::PendingLoad>,
    // Load completions flow back here rather than through the inbox, for the
    // same keep-the-actor-alive reason as `fork_tx`.
    load_tx: mpsc::Sender<unknown::LoadDone>,
}

/// Route inbound messages to per-session tasks, lazily spawning one per new
/// [`SessionId`]. Exits (stopping all sessions) when the inbox closes or
/// [`Holly::shutdown`][super::Holly::shutdown] is requested.
#[allow(clippy::too_many_arguments)]
pub(super) async fn supervisor(
    mut rx: mpsc::Receiver<InMsg>,
    events: broadcast::Sender<OutEvent>,
    inbound: broadcast::Sender<InMsg>,
    seqs: SeqRegistry,
    activity: ActivityRegistry,
    session_meta: SessionDirectory,
    cfg: EngineConfig,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let (fork_tx, mut fork_rx) = mpsc::channel::<ForkFrame>(FORK_CAPACITY);
    let (load_tx, mut load_rx) = mpsc::channel::<unknown::LoadDone>(FORK_CAPACITY);
    let (retired_tx, mut retired_rx) = mpsc::channel::<(SessionId, ExitSignal)>(FORK_CAPACITY);
    // Idle-TTL auto-hibernation sweep (#363). `None` (the default) leaves the
    // tick branch pending forever, so with no `idle_ttl` configured the loop
    // only ever wakes for messages. A coarse interval, not a per-session
    // timer: sweeping is O(sessions) and cheap, and a quarter of the TTL (floored
    // at 30s) is plenty precise for an eviction policy, not a scheduler.
    let ttl = cfg.idle_ttl;
    let mut sweep = ttl.map(|ttl| tokio::time::interval(sweep_period(ttl)));
    let mut sup = Supervisor {
        sessions: HashMap::new(),
        tasks: HashMap::new(),
        retiring: HashMap::new(),
        retired_tx,
        session_meta,
        closed: HashSet::new(),
        parent_links: HashMap::new(),
        modes: HashMap::new(),
        workdirs: HashMap::new(),
        events,
        seqs,
        activity,
        cfg,
        fork_tx,
        inherited: HashMap::new(),
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
            // Ahead of the inbox: under `biased`, a busy inbox would otherwise
            // starve completions and leave queued messages waiting unboundedly.
            // Ordering stays safe — later frames for a loading id queue behind it.
            Some(done) = load_rx.recv() => {
                sup.finish_load(done).await;
                continue;
            }
            Some((session, signal)) = retired_rx.recv() => {
                subtree::release(&mut sup.retiring, session, signal);
                continue;
            }
            m = rx.recv() => m,
            m = fork_rx.recv() => m.map(|frame| sup.accept_fork(frame)),
            _ = tick => {
                if let Some(ttl) = ttl {
                    sup.sweep_idle_sessions(ttl).await;
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
    /// Unwrap a fork-channel frame into the `InMsg` the loop handles, parking
    /// any inherited source state for the successor's `spawn` to pick up.
    fn accept_fork(&mut self, frame: ForkFrame) -> InMsg {
        if let (Some(inherit), Some(successor)) = (frame.inherit, frame.msg.session()) {
            self.inherited.insert(successor.clone(), inherit);
        }
        frame.msg
    }

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
            let _ = self.events.send(OutEvent::SessionList {
                correlation_id: correlation_id.clone(),
                sessions: self.session_meta.snapshot(),
            });
            return;
        }

        // Every remaining variant is session-scoped except `ListSessions`
        // (handled above), `ListQuestions` (#515), `ListOperations` (#607),
        // `ListPending` (#726),
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
            self.session_meta
                .successor_of(&session_id)
                .unwrap_or(session_id)
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
            self.hibernate_subtree(session).await;
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
            model,
            mode,
            workdir,
        } = &msg
        {
            let workdir = workdir.as_deref();
            self.spawn(
                child,
                parent,
                predecessor,
                agent,
                prompt,
                user,
                model,
                mode,
                workdir,
            )
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

        let Some(msg) = self.validate_set_workdir(msg) else {
            return;
        };

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
}
