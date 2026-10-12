//! Discovery (ADR-0226): six plain, read-only catalog tools — `list_agents`,
//! `list_models`, `list_skills`, `load_skill`, `list_mcp`, `mcp_tools` — each
//! with no argument or one `name`, all `Capability::Control` (never graded,
//! the `poll` pattern), plus the `mcp {server, tool, args}` envelope that is
//! the only way to call an MCP tool. `load_skill` is a registry tool
//! (`crate::skills`); the other five and the envelope are runtime-owned
//! pseudo-tools whose specs live here.
//!
//! [`lists`] renders the static rosters, [`mcp_lists`] the MCP pair (which
//! may connect a server), [`sections`] the internal index the TUI `/tools`
//! view browses, [`pending`] the spawn sub-tree walk.

mod lists;
mod mcp_lists;
mod pending;
mod sections;

pub use lists::{agents_list, models_list, skills_list};
pub use mcp_lists::{list_mcp, mcp_tools, McpSources};
pub(crate) use pending::subtree_sessions;
pub use sections::{index_rows, IndexRow};

use entanglement_core::ToolSpec;
use serde_json::{json, Value};

use crate::tool_names::{
    LIST_AGENTS_TOOL, LIST_MCP_TOOL, LIST_MODELS_TOOL, LIST_SKILLS_TOOL, MCP_TOOL, MCP_TOOLS_TOOL,
};

/// A spec rendered as JSON — the same three fields a native `<tools>` entry
/// carries, no reformatting. Shared by `mcp_tools` and the arg-validate
/// decline so both show one rendering.
pub(crate) fn spec_to_json(spec: &ToolSpec) -> Value {
    json!({
        "name": spec.name,
        "description": spec.description,
        "schema": spec.schema,
    })
}

fn no_args(name: &str, description: &str) -> ToolSpec {
    ToolSpec::with_schema(
        name,
        description,
        json!({ "type": "object", "properties": {} }),
    )
}

pub fn list_agents_spec() -> ToolSpec {
    no_args(
        LIST_AGENTS_TOOL,
        "List the agents you can start with agent or message with agent_send: \
         name, description, and model pin of each.",
    )
}

pub fn list_models_spec() -> ToolSpec {
    no_args(
        LIST_MODELS_TOOL,
        "List the models you can run a sub-agent on (agent's model parameter): \
         provider/id, display name, and context window of each.",
    )
}

pub fn list_skills_spec() -> ToolSpec {
    no_args(
        LIST_SKILLS_TOOL,
        "List the skills you can load with load_skill: name and one-line \
         description of each.",
    )
}

pub fn list_mcp_spec() -> ToolSpec {
    no_args(
        LIST_MCP_TOOL,
        "List the MCP servers you can call through mcp: name, description, \
         state (connected / available / auth required) and tool names when \
         known. Use mcp_tools to see a server's tools with their schemas.",
    )
}

pub fn mcp_tools_spec() -> ToolSpec {
    ToolSpec::with_schema(
        MCP_TOOLS_TOOL,
        "Show every tool of one MCP server with its JSON schema and how to call \
         it through mcp.",
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "The server name, as list_mcp shows it."
                }
            },
            "required": ["name"]
        }),
    )
}

/// The `mcp {server, tool, args}` envelope. Core unwraps a call to it only
/// when this spec is in the round's advertised set, so its name and schema
/// are a contract with core (`entanglement_core::MCP_TOOL`).
pub fn mcp_spec() -> ToolSpec {
    ToolSpec::with_schema(
        MCP_TOOL,
        "Call a tool on an MCP server: server and tool are names from list_mcp \
         and mcp_tools, args the tool's arguments object.",
        json!({
            "type": "object",
            "properties": {
                "server": { "type": "string" },
                "tool": { "type": "string" },
                "args": { "type": "object" }
            },
            "required": ["server", "tool"]
        }),
    )
}

/// The runtime-owned pseudo-tool specs that dispatch by name in
/// [`crate::tool_runner`]'s interception ladder rather than living in the
/// [`crate::tools::ToolRegistry`] — so their schema exists only as one of
/// these pure functions. The single place that builds this list (ADR-0190's
/// original `poll`-omission bug was exactly this kind of drift).
pub fn runtime_owned_specs() -> Vec<ToolSpec> {
    #[allow(unused_mut)] // only mutated when the `rhai` feature is on
    let mut specs = vec![
        crate::plan_tasks::update_tasks_spec(),
        crate::ask_user::ask_user_spec(),
        crate::propose_plan::propose_plan_spec(),
        crate::request_mode::request_mode_spec(),
        crate::poll::poll_spec(),
        list_agents_spec(),
        list_models_spec(),
        list_skills_spec(),
        list_mcp_spec(),
        mcp_tools_spec(),
        mcp_spec(),
    ];
    #[cfg(feature = "rhai")]
    specs.push(crate::script::rhai_spec());
    specs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_discovery_tool_takes_no_argument_or_one_name() {
        for spec in [
            list_agents_spec(),
            list_models_spec(),
            list_skills_spec(),
            list_mcp_spec(),
            mcp_tools_spec(),
        ] {
            let props = spec.schema["properties"].as_object().unwrap();
            let keys: Vec<&String> = props.keys().collect();
            assert!(
                keys.is_empty() || keys == ["name"],
                "{}: {keys:?}",
                spec.name
            );
        }
    }

    #[test]
    fn the_mcp_envelope_requires_server_and_tool() {
        let spec = mcp_spec();
        assert_eq!(spec.name, "mcp");
        assert_eq!(spec.schema["required"], json!(["server", "tool"]));
        assert_eq!(spec.schema["properties"]["args"]["type"], "object");
    }

    #[test]
    fn runtime_owned_specs_names_the_expected_roster() {
        let names: Vec<String> = runtime_owned_specs().into_iter().map(|s| s.name).collect();
        for expected in [
            "update_tasks",
            "ask_user",
            "poll",
            "propose_plan",
            "request_mode",
            "list_agents",
            "list_models",
            "list_skills",
            "list_mcp",
            "mcp_tools",
            "mcp",
        ] {
            assert!(names.iter().any(|n| n == expected), "{expected}: {names:?}");
        }
        #[cfg(feature = "rhai")]
        assert!(names.contains(&"rhai".to_string()));
    }
}
