//! The `agent` tool's advertised spec (ADR-0207 §4/§6), session-invariant
//! since ADR-0226: no roster, no enum — `list_agents` carries the roster.

use entanglement_core::ToolSpec;

use crate::tool_names::AGENT_TOOL;

/// The `agent`/`agent_send` tool specs, advertised unconditionally to every
/// session. Spawning is never *graded* (`Capability::Control`) and any
/// registered agent may be a target, bounded by the session's mode
/// `max_depth`/`max_agents`. The spec names no agent at all: a roster here
/// would change the cached `tools` array whenever an agent definition is
/// added or edited, so `list_agents` answers "who can I start" instead and an
/// unknown name is refused at spawn with the valid ones listed.
pub fn agent_specs() -> Vec<ToolSpec> {
    vec![agent_spec(), crate::agent_send::agent_send_spec()]
}

/// The `agent` tool schema advertised to the model (#606, ADR-0161 §1).
/// Blocks by default and returns the sub-agent's final answer directly;
/// `background: true` returns a handle immediately instead, joined later with
/// `poll`.
pub fn agent_spec() -> ToolSpec {
    ToolSpec::with_schema(
        AGENT_TOOL,
        "Delegate a focused subtask to a sub-agent. Blocks until it finishes \
         and returns its final answer directly — the one-call path for a \
         single delegation. Pass background: true to return an agent_id handle \
         immediately instead (it does not wait for the sub-agent to finish), \
         so you can launch several in a row and let them run concurrently; \
         collect each answer later by calling poll with its agent_id. \
         list_agents shows which agents exist.",
        agent_input_schema(),
    )
}

/// The `agent` tool's `{ agent, prompt, background?, model? }` input schema.
fn agent_input_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "agent": {
                "type": "string",
                "description": "Which agent profile (persona) to run the sub-agent under, by name from list_agents. Defaults to general. The child inherits this session's permission mode (or its agent's default mode when that is narrower)."
            },
            "prompt": {
                "type": "string",
                "description": "The task or question for the sub-agent to work on."
            },
            "background": {
                "type": "boolean",
                "description": "Return an agent_id handle immediately instead of \
                    waiting for the sub-agent's answer. Poll the handle with \
                    `poll` to collect it once it's done. Default false (blocks \
                    until the sub-agent finishes)."
            },
            "model": {
                "type": "string",
                "description": "Catalog model id to run the sub-agent on \
                    (list_models shows them). Omit to inherit the parent's \
                    model. An unknown id refuses the spawn and names the valid \
                    ids instead of silently falling back."
            }
        },
        "required": ["agent", "prompt"]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_specs_are_agent_and_agent_send() {
        let names: Vec<String> = agent_specs().into_iter().map(|s| s.name).collect();
        assert_eq!(names, vec![AGENT_TOOL, crate::tool_names::AGENT_SEND_TOOL]);
    }

    /// ADR-0226: the spec is session-invariant — no agent name appears in it,
    /// so adding or editing a definition never moves the cached `tools` array.
    #[test]
    fn the_agent_spec_names_no_agent_and_carries_no_enum() {
        let spec = agent_spec();
        let agent = &spec.schema["properties"]["agent"];
        assert_eq!(agent["type"], "string");
        assert!(agent.get("enum").is_none(), "{agent}");
        for name in ["general", "plan", "debug"] {
            assert!(
                !spec.description.contains(&format!("- {name}:")),
                "{}",
                spec.description
            );
        }
        assert!(spec.schema["properties"]["model"]["description"]
            .as_str()
            .unwrap()
            .contains("list_models"));
    }
}
