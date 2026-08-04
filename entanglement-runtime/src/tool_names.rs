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

/// Tool name the model calls to await a background `bash` job or a launched
/// sub-agent (#605, ADR-0161) — replaces `bash_output`/`agent_poll` outright,
/// no aliases.
pub const POLL_TOOL: &str = "poll";

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

/// Tool name the plan agent calls to submit its plan (`content` XOR `path`)
/// for approval (#141, ADR-0042; #513, ADR-0145 — the sole plan-authorship
/// tool, `update_plan` removed).
pub const PROPOSE_PLAN_TOOL: &str = "propose_plan";

/// Tool name the model calls to spawn a sub-agent — blocks for its answer by
/// default; `background: true` returns a handle immediately instead, joined
/// later with `poll` (#120; #606, ADR-0161 §1 — replaces the separate
/// `agent_spawn` tool it was renamed from).
pub const AGENT_TOOL: &str = "agent";

/// Tool name the model calls to send a follow-up prompt to a sub-agent it
/// already launched — steer one still working, follow up with one that
/// finished, or re-engage a `propose_plan` build child for another round
/// (#609, ADR-0162). Blocks for the child's next answer by default, exactly
/// like [`AGENT_TOOL`]; `background: true` returns immediately, joined later
/// with [`POLL_TOOL`].
pub const AGENT_SEND_TOOL: &str = "agent_send";

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

/// The compile-time vocabulary of literal tool names a mask entry or
/// permission rule key can spell out (#623): the root-contained quintet plus
/// `apply_patch`, the exec pair, [`RHAI_TOOL`], and every runtime-owned tool.
/// Deliberately independent of what's *actually registered* this run —
/// `bash`/`rhai` are env/feature-gated and MCP tools connect after profiles
/// load — so a config naming a real but currently-inactive tool never
/// false-positives here. Exists solely for [`is_recognized_mask_entry`]; not
/// the advertised roster (see `ToolRegistry::specs`/`names` for that).
const KNOWN_TOOL_NAMES: &[&str] = &[
    "read",
    "glob",
    "grep",
    "edit",
    "write",
    "apply_patch",
    "bash",
    "call",
    RHAI_TOOL,
    ASK_USER_TOOL,
    POLL_TOOL,
    PROPOSE_PLAN_TOOL,
    AGENT_TOOL,
    AGENT_SEND_TOOL,
    UPDATE_TASKS_TOOL,
    LOAD_SKILL_TOOL,
    "read_raw",
    "mcp_enable",
];

/// Whether `entry` — one item from an agent's `tools:`/`disallowed_tools:`
/// mask, or the tool part of a `permission:` rule key — names something a
/// stale-config check can vouch for (#623): a known literal tool name
/// ([`KNOWN_TOOL_NAMES`]), a capability key ([`is_capability_name`]), a
/// `*`/`?` wildcard pattern (ADR-0148 — matched dynamically, so it can't be
/// checked against a fixed list), or an MCP tool (`mcp__<server>__<tool>`,
/// unknowable until the server connects, #426). Anything else is very likely
/// a stale or typo'd name — e.g. a tool retired by a rename (#605/#606:
/// `bash_output`/`agent_poll`/`agent_spawn` replaced by `poll`/`agent`) —
/// worth a startup warning so a masked-out config doesn't silently degrade
/// (ADR-0161 "Config churn", ADR-0166).
pub fn is_recognized_mask_entry(entry: &str) -> bool {
    entry.contains('*')
        || entry.contains('?')
        || entry.starts_with("mcp__")
        || is_capability_name(entry)
        || KNOWN_TOOL_NAMES.contains(&entry)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_known_literal_tool_names() {
        for name in KNOWN_TOOL_NAMES {
            assert!(is_recognized_mask_entry(name), "{name} should be known");
        }
    }

    #[test]
    fn recognizes_capabilities_globs_and_mcp_tools() {
        assert!(is_recognized_mask_entry("read"));
        assert!(is_recognized_mask_entry("write"));
        assert!(is_recognized_mask_entry("call"));
        assert!(is_recognized_mask_entry("*"));
        assert!(is_recognized_mask_entry("mcp__*"));
        assert!(is_recognized_mask_entry("mcp__docs__search"));
        assert!(is_recognized_mask_entry("bash(git *)"));
    }

    #[test]
    fn flags_removed_and_typo_d_tool_names() {
        assert!(!is_recognized_mask_entry("bash_output"));
        assert!(!is_recognized_mask_entry("agent_poll"));
        assert!(!is_recognized_mask_entry("agent_spawn"));
        assert!(!is_recognized_mask_entry("raed"));
    }
}
