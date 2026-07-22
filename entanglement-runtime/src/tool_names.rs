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

/// Tool name the model calls to run a sandboxed script (ADR-0046, exec
/// bindings added by ADR-0115).
pub const RHAI_TOOL: &str = "rhai";

/// The host functions bound into every `rhai` script — the original
/// root-contained quintet (not the full `host::host_tools` sextet — `apply_patch`
/// has no rhai binding yet, #455) plus permission-gated process-exec
/// (`call`/`bash`, ADR-0115 amending ADR-0046) — so `rhai` is precisely as
/// privileged as the always-registered tools it does bind. `bash` is only
/// ever *reachable*, not just masked, when the host `bash` tool itself is
/// registered (`ENTANGLEMENT_ENABLE_BASH`); it stays in this mask/grade list
/// unconditionally since `BindingPolicy` grading is argument-independent of
/// whether the engine bound the function.
pub const BINDING_TOOLS: [&str; 7] = ["read", "glob", "grep", "edit", "write", "call", "bash"];

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
/// [`MULTI_GROUP`], not a single-group member — see there for why. This table
/// is the fixed, compile-time built-in membership only — an external MCP tool
/// (`mcp__<server>__<tool>`) is never a member here, since it isn't
/// self-describing; a bare capability key additionally fans out to whatever an
/// MCP server's config-side `capabilities:` annotation maps to it (#426,
/// `entanglement_runtime::mcp::capability_index`), a *data-driven* extension
/// of this same table applied alongside it in
/// `agents::expand_capabilities`.
pub const CAPABILITIES: &[(&str, &[&str])] = &[
    ("read", &["read", "grep", "glob"]),
    ("write", &["edit", "write", "apply_patch"]),
    ("call", &["bash"]),
];

/// Tools that belong to *every* capability at once, because they can
/// themselves read, write, or execute regardless of which capability key
/// graded them: the argv-exec `call` tool and the sandboxed `rhai` script
/// (bound to the quintet plus `call`/`bash`, see [`BINDING_TOOLS`]). Never
/// expanded by a bare/arg-scoped capability rule — instead, `permission_from_value`
/// grades them by the least-privileged bare `read`/`write`/`call` (+ literal
/// `rhai`) grade a profile sets, so restricting any one capability tightens
/// what these general-purpose tools may do.
pub const MULTI_GROUP: &[&str] = &["call", "rhai"];

/// Whether `name` names a capability (`read`/`write`/`call`) — shared by the
/// frontmatter/ceiling expansion above and by an MCP server's config-side
/// `capabilities` annotation (#426, `entanglement_runtime::mcp::capability_index`),
/// which validates its declared capability strings against the same table.
pub fn is_capability_name(name: &str) -> bool {
    CAPABILITIES.iter().any(|(n, _)| *n == name)
}

/// Whether `tool` is a member of the `read` capability (`read`/`grep`/`glob`,
/// #418) — the read-only triad eligible for `ApprovalScope::SessionDir`'s
/// directory-prefix widening (#486, ADR-0126). Shared by the grant store
/// (`grants::is_granted`/`record`) and the TUI's `[d]` approval-mode key gate
/// (`tui/event_loop.rs`) and footer (`tui/transcript.rs`) so the "is this tool
/// read-like" check can never drift from the capability table above.
pub fn is_read_capability_member(tool: &str) -> bool {
    CAPABILITIES
        .iter()
        .find(|(name, _)| *name == "read")
        .is_some_and(|(_, members)| members.contains(&tool))
}
