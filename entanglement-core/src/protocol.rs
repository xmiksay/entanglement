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

use entanglement_provider::ContentPart;
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

/// Kind of file change. `ApplyDiff` is reserved for future work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileChangeKind {
    Edit,
    ApplyDiff,
    Create,
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
    /// Resolved posture of the session's active profile (#189): mode, tool mask,
    /// and permission rules, so a reconnecting head can render the permission
    /// posture without re-reading the agent `.md` layers. `None` on the resume
    /// path, where only the profile *name* survives in the replay log.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_detail: Option<ProfileDetail>,
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

/// Per-tool permission rules. Evaluated against a tool name **and** an optional
/// tool-specific argument (the command for `bash`/`call`, the target path for
/// `edit`/`write`/`read`, #173); later matching rules win (so put `"*"` first,
/// specifics after — same semantics as opencode). Every tool — including the
/// runtime's `update_plan`/`update_tasks` state tools (#231, ADR-0049) —
/// resolves through this: there are no engine built-ins that bypass it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionProfile {
    /// `(pattern, permission)` pairs. `pattern` is a tool name, `"*"`, or a
    /// tool-with-argument glob `tool(pattern)` — e.g. `bash(git *)`,
    /// `edit(src/*)` (#173). See [`PermissionProfile::resolve`].
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

    /// Resolve the permission for a tool call, matching each rule key against
    /// the tool `name` **and** an optional tool-specific `arg` (#173). A rule
    /// key is one of:
    ///
    /// - `*` or a bare tool name — matches any call to that tool, ignoring the
    ///   argument (the pre-#173 behaviour);
    /// - `tool(pattern)` — matches only when `arg` is `Some` and the `*`/`?`
    ///   glob `pattern` matches it, e.g. `bash(git *)`, `edit(src/*)`.
    ///
    /// Last matching rule wins, so an argument-scoped rule placed after a
    /// coarse one refines it (`bash: ask` then `bash(git status): allow`).
    pub fn resolve(&self, name: &str, arg: Option<&str>) -> Permission {
        let mut result = self.default;
        for (key, p) in &self.rules {
            if rule_matches(key, name, arg) {
                result = *p;
            }
        }
        result
    }

    /// Name-only resolution: matches `*` and bare-name rules, treating every
    /// argument-scoped `tool(pattern)` rule as a non-match. Equivalent to
    /// [`resolve`][Self::resolve] with `arg = None`. Kept for callers that
    /// render a profile's coarse per-tool posture without a concrete call in
    /// hand (inspect views, the TUI panel).
    pub fn for_tool(&self, name: &str) -> Permission {
        self.resolve(name, None)
    }
}

/// Whether a rule `key` matches a tool `name` + optional `arg` (#173). `key` is
/// `*`, a bare tool name, or `tool(pattern)`; an argument-scoped rule matches
/// only when `arg` is present and its `*`/`?` glob matches.
fn rule_matches(key: &str, name: &str, arg: Option<&str>) -> bool {
    let (tool_pat, arg_pat) = split_rule_key(key);
    if tool_pat != "*" && tool_pat != name {
        return false;
    }
    match arg_pat {
        None => true,
        Some(pat) => arg.is_some_and(|a| glob_match(pat, a)),
    }
}

/// Split a rule key into its tool part and optional argument glob: `bash(git *)`
/// ⇒ `("bash", Some("git *"))`, `bash` ⇒ `("bash", None)`. A key without a
/// trailing `)` is treated as a plain tool name (no argument pattern).
fn split_rule_key(key: &str) -> (&str, Option<&str>) {
    if let Some(open) = key.find('(') {
        if key.ends_with(')') {
            return (&key[..open], Some(&key[open + 1..key.len() - 1]));
        }
    }
    (key, None)
}

/// Minimal `*`/`?` wildcard match for argument-scoped permission rules (#173):
/// `*` matches any run of characters (including `/` and the empty string), `?`
/// matches exactly one, everything else is literal. Deliberately
/// separator-agnostic and free of `**`/character-classes — so `bash(git *)` and
/// `edit(src/*)` both read naturally and core stays dependency-free (ADR-0006).
fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0, 0);
    // Backtrack point: the last `*` seen and the text index it was matched from.
    let (mut star, mut resume) = (None, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            resume = ti;
            pi += 1;
        } else if let Some(s) = star {
            // Mismatch under a prior `*`: let it swallow one more char and retry.
            pi = s + 1;
            resume += 1;
            ti = resume;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// How long an [`InMsg::Approve`] grant lasts (#174). A bare `Approve` is
/// [`Once`][ApprovalScope::Once] — the historical one-shot behavior — so the
/// field defaults to it and older heads that omit it are unaffected. The runtime
/// records the wider scopes in a grant set so an *identical* later call (same
/// tool + argument, [`crate::PermissionProfile::resolve`]'s `arg`) skips the
/// prompt: [`Session`][ApprovalScope::Session] for the life of the session (in
/// memory), [`Always`][ApprovalScope::Always] persisted to the user's managed
/// grants file. A grant only ever upgrades an `Ask` to `Allow`; it never
/// overrides a `Deny` (policy stays a hard floor).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalScope {
    /// Approve just this one call — the next identical call asks again.
    #[default]
    Once,
    /// Approve every identical call for the rest of this session (in-memory).
    Session,
    /// Approve every identical call, now and in future sessions — persisted to
    /// the runtime's managed grants file.
    Always,
}

impl ApprovalScope {
    /// Whether this is the default one-shot scope. Drives `skip_serializing_if`
    /// so a bare `Approve` stays wire-identical to the pre-#174 shape.
    pub fn is_once(&self) -> bool {
        matches!(self, ApprovalScope::Once)
    }
}

/// Whether an agent is directly user-facing, invoked by other agents, or both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentMode {
    /// User-facing entry agent; may spawn sub-agents. Never a valid spawn
    /// *target* itself — the target-side mode gate (#119, ADR-0040) refuses it,
    /// so `build`/`plan` are unreachable via spawn (see
    /// [`AgentProfile::spawnable_as_subagent`]).
    Primary,
    /// Reachable only via spawn; a read-only leaf that defaults to not spawning
    /// further (the `may_spawn` derivation, #119; spawner-side gate, ADR-0024).
    Subagent,
    /// Usable as both a primary entry agent *and* a spawnable sub-agent; spawns
    /// like a `Primary`. Lets one file-defined agent serve both roles
    /// (ADR-0034).
    All,
}

/// A bundle of system prompt + model + permissions that defines how a session
/// reasons and what it may do. A session runs under exactly one profile at a
/// time; switching (e.g. Build ↔ Plan) changes the profile. Mirrors opencode's
/// agent concept. The `name` is the switch key in [`InMsg::SetAgent`].
///
/// Profiles are **file-defined** in the runtime (markdown + YAML frontmatter,
/// ADR-0034): `name`/`mode`/`model`/`permission` come from the frontmatter and
/// `system_prompt` is the file body. `description` drives delegation matching —
/// it is the one field disclosed to a spawning model (via the `agent`/
/// `agent_spawn` tool descriptions).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentProfile {
    pub name: String,
    /// One-line summary; disclosed to a spawning model for delegation matching.
    #[serde(default)]
    pub description: String,
    pub mode: AgentMode,
    pub system_prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub permission: PermissionProfile,
    /// Tool allowlist (#116, ADR-0038). `Some` ⇒ only these tools are
    /// advertised to the model and accepted at dispatch (the registry is
    /// intersected with this set); `None` ⇒ inherit every advertised tool.
    /// Distinct from [`permission`][Self::permission]: this controls a tool's
    /// *existence*, not `Allow`/`Ask`/`Deny` among tools that exist.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,
    /// Tool denylist (#116, ADR-0038), applied *after* the allowlist. A tool
    /// named here is never advertised nor accepted, even if the allowlist (or an
    /// inherit-all `None`) would otherwise include it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disallowed_tools: Vec<String>,
    /// Whether this profile may spawn sub-agents at all (#119, ADR-0040). `None`
    /// ⇒ derive from [`mode`][Self::mode]: a `Subagent` leaf defaults closed,
    /// every other mode open. When it (or the derived default) is `false`, the
    /// whole `agent`/`agent_spawn`/`agent_poll` family is withheld from the model
    /// and refused at dispatch — the physical principle of #116 applied to spawn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub can_spawn: Option<bool>,
    /// Allowlist of agent names this profile may spawn (#119, ADR-0040). `None` ⇒
    /// any registered profile whose `mode` permits sub-agent use. A target
    /// outside the list is refused before a child session is minted. Checked per
    /// spawning session against *its own* profile, so the allowlist is not
    /// transitive (profile A allowed to spawn B does not imply A can spawn what B
    /// can).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawnable_agents: Option<Vec<String>>,
}

impl AgentProfile {
    /// Whether `tool` is in this profile's advertised set: present unless the
    /// denylist removes it, or an allowlist is set and omits it. This is the
    /// *physical* restriction of #116 — orthogonal to
    /// [`PermissionProfile::for_tool`], which grades `Allow`/`Ask`/`Deny` among
    /// the tools that survive this mask. Plan authorship rides this mask now
    /// (#231, ADR-0049): the runtime advertises `update_plan`/`propose_plan` only
    /// to a profile that *explicitly* allowlists them, so plan authority is
    /// default-closed without a dedicated flag.
    pub fn advertises_tool(&self, tool: &str) -> bool {
        if self.disallowed_tools.iter().any(|t| t == tool) {
            return false;
        }
        match &self.tools {
            Some(allow) => allow.iter().any(|t| t == tool),
            None => true,
        }
    }

    /// Whether this profile may spawn sub-agents at all (#119, ADR-0040).
    /// [`can_spawn`][Self::can_spawn] overrides the mode-derived default: a
    /// `Subagent` leaf defaults closed, every other mode open. When this is
    /// `false` the runtime withholds the whole `agent`/`agent_spawn`/`agent_poll`
    /// family and refuses a stale call.
    pub fn may_spawn(&self) -> bool {
        self.can_spawn.unwrap_or(self.mode != AgentMode::Subagent)
    }

    /// Whether this profile may spawn the named target (#119, ADR-0040). A `None`
    /// [`spawnable_agents`][Self::spawnable_agents] allowlist is open to any
    /// spawnable target; otherwise the name must be listed. Orthogonal to
    /// [`spawnable_as_subagent`][Self::spawnable_as_subagent], which gates the
    /// *target's* mode.
    pub fn spawn_target_allowed(&self, name: &str) -> bool {
        match &self.spawnable_agents {
            Some(list) => list.iter().any(|n| n == name),
            None => true,
        }
    }

    /// Whether this profile is a valid spawn *target* (#119, ADR-0040): only
    /// `subagent`/`all` modes are reachable via spawn; a `primary` entry agent
    /// never is, so `build`/`plan` fall out of the hierarchy from mode defaults
    /// with zero frontmatter changes.
    pub fn spawnable_as_subagent(&self) -> bool {
        matches!(self.mode, AgentMode::Subagent | AgentMode::All)
    }

    /// The wire-facing posture of this profile (#189): mode, the #116 tool mask
    /// (`tools`/`disallowed_tools`), and the permission rules. Carried on
    /// [`OutEvent::AgentChanged`] and [`SessionInfo`] so a reconnecting head — or
    /// a sub-agent debugger — can render *why* a tool is allowed/asked/denied
    /// without folding the broadcast or re-reading the agent `.md` layers.
    pub fn detail(&self) -> ProfileDetail {
        ProfileDetail {
            mode: self.mode,
            tools: self.tools.clone(),
            disallowed_tools: self.disallowed_tools.clone(),
            permission: self.permission.clone(),
        }
    }
}

/// Resolved permission posture of an [`AgentProfile`], carried on the wire so a
/// head need not re-read the agent `.md` layers to render it (#189). A projection
/// of [`AgentProfile`] — its policy-bearing fields minus the system prompt and
/// spawn/plan flags that heads don't render for a posture panel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileDetail {
    pub mode: AgentMode,
    /// Tool allowlist (#116); `None` ⇒ inherit every advertised tool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,
    /// Tool denylist (#116), applied after the allowlist.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disallowed_tools: Vec<String>,
    /// Per-tool `Allow | Ask | Deny` rules + fallback.
    pub permission: PermissionProfile,
}

// ┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// ┃ Messages
// ┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// Back-compat deserializer for [`InMsg::Prompt`]'s `content` (#197): accepts
/// either the current `[ContentPart]` array or the legacy bare-`String` under the
/// `text` alias. An empty legacy string yields no parts.
fn de_prompt_content<'de, D>(d: D) -> Result<Vec<ContentPart>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Repr {
        Text(String),
        Parts(Vec<ContentPart>),
    }
    Ok(match Repr::deserialize(d)? {
        Repr::Text(t) if t.is_empty() => Vec::new(),
        Repr::Text(t) => vec![ContentPart::text(t)],
        Repr::Parts(p) => p,
    })
}

/// Inbound: harness → engine. One connection multiplexes sessions via
/// [`SessionId`].
///
/// Not `Eq`: [`Resume`][InMsg::Resume] carries [`OutEvent`] records, which are
/// `PartialEq`-only because of the floating-point cost in
/// [`OutEvent::Usage`] (#192).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InMsg {
    /// Feed a new user prompt into the conversation. `content` carries the
    /// message body as multimodal [`ContentPart`]s (#197, ADR-0064) — text
    /// and/or image blocks. A serde back-compat shim accepts the legacy
    /// text-only `text: "…"` shape so logs persisted before the migration still
    /// deserialize; new writes emit `content`. Build the text-only case with
    /// [`InMsg::prompt`].
    Prompt {
        session: SessionId,
        #[serde(alias = "text", default, deserialize_with = "de_prompt_content")]
        content: Vec<ContentPart>,
    },
    /// Approve a pending tool request (`request_id` from [`OutEvent::ToolRequest`]).
    /// `scope` (#174) controls how long the approval lasts — [`ApprovalScope::Once`]
    /// by default, so a head that omits it keeps the historical one-shot behavior
    /// (and the default scope is omitted on the wire, additive for older heads).
    Approve {
        session: SessionId,
        request_id: String,
        #[serde(default, skip_serializing_if = "ApprovalScope::is_once")]
        scope: ApprovalScope,
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
    /// Switch the session to a different agent profile by name (e.g. `plan`).
    SetAgent { session: SessionId, agent: String },
    /// Switch the session's live model/provider without restarting the engine
    /// (#218). The runtime re-resolves `(provider, model)` against the catalog +
    /// user config (via [`EngineConfig::model_resolver`][crate::EngineConfig]),
    /// rebuilds the session's `Box<dyn Llm>`, and retargets generation + the
    /// context-window budget. Both fields are catalog-qualified — a head's model
    /// picker yields the provider alongside the model — so this covers a
    /// same-provider model change and a full provider switch uniformly. On
    /// success the session emits [`OutEvent::ModelChanged`]; an unknown
    /// provider / missing key surfaces [`OutEvent::Error`]. Applied once the live
    /// turn ends when one is running (stash replay), like [`SetAgent`][InMsg::SetAgent].
    SetModel {
        session: SessionId,
        provider: String,
        model: String,
    },
    /// Spawn a child session (sub-agent) under `parent`, running `prompt` beneath
    /// the named `agent` profile (#60, ADR-0021). `session` is the *child's* id.
    /// The supervisor records the parent link (populating the session tree the
    /// tree-walk helpers read) and starts the child. The runtime's `agent_spawn`
    /// tool (or blocking `agent`) issues this, then relays the child's final answer
    /// back to the parent as a tool result — core needs no notion of "child
    /// session" in its loop.
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
    /// Build a text-only [`Prompt`][InMsg::Prompt] — the common case. Empty text
    /// yields an empty `content` (no stray text part). Multimodal prompts build
    /// the `content` vec directly.
    pub fn prompt(session: SessionId, text: impl Into<String>) -> Self {
        let text = text.into();
        let content = if text.is_empty() {
            Vec::new()
        } else {
            vec![ContentPart::text(text)]
        };
        InMsg::Prompt { session, content }
    }

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
            | InMsg::SetAgent { session, .. }
            | InMsg::SetModel { session, .. }
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
/// [`TaskList`][OutEvent::TaskList], [`Usage`][OutEvent::Usage],
/// [`Error`][OutEvent::Error], [`Done`][OutEvent::Done]) carry a monotonic
/// per-session `seq`.
///
/// Not `Eq`: [`Usage::cost_usd`][OutEvent::Usage] is a floating-point dollar
/// amount, so the enum is `PartialEq` only (#192).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    /// The session switched agent profiles (point-in-time, no `seq`). Carries the
    /// resolved [`ProfileDetail`] (#189) so a head can render the new permission
    /// posture without re-reading the agent `.md` layers; `None` only if the
    /// emitter has no profile handle.
    AgentChanged {
        session: SessionId,
        agent: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        profile_detail: Option<ProfileDetail>,
    },
    /// The session switched to a different model/provider mid-run (point-in-time,
    /// no `seq`), in reply to [`InMsg::SetModel`] (#218). Carries the resolved
    /// `provider`/`model` and the new `context_window` (tokens) so a head can
    /// update its context bar / model display without re-reading the catalog.
    /// Replay re-applies it to re-bind a resumed session to the switched model.
    ModelChanged {
        session: SessionId,
        provider: String,
        model: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context_window: Option<usize>,
    },
    /// The agent's strategy plan (markdown prose), full snapshot on every change.
    /// Emitted by the runtime when it handles an `update_plan` state tool call
    /// (#231, ADR-0049); the engine never stores or consumes the plan.
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
    /// Incremental tool-call argument fragment (#194), correlated to the
    /// eventual [`ToolCall`][OutEvent::ToolCall]/[`ToolExec`][OutEvent::ToolExec]
    /// by `request_id`. Display-only: lets a head render file-sized `edit`/
    /// `write` arguments as they stream, before the assembled `ToolCall` (which
    /// core emits once at round end, ADR-0061) arrives. `tool` labels the stream;
    /// `delta` is a raw JSON-argument substring — fragments concatenated in
    /// arrival order rebuild the call `input`. Additive: a head that ignores it
    /// still gets the full `ToolCall`, so replay reconstructs state from that
    /// (this variant is skipped on the replay fold).
    ToolCallDelta {
        session: SessionId,
        seq: u64,
        request_id: String,
        tool: String,
        delta: String,
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
    /// Markdown, typically a `- [ ]`/`- [x]` checklist — displayed to the user
    /// as progress info, never parsed by the engine (mirrors
    /// [`Plan`][OutEvent::Plan]). Emitted by the runtime on an `update_tasks`
    /// state tool call (#231, ADR-0049).
    TaskList {
        session: SessionId,
        seq: u64,
        content: String,
    },
    /// Token usage + cost for one model round-trip, folded from the provider's
    /// `LlmEvent::Finish` (#192, ADR-0054). Counts are the normalized per-round-trip
    /// deltas (not cumulative); a head accumulates them for a session total.
    /// `cost_usd` is `None` when no catalog pricing covers the active model.
    Usage {
        session: SessionId,
        seq: u64,
        input_tokens: u64,
        output_tokens: u64,
        cached_input_tokens: u64,
        cache_write_tokens: u64,
        cost_usd: Option<f64>,
    },
    /// Recoverable error surfaced to the UI; the engine stays alive.
    Error {
        session: SessionId,
        seq: u64,
        message: String,
    },
    /// Turn finished cleanly. Heads waiting on a one-shot turn exit on this.
    Done { session: SessionId, seq: u64 },
    /// File change record (audit log entry). Emitted by the runtime's tool
    /// executor after each successful `edit`/`write` (#202). The record carries
    /// the `path`, `change_kind`, and a content **hash** (lowercase hex SHA-256
    /// of the after-content) — not the whole-file bytes: the event fans out to
    /// every subscriber, so a large edit must not clone its contents per head.
    FileChange {
        session: SessionId,
        seq: u64,
        path: String,
        change_kind: FileChangeKind,
        hash: String,
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
            | OutEvent::ModelChanged { session, .. }
            | OutEvent::Plan { session, .. }
            | OutEvent::TextDelta { session, .. }
            | OutEvent::ReasoningDelta { session, .. }
            | OutEvent::ToolCallDelta { session, .. }
            | OutEvent::ToolCall { session, .. }
            | OutEvent::ToolRequest { session, .. }
            | OutEvent::ToolExec { session, .. }
            | OutEvent::UserQuestion { session, .. }
            | OutEvent::ToolOutput { session, .. }
            | OutEvent::TaskList { session, .. }
            | OutEvent::Usage { session, .. }
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
            | OutEvent::AgentChanged { .. }
            | OutEvent::ModelChanged { .. } => 0,
            OutEvent::Plan { seq, .. }
            | OutEvent::TextDelta { seq, .. }
            | OutEvent::ReasoningDelta { seq, .. }
            | OutEvent::ToolCallDelta { seq, .. }
            | OutEvent::ToolCall { seq, .. }
            | OutEvent::ToolRequest { seq, .. }
            | OutEvent::ToolExec { seq, .. }
            | OutEvent::UserQuestion { seq, .. }
            | OutEvent::ToolOutput { seq, .. }
            | OutEvent::TaskList { seq, .. }
            | OutEvent::Usage { seq, .. }
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
        let msg = InMsg::prompt(SessionId::new("s1"), "hi");
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(
            json,
            r#"{"kind":"prompt","session":"s1","content":[{"type":"text","text":"hi"}]}"#
        );
        let back: InMsg = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, back);
    }

    #[test]
    fn prompt_accepts_legacy_text_shape() {
        // Logs persisted before #197 carry a bare `text` string; the shim must
        // still deserialize them into the content-block shape.
        let legacy = r#"{"kind":"prompt","session":"s1","text":"hi"}"#;
        let back: InMsg = serde_json::from_str(legacy).unwrap();
        assert_eq!(back, InMsg::prompt(SessionId::new("s1"), "hi"));
    }

    #[test]
    fn prompt_accepts_image_content_block() {
        let json = r#"{"kind":"prompt","session":"s1","content":[
            {"type":"text","text":"look"},
            {"type":"image","source":{"type":"base64","media_type":"image/png","data":"AAAA"}}
        ]}"#;
        let back: InMsg = serde_json::from_str(json).unwrap();
        match back {
            InMsg::Prompt { content, .. } => {
                assert_eq!(content.len(), 2);
                assert_eq!(content[0], ContentPart::text("look"));
                assert_eq!(content[1], ContentPart::image("image/png", "AAAA"));
            }
            other => panic!("expected Prompt, got {other:?}"),
        }
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
    fn approve_scope_defaults_to_once_and_omits_when_default() {
        // A bare approve serializes without a `scope` (default Once), and an older
        // head's frame (no `scope`) still deserializes to Once — additive on the wire.
        let msg = InMsg::Approve {
            session: SessionId::new("s1"),
            request_id: "r1".into(),
            scope: ApprovalScope::Once,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(!json.contains("scope"), "default scope must be omitted");
        let legacy = r#"{"kind":"approve","session":"s1","request_id":"r1"}"#;
        assert_eq!(serde_json::from_str::<InMsg>(legacy).unwrap(), msg);
    }

    #[test]
    fn approve_scope_roundtrips_when_set() {
        for scope in [ApprovalScope::Session, ApprovalScope::Always] {
            let msg = InMsg::Approve {
                session: SessionId::new("s1"),
                request_id: "r1".into(),
                scope,
            };
            let json = serde_json::to_string(&msg).unwrap();
            assert!(json.contains("scope"));
            assert_eq!(serde_json::from_str::<InMsg>(&json).unwrap(), msg);
        }
    }

    #[test]
    fn task_list_roundtrips() {
        let ev = OutEvent::TaskList {
            session: SessionId::new("s1"),
            seq: 3,
            content: "- [x] do thing\n- [ ] next thing".into(),
        };
        let json = serde_json::to_string(&ev).unwrap();
        let back: OutEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(ev, back);
    }

    #[test]
    fn usage_roundtrips_with_and_without_cost() {
        for cost in [Some(0.0123), None] {
            let ev = OutEvent::Usage {
                session: SessionId::new("s1"),
                seq: 5,
                input_tokens: 100,
                output_tokens: 40,
                cached_input_tokens: 30,
                cache_write_tokens: 0,
                cost_usd: cost,
            };
            let json = serde_json::to_string(&ev).unwrap();
            let back: OutEvent = serde_json::from_str(&json).unwrap();
            assert_eq!(ev, back);
            assert_eq!(back.seq(), 5);
        }
    }

    #[test]
    fn tool_call_delta_roundtrips() {
        let ev = OutEvent::ToolCallDelta {
            session: SessionId::new("s1"),
            seq: 7,
            request_id: "call_1".into(),
            tool: "edit".into(),
            delta: r#"{"path":"src/"#.into(),
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains(r#""kind":"tool_call_delta""#), "{json}");
        let back: OutEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(ev, back);
        assert_eq!(back.seq(), 7);
        assert_eq!(back.session(), &SessionId::new("s1"));
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
                    profile_detail: None,
                },
                SessionInfo {
                    session: SessionId::new("child"),
                    parent: Some(SessionId::new("root")),
                    profile: "explore".into(),
                    root: false,
                    profile_detail: Some(ProfileDetail {
                        mode: AgentMode::Subagent,
                        tools: Some(vec!["read".into(), "glob".into()]),
                        disallowed_tools: vec!["edit".into()],
                        permission: PermissionProfile::new(Permission::Deny)
                            .with("read", Permission::Allow),
                    }),
                },
            ],
        };
        assert_eq!(ev.seq(), 0, "SessionList is a lifecycle event, no seq");
        let json = serde_json::to_string(&ev).unwrap();
        let back: OutEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(ev, back);
    }

    #[test]
    fn agent_profile_detail_projects_the_wire_posture() {
        let profile = AgentProfile {
            name: "explore".into(),
            description: String::new(),
            mode: AgentMode::Subagent,
            system_prompt: "secret prompt body".into(),
            model: Some("glm-5.2".into()),
            permission: PermissionProfile::new(Permission::Deny).with("read", Permission::Allow),
            tools: Some(vec!["read".into(), "grep".into()]),
            disallowed_tools: vec!["edit".into()],
            can_spawn: None,
            spawnable_agents: None,
        };
        let detail = profile.detail();
        assert_eq!(detail.mode, AgentMode::Subagent);
        assert_eq!(detail.tools, Some(vec!["read".into(), "grep".into()]));
        assert_eq!(detail.disallowed_tools, vec!["edit".to_string()]);
        assert_eq!(detail.permission.for_tool("read"), Permission::Allow);
        assert_eq!(detail.permission.for_tool("edit"), Permission::Deny);
    }

    #[test]
    fn agent_changed_carries_profile_detail_and_stays_backward_compatible() {
        let ev = OutEvent::AgentChanged {
            session: SessionId::new("s"),
            agent: "plan".into(),
            profile_detail: Some(ProfileDetail {
                mode: AgentMode::Primary,
                tools: None,
                disallowed_tools: Vec::new(),
                permission: PermissionProfile::new(Permission::Ask).with("read", Permission::Allow),
            }),
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert_eq!(serde_json::from_str::<OutEvent>(&json).unwrap(), ev);

        // An older head's frame (no `profile_detail`) still deserializes — the
        // field defaults to `None`, so the enrichment is additive on the wire.
        let legacy = r#"{"kind":"agent_changed","session":"s","agent":"plan"}"#;
        assert_eq!(
            serde_json::from_str::<OutEvent>(legacy).unwrap(),
            OutEvent::AgentChanged {
                session: SessionId::new("s"),
                agent: "plan".into(),
                profile_detail: None,
            }
        );
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

    #[test]
    fn argument_scoped_rule_refines_the_coarse_grade() {
        // Every bash asks, but `git *` is pre-approved and `rm *` hard-denied.
        let p = PermissionProfile::new(Permission::Allow)
            .with("bash", Permission::Ask)
            .with("bash(git *)", Permission::Allow)
            .with("bash(rm *)", Permission::Deny);
        assert_eq!(p.resolve("bash", Some("git status")), Permission::Allow);
        assert_eq!(p.resolve("bash", Some("rm -rf /")), Permission::Deny);
        // A command matching neither refined rule falls back to the coarse `bash`.
        assert_eq!(p.resolve("bash", Some("ls -la")), Permission::Ask);
    }

    #[test]
    fn argument_scoped_rule_ignored_without_an_argument() {
        // Name-only resolution (`for_tool`, and any tool with no arg) never sees
        // an argument-scoped rule — it falls through to the coarse grade.
        let p = PermissionProfile::new(Permission::Ask).with("bash(git *)", Permission::Allow);
        assert_eq!(p.for_tool("bash"), Permission::Ask);
        assert_eq!(p.resolve("bash", None), Permission::Ask);
        assert_eq!(p.resolve("bash", Some("git status")), Permission::Allow);
    }

    #[test]
    fn path_scoped_rule_matches_the_edit_target() {
        // Edits under `src/` are allowed; everything else asks.
        let p = PermissionProfile::new(Permission::Ask).with("edit(src/*)", Permission::Allow);
        assert_eq!(p.resolve("edit", Some("src/main.rs")), Permission::Allow);
        assert_eq!(p.resolve("edit", Some("src/a/b.rs")), Permission::Allow);
        assert_eq!(p.resolve("edit", Some("Cargo.toml")), Permission::Ask);
    }

    #[test]
    fn wildcard_star_matches_across_separators_and_question_matches_one() {
        assert!(glob_match("git *", "git status"));
        // The space before `*` is literal, so a bare `git` (no subcommand) misses.
        assert!(!glob_match("git *", "git"));
        assert!(glob_match("git*", "git")); // trailing `*` may match empty
        assert!(glob_match("src/*", "src/a/b/c.rs"));
        assert!(glob_match("a?c", "abc"));
        assert!(!glob_match("a?c", "ac"));
        assert!(!glob_match("git *", "cargo build"));
        assert!(glob_match("*", "anything at all"));
        assert!(glob_match("exact", "exact"));
        assert!(!glob_match("exact", "exacts"));
    }

    #[test]
    fn split_rule_key_parses_tool_and_pattern() {
        assert_eq!(split_rule_key("bash"), ("bash", None));
        assert_eq!(split_rule_key("*"), ("*", None));
        assert_eq!(split_rule_key("bash(git *)"), ("bash", Some("git *")));
        assert_eq!(split_rule_key("edit(src/*)"), ("edit", Some("src/*")));
        // A malformed key with no closing paren stays a plain name.
        assert_eq!(split_rule_key("bash(oops"), ("bash(oops", None));
    }

    fn masked_profile(tools: Option<Vec<&str>>, disallowed: Vec<&str>) -> AgentProfile {
        AgentProfile {
            name: "m".into(),
            description: String::new(),
            mode: AgentMode::Primary,
            system_prompt: String::new(),
            model: None,
            permission: PermissionProfile::new(Permission::Allow),
            tools: tools.map(|v| v.into_iter().map(String::from).collect()),
            disallowed_tools: disallowed.into_iter().map(String::from).collect(),
            can_spawn: None,
            spawnable_agents: None,
        }
    }

    fn spawn_profile(
        mode: AgentMode,
        can_spawn: Option<bool>,
        spawnable_agents: Option<Vec<&str>>,
    ) -> AgentProfile {
        AgentProfile {
            name: "s".into(),
            description: String::new(),
            mode,
            system_prompt: String::new(),
            model: None,
            permission: PermissionProfile::new(Permission::Allow),
            tools: None,
            disallowed_tools: Vec::new(),
            can_spawn,
            spawnable_agents: spawnable_agents.map(|v| v.into_iter().map(String::from).collect()),
        }
    }

    #[test]
    fn may_spawn_defaults_from_mode() {
        // Primary/all default open; a subagent leaf defaults closed.
        assert!(spawn_profile(AgentMode::Primary, None, None).may_spawn());
        assert!(spawn_profile(AgentMode::All, None, None).may_spawn());
        assert!(!spawn_profile(AgentMode::Subagent, None, None).may_spawn());
    }

    #[test]
    fn can_spawn_overrides_the_mode_default() {
        // An explicit `can_spawn` wins over the mode-derived default either way.
        assert!(!spawn_profile(AgentMode::Primary, Some(false), None).may_spawn());
        assert!(spawn_profile(AgentMode::Subagent, Some(true), None).may_spawn());
    }

    #[test]
    fn spawn_target_allowlist_gates_by_name() {
        // `None` ⇒ open to any target; a list restricts to its entries.
        let open = spawn_profile(AgentMode::Primary, None, None);
        assert!(open.spawn_target_allowed("explore"));
        let scoped = spawn_profile(AgentMode::Primary, None, Some(vec!["explore"]));
        assert!(scoped.spawn_target_allowed("explore"));
        assert!(!scoped.spawn_target_allowed("build"));
    }

    #[test]
    fn spawnable_as_subagent_only_for_subagent_and_all() {
        assert!(spawn_profile(AgentMode::Subagent, None, None).spawnable_as_subagent());
        assert!(spawn_profile(AgentMode::All, None, None).spawnable_as_subagent());
        // A primary entry agent is never a valid spawn target.
        assert!(!spawn_profile(AgentMode::Primary, None, None).spawnable_as_subagent());
    }

    #[test]
    fn advertises_tool_inherits_all_when_unmasked() {
        let p = masked_profile(None, vec![]);
        assert!(p.advertises_tool("edit"));
        assert!(p.advertises_tool("anything"));
    }

    #[test]
    fn advertises_tool_allowlist_restricts_to_listed() {
        let p = masked_profile(Some(vec!["read", "glob", "grep"]), vec![]);
        assert!(p.advertises_tool("read"));
        assert!(p.advertises_tool("grep"));
        assert!(!p.advertises_tool("edit"));
        assert!(!p.advertises_tool("agent_spawn"));
    }

    #[test]
    fn advertises_tool_denylist_wins_over_allowlist() {
        // `edit` is in the allowlist yet also denied — denylist is applied last.
        let p = masked_profile(Some(vec!["read", "edit"]), vec!["edit"]);
        assert!(p.advertises_tool("read"));
        assert!(!p.advertises_tool("edit"));
    }

    #[test]
    fn advertises_tool_denylist_alone_subtracts_from_inherit_all() {
        let p = masked_profile(None, vec!["bash"]);
        assert!(p.advertises_tool("read"));
        assert!(!p.advertises_tool("bash"));
    }
}
