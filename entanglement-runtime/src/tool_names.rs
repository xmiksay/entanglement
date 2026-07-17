//! The single home for the runtime-owned tool *names* (#205).
//!
//! These string literals were previously declared across seven modules
//! (`ask_user`, `agent_poll`, `script`, `propose_plan`, `subagent`,
//! `plan_tasks`, `skills::load_skill`) and matched by string equality in the
//! executor, the TUI, and `run`. A rename touched every file that spelled the
//! name out; centralizing them here makes a rename a one-file edit and gives
//! the executor's interception dispatch a single vocabulary to match against.

/// Tool name the model calls to ask the user a decision question (#90, ADR-0027).
pub const ASK_USER_TOOL: &str = "ask_user";

/// Tool name the model calls to await a launched sub-agent's answer (#89, ADR-0026).
pub const AGENT_POLL_TOOL: &str = "agent_poll";

/// Tool name the model calls to run a sandboxed script (ADR-0046).
pub const RHAI_TOOL: &str = "rhai";

/// The host functions bound into every `rhai` script — exactly the
/// root-contained quintet, so `rhai` is precisely as privileged as the
/// always-registered tools.
pub const BINDING_TOOLS: [&str; 5] = ["read", "glob", "grep", "edit", "write"];

/// Tool name the plan agent calls to finalize and submit its plan for approval
/// (#141, ADR-0042).
pub const PROPOSE_PLAN_TOOL: &str = "propose_plan";

/// Tool name the model calls to spawn a sub-agent (non-blocking; renamed from
/// `spawn_agent` in #120).
pub const AGENT_SPAWN_TOOL: &str = "agent_spawn";

/// Tool name the model calls to spawn a sub-agent and block for its answer (#120).
pub const AGENT_TOOL: &str = "agent";

/// Records the working strategy plan (plan authorship, advertised per-profile).
pub const UPDATE_PLAN_TOOL: &str = "update_plan";

/// Records the user-facing task checklist (shared, general bookkeeping).
pub const UPDATE_TASKS_TOOL: &str = "update_tasks";

/// Tool name the model calls to load a skill's full instructions (#124).
pub const LOAD_SKILL_TOOL: &str = "load_skill";

/// Capability-level permission keys (#418, ADR-0114) and the tools each fans
/// out to when a profile's `permission:` map uses the capability name instead
/// of spelling out every member tool — `("read", &["read", "grep", "glob"])`
/// means a bare `read: allow` grades all three read-only tools identically.
/// `call`'s member list is `bash` only: the literal `call` tool is
/// [`MULTI_GROUP`], not a single-group member — see there for why.
pub const CAPABILITIES: &[(&str, &[&str])] = &[
    ("read", &["read", "grep", "glob"]),
    ("write", &["edit", "write"]),
    ("call", &["bash"]),
];

/// Tools that belong to *every* capability at once, because they can
/// themselves read, write, or execute regardless of which capability key
/// graded them: the argv-exec `call` tool and the sandboxed `rhai` script
/// (bound to the root-contained quintet, see [`BINDING_TOOLS`]). Never
/// expanded by a bare/arg-scoped capability rule — instead, `permission_from_value`
/// grades them by the least-privileged bare `read`/`write`/`call` (+ literal
/// `rhai`) grade a profile sets, so restricting any one capability tightens
/// what these general-purpose tools may do.
pub const MULTI_GROUP: &[&str] = &["call", "rhai"];
