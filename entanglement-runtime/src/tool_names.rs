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
/// registered (a head always registers it, ADR-0195; a bespoke registry
/// may not); it stays in this mask/grade list unconditionally since
/// `BindingPolicy` grading is argument-independent of whether the engine
/// bound the function.
pub const BINDING_TOOLS: [&str; 7] = ["read", "glob", "grep", "edit", "write", "call", "bash"];

/// Tool name the plan agent calls to submit its plan (`content` XOR `path`)
/// for approval (#141, ADR-0042; #513, ADR-0145 — the sole plan-authorship
/// tool, `update_plan` removed; ADR-0207 §7 grades it by `Capability::Plan`
/// instead of per-profile membership).
pub const PROPOSE_PLAN_TOOL: &str = "propose_plan";

/// Tool name a session calls to ask the user to widen its permission mode
/// when a needed tool is blocked (ADR-0207 §10) — `Capability::Control`,
/// never graded, but still force-parks an approval like [`PROPOSE_PLAN_TOOL`]:
/// widening is real authority, only the user grants it.
pub const REQUEST_MODE_TOOL: &str = "request_mode";

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

/// Tool name the model calls to search the tool catalog beyond what's
/// advertised — MCP servers, skills, and unadvertised built-ins (#560,
/// ADR-0196 §4). Always-on and non-maskable, like [`DESCRIBE_TOOL`].
pub const EXPLORE_TOOL: &str = "explore";

/// Tool name the model calls to fetch a discovered tool's full schema, byte-
/// identical to a native `<tools>` entry (#560, ADR-0196 §4). Under
/// `client_side` encoding also registers the tool into the session's
/// advertised array. Always-on and non-maskable, like [`EXPLORE_TOOL`].
pub const DESCRIBE_TOOL: &str = "describe";

/// Reserved [`entanglement_core::ToolCall::name`] the OpenAI Responses client
/// emits for a streamed, client-executed `tool_search_call` output item (P7,
/// ADR-0196 §3) — re-exported here (via `entanglement_core`, itself
/// re-exporting `entanglement-provider`) so this file stays the executor's
/// one interception-name vocabulary. Never a real registered tool, and not
/// advertised as a [`entanglement_core::ToolSpec`] at all — the model calls
/// it because the wire itself declares a `tool_search` primitive whenever a
/// deferred tool exists, not because it saw a schema in the tools array.
/// `discover::run_tool_search` answers it by running the same explore+
/// describe lookup [`EXPLORE_TOOL`]/[`DESCRIBE_TOOL`] do, replying with a
/// `ContentPart::ToolSearchOutput` block instead of `describe`'s plain text.
/// Always-on and non-maskable, like [`EXPLORE_TOOL`]/[`DESCRIBE_TOOL`] — the
/// model didn't choose it from an advertised name, so masking it out would
/// strand the wire's own round-trip with no way to answer it.
pub use entanglement_core::TOOL_SEARCH_CALL_TOOL as RESPONSES_TOOL_SEARCH_TOOL;

/// The `invoke {name, args}` envelope a `native_first`/`invoke` client-side
/// session advertises (ADR-0204). Core unwraps it, so a `ToolExec` carrying
/// this name is one core refused to unwrap — never a registered tool, and
/// deliberately not in [`TOOL_SEARCH_KERNEL`] or the runtime-owned roster,
/// since only those two strategies advertise it.
pub use entanglement_core::INVOKE_TOOL;

/// Tools exempt from the #116 agent-mask / session-overlay / deny-entry check
/// entirely (ADR-0196 §4): read-only catalog introspection, never a
/// capability decision. Narrow and deliberate, like ADR-0190's original
/// (now-retired, subsumed by ADR-0192's universal dispatch mask) `poll`
/// exemption — adding an entry removes a profile-author control.
pub const NON_MASKABLE_TOOLS: &[&str] = &[EXPLORE_TOOL, DESCRIBE_TOOL, RESPONSES_TOOL_SEARCH_TOOL];

/// Whether `tool` is exempt from the dispatch-side mask entirely
/// ([`NON_MASKABLE_TOOLS`]).
pub fn is_non_maskable(tool: &str) -> bool {
    NON_MASKABLE_TOOLS.contains(&tool)
}

/// The `ToolSearch`-mode advertised set (ADR-0196 §2): the fixed
/// high-frequency host tools plus the runtime-owned roster plus the
/// discovery pair. `propose_plan`/`request_mode` join it too (ADR-0207 §7/
/// §9/§10): both must be advertised unconditionally and their schema never
/// varies by profile. `agent`/`agent_send` are unconditional too now (stage
/// 5b retires the old per-profile spawn roster, ADR-0040) — they ride the
/// plain shared `cfg.tool_specs` alongside everything else here, not a
/// separate per-profile table. Keeping the kernel set here (not just in
/// [`crate::discover::runtime_owned_specs`]) is what keeps them visible from
/// round one under the default `client_side` `tool_search` encoding, which
/// filters its pool down to exactly this list
/// ([`crate::tool_advertising::client_side_surface`]).
pub const TOOL_SEARCH_KERNEL: &[&str] = &[
    "read",
    "edit",
    "write",
    "apply_patch",
    "bash",
    POLL_TOOL,
    ASK_USER_TOOL,
    UPDATE_TASKS_TOOL,
    LOAD_SKILL_TOOL,
    EXPLORE_TOOL,
    DESCRIBE_TOOL,
    PROPOSE_PLAN_TOOL,
    REQUEST_MODE_TOOL,
    // `agent`/`agent_send` (stage 5b, ADR-0207 §6/§9): previously appended
    // unconditionally *after* this kernel filter from the now-retired
    // per-profile `profile_tool_specs` table, so they were always visible
    // regardless of discovery state. Now that they ride the plain
    // `cfg.tool_specs` pool like everything else, they must be named here
    // too or `client_side_surface`'s kernel filter would silently drop them
    // until an explicit `describe` — a functional regression, not just a
    // representational one, for the default `tool_search` mode.
    AGENT_TOOL,
    AGENT_SEND_TOOL,
];

/// Capability-level permission keys (#418, ADR-0114). ADR-0207 §3 replaced
/// this table's role as the *grading* vocabulary — a mode/ceiling rule's bare
/// class key (`read`/`write`/`exec`/`plan`/`control`) now matches by each
/// tool's own declared [`crate::capability::Capability`] (`mode::rules`'s
/// private `capability_class`), not this static membership list, and the
/// agent-frontmatter/config-ceiling expansion that used to consume it
/// (`agents::expand_capabilities`/`permission_from_value`) is retired along
/// with it. What's left: validating an MCP server's
/// config-side `capabilities:` annotation strings
/// ([`is_capability_name`], `entanglement_runtime::mcp::capability_index`)
/// and the `SessionDir` grant-widening read-triad check
/// ([`is_read_capability_member`], ADR-0126) — both orthogonal to mode/
/// ceiling grading.
pub const CAPABILITIES: &[(&str, &[&str])] = &[
    ("read", &["read", "grep", "glob"]),
    ("write", &["edit", "write", "apply_patch"]),
    ("call", &["bash"]),
];

/// Tools that used to belong to *every* ADR-0114 capability at once for the
/// now-retired frontmatter/ceiling expansion this fed
/// (`agents::expand_capabilities`) — kept only as a historical marker;
/// nothing reads it any more. Superseded by each tool's own declared
/// [`crate::capability::Capability`] set (`call`/`rhai` both carry
/// `Capability::Exec` directly, `rhai` several — see
/// `crate::capability::runtime_owned`), which needs no such special-cased
/// multi-membership list.
pub const MULTI_GROUP: &[&str] = &["call", "rhai"];

/// Whether `name` names a capability (`read`/`write`/`call`) — used by an MCP
/// server's config-side `capabilities` annotation (#426,
/// `entanglement_runtime::mcp::capability_index`), which validates its
/// declared capability strings against the same table.
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
/// false-positives here. Two consumers: [`is_recognized_mask_entry`], and —
/// via [`known_tool_names`] — the engine-free roster `skutter inspect agents`
/// grades a profile against (it has no `ToolRegistry` to ask). Not the
/// advertised roster (see `ToolRegistry::specs`/`names` for that).
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
    "glob_json",
    "grep_json",
    "mcp_enable",
    EXPLORE_TOOL,
    DESCRIBE_TOOL,
    RESPONSES_TOOL_SEARCH_TOOL,
];

/// The compile-time literal tool vocabulary ([`KNOWN_TOOL_NAMES`]), for a
/// surface that must grade a profile with no live registry in hand
/// (`skutter inspect agents`, `crate::tool_state`). Names only — whether any
/// given one is registered this run is a runtime fact this list deliberately
/// does not claim.
pub fn known_tool_names() -> &'static [&'static str] {
    KNOWN_TOOL_NAMES
}

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
        // Definition-driven sources (#560 P8): a config-declared endpoint
        // (`endpoint__<name>`) or a skill-declared tool (`skill__<skill>__
        // <name>` — endpoint ref, rhai-backed, or alias) isn't knowable from
        // a fixed compile-time list either, exactly like an MCP tool above.
        || entry.starts_with("endpoint__")
        || entry.starts_with("skill__")
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
