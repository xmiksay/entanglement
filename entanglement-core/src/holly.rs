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

use crate::llm::{EchoLlm, LlmFactory, LlmSession, ToolSpec};
use crate::protocol::{
    AgentMode, AgentProfile, InMsg, OutEvent, Permission, PermissionProfile, SessionId, SessionInfo,
};
use crate::session::{session_loop, Session, SessionCmd};

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

/// Engine configuration: how to build per-session LLMs, which host tools to
/// advertise to the model, and the named agent profiles sessions can switch
/// between.
///
/// Core advertises tool *schemas* ([`tool_specs`][Self::tool_specs]) but no
/// longer holds executable tools — the runtime owns execution and answers
/// [`OutEvent::ToolExec`] with [`InMsg::ToolResult`] (ADR-0006/0010).
#[derive(Clone)]
pub struct EngineConfig {
    pub llm_factory: LlmFactory,
    pub tool_specs: Vec<ToolSpec>,
    pub profiles: ProfileRegistry,
    /// Per-profile tool specs appended to [`tool_specs`][Self::tool_specs] for
    /// the active profile only (#119, ADR-0040). `run_turn` looks the running
    /// session's profile name up here and appends its entry (also filtered
    /// through [`AgentProfile::advertises_tool`]) after the #116 mask. Populated
    /// by the runtime with each profile's spawnable roster (the
    /// `agent_spawn`/`agent`/`agent_poll` triple, target-name enum + description
    /// scoped to that profile). A generic table — later per-profile features
    /// reuse it. Empty for a profile that may not spawn or has no valid targets.
    pub profile_tool_specs: HashMap<String, Vec<ToolSpec>>,
}

impl EngineConfig {
    /// Fail if the config can't back a running engine — currently, a profile
    /// registry without the required `build` profile. Lets an embedder reject a
    /// bad config up front instead of relying on the supervisor's fallback.
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.profiles.validate()
    }
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            llm_factory: Arc::new(|| LlmSession::new(Box::new(EchoLlm))),
            tool_specs: Vec::new(),
            profiles: ProfileRegistry::new(),
            profile_tool_specs: HashMap::new(),
        }
    }
}

/// A malformed [`EngineConfig`]/[`ProfileRegistry`] the engine can't run with.
/// Surfaced by [`EngineConfig::validate`]/[`ProfileRegistry::validate`] so an
/// embedder gets a clean error instead of a panicking supervisor task.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ConfigError {
    /// The registry lacks the `build` profile every new session starts under.
    #[error("profile registry is missing the required `{DEFAULT_PROFILE}` profile")]
    MissingDefaultProfile,
}

/// Named set of [`AgentProfile`]s. Comes with `build`, `plan`, `explore`
/// built-ins (mirroring opencode); add your own with [`insert`][Self::insert].
#[derive(Clone, Default)]
pub struct ProfileRegistry {
    profiles: HashMap<String, AgentProfile>,
}

impl ProfileRegistry {
    pub fn new() -> Self {
        let mut reg = Self::default();
        for profile in built_in_profiles() {
            reg.insert(profile);
        }
        reg
    }

    pub fn get(&self, name: &str) -> Option<&AgentProfile> {
        self.profiles.get(name)
    }

    /// Every registered profile, name-sorted for a stable roster (the runtime
    /// discloses this to a spawning model — see the `agent`/`agent_spawn` tool
    /// descriptions). Sorting keeps the advertised order deterministic across
    /// runs regardless of `HashMap` iteration order.
    pub fn iter(&self) -> impl Iterator<Item = &AgentProfile> {
        let mut profiles: Vec<&AgentProfile> = self.profiles.values().collect();
        profiles.sort_by(|a, b| a.name.cmp(&b.name));
        profiles.into_iter()
    }

    pub fn insert(&mut self, profile: AgentProfile) {
        self.profiles.insert(profile.name.clone(), profile);
    }

    /// Fail if the required `build` profile is absent. Embedders that assemble a
    /// custom registry should call this before handing it to [`Holly::spawn`];
    /// the supervisor otherwise falls back to a synthesized default (see
    /// [`resolve`][Self::resolve]) rather than panicking.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.profiles.contains_key(DEFAULT_PROFILE) {
            Ok(())
        } else {
            Err(ConfigError::MissingDefaultProfile)
        }
    }

    /// Resolve a profile by name, falling back to the default `build` profile
    /// and finally to a synthesized built-in `build`. Never panics: a registry
    /// missing `build` (an unvalidated custom one) yields a degraded-but-safe
    /// session instead of crashing the supervisor and taking down every session.
    fn resolve(&self, name: &str) -> AgentProfile {
        self.get(name)
            .or_else(|| self.get(DEFAULT_PROFILE))
            .cloned()
            .unwrap_or_else(|| {
                tracing::warn!(
                    "profile registry missing `{DEFAULT_PROFILE}` and `{name}`; \
                     falling back to a synthesized default profile"
                );
                default_profile()
            })
    }
}

/// The built-in `build` profile — the synthesized fallback the supervisor uses
/// when a custom registry omits it (see [`ProfileRegistry::resolve`]).
fn default_profile() -> AgentProfile {
    let [build, ..] = built_in_profiles();
    build
}

fn built_in_profiles() -> [AgentProfile; 3] {
    [
        AgentProfile {
            name: "build".into(),
            description: "Coding agent — implements changes using the available tools.".into(),
            mode: AgentMode::Primary,
            system_prompt: "You are a coding agent. Implement the requested changes using the available tools.".into(),
            model: None,
            permission: PermissionProfile::new(Permission::Allow),
            tools: None,
            disallowed_tools: Vec::new(),
            // Default-closed plan authority (#140): `build` consumes the plan, it
            // does not author it — the accept flow hands it a ready plan.
            owns_plan: false,
            // `build` spawns everything except primaries (the target-side mode
            // gate, #119) — no `spawnable_agents` list, so user-defined
            // exploration agents stay spawnable without editing this built-in.
            can_spawn: None,
            spawnable_agents: None,
        },
        AgentProfile {
            name: "plan".into(),
            description: "Planning agent — produces a plan without making changes.".into(),
            mode: AgentMode::Primary,
            system_prompt: "You are a planning agent. Analyze the request and produce a plan without making changes. Record the working plan with the update_plan tool, and delegate research to exploration agents.".into(),
            model: None,
            permission: PermissionProfile::new(Permission::Ask).with("read", Permission::Allow),
            // Physically read-only (#140, ADR-0041): the plan agent authors the
            // plan and delegates research — no `edit`/`write`/`bash`. Via
            // `tool_masked`'s ancestor intersection, every child spawned under
            // plan is clamped to this read-only set too.
            tools: Some(vec![
                "read".into(),
                "glob".into(),
                "grep".into(),
                "agent".into(),
                "agent_spawn".into(),
                "agent_poll".into(),
                "ask_user".into(),
                "load_skill".into(),
            ]),
            disallowed_tools: Vec::new(),
            // The plan agent is the plan owner (#140): it advertises `update_plan`
            // and its calls mutate the session plan.
            owns_plan: true,
            // `plan` may spawn (a primary), but omits `spawnable_agents` so any
            // user-defined exploration agent stays reachable (#119).
            can_spawn: None,
            spawnable_agents: None,
        },
        AgentProfile {
            name: "explore".into(),
            description: "Read-only exploration agent — answers questions about the codebase.".into(),
            mode: AgentMode::Subagent,
            system_prompt: "You are a read-only exploration agent. Answer questions about the codebase using only read tools.".into(),
            model: None,
            permission: PermissionProfile::new(Permission::Deny)
                .with("read", Permission::Allow)
                .with("glob", Permission::Allow)
                .with("grep", Permission::Allow),
            // Reference read-only agent (#116): the read trio is *all* it can
            // reach — no `edit`/`write`, no `bash`, no `agent_spawn`. A physical
            // boundary, matching the `permission` denies above.
            tools: Some(vec!["read".into(), "glob".into(), "grep".into()]),
            disallowed_tools: Vec::new(),
            // Default-closed plan authority (#140): explore never authors a plan.
            owns_plan: false,
            // Reference leaf: a `Subagent` mode defaults `can_spawn` closed (#119),
            // so the whole `agent_*` family is withheld — matching the tool mask.
            can_spawn: None,
            spawnable_agents: None,
        },
    ]
}

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
            // Dropping the command channel makes the task's `rx.recv()` return
            // `None`; it emits `SessionEnded` and exits. Unknown id → no-op.
            if sessions.remove(session).is_some() {
                session_meta.remove(session);
                parent_links.remove(session);
            }
            // Tombstone the id regardless of liveness: it is spent (ADR-0028), so
            // a `Prompt` queued behind this `CloseSession` can't respawn it blank.
            closed.insert(session.clone());
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
            session_meta.insert(session_id.clone(), resume_meta(&session_id, records));
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

/// Route a command to a session without letting one saturated session block the
/// supervisor's single loop — and thereby delay routing to *every* other
/// session (ADR-0028). Tries a non-blocking send first; on a full channel it
/// retries a bounded number of times, yielding between attempts so a
/// merely-behind session can drain, then sheds the command with an
/// [`OutEvent::Error`] rather than parking the supervisor. A closed channel
/// (session already gone) is dropped silently.
async fn route_to_session(
    tx: &mpsc::Sender<SessionCmd>,
    cmd: SessionCmd,
    session: &SessionId,
    events: &broadcast::Sender<OutEvent>,
) {
    use mpsc::error::TrySendError;
    let mut cmd = cmd;
    for _ in 0..ROUTE_ATTEMPTS {
        match tx.try_send(cmd) {
            Ok(()) => return,
            Err(TrySendError::Closed(_)) => return,
            Err(TrySendError::Full(returned)) => {
                cmd = returned;
                tokio::task::yield_now().await;
            }
        }
    }
    tracing::warn!(%session, "session command channel saturated; command shed");
    emit_supervisor_error(
        events,
        session,
        "session busy: command dropped (command channel saturated)",
    );
}

/// Emit a supervisor-level [`OutEvent::Error`] for a session the supervisor
/// rejects or sheds (a refused resurrection, a failed replay, a saturated
/// channel). `seq` is `0` because the supervisor can't mint the session's
/// monotonic seq — the session task owns it. On these exceptional paths the
/// tracing log is the primary signal; the event tells any listening head the
/// message did not land, rather than letting it vanish silently.
fn emit_supervisor_error(events: &broadcast::Sender<OutEvent>, session: &SessionId, message: &str) {
    let _ = events.send(OutEvent::Error {
        session: session.clone(),
        seq: 0,
        message: message.to_string(),
    });
}

/// Best-effort [`SessionInfo`] for a resumed session, read from the first
/// `SessionStarted` record in its replay log. Absent (an older log), it's
/// treated as a root under the base `build` profile.
fn resume_meta(session: &SessionId, records: &[(Option<InMsg>, OutEvent)]) -> SessionInfo {
    for (_, ev) in records {
        if let OutEvent::SessionStarted {
            parent,
            profile,
            root,
            ..
        } = ev
        {
            return SessionInfo {
                session: session.clone(),
                parent: parent.clone(),
                profile: profile.clone(),
                root: *root,
            };
        }
    }
    SessionInfo {
        session: session.clone(),
        parent: None,
        profile: DEFAULT_PROFILE.to_string(),
        root: true,
    }
}

fn msg_to_cmd(msg: InMsg) -> SessionCmd {
    match msg {
        InMsg::Prompt { text, .. } => SessionCmd::Prompt(text),
        InMsg::ToolResult {
            request_id, output, ..
        } => SessionCmd::ToolResult(request_id, output),
        InMsg::Stop { .. } => SessionCmd::Stop,
        InMsg::SetPlan { content, .. } => SessionCmd::SetPlan(content),
        InMsg::SetTasks { content, .. } => SessionCmd::SetTasks(content),
        InMsg::SetAgent { agent, .. } => SessionCmd::SetAgent(agent),
        // Approve/Reject/AnswerQuestion and the ListSessions/CloseSession
        // lifecycle queries are filtered out before routing (see supervisor);
        // Resume and Spawn are handled specially. None reach here.
        InMsg::Approve { .. }
        | InMsg::Reject { .. }
        | InMsg::AnswerQuestion { .. }
        | InMsg::ListSessions { .. }
        | InMsg::CloseSession { .. }
        | InMsg::Resume { .. }
        | InMsg::Spawn { .. } => {
            unreachable!("Approve/Reject/AnswerQuestion/ListSessions/CloseSession/Resume/Spawn are not routed to sessions")
        }
    }
}
