//! The capability vocabulary tools declare (#560, ADR-0207 §3) — stage 1 of
//! the permission-modes rewrite: **vocabulary only**, nothing consumes it
//! yet. `tool_names.rs`'s `CAPABILITIES`/`MULTI_GROUP` tables are untouched
//! and still drive every real permission decision until a later stage cuts
//! over.
//!
//! `Capability::Plan` and `Capability::Control` didn't exist under the old
//! `tool_names.rs` capability-key table (`read`/`write`/`call` only) —
//! plan authorship and runtime orchestration tools had no capability at all,
//! just a hardcoded name check. Naming them here is what lets a later stage
//! grade every tool uniformly instead of special-casing the two families.

#[cfg(feature = "rhai")]
use crate::tool_names::RHAI_TOOL;
use crate::tool_names::{
    AGENT_SEND_TOOL, AGENT_TOOL, ASK_USER_TOOL, DESCRIBE_TOOL, EXPLORE_TOOL, POLL_TOOL,
    PROPOSE_PLAN_TOOL, RESPONSES_TOOL_SEARCH_TOOL, UPDATE_TASKS_TOOL,
};
use crate::tools::ToolRegistry;

/// What a tool does to the host or session, independent of *which* tool does
/// it. `Copy` because a `&'static [Capability]` slice is cheap to hand
/// around and every call site just wants to inspect it, never own it.
#[derive(Copy, Clone, Eq, PartialEq, Debug, serde::Serialize, serde::Deserialize)]
pub enum Capability {
    Read,
    Write,
    Exec,
    Plan,
    Control,
}

/// Capabilities for the runtime-owned pseudo-tools `tool_runner::Intercept::classify`
/// routes before they ever reach the [`ToolRegistry`] — they have no `impl Tool`
/// to ask, so this is the only place their capability can live. `rhai` is the
/// one multi-capability member: a sandboxed script binds the read/write/exec
/// quintet-plus-exec pair (`tool_names::BINDING_TOOLS`), so it can do all
/// three; `propose_plan` is `Plan`, not `Control`, because plan authorship is
/// itself the thing a mode grants or refuses (ADR-0207 §7) — everything else
/// here is pure session/tool orchestration that reads, writes or executes
/// nothing on its own.
pub fn runtime_owned(name: &str) -> Option<&'static [Capability]> {
    #[cfg(feature = "rhai")]
    if name == RHAI_TOOL {
        return Some(&[Capability::Read, Capability::Write, Capability::Exec]);
    }
    match name {
        AGENT_TOOL
        | AGENT_SEND_TOOL
        | POLL_TOOL
        | ASK_USER_TOOL
        | EXPLORE_TOOL
        | DESCRIBE_TOOL
        | RESPONSES_TOOL_SEARCH_TOOL
        | UPDATE_TASKS_TOOL => Some(&[Capability::Control]),
        PROPOSE_PLAN_TOOL => Some(&[Capability::Plan]),
        _ => None,
    }
}

/// Resolve `name`'s capability: the runtime-owned table first (it has no
/// registry entry to ask), then the registered tool's own
/// [`Tool::capabilities`]. `None` means `name` is neither — an unknown tool,
/// which every real caller already handles as a decline/error on its own
/// path; this function doesn't guess one.
pub fn capability_of(name: &str, registry: &ToolRegistry) -> Option<&'static [Capability]> {
    runtime_owned(name).or_else(|| registry.get(name).map(|tool| tool.capabilities()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::Tool;

    #[test]
    fn runtime_owned_covers_every_intercepted_name() {
        for name in [
            AGENT_TOOL,
            AGENT_SEND_TOOL,
            POLL_TOOL,
            ASK_USER_TOOL,
            EXPLORE_TOOL,
            DESCRIBE_TOOL,
            RESPONSES_TOOL_SEARCH_TOOL,
            UPDATE_TASKS_TOOL,
        ] {
            assert_eq!(
                runtime_owned(name),
                Some([Capability::Control].as_slice()),
                "{name} should be Control"
            );
        }
    }

    #[test]
    fn propose_plan_is_plan_not_control() {
        assert_eq!(
            runtime_owned(PROPOSE_PLAN_TOOL),
            Some([Capability::Plan].as_slice())
        );
    }

    #[cfg(feature = "rhai")]
    #[test]
    fn rhai_is_multi_capability() {
        assert_eq!(
            runtime_owned(RHAI_TOOL),
            Some([Capability::Read, Capability::Write, Capability::Exec].as_slice())
        );
    }

    #[test]
    fn unknown_name_is_not_runtime_owned() {
        assert_eq!(runtime_owned("not_a_real_tool"), None);
    }

    #[test]
    fn capability_of_falls_back_to_the_registry() {
        struct Echo;
        #[async_trait::async_trait]
        impl Tool for Echo {
            fn name(&self) -> std::borrow::Cow<'static, str> {
                std::borrow::Cow::Borrowed("echo")
            }
            async fn run(&self, input: &str) -> anyhow::Result<String> {
                Ok(input.to_string())
            }
            fn capabilities(&self) -> &'static [Capability] {
                &[Capability::Read]
            }
        }
        let mut registry = ToolRegistry::new();
        registry.register(Echo);
        assert_eq!(
            capability_of("echo", &registry),
            Some([Capability::Read].as_slice())
        );
        assert_eq!(capability_of("does_not_exist", &registry), None);
    }
}
