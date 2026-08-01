//! `update_tasks` — a runtime state tool (#231, ADR-0049).
//!
//! Migrated out of core, where it was an engine built-in. It carries no engine
//! state: each call replaces the session's *display* task outline and the
//! runtime emits the corresponding [`OutEvent::TaskList`] snapshot. It rides
//! the ordinary `Allow`/`Ask`/`Deny` permission path with no special casing —
//! the #175 read-only-mutation bug is closed by that gate together with the
//! #116 tool mask (a read-only profile's allowlist omits it and its permission
//! denies it).
//!
//! `update_tasks` is general bookkeeping and rides the shared `tool_specs`, so
//! every unmasked profile advertises it (unlike plan authorship, which is
//! default-closed — see `propose_plan::specs_for`, #513, ADR-0145).
//!
//! Seq note (#157): the runtime emits the `TaskList` snapshot with a **fresh**
//! per-session seq minted from the session's shared counter via
//! [`Holly::emit_for_session`][entanglement_core::Holly] — it no longer reuses
//! the parked `ToolExec` seq — so the snapshot takes its own ordered place in the
//! content stream and `(session, seq)` stays unique across authored events.
//! [`state_event`] itself is a pure builder that stamps whatever seq the caller
//! mints.

use entanglement_core::{AgentProfile, OutEvent, SessionId, ToolSpec};

use crate::tool_names::UPDATE_TASKS_TOOL;

/// Whether `tool` is the state tool handled here — the runtime executor emits
/// an event + acks instead of dispatching to the host [`ToolRegistry`].
pub fn is_state_tool(tool: &str) -> bool {
    tool == UPDATE_TASKS_TOOL
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

/// Whether `profile` *explicitly* names `tool` in its `tools` allowlist. An
/// inherit-all (`tools: None`) profile does **not** count — the default-closed
/// gate `propose_plan::specs_for` uses for plan authorship (#231, ADR-0049;
/// #513, ADR-0145). Deliberately literal-exact even though mask entries are
/// wildcard patterns since #537 — a glob (`"*"`, `"propose_*"`) widens the mask
/// without silently granting plan authorship, exactly as `tools: None` already
/// doesn't.
pub fn explicitly_allowlists(profile: &AgentProfile, tool: &str) -> bool {
    matches!(&profile.tools, Some(list) if list.iter().any(|t| t == tool))
}

/// The snapshot `OutEvent` a state-tool call emits, parsed from its `content`
/// input at `seq`. `None` when `tool` is not a state tool.
pub fn state_event(session: &SessionId, seq: u64, tool: &str, input: &str) -> Option<OutEvent> {
    if tool != UPDATE_TASKS_TOOL {
        return None;
    }
    Some(OutEvent::TaskList {
        session: session.clone(),
        seq,
        content: parse_content(input),
    })
}

/// The tool-result acknowledgement folded back into the model's context.
pub fn ack(_tool: &str) -> String {
    "tasks updated".to_string()
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
            provider: None,
            permission: PermissionProfile::new(Permission::Allow),
            tools: tools.map(|v| v.into_iter().map(String::from).collect()),
            disallowed_tools: Vec::new(),
            can_spawn: None,
            spawnable_agents: None,
            sandbox: None,
        }
    }

    #[test]
    fn parse_content_reads_json_field_and_degrades_to_raw() {
        assert_eq!(parse_content(r#"{"content":"- [x] a"}"#), "- [x] a");
        assert_eq!(parse_content("bare text"), "bare text");
    }

    #[test]
    fn state_event_maps_update_tasks_to_tasklist() {
        let s = SessionId::new("s");
        assert!(matches!(
            state_event(&s, 4, UPDATE_TASKS_TOOL, r#"{"content":"t"}"#),
            Some(OutEvent::TaskList { content, seq: 4, .. }) if content == "t"
        ));
        assert!(state_event(&s, 5, "read", "{}").is_none());
        assert!(state_event(&s, 5, "propose_plan", "{}").is_none());
    }

    #[test]
    fn explicitly_allowlists_requires_exact_membership() {
        assert!(!explicitly_allowlists(&profile(None), "propose_plan"));
        assert!(!explicitly_allowlists(
            &profile(Some(vec!["read"])),
            "propose_plan"
        ));
        assert!(explicitly_allowlists(
            &profile(Some(vec!["read", "propose_plan"])),
            "propose_plan"
        ));
    }

    #[test]
    fn explicitly_allowlists_never_matches_a_glob() {
        // #537: a wildcard widens the #116 mask but is not an explicit plan
        // opt-in — plan authorship stays default-closed under `"*"`.
        assert!(!explicitly_allowlists(
            &profile(Some(vec!["*"])),
            "propose_plan"
        ));
        assert!(!explicitly_allowlists(
            &profile(Some(vec!["propose_*"])),
            "propose_plan"
        ));
    }
}
