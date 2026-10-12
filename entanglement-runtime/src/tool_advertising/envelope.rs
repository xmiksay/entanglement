//! The runtime half of ADR-0226's `mcp {server, tool, args}` envelope. Core
//! unwraps a well-formed call before emitting `ToolExec`, so every gate sees
//! the inner `mcp__<server>__<tool>`. What is left here: a `ToolExec` still
//! named `mcp` (a malformed envelope core refused to unwrap), and the example
//! shape an arg-validate decline shows for an MCP tool.

use serde_json::{json, Value};

use crate::mcp::available::server_name_of;
use crate::tool_names::MCP_TOOL;
use crate::tools::ToolRegistry;

/// The decline for a tool name that isn't registered: the `mcp` envelope's
/// shape when the model tried the envelope and got it wrong, otherwise the
/// ordinary unknown-tool hint.
pub fn unknown_tool_reply(tools: &ToolRegistry, tool: &str) -> String {
    if tool != MCP_TOOL {
        return tools.unknown_tool_message(tool);
    }
    let schema =
        serde_json::to_string_pretty(&crate::discover::mcp_spec().schema).unwrap_or_default();
    format!(
        "malformed mcp call — expected {{\"server\": \"<server>\", \"tool\": \"<tool>\", \
         \"args\": {{...}}}}: `server` and `tool` are non-empty names from list_mcp/mcp_tools, \
         `args` the tool's arguments as a JSON object; `{MCP_TOOL}` itself cannot be a tool.\n\n\
         mcp schema:\n{schema}"
    )
}

/// The call that reaches MCP tool `tool` (`mcp__<server>__<tool>`) with
/// `args`, as the `mcp` envelope's arguments — `None` for any other name.
/// The server and tool halves are the registered (sanitized) spellings, which
/// core maps back to the same name.
pub fn mcp_example(tool: &str, args: Value) -> Option<Value> {
    let server = server_name_of(tool)?;
    let inner = tool.strip_prefix(&crate::mcp::tool::namespaced_tool_name(server, ""))?;
    Some(json!({ "server": server, "tool": inner, "args": args }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stray_mcp_call_explains_the_envelope_with_its_schema() {
        let out = unknown_tool_reply(&ToolRegistry::new(), "mcp");
        assert!(out.starts_with("malformed mcp call"), "{out}");
        assert!(out.contains(r#""required""#), "carries the schema: {out}");
    }

    #[test]
    fn any_other_unknown_name_gets_the_plain_hint() {
        let out = unknown_tool_reply(&ToolRegistry::new(), "nope");
        assert!(out.starts_with("unknown tool: `nope`"), "{out}");
    }

    #[test]
    fn mcp_example_splits_the_namespaced_name() {
        assert_eq!(
            mcp_example("mcp__gh__search_issues", json!({"q": "x"})),
            Some(json!({"server": "gh", "tool": "search_issues", "args": {"q": "x"}}))
        );
        assert_eq!(mcp_example("read", json!({})), None);
    }
}
