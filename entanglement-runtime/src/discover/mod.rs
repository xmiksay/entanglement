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
mod sections;
mod tool_search;

pub use describe::run_describe;
pub(crate) use describe::spec_to_json;
pub use explore::run_explore;
pub use sections::{index_rows, IndexRow};
pub use tool_search::run_tool_search;

use entanglement_core::{Discovery, ToolSpec};

use crate::tool_names::{DESCRIBE_TOOL, EXPLORE_TOOL, INVOKE_TOOL};

/// The `explore` tool schema advertised to the model. `discovery` is the
/// session's effective strategy (ADR-0204): only `append` makes a described
/// tool directly callable, so only it says so.
pub fn explore_spec(discovery: Discovery) -> ToolSpec {
    let callable = match discovery {
        Discovery::Append => " and make it directly callable",
        Discovery::NativeFirst | Discovery::Invoke => "",
    };
    ToolSpec::with_schema(
        EXPLORE_TOOL,
        format!(
            "Search the tool catalog beyond what's advertised above: registered \
             built-ins not in your kernel (e.g. call, glob, grep, rhai, MCP \
             management), MCP servers (enabled ones list their tools; allowed-but-\
             unconnected ones show a hint to enable with mcp_enable — enabling \
             is never automatic), endpoints, and skills (loaded with load_skill, \
             not describable). Always live — never stale, no need to re-check \
             after an mcp_enable. Results are grouped under a section header per \
             kind. Call describe on a name from the results to get its full \
             schema{callable}."
        ),
        serde_json::json!({
            "type": "object",
            "properties": {
                "filter": {
                    "type": "string",
                    "description": "Case-insensitive substring matched against \
                        each entry's name and description. Omit to list \
                        everything."
                },
                "kind": {
                    "type": "string",
                    "enum": ["tool", "mcp", "skill", "endpoint"],
                    "description": "Restrict the results to one section. \
                        Omit to list every kind."
                }
            },
        }),
    )
}

/// The `describe` tool schema advertised to the model, its call instruction
/// worded per the session's effective strategy (ADR-0204).
pub fn describe_spec(discovery: Discovery) -> ToolSpec {
    let then = match discovery {
        Discovery::Append => ", so you can call it immediately by its real name afterward",
        Discovery::NativeFirst => {
            "; afterward call it directly by its real name, or through invoke if you cannot"
        }
        Discovery::Invoke => "; afterward call it through invoke",
    };
    ToolSpec::with_schema(
        DESCRIBE_TOOL,
        format!(
            "Get the full schema for one or more tools found with explore — \
             byte-identical to how a natively-advertised tool's schema looks{then}. \
             Unknown names get a closest-match hint instead of a schema. Skills \
             aren't describable (load them with load_skill instead)."
        ),
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

/// The `invoke {name, args}` envelope (ADR-0204), advertised only by
/// `native_first`/`invoke` client-side sessions. Core unwraps a call to it
/// only when this spec is in the round's advertised set, so its name and
/// schema are a contract with core.
pub fn invoke_spec() -> ToolSpec {
    ToolSpec::with_schema(
        INVOKE_TOOL,
        "Call a tool loaded with describe that you cannot call directly: name is \
         the tool's name, args its arguments object.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" },
                "args": { "type": "object" }
            },
            "required": ["name"],
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
        let explore = explore_spec(Discovery::Append);
        assert_eq!(explore.name, "explore");
        assert!(explore.schema["properties"]["filter"].is_object());

        let describe = describe_spec(Discovery::Append);
        assert_eq!(describe.name, "describe");
        assert_eq!(describe.schema["required"][0], "names");

        let invoke = invoke_spec();
        assert_eq!(invoke.name, "invoke");
        assert_eq!(
            invoke.schema,
            serde_json::json!({
                "type": "object",
                "properties": {"name": {"type": "string"}, "args": {"type": "object"}},
                "required": ["name"]
            })
        );
    }

    #[test]
    fn append_wording_is_unchanged_and_fixed_array_strategies_point_at_invoke() {
        assert!(describe_spec(Discovery::Append)
            .description
            .contains("looks, so you can call it immediately by its real name afterward. Unknown"));
        assert!(explore_spec(Discovery::Append)
            .description
            .ends_with("full schema and make it directly callable."));
        assert!(describe_spec(Discovery::NativeFirst)
            .description
            .contains("directly by its real name, or through invoke if you cannot."));
        assert!(describe_spec(Discovery::Invoke)
            .description
            .contains("call it through invoke."));
        assert!(explore_spec(Discovery::Invoke)
            .description
            .ends_with("get its full schema."));
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
