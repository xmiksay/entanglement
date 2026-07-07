//! The engine actor. [`Holly`] owns a process-wide inbox (`mpsc<InMsg>`) and
//! outbox (`broadcast<OutEvent>`). The supervisor routes inbound messages to
//! per-session tasks (lazily spawned, one per [`SessionId`]).
//!
//! This is the ABI foundation: an embedder holds a (cheaply-cloned) `Holly`,
//! calls [`Holly::send`] with typed [`InMsg`]s and drains
//! [`Holly::subscribe`] for [`OutEvent`]s — no serialization. Every transport
//! (stdio, WS, TUI) is a thin adapter over these two methods.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{broadcast, mpsc};

use crate::llm::{DummyLlm, Llm, LlmFactory};
use crate::protocol::{
    AgentMode, AgentProfile, InMsg, OutEvent, Permission, PermissionProfile, SessionId,
};
use crate::session::{session_loop, SessionCmd};
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
            llm_factory: Arc::new(|| Box::new(DummyLlm::default()) as Box<dyn Llm>),
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
pub struct Holly {
    inbox: mpsc::Sender<InMsg>,
    events: broadcast::Sender<OutEvent>,
}

impl Holly {
    /// Spawn the engine actor with `cfg` and return a handle.
    pub fn spawn(cfg: EngineConfig) -> Self {
        let (inbox, rx) = mpsc::channel::<InMsg>(INBOX_CAPACITY);
        let (events, _) = broadcast::channel::<OutEvent>(OUTBOX_CAPACITY);
        let supervisor_events = events.clone();
        tokio::spawn(async move { supervisor(rx, supervisor_events, cfg).await });
        Self { inbox, events }
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
}

/// Route inbound messages to per-session tasks, lazily spawning one per new
/// [`SessionId`]. Exits (stopping all sessions) when the inbox closes.
async fn supervisor(
    mut rx: mpsc::Receiver<InMsg>,
    events: broadcast::Sender<OutEvent>,
    cfg: EngineConfig,
) {
    let mut sessions: HashMap<SessionId, mpsc::Sender<SessionCmd>> = HashMap::new();

    while let Some(msg) = rx.recv().await {
        let session_id = msg.session().clone();
        let cmd = msg_to_cmd(msg);

        // Stop is cancel-semantics (ADR-0017): it interrupts the in-flight
        // turn inside the session task (or no-ops when idle) but does *not*
        // destroy the task. Routing it as a regular command preserves the
        // session's `Context` across a Stop+Prompt round-trip.
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
            tokio::spawn(async move { session_loop(sid, srx, ev, cfg2, profile).await });
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
    }
}
