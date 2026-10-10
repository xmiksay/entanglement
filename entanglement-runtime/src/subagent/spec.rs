//! The `agent` tool's advertised spec: a constant roster of every
//! registered agent (ADR-0207 §4/§6).

use entanglement_core::{Agent, AgentCatalog, ToolSpec};

use crate::tool_names::AGENT_TOOL;

/// The `agent`/`agent_send` tool specs, advertised unconditionally to every
/// session (ADR-0207 §4/§6): spawning is never *graded* — `agent`/
/// `agent_send` are `Capability::Control` — and any registered agent may be a
/// session root or a spawn target, so the roster is a **constant**: every
/// profile in `registry`, not a per-spawner subset (the old `can_spawn`/
/// `spawnable_agents` gates are retired, ADR-0040 superseded). Spawning is
/// bounded instead by the session's mode `max_depth`/`max_agents`
/// ([`SpawnGuard::try_spawn`]). Because this no longer varies by profile, it
/// joins the shared `cfg.tool_specs` like `ask_user`/`poll` rather than a
/// per-profile table — ADR-0207 §9: "the advertised tools array no longer
/// varies by agent or by mode".
pub fn agent_specs(registry: &AgentCatalog) -> Vec<ToolSpec> {
    let targets: Vec<&Agent> = registry.iter().collect();
    if targets.is_empty() {
        return Vec::new();
    }
    vec![agent_spec(&targets), crate::agent_send::agent_send_spec()]
}

/// The `agent` tool schema advertised to the model (#606, ADR-0161 §1 —
/// replaces the `agent`/`agent_spawn` split of ADR-0033). Blocks by default and
/// returns the sub-agent's final answer directly; `background: true` returns a
/// handle immediately instead, joined later with `poll`. The `targets` roster
/// is disclosed inline (#112): each spawnable agent's `name: description` is
/// listed in the tool description and the `agent` argument is constrained to
/// that set.
pub fn agent_spec(targets: &[&Agent]) -> ToolSpec {
    ToolSpec::with_schema(
        AGENT_TOOL,
        format!(
            "Delegate a focused subtask to a sub-agent. Blocks until it \
             finishes and returns its final answer directly — the one-call \
             path for a single delegation. Pass background: true to return an \
             agent_id handle immediately instead (it does not wait for the \
             sub-agent to finish), so you can launch several in a row and let \
             them run concurrently; collect each answer later by calling poll \
             with its agent_id.\n\n{}",
            roster(targets)
        ),
        agent_input_schema(targets),
    )
}

/// The `name: description` roster line block disclosed to the spawning model —
/// `description` is the only field of a definition a parent ever sees (#112).
/// Scoped to the profiles this spawner may target (#119).
fn roster(targets: &[&Agent]) -> String {
    let mut out = String::from("Available agents:");
    for p in targets {
        out.push_str(&format!("\n- {}: {}", p.name, p.description));
    }
    out
}

/// The `agent` tool's `{ agent, prompt, background? }` input schema. The
/// `agent` name is constrained to `targets` (an enum) so the model can only
/// pick a profile it is actually allowed to spawn (#119); `background` (#606)
/// flips the return shape from the blocking default to an immediate handle.
fn agent_input_schema(targets: &[&Agent]) -> serde_json::Value {
    let names: Vec<&str> = targets.iter().map(|p| p.name.as_str()).collect();
    serde_json::json!({
        "type": "object",
        "properties": {
            "agent": {
                "type": "string",
                "enum": names,
                "description": "Which agent profile (persona) to run the sub-agent under. Defaults to general. The child inherits this session's permission mode (or its agent's default mode when that is narrower)."
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
                    (see explore kind: models for the active roster). Omit \
                    to inherit the parent's model. An unknown id refuses the \
                    spawn and names the valid ids instead of silently \
                    falling back."
            }
        },
        "required": ["agent", "prompt"]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_specs_list_every_registered_agent() {
        // ADR-0207 §6/§9: spawning is unconditional and the roster is
        // constant — every registered agent is a valid target (stage 6a's
        // collapsed three-persona roster: `general`/`plan`/`debug`).
        let reg = crate::agents::built_in_registry().expect("built-in agents must parse");
        let specs = agent_specs(&reg);
        let names: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec![AGENT_TOOL, crate::tool_names::AGENT_SEND_TOOL]);
        let enum_names = specs[0].schema["properties"]["agent"]["enum"]
            .as_array()
            .unwrap();
        assert!(enum_names.iter().any(|n| n == "general"));
        assert!(enum_names.iter().any(|n| n == "debug"));
        assert!(enum_names.iter().any(|n| n == "plan"));
        // #560 P12, ADR-0207 §12: `model` rides the same spec every session
        // advertises unconditionally (`agent` is a `TOOL_SEARCH_KERNEL`
        // member) — verified here against the exact schema a session
        // receives, not a separate description.
        assert!(
            specs[0].schema["properties"]["model"].is_object(),
            "{:?}",
            specs[0].schema
        );
    }

    #[test]
    fn agent_specs_are_identical_regardless_of_which_profile_asks() {
        // The whole point of the constant roster (ADR-0207 §9): the array
        // must not depend on the caller's own profile, since `SetAgent` must
        // stay free of prompt-cache invalidation. `agent_specs` takes no
        // spawner argument at all now, so calling it twice against the same
        // registry is the only meaningful "regardless of who asks" check —
        // compare by (name, description, schema) since `ToolSpec` has no
        // `PartialEq`.
        let reg = crate::agents::built_in_registry().expect("built-in agents must parse");
        let a = agent_specs(&reg);
        let b = agent_specs(&reg);
        let project = |specs: &[ToolSpec]| -> Vec<(String, String, serde_json::Value)> {
            specs
                .iter()
                .map(|s| (s.name.clone(), s.description.clone(), s.schema.clone()))
                .collect()
        };
        assert_eq!(project(&a), project(&b));
    }
}
