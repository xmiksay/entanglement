//! `update_plan` / `update_tasks` — runtime state tools (#231, ADR-0049).
//!
//! Migrated out of core, where they were engine built-ins. They carry no engine
//! state: each call replaces the session's *display* plan/task outline and the
//! runtime emits the corresponding [`OutEvent::Plan`]/[`OutEvent::TaskList`]
//! snapshot. They round-trip via `ToolExec`/`ToolResult` like every host tool
//! and resolve through the ordinary `Allow`/`Ask`/`Deny` permission path with no
//! plan-authority special casing — the #175 read-only-mutation bug is closed by
//! that gate together with the #116 tool mask (a read-only profile's allowlist
//! omits them and its permission denies them).
//!
//! Plan authorship is default-closed: `update_plan` (and `propose_plan`) is
//! advertised only to a profile that *explicitly* allowlists it (see
//! [`plan_specs_for`] / [`explicitly_allowlists`]), so an inherit-all profile
//! never gets it by accident — the replacement for the old `owns_plan` flag.
//! `update_tasks` is general bookkeeping and rides the shared `tool_specs`.
//!
//! Seq note: the runtime emits the `Plan`/`TaskList` snapshot reusing the
//! `ToolExec` seq (it has no handle on core's per-session counter). That is
//! monotonic and head-safe on the `Allow` path, since `ToolExec` itself carries
//! no head-visible seq bump. State tools are therefore expected to resolve to
//! `Allow` where advertised (a head dedupes an `Ask`-path snapshot against the
//! preceding `ToolRequest` at the same seq); the built-in profiles keep them
//! `Allow`.

use entanglement_core::{AgentProfile, OutEvent, SessionId, ToolSpec};

use crate::tool_names::{UPDATE_PLAN_TOOL, UPDATE_TASKS_TOOL};

/// Whether `tool` is one of the state tools handled here — the runtime executor
/// emits an event + acks instead of dispatching to the host [`ToolRegistry`].
pub fn is_state_tool(tool: &str) -> bool {
    tool == UPDATE_PLAN_TOOL || tool == UPDATE_TASKS_TOOL
}

/// `update_tasks` schema, registered into `EngineConfig::tool_specs` so every
/// unmasked profile advertises it; a read-only profile's allowlist omits it.
pub fn update_tasks_spec() -> ToolSpec {
    ToolSpec::with_schema(
        UPDATE_TASKS_TOOL,
        "Replace the task list (markdown). Shown to the user as progress info — \
         it is not fed back to you, so keep it a short checklist.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "content": {
                    "type": "string",
                    "description": "The full task list, in markdown — e.g. `- [ ]` / `- [x]` checkbox lines."
                }
            },
            "required": ["content"]
        }),
    )
}

/// `update_plan` schema — plan authorship, advertised only per-profile.
pub fn update_plan_spec() -> ToolSpec {
    ToolSpec::with_schema(
        UPDATE_PLAN_TOOL,
        "Replace the strategy plan (markdown prose).",
        serde_json::json!({
            "type": "object",
            "properties": {
                "content": {
                    "type": "string",
                    "description": "The full plan document, in markdown."
                }
            },
            "required": ["content"]
        }),
    )
}

/// The per-profile `update_plan` spec (#231): advertised only to a profile that
/// *explicitly* allowlists `update_plan` — the default-closed plan-authority
/// gate replacing the old core `owns_plan` flag. Empty otherwise. Appended to
/// `EngineConfig::profile_tool_specs` and filtered again through core's #116
/// mask, so the same allowlist entry keeps it advertised.
pub fn plan_specs_for(profile: &AgentProfile) -> Vec<ToolSpec> {
    if explicitly_allowlists(profile, UPDATE_PLAN_TOOL) {
        vec![update_plan_spec()]
    } else {
        Vec::new()
    }
}

/// Whether `profile` *explicitly* names `tool` in its `tools` allowlist. An
/// inherit-all (`tools: None`) profile does **not** count: that keeps plan
/// authorship default-closed for a profile that never opted in (#231, ADR-0049).
pub fn explicitly_allowlists(profile: &AgentProfile, tool: &str) -> bool {
    matches!(&profile.tools, Some(list) if list.iter().any(|t| t == tool))
}

/// The snapshot `OutEvent` a state-tool call emits, parsed from its `content`
/// input at `seq`. `None` when `tool` is not a state tool.
pub fn state_event(session: &SessionId, seq: u64, tool: &str, input: &str) -> Option<OutEvent> {
    let content = parse_content(input);
    match tool {
        UPDATE_PLAN_TOOL => Some(OutEvent::Plan {
            session: session.clone(),
            seq,
            content,
        }),
        UPDATE_TASKS_TOOL => Some(OutEvent::TaskList {
            session: session.clone(),
            seq,
            content,
        }),
        _ => None,
    }
}

/// The tool-result acknowledgement folded back into the model's context.
pub fn ack(tool: &str) -> String {
    match tool {
        UPDATE_PLAN_TOOL => "plan updated".to_string(),
        _ => "tasks updated".to_string(),
    }
}

/// Extract the `content` field from a state-tool input, degrading to the raw
/// string for a scripted/test backend that sends bare text (mirrors the
/// tolerance in `ask_user`/`propose_plan`).
pub fn parse_content(input: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(input) {
        Ok(v) => match v.get("content") {
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(other) if !other.is_null() => other.to_string(),
            _ => input.to_string(),
        },
        Err(_) => input.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use entanglement_core::{AgentMode, Permission, PermissionProfile};

    fn profile(tools: Option<Vec<&str>>) -> AgentProfile {
        AgentProfile {
            name: "x".into(),
            description: String::new(),
            mode: AgentMode::Primary,
            system_prompt: String::new(),
            model: None,
            permission: PermissionProfile::new(Permission::Allow),
            tools: tools.map(|v| v.into_iter().map(String::from).collect()),
            disallowed_tools: Vec::new(),
            can_spawn: None,
            spawnable_agents: None,
        }
    }

    #[test]
    fn parse_content_reads_json_field_and_degrades_to_raw() {
        assert_eq!(parse_content(r#"{"content":"- [x] a"}"#), "- [x] a");
        assert_eq!(parse_content("bare text"), "bare text");
    }

    #[test]
    fn state_event_maps_tool_to_variant() {
        let s = SessionId::new("s");
        assert!(matches!(
            state_event(&s, 3, UPDATE_PLAN_TOOL, r#"{"content":"p"}"#),
            Some(OutEvent::Plan { content, seq: 3, .. }) if content == "p"
        ));
        assert!(matches!(
            state_event(&s, 4, UPDATE_TASKS_TOOL, r#"{"content":"t"}"#),
            Some(OutEvent::TaskList { content, seq: 4, .. }) if content == "t"
        ));
        assert!(state_event(&s, 5, "read", "{}").is_none());
    }

    #[test]
    fn plan_authorship_is_default_closed() {
        // Inherit-all never opts in; only an explicit allowlist entry grants it.
        assert!(plan_specs_for(&profile(None)).is_empty());
        assert!(plan_specs_for(&profile(Some(vec!["read"]))).is_empty());
        let specs = plan_specs_for(&profile(Some(vec!["read", "update_plan"])));
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, UPDATE_PLAN_TOOL);
    }
}
