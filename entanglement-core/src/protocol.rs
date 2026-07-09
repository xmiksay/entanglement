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

use serde::{Deserialize, Serialize};

/// Stable identifier for a conversation session. Serialized transparently as a
/// plain string on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(pub String);

impl SessionId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn new_uuid() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
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
    /// Session is live but idle, waiting for the next prompt.
    Idle,
    /// Engine is actively reasoning / calling the model.
    Thinking,
    /// Engine emitted a tool request and is parked waiting for approval.
    WaitingApproval,
    /// Last turn finished cleanly.
    Done,
    /// Last turn ended with an error.
    Error,
}

/// Lifecycle state of a single task in the session's outline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    InProgress,
    Completed,
    Cancelled,
}

/// Kind of file change. `ApplyDiff` and `Plugin` are reserved for future work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileChangeKind {
    Edit,
    ApplyDiff,
    Create,
}

/// One item in the session's task outline. The engine owns the list and emits
/// a full [`OutEvent::TaskList`] snapshot whenever it changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskItem {
    pub id: String,
    pub content: String,
    pub status: TaskStatus,
}

/// A live session's identity + lineage, as reported in an
/// [`OutEvent::SessionList`] enumeration snapshot (ADR-0028). Mirrors the fields
/// a head would otherwise have to reconstruct by folding the `SessionStarted` /
/// `SessionEnded` broadcast itself. `profile` is the session's *starting*
/// profile (the supervisor tracks creation, not per-turn `SetAgent` switches —
/// a head follows those via [`OutEvent::AgentChanged`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub session: SessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<SessionId>,
    pub profile: String,
    pub root: bool,
}

/// One labelled choice in a model-driven [`OutEvent::UserQuestion`] prompt
/// (ADR-0027). The `label` is what flows back as the answer when picked; the
/// optional `description` is a short hint shown beneath it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionOption {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

// ┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// ┃ Agent profiles (opencode-style: system prompt + permission profile)
// ┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// What the engine does when the model asks to run a host tool. Driven by the
/// session's active [`AgentProfile`] permission profile — e.g. a `plan` profile
/// denies edits, a `build` profile allows everything.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
    /// Run immediately, no approval request.
    Allow,
    /// Emit [`OutEvent::ToolRequest`] and wait for the user.
    Ask,
    /// Refuse outright; the model is told the tool was denied by policy.
    Deny,
}

/// Per-tool permission rules. Evaluated against a tool name; later matching
/// rules win (so put `"*"` first, specifics after — same semantics as opencode).
/// Built-in engine tools (`update_plan`, `update_tasks`) bypass this and always
/// run, since they only mutate session state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionProfile {
    /// `(pattern, permission)` pairs. `pattern` is either a tool name or `"*"`.
    pub rules: Vec<(String, Permission)>,
    /// Used when no rule matches.
    pub default: Permission,
}

impl PermissionProfile {
    pub fn new(default: Permission) -> Self {
        Self {
            rules: Vec::new(),
            default,
        }
    }

    /// Add a rule (evaluated after previously-added rules on conflict).
    pub fn with(mut self, pattern: impl Into<String>, perm: Permission) -> Self {
        self.rules.push((pattern.into(), perm));
        self
    }

    /// Resolve the permission for a tool. Last matching rule wins.
    pub fn for_tool(&self, name: &str) -> Permission {
        let mut result = self.default;
        for (pat, p) in &self.rules {
            if pat == name || pat == "*" {
                result = *p;
            }
        }
        result
    }
}

/// Whether an agent is directly user-facing or invoked by other agents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentMode {
    Primary,
    Subagent,
}

/// A bundle of system prompt + model + permissions that defines how a session
/// reasons and what it may do. A session runs under exactly one profile at a
/// time; switching (e.g. Build ↔ Plan) changes the profile. Mirrors opencode's
/// agent concept. The `name` is the switch key in [`InMsg::SetAgent`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentProfile {
    pub name: String,
    pub mode: AgentMode,
    pub system_prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub permission: PermissionProfile,
}

// ┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// ┃ Messages
// ┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// Inbound: harness → engine. One connection multiplexes sessions via
/// [`SessionId`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InMsg {
    /// Feed a new user prompt into the conversation.
    Prompt { session: SessionId, text: String },
    /// Approve a pending tool request (`request_id` from [`OutEvent::ToolRequest`]).
    Approve {
        session: SessionId,
        request_id: String,
    },
    /// Reject a pending tool request.
    Reject {
        session: SessionId,
        request_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// Result of a runtime-executed tool (`request_id` from
    /// [`OutEvent::ToolExec`]). The runtime owns tool execution (ADR-0006/0010):
    /// core emits a `ToolExec` request and parks the turn until this arrives.
    /// Distinct from [`Approve`][InMsg::Approve]/[`Reject`][InMsg::Reject], which
    /// stay for approval semantics.
    ToolResult {
        session: SessionId,
        request_id: String,
        output: String,
    },
    /// Answer a pending model-driven question (`request_id` from
    /// [`OutEvent::UserQuestion`]). Like [`Approve`][InMsg::Approve]/
    /// [`Reject`][InMsg::Reject], it is consumed by the runtime off the inbound
    /// fan-out (the `ask_user` executor parks on it), not routed to a session —
    /// core stays unaware of the interaction (ADR-0027). `answer` is the picked
    /// option's label or the free-form text; the runtime folds it back as the
    /// `ask_user` tool's [`ToolResult`][InMsg::ToolResult].
    AnswerQuestion {
        session: SessionId,
        request_id: String,
        answer: String,
    },
    /// Cancel the current turn and park the session at idle.
    Stop { session: SessionId },
    /// Enumerate the engine's currently-live sessions. The supervisor answers
    /// with a single [`OutEvent::SessionList`] snapshot (ADR-0028); this message
    /// is supervisor-global, not routed to a session task. `session` is a
    /// correlation id echoed back on the reply so a multiplexed head can match
    /// the snapshot to its request (pass any id the head owns).
    ListSessions { session: SessionId },
    /// Explicitly terminate a live session: the supervisor drops its command
    /// channel and the session task exits, emitting [`OutEvent::SessionEnded`]
    /// (ADR-0028). Distinct from [`Stop`][InMsg::Stop], which only cancels the
    /// in-flight turn and leaves the session alive (ADR-0017) — `CloseSession`
    /// is the lifecycle destroy Stop no longer performs. Unknown / already-closed
    /// ids are a no-op. Session ids are single-use: mint a fresh one
    /// ([`SessionId::new_uuid`]) rather than reusing a closed id.
    CloseSession { session: SessionId },
    /// Rewrite the session's task outline from the harness (user-edited plan).
    SetTasks {
        session: SessionId,
        tasks: Vec<TaskItem>,
    },
    /// Rewrite the session's strategy plan from the harness (markdown prose).
    SetPlan { session: SessionId, content: String },
    /// Switch the session to a different agent profile by name (e.g. `plan`).
    SetAgent { session: SessionId, agent: String },
    /// Spawn a child session (sub-agent) under `parent`, running `prompt` beneath
    /// the named `agent` profile (#60, ADR-0021). `session` is the *child's* id.
    /// The supervisor records the parent link (populating the session tree the
    /// tree-walk helpers read) and starts the child. The runtime's `spawn_agent`
    /// tool issues this, then relays the child's final answer back to the parent
    /// as a tool result — core needs no notion of "child session" in its loop.
    Spawn {
        session: SessionId,
        parent: SessionId,
        agent: String,
        prompt: String,
    },
    /// Resume a session from replayed log records (internal, not serialized).
    #[serde(skip)]
    Resume {
        session: SessionId,
        records: Vec<(Option<InMsg>, OutEvent)>,
    },
}

impl InMsg {
    /// The session this message targets.
    pub fn session(&self) -> &SessionId {
        match self {
            InMsg::Prompt { session, .. }
            | InMsg::Approve { session, .. }
            | InMsg::Reject { session, .. }
            | InMsg::ToolResult { session, .. }
            | InMsg::AnswerQuestion { session, .. }
            | InMsg::Stop { session }
            | InMsg::ListSessions { session }
            | InMsg::CloseSession { session }
            | InMsg::SetTasks { session, .. }
            | InMsg::SetPlan { session, .. }
            | InMsg::SetAgent { session, .. }
            | InMsg::Spawn { session, .. }
            | InMsg::Resume { session, .. } => session,
        }
    }
}

/// Outbound: engine → harness. Cloned through a `broadcast` channel, so every
/// variant must be `Clone`.
///
/// Content variants ([`Plan`][OutEvent::Plan],
/// [`TextDelta`][OutEvent::TextDelta],
/// [`ToolRequest`][OutEvent::ToolRequest], [`ToolOutput`][OutEvent::ToolOutput],
/// [`TaskList`][OutEvent::TaskList], [`Error`][OutEvent::Error],
/// [`Done`][OutEvent::Done]) carry a monotonic per-session `seq`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OutEvent {
    /// Session started (lifecycle event, no `seq`). Emits when a session is spawned.
    SessionStarted {
        session: SessionId,
        parent: Option<SessionId>,
        profile: String,
        model: Option<String>,
        root: bool,
        ts: u64,
    },
    /// Session ended (lifecycle event, no `seq`). Emits when a session exits.
    SessionEnded { session: SessionId, ts: u64 },
    /// Snapshot of every currently-live session (lifecycle event, no `seq`),
    /// sent in reply to [`InMsg::ListSessions`] (ADR-0028). `session` echoes the
    /// requester's correlation id from that message so a multiplexed head can
    /// pair the reply with its request.
    SessionList {
        session: SessionId,
        sessions: Vec<SessionInfo>,
    },
    /// Lifecycle state change (point-in-time, no `seq`).
    Status {
        session: SessionId,
        state: AgentState,
    },
    /// The session switched agent profiles (point-in-time, no `seq`).
    AgentChanged { session: SessionId, agent: String },
    /// The agent's strategy plan (markdown prose), full snapshot on every change.
    Plan {
        session: SessionId,
        seq: u64,
        content: String,
    },
    /// Incremental assistant text.
    TextDelta {
        session: SessionId,
        seq: u64,
        text: String,
    },
    /// Incremental reasoning/thinking text.
    ReasoningDelta {
        session: SessionId,
        seq: u64,
        text: String,
    },
    /// A tool is being called or about to be approved (display-only). Emitted
    /// for every tool call, before execution, so heads can show what's being called
    /// (tool name + input arguments). Separate from `ToolRequest` which handles
    /// approval mode.
    ToolCall {
        session: SessionId,
        seq: u64,
        request_id: String,
        tool: String,
        input: String,
    },
    /// Engine wants to run a host tool (permission `Ask`) and is pausing for approval.
    ToolRequest {
        session: SessionId,
        seq: u64,
        request_id: String,
        tool: String,
        input: String,
    },
    /// Core asks the runtime to execute a host tool that is cleared to run
    /// (permission `Allow`, or `Ask` after approval). The runtime executes it
    /// and replies with [`InMsg::ToolResult`]. Distinct from
    /// [`ToolRequest`][OutEvent::ToolRequest] (human approval) and
    /// [`ToolCall`][OutEvent::ToolCall] (display-only): only `ToolExec` drives
    /// execution, so a denied tool never runs (ADR-0006/0010).
    ToolExec {
        session: SessionId,
        seq: u64,
        request_id: String,
        tool: String,
        input: String,
    },
    /// The model asked the user a decision question via the runtime-owned
    /// `ask_user` tool (ADR-0027). Carries the prompt, labelled `options`, and
    /// whether a free-form ("Other") answer is allowed. A head renders it as a
    /// multiple-choice prompt and replies with [`InMsg::AnswerQuestion`]; the
    /// runtime folds that answer back as the tool's output. Dedicated (not
    /// [`ToolRequest`][OutEvent::ToolRequest]) so choices render cleanly.
    UserQuestion {
        session: SessionId,
        seq: u64,
        request_id: String,
        question: String,
        options: Vec<QuestionOption>,
        allow_free_form: bool,
    },
    /// Result of an executed tool, a denied tool, or a built-in tool.
    ToolOutput {
        session: SessionId,
        seq: u64,
        request_id: String,
        tool: String,
        output: String,
    },
    /// Full snapshot of the session's task outline (sent on every change).
    TaskList {
        session: SessionId,
        seq: u64,
        tasks: Vec<TaskItem>,
    },
    /// Recoverable error surfaced to the UI; the engine stays alive.
    Error {
        session: SessionId,
        seq: u64,
        message: String,
    },
    /// Turn finished cleanly. Heads waiting on a one-shot turn exit on this.
    Done { session: SessionId, seq: u64 },
    /// File change record (audit log entry). Emitted after each successful edit
    /// or create. The record carries before/after bytes and change kind for
    /// diff rendering and audit tracking.
    FileChange {
        session: SessionId,
        seq: u64,
        path: String,
        before: Option<Vec<u8>>,
        after: Option<Vec<u8>>,
        change_kind: FileChangeKind,
    },
}

impl OutEvent {
    pub fn session(&self) -> &SessionId {
        match self {
            OutEvent::SessionStarted { session, .. }
            | OutEvent::SessionEnded { session, .. }
            | OutEvent::SessionList { session, .. }
            | OutEvent::Status { session, .. }
            | OutEvent::AgentChanged { session, .. }
            | OutEvent::Plan { session, .. }
            | OutEvent::TextDelta { session, .. }
            | OutEvent::ReasoningDelta { session, .. }
            | OutEvent::ToolCall { session, .. }
            | OutEvent::ToolRequest { session, .. }
            | OutEvent::ToolExec { session, .. }
            | OutEvent::UserQuestion { session, .. }
            | OutEvent::ToolOutput { session, .. }
            | OutEvent::TaskList { session, .. }
            | OutEvent::Error { session, .. }
            | OutEvent::Done { session, .. }
            | OutEvent::FileChange { session, .. } => session,
        }
    }

    /// Returns the sequence number for this event, or 0 for lifecycle events
    /// that don't carry a seq (SessionStarted, SessionEnded, SessionList,
    /// Status, AgentChanged).
    pub fn seq(&self) -> u64 {
        match self {
            OutEvent::SessionStarted { .. }
            | OutEvent::SessionEnded { .. }
            | OutEvent::SessionList { .. }
            | OutEvent::Status { .. }
            | OutEvent::AgentChanged { .. } => 0,
            OutEvent::Plan { seq, .. }
            | OutEvent::TextDelta { seq, .. }
            | OutEvent::ReasoningDelta { seq, .. }
            | OutEvent::ToolCall { seq, .. }
            | OutEvent::ToolRequest { seq, .. }
            | OutEvent::ToolExec { seq, .. }
            | OutEvent::UserQuestion { seq, .. }
            | OutEvent::ToolOutput { seq, .. }
            | OutEvent::TaskList { seq, .. }
            | OutEvent::Error { seq, .. }
            | OutEvent::Done { seq, .. }
            | OutEvent::FileChange { seq, .. } => *seq,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_id_new_uuid_generates_unique_ids() {
        let id1 = SessionId::new_uuid();
        let id2 = SessionId::new_uuid();
        let id3 = SessionId::new_uuid();

        assert_ne!(id1, id2, "UUIDs should be unique");
        assert_ne!(id2, id3, "UUIDs should be unique");
        assert_ne!(id1, id3, "UUIDs should be unique");

        assert!(
            uuid::Uuid::parse_str(&id1.0).is_ok(),
            "SessionId should contain valid UUID string"
        );
        assert!(
            uuid::Uuid::parse_str(&id2.0).is_ok(),
            "SessionId should contain valid UUID string"
        );
    }

    #[test]
    fn inbound_roundtrips_as_tagged_json() {
        let msg = InMsg::Prompt {
            session: SessionId::new("s1"),
            text: "hi".into(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(json, r#"{"kind":"prompt","session":"s1","text":"hi"}"#);
        let back: InMsg = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, back);
    }

    #[test]
    fn reject_reason_omits_when_none() {
        let msg = InMsg::Reject {
            session: SessionId::new("s1"),
            request_id: "r1".into(),
            reason: None,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(!json.contains("reason"));
    }

    #[test]
    fn task_list_roundtrips() {
        let ev = OutEvent::TaskList {
            session: SessionId::new("s1"),
            seq: 3,
            tasks: vec![TaskItem {
                id: "t1".into(),
                content: "do thing".into(),
                status: TaskStatus::InProgress,
            }],
        };
        let json = serde_json::to_string(&ev).unwrap();
        let back: OutEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(ev, back);
    }

    #[test]
    fn plan_roundtrips() {
        let ev = OutEvent::Plan {
            session: SessionId::new("s1"),
            seq: 2,
            content: "# Plan\n1. ...".into(),
        };
        let json = serde_json::to_string(&ev).unwrap();
        let back: OutEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(ev, back);
    }

    #[test]
    fn user_question_roundtrips_with_options() {
        let ev = OutEvent::UserQuestion {
            session: SessionId::new("s1"),
            seq: 4,
            request_id: "q1".into(),
            question: "Which approach?".into(),
            options: vec![
                QuestionOption {
                    label: "REST".into(),
                    description: Some("HTTP + JSON".into()),
                },
                QuestionOption {
                    label: "gRPC".into(),
                    description: None,
                },
            ],
            allow_free_form: true,
        };
        let json = serde_json::to_string(&ev).unwrap();
        let back: OutEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(ev, back);
    }

    #[test]
    fn answer_question_roundtrips_as_tagged_json() {
        let msg = InMsg::AnswerQuestion {
            session: SessionId::new("s1"),
            request_id: "q1".into(),
            answer: "REST".into(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(
            json,
            r#"{"kind":"answer_question","session":"s1","request_id":"q1","answer":"REST"}"#
        );
        let back: InMsg = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, back);
    }

    #[test]
    fn list_and_close_session_roundtrip_as_tagged_json() {
        let list = InMsg::ListSessions {
            session: SessionId::new("q1"),
        };
        assert_eq!(
            serde_json::to_string(&list).unwrap(),
            r#"{"kind":"list_sessions","session":"q1"}"#
        );
        assert_eq!(
            serde_json::from_str::<InMsg>(&serde_json::to_string(&list).unwrap()).unwrap(),
            list
        );

        let close = InMsg::CloseSession {
            session: SessionId::new("s1"),
        };
        assert_eq!(
            serde_json::to_string(&close).unwrap(),
            r#"{"kind":"close_session","session":"s1"}"#
        );
    }

    #[test]
    fn session_list_event_roundtrips() {
        let ev = OutEvent::SessionList {
            session: SessionId::new("q1"),
            sessions: vec![
                SessionInfo {
                    session: SessionId::new("root"),
                    parent: None,
                    profile: "build".into(),
                    root: true,
                },
                SessionInfo {
                    session: SessionId::new("child"),
                    parent: Some(SessionId::new("root")),
                    profile: "explore".into(),
                    root: false,
                },
            ],
        };
        assert_eq!(ev.seq(), 0, "SessionList is a lifecycle event, no seq");
        let json = serde_json::to_string(&ev).unwrap();
        let back: OutEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(ev, back);
    }

    #[test]
    fn permission_last_match_wins() {
        let p = PermissionProfile::new(Permission::Deny)
            .with("*", Permission::Ask)
            .with("read", Permission::Allow);
        assert_eq!(p.for_tool("read"), Permission::Allow);
        assert_eq!(p.for_tool("bash"), Permission::Ask);
        assert_eq!(p.for_tool("edit"), Permission::Ask);
    }

    #[test]
    fn permission_defaults_when_no_rule() {
        let p = PermissionProfile::new(Permission::Allow);
        assert_eq!(p.for_tool("anything"), Permission::Allow);
    }
}
