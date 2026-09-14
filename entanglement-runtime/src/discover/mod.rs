//! `explore`/`describe` — the ADR-0196 §4 discovery pair (#560).
//!
//! Two always-on, non-maskable, always-`Allow`, read-only internal tools
//! (the [`ADR-0190`](../../../docs/adr/0190-poll-is-always-on-non-maskable-internal-tool.md)
//! pattern extended from `poll` to these two — see
//! [`crate::tool_names::NON_MASKABLE_TOOLS`]) that let a `ToolSearch`-mode
//! session reach the rest of the registry: `explore(filter?)` is a terse,
//! always-live index over built-ins, MCP servers (three-state), and skills;
//! `describe(names)` answers with each name's full schema, byte-identical in
//! shape to a native `<tools>` entry, and — under `client_side` encoding —
//! joins the session's discovered set so the next round advertises it
//! directly.
//!
//! Split into [`explore`] (the index) and [`describe`] (the schema lookup)
//! to keep each half under the 400-line file cap; this module is just the
//! two `ToolSpec`s plus the one piece both the resolver (`main.rs`) and
//! `describe` need — the runtime-owned pseudo-tool specs that dispatch by
//! name rather than living in the [`crate::tools::ToolRegistry`].

mod describe;
mod explore;
mod tool_search;

pub use describe::run_describe;
pub(crate) use describe::spec_to_json;
pub use explore::run_explore;
pub use tool_search::run_tool_search;

use entanglement_core::ToolSpec;

use crate::tool_names::{DESCRIBE_TOOL, EXPLORE_TOOL};

/// The `explore` tool schema advertised to the model.
pub fn explore_spec() -> ToolSpec {
    ToolSpec::with_schema(
        EXPLORE_TOOL,
        "Search the tool catalog beyond what's advertised above: registered \
         built-ins not in your kernel (e.g. call, glob, grep, rhai, MCP \
         management), MCP servers (enabled ones list their tools; allowed-but-\
         unconnected ones show a hint to enable with mcp_enable — enabling \
         is never automatic), and skills (loaded with load_skill, not \
         describable). Always live — never stale, no need to re-check after \
         an mcp_enable. Call describe on a name from the results to get its \
         full schema and make it directly callable.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "filter": {
                    "type": "string",
                    "description": "Case-insensitive substring matched against \
                        each entry's name and description. Omit to list \
                        everything."
                }
            },
        }),
    )
}

/// The `describe` tool schema advertised to the model.
pub fn describe_spec() -> ToolSpec {
    ToolSpec::with_schema(
        DESCRIBE_TOOL,
        "Get the full schema for one or more tools found with explore — \
         byte-identical to how a natively-advertised tool's schema looks, so \
         you can call it immediately by its real name afterward. Unknown \
         names get a closest-match hint instead of a schema. Skills aren't \
         describable (load them with load_skill instead).",
        serde_json::json!({
            "type": "object",
            "properties": {
                "names": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Tool names to describe, as found via explore."
                }
            },
            "required": ["names"],
        }),
    )
}

/// The runtime-owned pseudo-tool specs that dispatch by name in
/// [`crate::tool_runner`]'s interception ladder rather than living in the
/// [`crate::tools::ToolRegistry`] — so their schema exists only as one of
/// these pure functions. The single place that builds this list: both the
/// `tool_spec_resolver` closure (`main.rs`) and `describe`'s by-name lookup
/// call it, replacing the vec each used to build separately (ADR-0190's
/// original `poll`-omission bug was exactly this kind of drift).
pub fn runtime_owned_specs() -> Vec<ToolSpec> {
    #[allow(unused_mut)] // only mutated when the `rhai` feature is on
    let mut specs = vec![
        crate::plan_tasks::update_tasks_spec(),
        crate::ask_user::ask_user_spec(),
        crate::poll::poll_spec(),
    ];
    #[cfg(feature = "rhai")]
    specs.push(crate::script::rhai_spec());
    specs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specs_carry_the_right_names_and_required_fields() {
        let explore = explore_spec();
        assert_eq!(explore.name, "explore");
        assert!(explore.schema["properties"]["filter"].is_object());

        let describe = describe_spec();
        assert_eq!(describe.name, "describe");
        assert_eq!(describe.schema["required"][0], "names");
    }

    #[test]
    fn runtime_owned_specs_names_the_expected_roster() {
        let names: Vec<String> = runtime_owned_specs().into_iter().map(|s| s.name).collect();
        assert!(names.contains(&"update_tasks".to_string()));
        assert!(names.contains(&"ask_user".to_string()));
        assert!(names.contains(&"poll".to_string()));
        #[cfg(feature = "rhai")]
        assert!(names.contains(&"rhai".to_string()));
    }
}
