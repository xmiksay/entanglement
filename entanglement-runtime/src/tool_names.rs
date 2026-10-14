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

/// The host tools bound into every `rhai` script under their own names (`call`
/// as `exec`) — every call through the dispatch seam, graded like a direct
/// call (ADR-0227). `bash` is *reachable* only when the host `bash` tool is
/// registered. The script-only `http` binding and the generated `mcp::`/
/// `skill::` namespaces are not in this list.
pub const BINDING_TOOLS: [&str; 7] = ["read", "glob", "grep", "edit", "write", "call", "bash"];

/// The rhai `http(method, url, #{headers, body})` binding's tool (ADR-0227):
/// registered but never advertised — reachable from a script only, graded as
/// `Exec` on `"{METHOD} {url}"`, with hidden `http_auth` credentials.
pub const HTTP_TOOL: &str = "http";

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

/// Tool name the model calls to list the registered agent roster — name,
/// description, and model pin per agent (ADR-0226). Replaces the spawn enum
/// the `agent` tool's own schema used to carry, so that spec is
/// session-invariant.
pub const LIST_AGENTS_TOOL: &str = "list_agents";

/// Tool name the model calls to list the active catalog's model ids with
/// display name and provider (ADR-0226) — what the `agent` tool's `model`
/// parameter description points at.
pub const LIST_MODELS_TOOL: &str = "list_models";

/// Tool name the model calls to list the skill index — name + one-line
/// description each (ADR-0226). Replaces the system prompt's tier-1 skill
/// roster.
pub const LIST_SKILLS_TOOL: &str = "list_skills";

/// Tool name the model calls to list every available MCP server — name,
/// description, state, and tool names when known (ADR-0226).
pub const LIST_MCP_TOOL: &str = "list_mcp";

/// Tool name the model calls to read one MCP server's tool catalog: every
/// tool with its JSON schema and one `mcp {"server", "tool", "args"}` call
/// line (ADR-0226). The MCP counterpart of [`LOAD_SKILL_TOOL`] — a read, no
/// activation semantics; the first read of a server in the process connects
/// it as an implementation detail.
pub const MCP_TOOLS_TOOL: &str = "mcp_tools";

/// The `mcp {server, tool, args}` envelope (ADR-0226) — the only way to call
/// an MCP tool. Advertised (a [`KERNEL`] member) but never a registered tool:
/// core unwraps it, so a `ToolExec` still carrying this name is one core
/// refused to unwrap (malformed, or a reserved inner name) and is declined.
pub use entanglement_core::MCP_TOOL;

/// The six ADR-0226 discovery tools: read-only catalog lookups, each with no
/// argument or one name (`load_skill` keeps its `skill_name`), all
/// `Capability::Control`, never graded (the `poll` pattern).
pub const DISCOVERY_TOOLS: &[&str] = &[
    LIST_AGENTS_TOOL,
    LIST_MODELS_TOOL,
    LIST_SKILLS_TOOL,
    LOAD_SKILL_TOOL,
    LIST_MCP_TOOL,
    MCP_TOOLS_TOOL,
];

/// The names an overlay entry or a mode/ceiling rule can spell but never
/// match (#843, #849): the six discovery tools and the bookkeeping Control
/// tools take a bypass before any overlay or rule is read (ADR-0226
/// Maskability, ADR-0232), core unwraps the `mcp` envelope to
/// `mcp__<server>__<tool>` before dispatch, and the script-only read variants
/// grade as the tool they ride on (`dispatch::grade::graded_name`).
/// `propose_plan` is graded, but only by its `plan` class — a rule naming it
/// is skipped by the matcher (`mode::rules`), so the class is the one gate.
/// The TUI's toggle roster leaves them out, `/enable`/`/disable` refuses
/// them, and a config rule or a wire overlay entry naming one warns
/// ([`non_maskable_reason`]). `agent`/`agent_send` are absent on purpose:
/// they are graded like any tool (ADR-0232).
pub const NON_MASKABLE_TOOLS: &[&str] = &[
    LIST_AGENTS_TOOL,
    LIST_MODELS_TOOL,
    LIST_SKILLS_TOOL,
    LOAD_SKILL_TOOL,
    LIST_MCP_TOOL,
    MCP_TOOLS_TOOL,
    MCP_TOOL,
    "read_raw",
    "glob_json",
    "grep_json",
    POLL_TOOL,
    ASK_USER_TOOL,
    UPDATE_TASKS_TOOL,
    REQUEST_MODE_TOOL,
    PROPOSE_PLAN_TOOL,
];

/// Whether `tool` is one of the [`NON_MASKABLE_TOOLS`].
pub fn is_non_maskable(tool: &str) -> bool {
    NON_MASKABLE_TOOLS.contains(&tool)
}

/// Why an overlay entry or rule naming `tool` can never take effect, and
/// what to name instead — `None` for any other name (globs included: a
/// pattern may still match real tools).
pub fn non_maskable_reason(tool: &str) -> Option<String> {
    if !is_non_maskable(tool) {
        return None;
    }
    Some(match tool {
        MCP_TOOL => "`mcp` is the MCP call envelope, graded under each inner \
                     `mcp__<server>__<tool>` name — name those, or remove the \
                     server from config"
            .to_string(),
        "read_raw" => "`read_raw` is graded as the `read` tool — name `read(<pattern>)` \
                       (the tool); a bare `read` rule is the whole read class \
                       (read/grep/glob)"
            .to_string(),
        "glob_json" => "`glob_json` is graded as `glob` — name `glob` instead".to_string(),
        "grep_json" => "`grep_json` is graded as `grep` — name `grep` instead".to_string(),
        POLL_TOOL | ASK_USER_TOOL | UPDATE_TASKS_TOOL => format!(
            "`{tool}` is session bookkeeping — it only collects or reports work already \
             authorized, so it is always available, not maskable (ADR-0232)"
        ),
        REQUEST_MODE_TOOL => "`request_mode` only asks the user to widen the mode — it always \
                              parks an approval and is refused in `auto`; always available, \
                              not maskable (ADR-0232)"
            .to_string(),
        PROPOSE_PLAN_TOOL => "`propose_plan` is gated by the `plan` capability class alone \
                              (allowed only in `plan` mode) — name `plan` instead (ADR-0232)"
            .to_string(),
        discovery => format!(
            "`{discovery}` is a read-only discovery tool — always available, \
             not maskable (ADR-0226)"
        ),
    })
}

/// The one advertised surface (ADR-0226; ADR-0196's `TOOL_SEARCH_KERNEL`,
/// renamed): the fixed host tools, the runtime-owned roster, the six
/// discovery tools and the `mcp` envelope — byte-identical on every wire and
/// snapshotted per session. Every model-facing tool must be named here or it
/// is unreachable: `agent`/`agent_send` and `propose_plan`/`request_mode`
/// ride the shared spec pool like everything else, so the filter keeps only
/// what is listed. Registered but hidden: `call`/`glob`/`grep` (script-only
/// rhai bindings, still graded under their names) and `read_raw`/
/// `glob_json`/`grep_json`, and `http` (ADR-0227).
pub const KERNEL: &[&str] = &[
    "read",
    "edit",
    "write",
    "apply_patch",
    "bash",
    POLL_TOOL,
    ASK_USER_TOOL,
    UPDATE_TASKS_TOOL,
    LOAD_SKILL_TOOL,
    PROPOSE_PLAN_TOOL,
    REQUEST_MODE_TOOL,
    AGENT_TOOL,
    AGENT_SEND_TOOL,
    RHAI_TOOL,
    "web_fetch",
    LIST_AGENTS_TOOL,
    LIST_MODELS_TOOL,
    LIST_SKILLS_TOOL,
    LIST_MCP_TOOL,
    MCP_TOOLS_TOOL,
    MCP_TOOL,
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
    "web_fetch",
    ASK_USER_TOOL,
    POLL_TOOL,
    PROPOSE_PLAN_TOOL,
    AGENT_TOOL,
    AGENT_SEND_TOOL,
    UPDATE_TASKS_TOOL,
    LOAD_SKILL_TOOL,
    LIST_AGENTS_TOOL,
    LIST_MODELS_TOOL,
    LIST_SKILLS_TOOL,
    LIST_MCP_TOOL,
    MCP_TOOLS_TOOL,
    MCP_TOOL,
    "read_raw",
    "glob_json",
    "grep_json",
    HTTP_TOOL,
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
/// `bash_output`/`agent_poll`/`agent_spawn` replaced by `poll`/`agent`, or
/// ADR-0226's retired discovery/envelope names) — worth a startup warning so a masked-out config doesn't
/// silently degrade (ADR-0161 "Config churn", ADR-0166).
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
        for gone in [
            "bash_output",
            "agent_poll",
            "agent_spawn",
            "raed",
            // ADR-0226 retirements.
            "explore",
            "describe",
            "invoke",
            "mcp_enable",
            "responses_tool_search",
        ] {
            assert!(!is_recognized_mask_entry(gone), "{gone} is retired");
        }
    }

    /// ADR-0226: the kernel is exactly the one advertised surface — the
    /// retired trio and the old envelope are out, `rhai`/`web_fetch` and the
    /// six discovery tools plus `mcp` are in.
    #[test]
    fn kernel_is_the_one_advertised_surface() {
        for gone in ["explore", "describe", "invoke", "mcp_enable"] {
            assert!(!KERNEL.contains(&gone), "{gone} left the kernel");
        }
        for present in [
            RHAI_TOOL,
            "web_fetch",
            LIST_AGENTS_TOOL,
            LIST_MODELS_TOOL,
            LIST_SKILLS_TOOL,
            LIST_MCP_TOOL,
            MCP_TOOLS_TOOL,
            MCP_TOOL,
        ] {
            assert!(KERNEL.contains(&present), "{present} joined the kernel");
        }
        // Hidden-but-registered, script-only: never advertised.
        for hidden in ["call", "glob", "grep", "read_raw", "glob_json", "grep_json"] {
            assert!(!KERNEL.contains(&hidden), "{hidden} is not model-facing");
        }
    }

    #[test]
    fn non_maskable_names_are_the_ones_dispatch_never_grades_by_name() {
        for tool in DISCOVERY_TOOLS
            .iter()
            .chain(&[MCP_TOOL, "read_raw", "glob_json", "grep_json"])
        {
            assert!(is_non_maskable(tool), "{tool} must be non-maskable");
            assert!(non_maskable_reason(tool).is_some(), "{tool} needs a reason");
        }
        for maskable in ["bash", "read", "glob", RHAI_TOOL, "mcp__gh__x", "mcp__*"] {
            assert!(non_maskable_reason(maskable).is_none(), "{maskable}");
        }
        let mcp = non_maskable_reason(MCP_TOOL).unwrap();
        assert!(mcp.contains("remove the server from config"), "{mcp}");
        let glob = non_maskable_reason("glob_json").unwrap();
        assert!(glob.contains("name `glob`"), "{glob}");
        let raw = non_maskable_reason("read_raw").unwrap();
        assert!(
            raw.contains("read(<pattern>)") && raw.contains("read class"),
            "{raw}"
        );
        let list = non_maskable_reason(LIST_MCP_TOOL).unwrap();
        assert!(list.contains("not maskable"), "{list}");
    }

    /// #849: the bookkeeping Control tools join the set, each with a reason;
    /// `agent`/`agent_send` stay maskable — they are graded (ADR-0232).
    #[test]
    fn bookkeeping_control_tools_are_non_maskable_but_spawning_is_not() {
        for tool in [POLL_TOOL, ASK_USER_TOOL, UPDATE_TASKS_TOOL] {
            let why = non_maskable_reason(tool).unwrap();
            assert!(why.contains("bookkeeping"), "{tool}: {why}");
        }
        let mode = non_maskable_reason(REQUEST_MODE_TOOL).unwrap();
        assert!(mode.contains("parks an approval"), "{mode}");
        let plan = non_maskable_reason(PROPOSE_PLAN_TOOL).unwrap();
        assert!(plan.contains("name `plan` instead"), "{plan}");
        for graded in [AGENT_TOOL, AGENT_SEND_TOOL] {
            assert!(!is_non_maskable(graded), "{graded} is graded");
        }
    }
}
