//! The engine actor. [`Holly`] owns a process-wide inbox (`mpsc<InMsg>`) and
//! outbox (`broadcast<OutEvent>`). The supervisor routes inbound messages to
//! per-session tasks (lazily spawned, one per [`SessionId`]).
//!
//! This is the ABI foundation: an embedder holds a (cheaply-cloned) `Holly`,
//! calls [`Holly::send`] with typed [`InMsg`]s and drains
//! [`Holly::subscribe`] for [`OutEvent`]s — no serialization. Every transport
//! (stdio, WS, TUI) is a thin adapter over these two methods.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::{broadcast, mpsc};

use crate::protocol::{InMsg, OutEvent, SessionId, SessionInfo};
use crate::session::{session_loop, Session, SessionCmd};

mod config;
mod routing;

pub use config::{ConfigError, EngineConfig, ProfileRegistry};
use routing::{emit_supervisor_error, msg_to_cmd, resume_meta, route_to_session};

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
#[allow(dead_code)]
pub struct Holly {
    inbox: mpsc::Sender<InMsg>,
    events: broadcast::Sender<OutEvent>,
    /// Fan-out of every inbound [`InMsg`] (cloned before routing). Lets a
    /// runtime-side service observe protocol messages it doesn't route itself —
    /// e.g. the tool executor watching `Approve`/`Reject`/`Stop` while it owns
    /// permission dispatch + approval (ADR-0010, #59).
    inbound: broadcast::Sender<InMsg>,
    cfg: Arc<EngineConfig>,
    root: Arc<PathBuf>,
}

impl Holly {
    /// Spawn the engine actor with `cfg` and return a handle.
    pub fn spawn(cfg: EngineConfig) -> Self {
        let (inbox, rx) = mpsc::channel::<InMsg>(INBOX_CAPACITY);
        let (events, _) = broadcast::channel::<OutEvent>(OUTBOX_CAPACITY);
        let (inbound, _) = broadcast::channel::<InMsg>(INBOX_CAPACITY);
        let supervisor_events = events.clone();
        let supervisor_inbound = inbound.clone();
        let root = Arc::new(std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        let cfg_arc = Arc::new(cfg.clone());
        let root_for_supervisor = root.clone();
        tokio::spawn(async move {
            supervisor(
                rx,
                supervisor_events,
                supervisor_inbound,
                cfg,
                root_for_supervisor,
            )
            .await
        });
        Self {
            inbox,
            events,
            inbound,
            cfg: cfg_arc,
            root,
        }
    }

    /// Push an [`InMsg`] into the engine (the ABI entry point).
    pub async fn send(&self, msg: InMsg) -> Result<(), mpsc::error::SendError<InMsg>> {
        self.inbox.send(msg).await
    }

    /// Subscribe to the outbound event stream (every session, fan-out).
    pub fn subscribe(&self) -> broadcast::Receiver<OutEvent> {
        self.events.subscribe()
    }

    /// Borrow the outbound sender (for heads that want to subscribe once).
    pub fn events(&self) -> &broadcast::Sender<OutEvent> {
        &self.events
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
}

/// Route inbound messages to per-session tasks, lazily spawning one per new
/// [`SessionId`]. Exits (stopping all sessions) when the inbox closes.
async fn supervisor(
    mut rx: mpsc::Receiver<InMsg>,
    events: broadcast::Sender<OutEvent>,
    inbound: broadcast::Sender<InMsg>,
    cfg: EngineConfig,
    root: Arc<PathBuf>,
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

    while let Some(msg) = rx.recv().await {
        let session_id = msg.session().clone();

        // Fan the message out to inbound subscribers (runtime services) before
        // routing it. A closed/lagging subscriber is not fatal to routing.
        let _ = inbound.send(msg.clone());

        // Approval decisions are a runtime concern now (#59): the tool executor
        // consumes `Approve`/`Reject` off the inbound fan-out above. The engine
        // no longer parks on them, so there is nothing to route to a session.
        // `AnswerQuestion` is the same shape for the `ask_user` tool (ADR-0027).
        if matches!(
            msg,
            InMsg::Approve { .. } | InMsg::Reject { .. } | InMsg::AnswerQuestion { .. }
        ) {
            continue;
        }

        // Supervisor-global lifecycle queries (ADR-0028): answered here, never
        // routed to a session task.
        if let InMsg::ListSessions { session } = &msg {
            let mut list: Vec<SessionInfo> = session_meta.values().cloned().collect();
            list.sort_by(|a, b| a.session.0.cmp(&b.session.0));
            let _ = events.send(OutEvent::SessionList {
                session: session.clone(),
                sessions: list,
            });
            continue;
        }
        if let InMsg::CloseSession { session } = &msg {
            // Cascade (#180): closing a session retires its whole sub-tree, not
            // just the target. `parent_links` is child→parent, so a spawned
            // descendant left running has no consumer for its answers and keeps
            // burning provider tokens. Walk the tree and close every descendant
            // alongside the target.
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
            continue;
        }

        if let InMsg::Resume { records, .. } = &msg {
            // A retired id is single-use; refuse rather than resurrect (ADR-0028).
            if closed.contains(&session_id) {
                emit_supervisor_error(
                    &events,
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
            let initial_session = match Session::replay(records, &cfg, root.as_path()) {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!("Failed to replay session {}: {}", session_id, e);
                    emit_supervisor_error(
                        &events,
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
            tokio::spawn(async move {
                session_loop(sid, srx, ev, cfg2, profile, Some(initial_session), parent).await;
            });
            sessions.insert(session_id.clone(), stx);
            continue;
        }

        if let InMsg::Spawn {
            session: child,
            parent,
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
                        child,
                        &format!("cannot spawn unknown agent profile `{agent}`"),
                    );
                    continue;
                }
            };
            // Record the parent link *before* spawning so it's in place for any
            // later lazy path, and so the child starts under the requested profile.
            parent_links.insert(child.clone(), Some(parent.clone()));
            session_meta.insert(
                child.clone(),
                SessionInfo {
                    session: child.clone(),
                    parent: Some(parent.clone()),
                    profile: profile.name.clone(),
                    root: false,
                    profile_detail: Some(profile.detail()),
                },
            );
            let (stx, srx) = mpsc::channel::<SessionCmd>(SESSION_CMD_CAPACITY);
            let ev = events.clone();
            let cfg2 = cfg.clone();
            let sid = child.clone();
            let parent = Some(parent.clone());
            tokio::spawn(
                async move { session_loop(sid, srx, ev, cfg2, profile, None, parent).await },
            );
            // Queue the initial prompt; the child drains it after its lifecycle events.
            let _ = stx.send(SessionCmd::Prompt(prompt.clone())).await;
            sessions.insert(child.clone(), stx);
            continue;
        }

        let cmd = msg_to_cmd(msg.clone());

        if !sessions.contains_key(&session_id) {
            // A closed id is spent (ADR-0028): a `Prompt` that raced behind its
            // `CloseSession` must not lazily respawn a blank session under it
            // (issue #105). Refuse with feedback instead of silently resurrecting.
            if closed.contains(&session_id) {
                emit_supervisor_error(
                    &events,
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
            tokio::spawn(
                async move { session_loop(sid, srx, ev, cfg2, profile, None, parent).await },
            );
            sessions.insert(session_id.clone(), stx);
        }

        if let Some(tx) = sessions.get(&session_id) {
            route_to_session(tx, cmd, &session_id, &events).await;
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
