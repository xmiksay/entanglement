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
//!
//! `(session, seq)` is unique across every authored content event (#157): the
//! seq is drawn from one per-session counter shared by the core session task and
//! the runtime (via [`Holly::emit_for_session`][crate::Holly]), so a
//! runtime-authored event minted while the session is parked — an approval
//! [`ToolRequest`][OutEvent::ToolRequest]/[`UserQuestion`][OutEvent::UserQuestion],
//! a [`Plan`][OutEvent::Plan]/[`TaskList`][OutEvent::TaskList] snapshot, a
//! [`FileChange`][OutEvent::FileChange] — gets a fresh seq rather than reusing
//! the parked [`ToolExec`][OutEvent::ToolExec] seq. The one exemption is a
//! supervisor lifecycle [`Error`][OutEvent::Error] for an id with no live session
//! (a refused resume/spawn of a closed/unknown id): it has no counter to draw
//! from and carries `seq == 0` — a value core never mints — which a head renders
//! unconditionally instead of dropping under a `seq > last` dedupe.

use std::collections::HashMap;

use entanglement_provider::{ContentPart, GenerationParams};
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
    /// Engine emitted a tool request and is parked waiting for approval
    /// (`Approve`/`Reject`).
    WaitingApproval,
    /// Parked on a model-driven `ask_user` question, waiting for the user to pick
    /// an option or type a free-form answer (`AnswerQuestion`). Distinct from
    /// [`WaitingApproval`][AgentState::WaitingApproval] (#160): a question is not
    /// a permission decision, and heads render the two differently.
    WaitingAnswer,
    /// Last turn finished cleanly.
    Done,
    /// Last turn ended with an error.
    Error,
}

/// Kind of file change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileChangeKind {
    Edit,
    /// Multi-hunk unified-diff apply, produced by the runtime's `apply_patch`
    /// host tool (#455) — beside `edit`'s single exact-string replace.
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

/// One question in a model-driven [`OutEvent::UserQuestion`] call (#488,
/// supersedes parts of ADR-0027): several may ride in a single `ask_user`
/// invocation. `multi_select` lets the user check off any number of
/// `options`; a free-text "Other" answer is always offered by every head
/// regardless of this flag — unlike the v1 shape this supersedes, there is no
/// `allow_free_form` to opt out of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Question {
    pub question: String,
    pub options: Vec<QuestionOption>,
    #[serde(default)]
    pub multi_select: bool,
}

/// The `questions` payload of an [`OutEvent::UserQuestion`] (#488). A plain
/// `Vec<Question>` field can't absorb a legacy log's sibling top-level keys
/// (`question`/`options`/`allow_free_form`) — `serde`'s field-level
/// `deserialize_with` only ever sees the value already keyed under its own
/// field name, and merging *sibling* keys into one field needs `#[serde(flatten)]`
/// over an `untagged` shape instead (unlike [`de_prompt_content`], which only
/// ever swaps one field's own shape). This newtype is that flattened field:
/// it deserializes either the current `{"questions": [...]}` shape or the
/// pre-#488 flat `{"question", "options", "allow_free_form"}` shape (folded
/// into a one-element vec, `allow_free_form` dropped — free text is
/// unconditional now), and always serializes back out as `{"questions": [...]}`.
#[derive(Debug, Clone, PartialEq)]
pub struct Questions(pub Vec<Question>);

impl Serialize for Questions {
    fn serialize<S>(&self, s: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;
        let mut st = s.serialize_struct("Questions", 1)?;
        st.serialize_field("questions", &self.0)?;
        st.end()
    }
}

impl<'de> Deserialize<'de> for Questions {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Multi {
                questions: Vec<Question>,
            },
            Legacy {
                question: String,
                #[serde(default)]
                options: Vec<QuestionOption>,
                #[serde(default)]
                #[allow(dead_code)]
                allow_free_form: bool,
            },
        }
        Ok(match Repr::deserialize(d)? {
            Repr::Multi { questions } => Questions(questions),
            Repr::Legacy {
                question, options, ..
            } => Questions(vec![Question {
                question,
                options,
                multi_select: false,
            }]),
        })
    }
}

// ┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// ┃ Live MCP server management (#375)
// ┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// Wire shape of one MCP server's spawn/connect config, carried by
/// [`InMsg::McpAdd`]. Mirrors `entanglement_runtime::mcp::McpServerConfig`
/// field-for-field, but lives here as a passive DTO: core holds no MCP logic
/// (ADR-0067) — the `command` XOR `url` transport choice is validated
/// runtime-side, on connect.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpServerSpec {
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    #[serde(default)]
    pub disabled: bool,
}

/// One server's live status, as reported in an [`OutEvent::McpList`] snapshot
/// (#375). `connected`/`tools` reflect the runtime's `ActiveServers` map;
/// `error` is set for a server that failed to connect (empty `tools`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpServerStatus {
    pub name: String,
    /// `"stdio"` or `"http"`.
    pub transport: String,
    pub connected: bool,
    pub tools: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// What changed in reply to [`InMsg::McpAdd`]/[`InMsg::McpRemove`] (#375),
/// carried by [`OutEvent::McpChanged`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpAction {
    Added,
    Removed,
}

// ┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// ┃ Live bash enablement (#498)
// ┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// How a live-registered `bash`/`bash_output` pair is graded, carried by
/// [`InMsg::BashEnable`] and echoed back in [`OutEvent::BashChanged`] (#498,
/// ADR-0133). Registering the tools live (mirroring the MCP `SharedRegistry`
/// seam, #372/#375) is only half the feature — the enablement itself is
/// expressed *through the permission model* rather than a bare on/off:
///
/// - [`BashGrade::Ask`] — the safe default: every `bash` call still goes
///   through the normal [`OutEvent::ToolRequest`] approval prompt.
/// - [`BashGrade::Allow`] — grants permission outright; an optional command
///   `pattern` narrows the grant to matching commands only (e.g. `git *`),
///   materializing an argument-scoped rule like `bash(git *): allow`
///   ([`PermissionProfile`]'s existing `tool(pattern)` syntax, #173) rather
///   than a bespoke mechanism. `None` is a blanket allow.
///
/// Runtime-side, this overrides the session's own profile grade for `bash`/
/// `bash_output` specifically while live-enabled (a profile authored before
/// bash was live-enabled has no real opinion on it), but is still clamped by
/// the config ceiling (#172) exactly like any other grade — a ceiling of
/// `bash: deny` still wins over a live `Allow`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BashGrade {
    Ask,
    Allow {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pattern: Option<String>,
    },
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
/// `edit`/`write`/`read`, #173) — and, for `bash`/`call`, an optional `workdir`
/// (#425); later matching rules win (so put `"*"` first, specifics after —
/// same semantics as opencode). Every tool — including the runtime's
/// `update_plan`/`update_tasks` state tools (#231, ADR-0049) — resolves
/// through this: there are no engine built-ins that bypass it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionProfile {
    /// `(pattern, permission)` pairs. `pattern` is a tool name, `"*"`, a
    /// tool-with-argument glob `tool(pattern)` (#173), or a tool-with-workdir
    /// glob `tool{pattern}` (#425) — e.g. `bash(git *)`, `edit(src/*)`,
    /// `bash{/tmp/*}`. See [`PermissionProfile::resolve`].
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
    /// the tool `name` **and** an optional tool-specific `arg` (#173).
    /// Equivalent to [`resolve_scoped`][Self::resolve_scoped] with
    /// `workdir = None` — every `tool{pattern}` workdir-scoped rule (#425)
    /// simply never matches through this entry point. A rule key is one of:
    ///
    /// - `*` or a bare tool name — matches any call to that tool, ignoring the
    ///   argument (the pre-#173 behaviour);
    /// - `tool(pattern)` — matches only when `arg` is `Some` and the `*`/`?`
    ///   glob `pattern` matches it, e.g. `bash(git *)`, `edit(src/*)`.
    ///
    /// Last matching rule wins, so an argument-scoped rule placed after a
    /// coarse one refines it (`bash: ask` then `bash(git status): allow`).
    pub fn resolve(&self, name: &str, arg: Option<&str>) -> Permission {
        self.resolve_scoped(name, arg, None)
    }

    /// Resolve the permission for a tool call, matching each rule key against
    /// the tool `name`, an optional tool-specific `arg` (#173, `tool(pattern)`),
    /// **and** an optional `workdir` (#425, `tool{pattern}`) — the working
    /// directory a `bash`/`call` invocation runs in, distinct from its command
    /// line. Both scopes are independent single-pattern clauses (not a
    /// compound key): a profile mixes `tool(cmd-pattern)` and `tool{workdir-
    /// pattern}` rules freely in one ordered list, and last-matching-rule-wins
    /// applies across the whole list exactly as it does for `resolve`. A tool
    /// with no workdir concept (anything but `bash`/`call`) simply never
    /// matches a `tool{pattern}` rule, since callers pass `workdir = None` for
    /// it — safe by construction, not by convention.
    pub fn resolve_scoped(
        &self,
        name: &str,
        arg: Option<&str>,
        workdir: Option<&str>,
    ) -> Permission {
        let mut result = self.default;
        for (key, p) in &self.rules {
            if rule_matches(key, name, arg, workdir) {
                result = *p;
            }
        }
        result
    }

    /// Name-only resolution: matches `*` and bare-name rules, treating every
    /// argument-scoped `tool(pattern)`/`tool{pattern}` rule as a non-match.
    /// Equivalent to [`resolve`][Self::resolve] with `arg = None`. Kept for
    /// callers that render a profile's coarse per-tool posture without a
    /// concrete call in hand (inspect views, the TUI panel).
    pub fn for_tool(&self, name: &str) -> Permission {
        self.resolve(name, None)
    }
}

/// A rule key's argument scope, once split from its tool/capability part
/// (#173, #425): unscoped, a `tool(pattern)` command/path glob, or a
/// `tool{pattern}` workdir glob.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuleScope<'a> {
    None,
    Arg(&'a str),
    Workdir(&'a str),
}

/// Whether a rule `key` matches a tool `name` + optional `arg`/`workdir`
/// (#173/#425). `key` is `*`, a bare tool name, `tool(pattern)`, or
/// `tool{pattern}`; a scoped rule matches only when the corresponding value is
/// present and its `*`/`?` glob matches.
fn rule_matches(key: &str, name: &str, arg: Option<&str>, workdir: Option<&str>) -> bool {
    let (tool_pat, scope) = split_rule_key(key);
    if tool_pat != "*" && tool_pat != name {
        return false;
    }
    match scope {
        RuleScope::None => true,
        RuleScope::Arg(pat) => arg.is_some_and(|a| glob_match(pat, a)),
        RuleScope::Workdir(pat) => workdir.is_some_and(|w| glob_match(pat, w)),
    }
}

/// Split a rule key into its tool part and scope: `bash(git *)` ⇒
/// `("bash", Arg("git *"))`, `bash{/tmp/*}` ⇒ `("bash", Workdir("/tmp/*"))`,
/// `bash` ⇒ `("bash", None)`. A key without a matching trailing `)`/`}` is
/// treated as a plain tool name (no pattern).
fn split_rule_key(key: &str) -> (&str, RuleScope<'_>) {
    if let Some(open) = key.find('(') {
        if key.ends_with(')') {
            return (&key[..open], RuleScope::Arg(&key[open + 1..key.len() - 1]));
        }
    }
    if let Some(open) = key.find('{') {
        if key.ends_with('}') {
            return (
                &key[..open],
                RuleScope::Workdir(&key[open + 1..key.len() - 1]),
            );
        }
    }
    (key, RuleScope::None)
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
/// grants file, or [`SessionDir`][ApprovalScope::SessionDir] (#486, ADR-0126) —
/// session-only like `Session`, but widened to every call under the approved
/// call's directory instead of an exact match, and restricted to the read-only
/// triad (`read`/`grep`/`glob`); on any other tool the runtime degrades it to
/// an exact `Session` grant rather than widening it. A grant only ever
/// upgrades an `Ask` to `Allow`; it never overrides a `Deny` (policy stays a
/// hard floor).
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
    /// Approve every call under this call's directory for the rest of this
    /// session (#486) — the read-only triad (`read`/`grep`/`glob`) only; any
    /// other tool degrades this to an exact [`Session`][ApprovalScope::Session]
    /// grant instead of widening it. Never persisted (no `Always`-directory
    /// scope).
    SessionDir,
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
    /// Provider this profile pins its [`model`][Self::model] to (#323, ADR-0081).
    /// A profile with **both** `provider` and `model` set forms a *model pin*
    /// ([`model_pin`][Self::model_pin]): the runtime re-binds the session's
    /// backend to `(provider, model)` on `SetAgent` and at session start, so a
    /// profile carries its own endpoint, not just a model id within the startup
    /// provider. `model` without `provider` keeps today's request-level fallback
    /// (no rebind) — the legacy behaviour. Back-compat: `#[serde(default)]`, so
    /// logs/frames written before #323 deserialize with `provider: None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
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
    /// Per-profile bubblewrap confinement override for `bash`/`call` (#479,
    /// ADR-0104 amendment): `Some("bwrap" | "bubblewrap")` confines every exec
    /// call this profile makes, `Some("none")` forces them unconfined, `None`
    /// inherits the process-global `ENTANGLEMENT_SANDBOX` default. Opaque to
    /// core — validated and interpreted entirely by the runtime
    /// (`host::sandbox`), which owns the `bwrap` mechanism; core only carries
    /// and serializes it, same as `permission`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<String>,
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
    /// The profile's model pin (#323, ADR-0081): `Some((provider, model))` only
    /// when **both** [`provider`][Self::provider] and [`model`][Self::model] are
    /// set, so the runtime can re-bind the session's backend to that endpoint on
    /// `SetAgent`/session start. A `model`-only profile returns `None` — it keeps
    /// the legacy request-level model fallback and triggers no rebind.
    pub fn model_pin(&self) -> Option<(&str, &str)> {
        match (self.provider.as_deref(), self.model.as_deref()) {
            (Some(provider), Some(model)) => Some((provider, model)),
            _ => None,
        }
    }

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

/// Back-compat deserializer for a content field that migrated from a bare string
/// to `[ContentPart]`: [`InMsg::Prompt`]'s `content` (#197, legacy `text`) and
/// [`InMsg::ToolResult`]'s `content` (#221, legacy `output`) both use it. Accepts
/// either the array or the legacy string (aliased on the field); an empty legacy
/// string yields no parts.
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
    /// stay for approval semantics. `content` carries the result as multimodal
    /// [`ContentPart`]s (#221) — text today, an image block when `read` opens an
    /// image file. A serde back-compat shim accepts the legacy text-only
    /// `output: "…"` shape so pre-migration logs still deserialize.
    ToolResult {
        session: SessionId,
        request_id: String,
        #[serde(alias = "output", default, deserialize_with = "de_prompt_content")]
        content: Vec<ContentPart>,
    },
    /// Answer a pending model-driven `ask_user` call (`request_id` from
    /// [`OutEvent::UserQuestion`]). Like [`Approve`][InMsg::Approve]/
    /// [`Reject`][InMsg::Reject], it is consumed by the runtime off the inbound
    /// fan-out (the `ask_user` executor parks on it), not routed to a session —
    /// core stays unaware of the interaction (ADR-0027). `answers` (#488) is
    /// one inner vec of chosen option labels (multi-select) or a single
    /// free-form string per question, in the call's `questions` order — the
    /// always-available "Other" answer is just a string among them, no longer
    /// gated by a dropped `allow_free_form`. Build one with
    /// [`InMsg::answer_question`]. Legacy `answer: String` (pre-#488, a single
    /// question's picked label/free text) still deserializes; a current head
    /// never writes it, leaving it at its empty-string default. The runtime
    /// folds the answers back as the `ask_user` tool's
    /// [`ToolResult`][InMsg::ToolResult].
    AnswerQuestion {
        session: SessionId,
        request_id: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        answers: Vec<Vec<String>>,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        answer: String,
    },
    /// Cancel the current turn and park the session at idle.
    Stop { session: SessionId },
    /// Enumerate the engine's currently-live sessions. The supervisor answers
    /// with a single [`OutEvent::SessionList`] snapshot (ADR-0028); this message
    /// is supervisor-global, not routed to a session task. `correlation_id` is an
    /// opaque token the head mints and the reply echoes back, so a multiplexed
    /// head can pair the snapshot to its request without overloading a
    /// [`SessionId`] as a correlation key (#160, ADR-0072).
    ListSessions { correlation_id: String },
    /// Enumerate the engine's currently-attached MCP servers (#375). MCP config
    /// is global (not per-session), so — like [`ListSessions`][InMsg::ListSessions]
    /// — this is supervisor-global: core routes it to no session task, and it is
    /// answered by a runtime service that owns the tool registry + live server
    /// connections, off the inbound fan-out (mirrors how
    /// [`ReplayFrom`][InMsg::ReplayFrom] is answered by the runtime's history
    /// responder). `correlation_id` pairs the reply
    /// ([`OutEvent::McpList`][crate::protocol::OutEvent::McpList]) to this query.
    McpList { correlation_id: String },
    /// Hot-connect an MCP server in the running process and persist it to
    /// `config.yml` so it survives a restart (#375). Best-effort like startup
    /// connect: a failed connect/handshake is logged, not surfaced as a session
    /// error (there is no session to attach one to). On success emits
    /// [`OutEvent::McpChanged`] with [`McpAction::Added`]. **Trusted-only**
    /// (#472, ADR-0124): a stdio `config` spawns an arbitrary local subprocess
    /// with no approval prompt, so this must never arrive over an untrusted
    /// wire — see [`wire_allowed`][InMsg::wire_allowed].
    McpAdd { name: String, config: McpServerSpec },
    /// Disconnect an MCP server (killing its subprocess / closing its HTTP
    /// session), drop its tools from the registry, and persist the removal
    /// (#375). Unknown name is a no-op (logged). On success emits
    /// [`OutEvent::McpChanged`] with [`McpAction::Removed`]. **Trusted-only**
    /// (#472, ADR-0124) like [`McpAdd`][InMsg::McpAdd]: it mutates engine-global
    /// config and tears down live tools.
    McpRemove { name: String },
    /// Hot-register the `bash`/`bash_output` pair in the running process,
    /// graded by `grade` (#498, ADR-0133) — the live counterpart to the
    /// startup-only `ENTANGLEMENT_ENABLE_BASH` env var. Engine-global like
    /// [`McpAdd`][InMsg::McpAdd] (the tool registry is process-wide, not
    /// per-session): a runtime responder registers the pair into the shared
    /// tool registry (a no-op if already registered) and installs `grade` as
    /// the live permission override for `bash`/`bash_output`, still clamped by
    /// the config ceiling (#172). On success emits
    /// [`OutEvent::BashChanged`] with `enabled: true`. **Trusted-only**
    /// (#472, ADR-0124, same rationale as `McpAdd`): live-enabling `bash`
    /// hands the model a full shell with no approval prompt when graded
    /// `Allow`, so this must never arrive over an untrusted wire.
    BashEnable { grade: BashGrade },
    /// Unregister the `bash`/`bash_output` pair and clear the live grade
    /// override (#498, ADR-0133) — the counterpart to
    /// [`BashEnable`][InMsg::BashEnable]. A pair registered at startup via
    /// `ENTANGLEMENT_ENABLE_BASH` is unregistered the same way. On success
    /// emits [`OutEvent::BashChanged`] with `enabled: false`.
    /// **Trusted-only** (#472, ADR-0124) like [`BashEnable`][InMsg::BashEnable]:
    /// it mutates engine-global tool registration.
    BashDisable,
    /// Fetch a session's persisted content history from `after_seq` onward, for a
    /// head that subscribed late and missed the live broadcast (#160, ADR-0072).
    /// Answered out-of-core by the runtime's history responder — which owns the
    /// event log — with a single [`OutEvent::History`] snapshot carrying every
    /// content event whose `seq` exceeds `after_seq`, `correlation_id` echoed so
    /// the requester can pair the reply. `after_seq == 0` requests the whole
    /// content history. A head-authored query, so it is wire-allowed.
    ReplayFrom {
        session: SessionId,
        correlation_id: String,
        after_seq: u64,
    },
    /// Explicitly terminate a live session: the supervisor drops its command
    /// channel and the session task exits, emitting [`OutEvent::SessionEnded`]
    /// (ADR-0028). Distinct from [`Stop`][InMsg::Stop], which only cancels the
    /// in-flight turn and leaves the session alive (ADR-0017) — `CloseSession`
    /// is the lifecycle destroy Stop no longer performs. Unknown / already-closed
    /// ids are a no-op. Session ids are single-use: mint a fresh one
    /// ([`SessionId::new_uuid`]) rather than reusing a closed id.
    CloseSession { session: SessionId },
    /// Evict a live session from memory **without** tombstoning its id (#318,
    /// ADR-0077). The supervisor tears down the session task and drops its
    /// in-memory [`Context`][crate::context::Context]/history — cascading over the
    /// spawn sub-tree like [`CloseSession`][InMsg::CloseSession] — but records no
    /// tombstone, so the id stays **resumable**: a later
    /// [`Holly::resume`][crate::Holly::resume] rebuilds it from the embedder's
    /// event log exactly like the restart path, re-offering any pending
    /// `ToolExec` for a turn parked on approval (ADR-0061/0071). Distinct from
    /// `CloseSession` (terminal, tombstoned) and [`Stop`][InMsg::Stop] (cancels a
    /// turn, keeps the session live). Emits [`OutEvent::SessionHibernated`].
    /// A mid-stream turn is torn down (stop-then-hibernate): the uncommitted
    /// round is discarded exactly as replay drops a text-only tail, so resume is
    /// lossless w.r.t. the embedder's log. **Embedder-initiated and trusted-only**
    /// (like [`Resume`][InMsg::Resume]): it is *not* wire-allowed — a wire head
    /// cannot evict another session's memory. Unknown ids are a no-op.
    HibernateSession { session: SessionId },
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
    /// Live-adjust generation knobs (temperature / max-output / thinking budget /
    /// reasoning effort) without restarting the engine (#374, ADR-0094). `overrides`
    /// is **partial**: a `None` field means "leave unchanged" — only the fields the
    /// caller sets are merged onto the session's current
    /// [`Session::generation`][crate::session::Session] via
    /// [`GenerationParams::apply_overrides`]. Unlike [`SetModel`][InMsg::SetModel],
    /// there is no resolver to fail against, so this always succeeds and always
    /// emits [`OutEvent::GenerationChanged`] with the merged, full effective
    /// params — even when every override happens to match the current value — so a
    /// head can rely on the reply to confirm the write landed. Applied once the
    /// live turn ends when one is running (stash replay), like
    /// [`SetAgent`][InMsg::SetAgent]/[`SetModel`][InMsg::SetModel]. The merged
    /// result is also recorded as this session's per-profile memory
    /// (`Session::profile_generation`), so a later `SetAgent` switch back to the
    /// same profile re-applies it — the generation-parameter analogue of the model
    /// pin's session memory (#323, ADR-0081).
    SetGeneration {
        session: SessionId,
        overrides: GenerationParams,
    },
    /// Run a single out-of-band LLM op outside the turn loop (#324, ADR-0082).
    /// The generic surface is the **wire shape** — an opaque `op` string plus
    /// `args` — not a plugin registry: `session::ops::run_oneshot` matches on
    /// `op` (`"compact"` today; an unknown op emits a recoverable `Error`).
    /// Mutates only the caller's own `Context`, so it is wire-allowed. Deferred
    /// while a turn is live (stash replay), like `SetAgent`/`SetModel`.
    /// `"compact"`'s `args`: `instructions` (optional free-text steer) and
    /// `kept` (optional `u64`, default `0` — a keep-tail request, #397/
    /// ADR-0102, clamped to the nearest safe turn boundary).
    Oneshot {
        session: SessionId,
        op: String,
        #[serde(default)]
        args: serde_json::Value,
    },
    /// Spawn a session running `prompt` beneath the named `agent` profile (#60,
    /// ADR-0021). `session` is the new session's id. With `parent = Some(p)` it is
    /// a **child** sub-agent under `p` (the common case): the supervisor records
    /// the parent link (populating the spawn tree the tree-walk helpers read) so a
    /// `CloseSession`/hibernate cascade and the permission ancestor clamp cover it.
    /// With `parent = None` it is a **root** — used by the `/compact` successor
    /// fork (ADR-0110), which sets `predecessor = Some(source)` to record the
    /// session it succeeds *without* joining the source's spawn sub-tree (so
    /// closing the source doesn't cascade onto the successor). The runtime's
    /// `agent_spawn` tool (or blocking `agent`) issues the child form, then relays
    /// the child's final answer back to the parent as a tool result — core needs
    /// no notion of "child session" in its loop.
    Spawn {
        session: SessionId,
        #[serde(default)]
        parent: Option<SessionId>,
        /// The session this one succeeds (a `/compact` fork, ADR-0110). Non-live
        /// lineage only — never a spawn edge.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        predecessor: Option<SessionId>,
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

    /// Build a text [`ToolResult`][InMsg::ToolResult] — the common case. Empty
    /// text yields an empty `content` (matching [`Message::tool`]'s fold);
    /// multimodal results (an image `read`, #221) build the `content` vec
    /// directly.
    pub fn tool_result(
        session: SessionId,
        request_id: impl Into<String>,
        output: impl Into<String>,
    ) -> Self {
        let output = output.into();
        let content = if output.is_empty() {
            Vec::new()
        } else {
            vec![ContentPart::text(output)]
        };
        InMsg::ToolResult {
            session,
            request_id: request_id.into(),
            content,
        }
    }

    /// Build an [`AnswerQuestion`][InMsg::AnswerQuestion] in the current v2
    /// shape (#488) — one inner vec of chosen labels / free text per question,
    /// in the answered call's `questions` order. Leaves the legacy `answer`
    /// field at its empty default.
    pub fn answer_question(
        session: SessionId,
        request_id: impl Into<String>,
        answers: Vec<Vec<String>>,
    ) -> Self {
        InMsg::AnswerQuestion {
            session,
            request_id: request_id.into(),
            answers,
            answer: String::new(),
        }
    }

    /// The session this message targets, or `None` for a supervisor-global query
    /// that names no session — [`ListSessions`][InMsg::ListSessions], the MCP
    /// ops [`McpList`][InMsg::McpList]/[`McpAdd`][InMsg::McpAdd]/
    /// [`McpRemove`][InMsg::McpRemove] (#375; MCP config is engine-global, not
    /// per-session), and the bash-live ops
    /// [`BashEnable`][InMsg::BashEnable]/[`BashDisable`][InMsg::BashDisable]
    /// (#498; the tool registry is likewise engine-global). Every other
    /// variant, including the session-scoped [`ReplayFrom`][InMsg::ReplayFrom]
    /// query, carries one.
    pub fn session(&self) -> Option<&SessionId> {
        match self {
            InMsg::Prompt { session, .. }
            | InMsg::Approve { session, .. }
            | InMsg::Reject { session, .. }
            | InMsg::ToolResult { session, .. }
            | InMsg::AnswerQuestion { session, .. }
            | InMsg::Stop { session }
            | InMsg::ReplayFrom { session, .. }
            | InMsg::CloseSession { session }
            | InMsg::HibernateSession { session }
            | InMsg::SetAgent { session, .. }
            | InMsg::SetModel { session, .. }
            | InMsg::SetGeneration { session, .. }
            | InMsg::Oneshot { session, .. }
            | InMsg::Spawn { session, .. }
            | InMsg::Resume { session, .. } => Some(session),
            InMsg::ListSessions { .. }
            | InMsg::McpList { .. }
            | InMsg::McpAdd { .. }
            | InMsg::McpRemove { .. }
            | InMsg::BashEnable { .. }
            | InMsg::BashDisable => None,
        }
    }

    /// Whether this variant may originate from an **untrusted wire head** (#155).
    ///
    /// The trusted/untrusted frame split. A head deserializing attacker-adjacent
    /// bytes (stdio `pipe`, WebSocket `serve`) forwards only the allowlisted
    /// frames below. Everything else is **runtime-authored in process** (or an
    /// engine-privileged control), never wire-forgeable:
    ///
    /// - [`ToolResult`][InMsg::ToolResult] resolves a parked turn matched on
    ///   `request_id` alone — a forged one bypasses execution *and* permission;
    /// - [`Spawn`][InMsg::Spawn] mints a child session bypassing the tool path's
    ///   `spawn_refusal` gate;
    /// - [`Resume`][InMsg::Resume] is internal (`#[serde(skip)]`, never on wire);
    /// - [`HibernateSession`][InMsg::HibernateSession] is an embedder memory-eviction
    ///   control (#318) — a wire head must not be able to evict another session's
    ///   in-memory state;
    /// - [`McpAdd`][InMsg::McpAdd]/[`McpRemove`][InMsg::McpRemove] (#472,
    ///   ADR-0124, reversing #375's wire tier): an unapproved `McpAdd` spawns an
    ///   arbitrary local subprocess, and the `serve` head's origin gate is
    ///   opt-in — a hostile web page could otherwise drive it cross-origin over
    ///   the local WebSocket. ADR-0047's "config is consent" covers the *config
    ///   file*, not an unauthenticated wire frame. Trusted heads (the TUI
    ///   `/mcp` command) keep both via [`Holly::send`][crate::Holly::send];
    ///   `McpList` is read-only and stays wire-allowed.
    /// - [`BashEnable`][InMsg::BashEnable]/[`BashDisable`][InMsg::BashDisable]
    ///   (#498, ADR-0133, same rationale as `McpAdd`/`McpRemove`): live-enabling
    ///   `bash` hands the model a full shell, optionally graded `Allow` with no
    ///   approval prompt at all — a wire frame must never grant that.
    ///
    /// The executor submits `ToolResult`/`Spawn` over the privileged in-process
    /// [`Holly::send`][crate::Holly::send] (it holds the handle); a wire head uses
    /// [`Holly::send_from_wire`][crate::Holly::send_from_wire], which enforces this
    /// allowlist.
    ///
    /// An explicit allow-list `match` — not a negated blocklist — so a **new**
    /// variant fails closed: it is wire-refused until someone adds it here
    /// deliberately, and the exhaustive match makes that decision a compile
    /// error to skip (mirroring [`session`][Self::session] /
    /// [`variant_name`][Self::variant_name]).
    pub fn wire_allowed(&self) -> bool {
        match self {
            InMsg::Prompt { .. }
            | InMsg::Approve { .. }
            | InMsg::Reject { .. }
            | InMsg::AnswerQuestion { .. }
            | InMsg::Stop { .. }
            | InMsg::ListSessions { .. }
            | InMsg::McpList { .. }
            | InMsg::ReplayFrom { .. }
            | InMsg::CloseSession { .. }
            | InMsg::SetAgent { .. }
            | InMsg::SetModel { .. }
            | InMsg::SetGeneration { .. }
            | InMsg::Oneshot { .. } => true,
            InMsg::ToolResult { .. }
            | InMsg::Spawn { .. }
            | InMsg::Resume { .. }
            | InMsg::HibernateSession { .. }
            | InMsg::McpAdd { .. }
            | InMsg::McpRemove { .. }
            | InMsg::BashEnable { .. }
            | InMsg::BashDisable => false,
        }
    }

    /// The serde `kind` tag of this variant, for diagnostics (e.g. a rejected
    /// wire frame). Matches the `snake_case` wire discriminant.
    pub fn variant_name(&self) -> &'static str {
        match self {
            InMsg::Prompt { .. } => "prompt",
            InMsg::Approve { .. } => "approve",
            InMsg::Reject { .. } => "reject",
            InMsg::ToolResult { .. } => "tool_result",
            InMsg::AnswerQuestion { .. } => "answer_question",
            InMsg::Stop { .. } => "stop",
            InMsg::ListSessions { .. } => "list_sessions",
            InMsg::McpList { .. } => "mcp_list",
            InMsg::McpAdd { .. } => "mcp_add",
            InMsg::McpRemove { .. } => "mcp_remove",
            InMsg::BashEnable { .. } => "bash_enable",
            InMsg::BashDisable => "bash_disable",
            InMsg::ReplayFrom { .. } => "replay_from",
            InMsg::CloseSession { .. } => "close_session",
            InMsg::HibernateSession { .. } => "hibernate_session",
            InMsg::SetAgent { .. } => "set_agent",
            InMsg::SetModel { .. } => "set_model",
            InMsg::SetGeneration { .. } => "set_generation",
            InMsg::Oneshot { .. } => "oneshot",
            InMsg::Spawn { .. } => "spawn",
            InMsg::Resume { .. } => "resume",
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
        /// The session this one succeeds (a `/compact` fork, ADR-0110); `None`
        /// for an ordinary session. Non-live lineage — the predecessor's
        /// interactive session is closed once the successor starts.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        predecessor: Option<SessionId>,
        profile: String,
        model: Option<String>,
        root: bool,
        ts: u64,
    },
    /// Session ended (lifecycle event, no `seq`). Emits when a session exits.
    SessionEnded { session: SessionId, ts: u64 },
    /// Session evicted from memory but **not** tombstoned (lifecycle event, no
    /// `seq`), in reply to [`InMsg::HibernateSession`] (#318, ADR-0077). The
    /// session task tore down and released its [`Context`][crate::context::Context],
    /// but the id stays resumable — distinct from
    /// [`SessionEnded`][OutEvent::SessionEnded], whose `CloseSession` origin
    /// tombstones the id. A head renders it like an end; a persistence tap can
    /// observe the eviction. `Holly::resume` on the id rebuilds it from the log.
    SessionHibernated { session: SessionId, ts: u64 },
    /// Snapshot of every currently-live session (lifecycle event, no `seq`),
    /// sent in reply to [`InMsg::ListSessions`] (ADR-0028). `correlation_id`
    /// echoes the requester's opaque token so a multiplexed head can pair the
    /// reply with its request (#160, ADR-0072) — no longer an overloaded
    /// [`SessionId`], so this event names no session and [`session`][OutEvent::session]
    /// is `None` for it.
    SessionList {
        correlation_id: String,
        sessions: Vec<SessionInfo>,
    },
    /// Snapshot of every currently-attached MCP server (lifecycle event, no
    /// `seq`), in reply to [`InMsg::McpList`] (#375). Answered by the runtime
    /// service that owns the live server connections — same "engine-global, not
    /// core's business" shape as [`SessionList`][OutEvent::SessionList].
    McpList {
        correlation_id: String,
        servers: Vec<McpServerStatus>,
    },
    /// An MCP server was hot-added or removed (lifecycle event, no `seq`), in
    /// reply to [`InMsg::McpAdd`]/[`InMsg::McpRemove`] (#375).
    McpChanged { name: String, action: McpAction },
    /// `bash`/`bash_output` were live-registered or unregistered (lifecycle
    /// event, no `seq`), in reply to
    /// [`InMsg::BashEnable`]/[`InMsg::BashDisable`] (#498, ADR-0133).
    /// `grade` is the live permission override now in effect — `Some` when
    /// `enabled` is `true`, `None` when `false`.
    BashChanged {
        enabled: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        grade: Option<BashGrade>,
    },
    /// A session's persisted content history from a requested `after_seq`, in
    /// reply to [`InMsg::ReplayFrom`] (#160, ADR-0072). Answered by the runtime's
    /// history responder — which owns the event log — not the core supervisor.
    /// `events` holds every content [`OutEvent`] whose `seq` exceeds the requested
    /// `after_seq`, in log order; `correlation_id` echoes the request so a
    /// multiplexed head can pair the reply. A lifecycle query reply (no `seq`); it
    /// is neither persisted nor folded on replay.
    History {
        correlation_id: String,
        session: SessionId,
        events: Vec<OutEvent>,
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
    /// The session's live generation knobs changed (point-in-time, no `seq`),
    /// in reply to [`InMsg::SetGeneration`] (#374, ADR-0094) or an implicit
    /// overlay applied on `SetAgent`/session start. Carries the **full** resolved
    /// [`GenerationParams`] — not just what changed — so a head can render the
    /// effective state directly and so replay can restore it verbatim by simply
    /// overwriting [`Session::generation`][crate::session::Session]. Also folded
    /// into `Session::profile_generation` on replay, keyed by the active profile
    /// at the time (mirrors [`ModelChanged`][OutEvent::ModelChanged]'s
    /// `profile_models` reconstruction).
    GenerationChanged {
        session: SessionId,
        generation: GenerationParams,
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
    ///
    /// `agent` carries the emitting session's active profile name (#156): the
    /// runtime executor resolves permission/mask against it *authoritatively*,
    /// self-healing its per-session profile map from this field instead of the
    /// lossy `SessionStarted`/`AgentChanged` broadcast fold — so a dropped
    /// lifecycle event under burst can no longer silently downgrade a restricted
    /// session to allow-all/unmasked. `#[serde(default)]` keeps pre-#156 logs
    /// deserializable (empty ⇒ the executor falls back to its folded state).
    ToolExec {
        session: SessionId,
        seq: u64,
        request_id: String,
        tool: String,
        input: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        agent: String,
    },
    /// The model asked the user one or more decision questions in a single
    /// `ask_user` call (#488, supersedes parts of ADR-0027: one event now
    /// carries a `questions` array, and a free-text answer is always
    /// available rather than gated by a per-call `allow_free_form`). A head
    /// walks `questions` in order and replies once with
    /// [`InMsg::AnswerQuestion`]; the runtime folds every answer back as the
    /// one tool call's output. Dedicated (not
    /// [`ToolRequest`][OutEvent::ToolRequest]) so choices render cleanly.
    /// `questions` flattens onto the wire via [`Questions`] so a legacy
    /// single-question log still deserializes.
    UserQuestion {
        session: SessionId,
        seq: u64,
        request_id: String,
        #[serde(flatten)]
        questions: Questions,
    },
    /// Result of an executed tool, a denied tool, or a built-in tool. `output` is
    /// the text rendering heads display (for an image result, a short
    /// placeholder). `content` carries the full multimodal result (#221) — empty
    /// for the common text-only case (heads read `output`), populated with the
    /// image block(s) when `read` opens an image so **replay** reconstructs the
    /// model's view faithfully instead of degrading it to the placeholder.
    ToolOutput {
        session: SessionId,
        seq: u64,
        request_id: String,
        tool: String,
        output: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        content: Vec<ContentPart>,
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
    /// Session compaction ran (#324, ADR-0082 → ADR-0101/0103): the engine
    /// produced an LLM-generated `summary` of the conversation. Two distinct
    /// mutation semantics share this one variant, told apart by `auto`:
    ///
    /// - `auto: false` (the default) — **manual `/compact`, copy-on-write
    ///   (ADR-0101)**: the source session's `Context` is **not** mutated, this
    ///   is a *report* ("summary ready, source untouched"). The head that
    ///   issued the compaction forks the summary into a new session via
    ///   `InMsg::Spawn`; the original stays idle, intact, independently
    ///   resumable. `Session::replay`'s fold is a no-op for this case — there
    ///   is nothing to reconstruct, the source was never mutated.
    /// - `auto: true` — **automatic in-place compaction on context overflow**
    ///   (#398, ADR-0103): `session/turn.rs` mutated the *live* session's
    ///   `Context` via `Context::apply_compaction` before continuing the turn,
    ///   because a turn mid-flight has no head to fork into. `Session::replay`
    ///   folds this case by replaying the same `apply_compaction` call, so a
    ///   resumed session's history matches the live one.
    ///
    /// A persisted, seq-bearing content event either way (persistence is
    /// variant-agnostic; seq-bearing ⇒ folded into `ReplayFrom` history).
    /// `kept` is how many trailing messages — clamped to the nearest safe
    /// turn boundary (#397, ADR-0102) — ride verbatim inside `summary`,
    /// appended after the LLM-generated summary of everything before them;
    /// `0` (the default) means the whole history was summarized with no
    /// verbatim tail, matching every pre-#397 record. `auto` defaults to
    /// `false` on the wire, matching every pre-#398 record (all of which were
    /// the manual, copy-on-write path).
    Compacted {
        session: SessionId,
        seq: u64,
        summary: String,
        #[serde(default)]
        kept: u64,
        #[serde(default)]
        auto: bool,
    },
    /// File change record (audit log entry). Emitted by the runtime's tool
    /// executor after each successful `edit`/`write`/`apply_patch` (#202, #455).
    /// The record carries
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
    /// The session's active-skill tool mask changed (#400, ADR-0106). Emitted by
    /// the runtime's tool executor: `Some(skill_id)` when a `load_skill` call
    /// activates a skill (`allowed_tools` is that skill's mask, `None` meaning it
    /// imposes none), `None` when the skill's scope ends — the current turn's
    /// `Done`, or the session ending. Wire-facing posture only: core neither
    /// interprets nor enforces this (skills are runtime-only, ADR-0037); a head
    /// combines it with `ProfileDetail`'s #116 agent mask to render the session's
    /// full effective tool set. Mirrors `FileChange`: a fresh per-session seq
    /// (#157), no core replay-fold semantics (a head just tracks the latest
    /// value).
    SkillActive {
        session: SessionId,
        seq: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        skill_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        allowed_tools: Option<Vec<String>>,
    },
    /// An ambiguous LLM stop (ADR-0118) triggered a bounded in-place retry: the
    /// round ended with no tool calls and no confident finish signal, so core
    /// committed whatever partial text streamed, injected `nudge` as a synthetic
    /// user-role steering message, and re-queried the model within the same
    /// turn. Persisted + seq-bearing (like `Compacted`) so `Session::replay`
    /// reconstructs the exact round boundary — flushing the partial assistant
    /// message and pushing the nudge — instead of merging every retry round's
    /// `TextDelta`s into one assistant message and dropping the nudge, which
    /// would resume a session from a history the live model never saw. Its
    /// non-delta arrival also delimits the re-streamed partial text so a head
    /// (the TUI transcript, a sub-agent answer collector) starts a fresh
    /// segment rather than concatenating consecutive rounds' text. Heads render
    /// it as a one-line "retrying" notice.
    AmbiguousRetry {
        session: SessionId,
        seq: u64,
        nudge: String,
    },
    /// A provider-side web-search result block was minted this round (#481,
    /// follow-up to #305/ADR-0075's "not persisted" MVP limitation). Persisted
    /// and seq-bearing (like `AmbiguousRetry`) so `Session::replay` reconstructs
    /// the assistant message's content verbatim — including the provider-native
    /// `data` payload `part` carries, needed to replay the search back to the
    /// same provider on a later turn (mirrors `ToolCall.provider_meta`'s opaque
    /// round-trip contract). A different provider's converter (and any
    /// renderer) reads only `part`'s `summary`, never its opaque `data`. Heads
    /// render `summary` as a one-line notice, mirroring how `ReasoningDelta`
    /// already renders the live query/source lines for this same search.
    SearchResult {
        session: SessionId,
        seq: u64,
        part: ContentPart,
    },
}

impl OutEvent {
    /// The session this event belongs to, or `None` for a supervisor-global query
    /// reply that names no single session — [`SessionList`][OutEvent::SessionList]
    /// (#160), [`McpList`][OutEvent::McpList] and
    /// [`McpChanged`][OutEvent::McpChanged] (#375; MCP config is engine-global).
    /// [`History`][OutEvent::History] does name a session (the one whose
    /// history it carries), so it returns `Some`.
    pub fn session(&self) -> Option<&SessionId> {
        match self {
            OutEvent::SessionStarted { session, .. }
            | OutEvent::SessionEnded { session, .. }
            | OutEvent::SessionHibernated { session, .. }
            | OutEvent::History { session, .. }
            | OutEvent::Status { session, .. }
            | OutEvent::AgentChanged { session, .. }
            | OutEvent::ModelChanged { session, .. }
            | OutEvent::GenerationChanged { session, .. }
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
            | OutEvent::Compacted { session, .. }
            | OutEvent::FileChange { session, .. }
            | OutEvent::SkillActive { session, .. }
            | OutEvent::AmbiguousRetry { session, .. }
            | OutEvent::SearchResult { session, .. } => Some(session),
            OutEvent::SessionList { .. }
            | OutEvent::McpList { .. }
            | OutEvent::McpChanged { .. }
            | OutEvent::BashChanged { .. } => None,
        }
    }

    /// The monotonic per-session sequence number for a **content** event, or
    /// `None` for a point-in-time lifecycle/query event that carries no `seq`
    /// (`SessionStarted`, `SessionEnded`, `SessionList`, `History`, `Status`,
    /// `AgentChanged`, `ModelChanged`, `GenerationChanged`). Returning `Option`
    /// instead of a fake `0`
    /// (#160, ADR-0072) lets a head tell "seq 0" apart from "no seq" — the
    /// supervisor-shed `Error` sentinel (seq `0`) is a real `Some(0)`, distinct
    /// from a lifecycle event's `None`.
    pub fn seq(&self) -> Option<u64> {
        match self {
            OutEvent::SessionStarted { .. }
            | OutEvent::SessionEnded { .. }
            | OutEvent::SessionHibernated { .. }
            | OutEvent::SessionList { .. }
            | OutEvent::McpList { .. }
            | OutEvent::McpChanged { .. }
            | OutEvent::BashChanged { .. }
            | OutEvent::History { .. }
            | OutEvent::Status { .. }
            | OutEvent::AgentChanged { .. }
            | OutEvent::ModelChanged { .. }
            | OutEvent::GenerationChanged { .. } => None,
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
            | OutEvent::Compacted { seq, .. }
            | OutEvent::FileChange { seq, .. }
            | OutEvent::SkillActive { seq, .. }
            | OutEvent::AmbiguousRetry { seq, .. }
            | OutEvent::SearchResult { seq, .. } => Some(*seq),
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
    fn wire_allowlist_refuses_the_privileged_frames() {
        let s = SessionId::new("s1");
        // The runtime/embedder authors these in process; a wire head must never
        // forward them (#155, #318).
        assert!(!InMsg::tool_result(s.clone(), "r1", "x").wire_allowed());
        assert!(!InMsg::Spawn {
            session: s.clone(),
            parent: Some(s.clone()),
            predecessor: None,
            agent: "build".into(),
            prompt: "go".into(),
        }
        .wire_allowed());
        assert!(!InMsg::Resume {
            session: s.clone(),
            records: Vec::new(),
        }
        .wire_allowed());
        // Memory eviction is an embedder control, trusted-only (#318): a wire head
        // must not be able to evict another session's in-memory state.
        assert!(!InMsg::HibernateSession { session: s.clone() }.wire_allowed());
        // MCP mutation is trusted-only (#472, ADR-0124): an unapproved `McpAdd`
        // spawns an arbitrary local subprocess, so neither mutating op may
        // arrive over an untrusted wire. The read-only `McpList` stays allowed.
        assert!(!InMsg::McpAdd {
            name: "srv".into(),
            config: McpServerSpec {
                command: Some("evil".into()),
                args: vec![],
                env: std::collections::HashMap::new(),
                url: None,
                headers: std::collections::HashMap::new(),
                disabled: false,
            },
        }
        .wire_allowed());
        assert!(!InMsg::McpRemove { name: "srv".into() }.wire_allowed());
        // Every head-authored frame stays acceptable off the wire.
        for msg in [
            InMsg::prompt(s.clone(), "hi"),
            InMsg::Approve {
                session: s.clone(),
                request_id: "r".into(),
                scope: ApprovalScope::Once,
            },
            InMsg::Reject {
                session: s.clone(),
                request_id: "r".into(),
                reason: None,
            },
            InMsg::answer_question(s.clone(), "r", vec![vec!["a".into()]]),
            InMsg::Stop { session: s.clone() },
            InMsg::ListSessions {
                correlation_id: "c1".into(),
            },
            InMsg::ReplayFrom {
                session: s.clone(),
                correlation_id: "c1".into(),
                after_seq: 0,
            },
            InMsg::CloseSession { session: s.clone() },
            InMsg::SetAgent {
                session: s.clone(),
                agent: "plan".into(),
            },
            InMsg::SetModel {
                session: s.clone(),
                provider: "zai".into(),
                model: "glm-5.2".into(),
            },
            InMsg::SetGeneration {
                session: s.clone(),
                overrides: entanglement_provider::GenerationParams::default(),
            },
            InMsg::Oneshot {
                session: s.clone(),
                op: "compact".into(),
                args: serde_json::Value::Null,
            },
            InMsg::McpList {
                correlation_id: "c1".into(),
            },
        ] {
            assert!(
                msg.wire_allowed(),
                "`{}` should be wire-allowed",
                msg.variant_name()
            );
        }
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
    fn tool_result_accepts_legacy_output_shape() {
        // Tool results persisted before #221 carry a bare `output` string; the
        // shim aliases it into the multimodal `content` shape.
        let legacy = r#"{"kind":"tool_result","session":"s1","request_id":"r1","output":"done"}"#;
        let back: InMsg = serde_json::from_str(legacy).unwrap();
        assert_eq!(back, InMsg::tool_result(SessionId::new("s1"), "r1", "done"));
    }

    #[test]
    fn tool_result_roundtrips_image_content_block() {
        let msg = InMsg::ToolResult {
            session: SessionId::new("s1"),
            request_id: "r1".into(),
            content: vec![ContentPart::image("image/png", "AAAA")],
        };
        let json = serde_json::to_string(&msg).unwrap();
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
        for scope in [
            ApprovalScope::Session,
            ApprovalScope::Always,
            ApprovalScope::SessionDir,
        ] {
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

    /// #486: `SessionDir` is an additive variant — pin its exact wire spelling
    /// (`snake_case`, matching the existing `session`/`always`) so a client
    /// implementation has a stable literal to target.
    #[test]
    fn approve_scope_session_dir_serializes_as_session_dir() {
        let json = serde_json::to_string(&ApprovalScope::SessionDir).unwrap();
        assert_eq!(json, "\"session_dir\"");
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
    fn skill_active_roundtrips_when_set_and_when_cleared() {
        let active = OutEvent::SkillActive {
            session: SessionId::new("s1"),
            seq: 3,
            skill_id: Some("commit".into()),
            allowed_tools: Some(vec!["bash".into(), "read".into()]),
        };
        let json = serde_json::to_string(&active).unwrap();
        let back: OutEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(active, back);

        let cleared = OutEvent::SkillActive {
            session: SessionId::new("s1"),
            seq: 4,
            skill_id: None,
            allowed_tools: None,
        };
        let json = serde_json::to_string(&cleared).unwrap();
        let back: OutEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(cleared, back);
        assert_eq!(cleared.session(), Some(&SessionId::new("s1")));
        assert_eq!(cleared.seq(), Some(4));
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
            assert_eq!(back.seq(), Some(5));
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
        assert_eq!(back.seq(), Some(7));
        assert_eq!(back.session(), Some(&SessionId::new("s1")));
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
    fn user_question_roundtrips_with_multiple_questions() {
        let ev = OutEvent::UserQuestion {
            session: SessionId::new("s1"),
            seq: 4,
            request_id: "q1".into(),
            questions: Questions(vec![
                Question {
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
                    multi_select: false,
                },
                Question {
                    question: "Which regions?".into(),
                    options: vec![
                        QuestionOption {
                            label: "us-east".into(),
                            description: None,
                        },
                        QuestionOption {
                            label: "eu-west".into(),
                            description: None,
                        },
                    ],
                    multi_select: true,
                },
            ]),
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains(r#""questions":[{"#), "{json}");
        let back: OutEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(ev, back);
    }

    #[test]
    fn user_question_deserializes_legacy_flat_shape() {
        let json = r#"{"kind":"user_question","session":"s1","seq":4,"request_id":"q1","question":"Which?","options":[{"label":"A"}],"allow_free_form":true}"#;
        let ev: OutEvent = serde_json::from_str(json).unwrap();
        match ev {
            OutEvent::UserQuestion { questions, .. } => {
                assert_eq!(questions.0.len(), 1);
                assert_eq!(questions.0[0].question, "Which?");
                assert_eq!(questions.0[0].options.len(), 1);
                assert!(!questions.0[0].multi_select);
            }
            _ => panic!("expected UserQuestion"),
        }
    }

    #[test]
    fn answer_question_roundtrips_as_tagged_json() {
        let msg = InMsg::answer_question(SessionId::new("s1"), "q1", vec![vec!["REST".into()]]);
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(
            json,
            r#"{"kind":"answer_question","session":"s1","request_id":"q1","answers":[["REST"]]}"#
        );
        let back: InMsg = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, back);
    }

    #[test]
    fn answer_question_deserializes_legacy_shape() {
        let json = r#"{"kind":"answer_question","session":"s1","request_id":"q1","answer":"REST"}"#;
        let msg: InMsg = serde_json::from_str(json).unwrap();
        match msg {
            InMsg::AnswerQuestion {
                answers, answer, ..
            } => {
                assert!(answers.is_empty());
                assert_eq!(answer, "REST");
            }
            _ => panic!("expected AnswerQuestion"),
        }
    }

    #[test]
    fn list_and_close_session_roundtrip_as_tagged_json() {
        let list = InMsg::ListSessions {
            correlation_id: "q1".into(),
        };
        assert_eq!(
            serde_json::to_string(&list).unwrap(),
            r#"{"kind":"list_sessions","correlation_id":"q1"}"#
        );
        assert_eq!(
            serde_json::from_str::<InMsg>(&serde_json::to_string(&list).unwrap()).unwrap(),
            list
        );
        // The supervisor-global list query names no session.
        assert_eq!(list.session(), None);

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
            correlation_id: "q1".into(),
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
        assert_eq!(ev.seq(), None, "SessionList is a lifecycle event, no seq");
        assert_eq!(ev.session(), None, "SessionList names no single session");
        let json = serde_json::to_string(&ev).unwrap();
        let back: OutEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(ev, back);
    }

    #[test]
    fn mcp_ops_are_session_less_and_only_the_query_is_wire_allowed() {
        let list = InMsg::McpList {
            correlation_id: "c1".into(),
        };
        let add = InMsg::McpAdd {
            name: "everything".into(),
            config: McpServerSpec {
                command: Some("npx".into()),
                args: vec![
                    "-y".into(),
                    "@modelcontextprotocol/server-everything".into(),
                ],
                env: HashMap::new(),
                url: None,
                headers: HashMap::new(),
                disabled: false,
            },
        };
        let remove = InMsg::McpRemove {
            name: "everything".into(),
        };
        for msg in [&list, &add, &remove] {
            assert_eq!(msg.session(), None, "{msg:?} is engine-global");
            let json = serde_json::to_string(msg).unwrap();
            let back: InMsg = serde_json::from_str(&json).unwrap();
            assert_eq!(msg, &back);
        }
        // The read-only query stays wire-allowed; the mutating pair is
        // trusted-only (#472, ADR-0124) — an unapproved `McpAdd` would spawn an
        // arbitrary local subprocess straight off the wire.
        assert!(list.wire_allowed());
        assert!(!add.wire_allowed(), "McpAdd must be wire-refused");
        assert!(!remove.wire_allowed(), "McpRemove must be wire-refused");
        assert_eq!(list.variant_name(), "mcp_list");
        assert_eq!(add.variant_name(), "mcp_add");
        assert_eq!(remove.variant_name(), "mcp_remove");
    }

    #[test]
    fn mcp_list_and_changed_events_roundtrip() {
        let list_ev = OutEvent::McpList {
            correlation_id: "c1".into(),
            servers: vec![McpServerStatus {
                name: "everything".into(),
                transport: "stdio".into(),
                connected: true,
                tools: vec!["mcp__everything__echo".into()],
                error: None,
            }],
        };
        assert_eq!(list_ev.seq(), None);
        assert_eq!(list_ev.session(), None);
        let json = serde_json::to_string(&list_ev).unwrap();
        assert_eq!(serde_json::from_str::<OutEvent>(&json).unwrap(), list_ev);

        let changed_ev = OutEvent::McpChanged {
            name: "everything".into(),
            action: McpAction::Added,
        };
        assert_eq!(changed_ev.seq(), None);
        assert_eq!(changed_ev.session(), None);
        let json = serde_json::to_string(&changed_ev).unwrap();
        assert_eq!(serde_json::from_str::<OutEvent>(&json).unwrap(), changed_ev);
        assert!(json.contains(r#""action":"added""#), "{json}");
    }

    #[test]
    fn replay_from_roundtrips_and_is_wire_allowed() {
        let msg = InMsg::ReplayFrom {
            session: SessionId::new("s1"),
            correlation_id: "c1".into(),
            after_seq: 12,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(
            json,
            r#"{"kind":"replay_from","session":"s1","correlation_id":"c1","after_seq":12}"#
        );
        assert_eq!(serde_json::from_str::<InMsg>(&json).unwrap(), msg);
        // A head-authored query the wire may forward, unlike the runtime-authored trio.
        assert!(msg.wire_allowed());
        assert_eq!(msg.session(), Some(&SessionId::new("s1")));
    }

    #[test]
    fn oneshot_roundtrips_defaults_args_and_is_wire_allowed() {
        let msg = InMsg::Oneshot {
            session: SessionId::new("s1"),
            op: "compact".into(),
            args: serde_json::json!({"instructions": "keep the file list"}),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(serde_json::from_str::<InMsg>(&json).unwrap(), msg);
        assert!(msg.wire_allowed());
        assert_eq!(msg.session(), Some(&SessionId::new("s1")));
        assert_eq!(msg.variant_name(), "oneshot");

        // `args` defaults to `Value::Null` when omitted on the wire.
        let bare = r#"{"kind":"oneshot","session":"s1","op":"compact"}"#;
        assert_eq!(
            serde_json::from_str::<InMsg>(bare).unwrap(),
            InMsg::Oneshot {
                session: SessionId::new("s1"),
                op: "compact".into(),
                args: serde_json::Value::Null,
            }
        );
    }

    #[test]
    fn compacted_event_roundtrips_and_carries_seq() {
        let ev = OutEvent::Compacted {
            session: SessionId::new("s1"),
            seq: 6,
            summary: "user asked for X, agent did Y".into(),
            kept: 0,
            auto: false,
        };
        let json = serde_json::to_string(&ev).unwrap();
        let back: OutEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(ev, back);
        assert_eq!(back.seq(), Some(6));
        assert_eq!(back.session(), Some(&SessionId::new("s1")));

        // `kept`/`auto` default to `0`/`false` when omitted on the wire (older
        // writer shape — every pre-#398 record is the manual copy-on-write path).
        let bare = r#"{"kind":"compacted","session":"s1","seq":6,"summary":"x"}"#;
        assert_eq!(
            serde_json::from_str::<OutEvent>(bare).unwrap(),
            OutEvent::Compacted {
                session: SessionId::new("s1"),
                seq: 6,
                summary: "x".into(),
                kept: 0,
                auto: false,
            }
        );
    }

    #[test]
    fn compacted_event_auto_flag_roundtrips() {
        let ev = OutEvent::Compacted {
            session: SessionId::new("s1"),
            seq: 6,
            summary: "auto-summarized on overflow".into(),
            kept: 2,
            auto: true,
        };
        let json = serde_json::to_string(&ev).unwrap();
        let back: OutEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(ev, back);
        match back {
            OutEvent::Compacted { auto, .. } => assert!(auto),
            other => panic!("expected Compacted, got {other:?}"),
        }
    }

    #[test]
    fn history_event_roundtrips_and_carries_no_seq() {
        let ev = OutEvent::History {
            correlation_id: "c1".into(),
            session: SessionId::new("s1"),
            events: vec![
                OutEvent::TextDelta {
                    session: SessionId::new("s1"),
                    seq: 3,
                    text: "hi".into(),
                },
                OutEvent::Done {
                    session: SessionId::new("s1"),
                    seq: 4,
                },
            ],
        };
        assert_eq!(ev.seq(), None, "History is a query reply, no seq");
        assert_eq!(ev.session(), Some(&SessionId::new("s1")));
        let json = serde_json::to_string(&ev).unwrap();
        let back: OutEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(ev, back);
    }

    #[test]
    fn waiting_answer_state_serializes_snake_case() {
        let ev = OutEvent::Status {
            session: SessionId::new("s1"),
            state: AgentState::WaitingAnswer,
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains(r#""state":"waiting_answer""#), "{json}");
        assert_eq!(serde_json::from_str::<OutEvent>(&json).unwrap(), ev);
    }

    #[test]
    fn agent_profile_detail_projects_the_wire_posture() {
        let profile = AgentProfile {
            name: "explore".into(),
            description: String::new(),
            mode: AgentMode::Subagent,
            system_prompt: "secret prompt body".into(),
            model: Some("glm-5.2".into()),
            provider: None,
            permission: PermissionProfile::new(Permission::Deny).with("read", Permission::Allow),
            tools: Some(vec!["read".into(), "grep".into()]),
            disallowed_tools: vec!["edit".into()],
            can_spawn: None,
            spawnable_agents: None,
            sandbox: None,
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
        assert_eq!(split_rule_key("bash"), ("bash", RuleScope::None));
        assert_eq!(split_rule_key("*"), ("*", RuleScope::None));
        assert_eq!(
            split_rule_key("bash(git *)"),
            ("bash", RuleScope::Arg("git *"))
        );
        assert_eq!(
            split_rule_key("edit(src/*)"),
            ("edit", RuleScope::Arg("src/*"))
        );
        assert_eq!(
            split_rule_key("bash{/tmp/*}"),
            ("bash", RuleScope::Workdir("/tmp/*"))
        );
        // A malformed key with no closing paren/brace stays a plain name.
        assert_eq!(split_rule_key("bash(oops"), ("bash(oops", RuleScope::None));
        assert_eq!(split_rule_key("bash{oops"), ("bash{oops", RuleScope::None));
    }

    #[test]
    fn workdir_scoped_rule_matches_the_bash_call_workdir() {
        // A workdir-scoped rule is independent of the command-scoped rule and
        // composes with it via ordinary last-match-wins.
        let p = PermissionProfile::new(Permission::Allow)
            .with("bash", Permission::Ask)
            .with("bash{/tmp/*}", Permission::Allow)
            .with("bash{/etc/*}", Permission::Deny);
        assert_eq!(
            p.resolve_scoped("bash", Some("ls"), Some("/tmp/scratch")),
            Permission::Allow
        );
        assert_eq!(
            p.resolve_scoped("bash", Some("ls"), Some("/etc/cron.d")),
            Permission::Deny
        );
        // Falls through to the coarse rule for a workdir matching neither.
        assert_eq!(
            p.resolve_scoped("bash", Some("ls"), Some("/home/x")),
            Permission::Ask
        );
        // The plain `resolve` entry point never sees `workdir` (equivalent to
        // `workdir = None`), so a workdir-scoped rule never matches through it.
        assert_eq!(p.resolve("bash", Some("ls")), Permission::Ask);
    }

    fn masked_profile(tools: Option<Vec<&str>>, disallowed: Vec<&str>) -> AgentProfile {
        AgentProfile {
            name: "m".into(),
            description: String::new(),
            mode: AgentMode::Primary,
            system_prompt: String::new(),
            model: None,
            provider: None,
            permission: PermissionProfile::new(Permission::Allow),
            tools: tools.map(|v| v.into_iter().map(String::from).collect()),
            disallowed_tools: disallowed.into_iter().map(String::from).collect(),
            can_spawn: None,
            spawnable_agents: None,
            sandbox: None,
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
            provider: None,
            permission: PermissionProfile::new(Permission::Allow),
            tools: None,
            disallowed_tools: Vec::new(),
            can_spawn,
            spawnable_agents: spawnable_agents.map(|v| v.into_iter().map(String::from).collect()),
            sandbox: None,
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
