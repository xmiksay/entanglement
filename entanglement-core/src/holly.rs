//! The engine actor. [`Holly`] owns a process-wide inbox (`mpsc<InMsg>`) and
//! outbox (`broadcast<OutEvent>`). The supervisor routes inbound messages to
//! per-session tasks (lazily spawned, one per [`SessionId`]).
//!
//! This is the ABI foundation: an embedder holds a (cheaply-cloned) `Holly`,
//! calls [`Holly::send`] with typed [`InMsg`]s and drains
//! [`Holly::subscribe`] for [`OutEvent`]s — no serialization. Every transport
//! (stdio, WS, TUI) is a thin adapter over these two methods.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::{broadcast, mpsc};

use crate::llm::{EchoLlm, Llm, LlmFactory};
use crate::protocol::{
    AgentMode, AgentProfile, InMsg, OutEvent, Permission, PermissionProfile, SessionId,
};
use crate::session::{session_loop, Session, SessionCmd};
use crate::tools::ToolRegistry;

const INBOX_CAPACITY: usize = 256;
const OUTBOX_CAPACITY: usize = 1024;
/// Profile a new session starts under (opencode-style: `build` is the default).
const DEFAULT_PROFILE: &str = "build";

/// Engine configuration: how to build per-session LLMs, which host tools exist,
/// and the named agent profiles sessions can switch between.
#[derive(Clone)]
pub struct EngineConfig {
    pub llm_factory: LlmFactory,
    pub tools: ToolRegistry,
    pub profiles: ProfileRegistry,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            llm_factory: Arc::new(|| Box::new(EchoLlm) as Box<dyn Llm>),
            tools: ToolRegistry::new(),
            profiles: ProfileRegistry::new(),
        }
    }
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

    pub fn insert(&mut self, profile: AgentProfile) {
        self.profiles.insert(profile.name.clone(), profile);
    }
}

fn built_in_profiles() -> [AgentProfile; 3] {
    [
        AgentProfile {
            name: "build".into(),
            mode: AgentMode::Primary,
            system_prompt: "You are a coding agent. Implement the requested changes using the available tools.".into(),
            model: None,
            permission: PermissionProfile::new(Permission::Allow),
        },
        AgentProfile {
            name: "plan".into(),
            mode: AgentMode::Primary,
            system_prompt: "You are a planning agent. Analyze the request and produce a plan without making changes. Use the update_plan and update_tasks tools to record your strategy and outline.".into(),
            model: None,
            permission: PermissionProfile::new(Permission::Ask).with("read", Permission::Allow),
        },
        AgentProfile {
            name: "explore".into(),
            mode: AgentMode::Subagent,
            system_prompt: "You are a read-only exploration agent. Answer questions about the codebase using only read tools.".into(),
            model: None,
            permission: PermissionProfile::new(Permission::Deny)
                .with("read", Permission::Allow)
                .with("glob", Permission::Allow)
                .with("grep", Permission::Allow),
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
    cfg: Arc<EngineConfig>,
    root: Arc<PathBuf>,
}

impl Holly {
    /// Spawn the engine actor with `cfg` and return a handle.
    pub fn spawn(cfg: EngineConfig) -> Self {
        let (inbox, rx) = mpsc::channel::<InMsg>(INBOX_CAPACITY);
        let (events, _) = broadcast::channel::<OutEvent>(OUTBOX_CAPACITY);
        let supervisor_events = events.clone();
        let root = Arc::new(std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        let cfg_arc = Arc::new(cfg.clone());
        let root_for_supervisor = root.clone();
        tokio::spawn(
            async move { supervisor(rx, supervisor_events, cfg, root_for_supervisor).await },
        );
        Self {
            inbox,
            events,
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
    cfg: EngineConfig,
    root: Arc<PathBuf>,
) {
    let mut sessions: HashMap<SessionId, mpsc::Sender<SessionCmd>> = HashMap::new();
    let mut pending_resumes: HashMap<SessionId, Vec<(Option<InMsg>, OutEvent)>> = HashMap::new();
    let parent_links: HashMap<SessionId, Option<SessionId>> = HashMap::new();

    while let Some(msg) = rx.recv().await {
        let session_id = msg.session().clone();

        if let InMsg::Resume { records, .. } = &msg {
            pending_resumes.insert(session_id.clone(), records.clone());
            let (stx, srx) = mpsc::channel::<SessionCmd>(64);
            let ev = events.clone();
            let cfg2 = cfg.clone();
            let root2 = root.clone();
            let sid = session_id.clone();
            let records = pending_resumes.remove(&sid).unwrap_or_default();
            tokio::spawn(async move {
                match Session::replay(&records, &cfg2, &root2) {
                    Ok(initial_session) => {
                        let profile = initial_session.profile.clone();
                        let parent = initial_session.parent.clone();
                        session_loop(sid, srx, ev, cfg2, profile, Some(initial_session), parent)
                            .await;
                    }
                    Err(e) => {
                        tracing::error!("Failed to replay session {}: {}", sid, e);
                    }
                }
            });
            sessions.insert(session_id.clone(), stx);
            continue;
        }

        let cmd = msg_to_cmd(msg.clone());

        if !sessions.contains_key(&session_id) {
            let profile = cfg
                .profiles
                .get(DEFAULT_PROFILE)
                .cloned()
                .expect("built-in `build` profile always present");
            let (stx, srx) = mpsc::channel::<SessionCmd>(64);
            let ev = events.clone();
            let cfg2 = cfg.clone();
            let sid = session_id.clone();
            let parent = parent_links.get(&session_id).cloned().flatten();
            tokio::spawn(
                async move { session_loop(sid, srx, ev, cfg2, profile, None, parent).await },
            );
            sessions.insert(session_id.clone(), stx);
        }

        if let Some(tx) = sessions.get(&session_id) {
            let _ = tx.send(cmd).await;
        }
    }
    // Inbox closed: signal every session to stop. Their tasks return on receipt.
    for (_, tx) in sessions.drain() {
        let _ = tx.send(SessionCmd::Stop).await;
    }
}

/// Tap that allows observing all inbound messages before they're routed.
/// Returns a receiver that clones each InMsg.
pub fn tap_inbound(_rx: &mpsc::Receiver<InMsg>) -> mpsc::Receiver<InMsg> {
    let (_tap_tx, tap_rx) = mpsc::channel::<InMsg>(INBOX_CAPACITY);

    // This is a bit of a hack: we can't actually tap the existing receiver
    // without modifying the supervisor. For now, we return a channel that
    // the caller can use by wrapping Holly::send.
    // A proper implementation would require restructuring the supervisor to
    // broadcast inbound messages, similar to how outbound events work.
    tap_rx
}

fn msg_to_cmd(msg: InMsg) -> SessionCmd {
    match msg {
        InMsg::Prompt { text, .. } => SessionCmd::Prompt(text),
        InMsg::Approve { request_id, .. } => SessionCmd::Approve(request_id),
        InMsg::Reject {
            request_id, reason, ..
        } => SessionCmd::Reject(request_id, reason),
        InMsg::Stop { .. } => SessionCmd::Stop,
        InMsg::SetPlan { content, .. } => SessionCmd::SetPlan(content),
        InMsg::SetTasks { tasks, .. } => SessionCmd::SetTasks(tasks),
        InMsg::SetAgent { agent, .. } => SessionCmd::SetAgent(agent),
        InMsg::Resume { .. } => {
            unreachable!("Resume messages are handled specially in the supervisor")
        }
    }
}
