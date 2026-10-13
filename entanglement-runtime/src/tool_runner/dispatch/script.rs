//! What the dispatch seam knows about a script-issued call (ADR-0227): the
//! per-run approval cache, the detached flag, the run's deadline, and which
//! tools a script may never reach.

use std::collections::HashSet;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::capability::{self, Capability};
use crate::tool_names::{MCP_TOOL, RHAI_TOOL};
use crate::tools::ToolRegistry;

use super::Via;

/// One script run's state at the seam.
pub(crate) struct ScriptCall {
    /// Calls the user approved this run, keyed on tool + full input — an
    /// approved `edit` of one file never clears an edit of another.
    approved: Mutex<HashSet<String>>,
    /// A `background: true` run (#637): no session state transitions.
    pub detached: bool,
    posture: Posture,
    /// When the script's own wall-clock budget runs out.
    deadline: Instant,
}

/// How a script run's nested calls are cleared (ADR-0228).
// The lean build has no script engine, so only tests construct a posture.
#[cfg_attr(not(feature = "rhai"), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Posture {
    /// Graded exactly like direct calls; an `Ask` prompts per call.
    AskPerCommand,
    /// The user chose **Run all** on the launch card: every nested `Ask`
    /// passes, except a floored write (its own card).
    RunAll,
    /// A `rhai.trusted` script: like Run all, floored writes included.
    Trusted,
}

impl ScriptCall {
    #[cfg_attr(not(feature = "rhai"), allow(dead_code))]
    pub(crate) fn new(detached: bool, posture: Posture, deadline: Instant) -> Self {
        Self {
            approved: Mutex::default(),
            detached,
            posture,
            deadline,
        }
    }

    /// Force-allow this run's nested calls. Explicit denies still win.
    pub(crate) fn run_all(&self) -> bool {
        self.posture != Posture::AskPerCommand
    }

    pub(crate) fn trusted(&self) -> bool {
        self.posture == Posture::Trusted
    }

    pub(super) fn was_approved(&self, tool: &str, input: &str) -> bool {
        self.approved
            .lock()
            .is_ok_and(|set| set.contains(&cache_key(tool, input)))
    }

    pub(super) fn approve(&self, tool: &str, input: &str) {
        if let Ok(mut set) = self.approved.lock() {
            set.insert(cache_key(tool, input));
        }
    }
}

/// Tools never cut by the bridge clock (see [`Via::budget`]).
const UNBOUNDED: &[&str] = &["bash", "call", "write", "edit", "apply_patch"];

/// How much of the script's budget is left for it after a nested call is cut.
const GRACE: Duration = Duration::from_millis(300);

fn cache_key(tool: &str, input: &str) -> String {
    format!("{tool}\u{0}{input}")
}

impl Via<'_> {
    /// The bridge-level timeout a nested call runs under: the script's
    /// remaining budget less a grace margin, so the script still gets to
    /// handle the error before its own deadline ends it. Exempt: `bash`/
    /// `call`, whose own `timeout` parameter is already derived from the
    /// budget (#419), and the file-mutating tools, which a dropped future
    /// could cut between backup and write (ADR-0212).
    pub(super) fn budget(&self, tool: &str) -> Option<Duration> {
        match self {
            Via::Script(sc) if !UNBOUNDED.contains(&tool) => Some(
                sc.deadline
                    .saturating_duration_since(Instant::now())
                    .saturating_sub(GRACE),
            ),
            _ => None,
        }
    }
}

/// Why `tool` is out of a script's reach, or `None` when it may be called:
/// nested `rhai`, the `mcp` envelope (a script calls `mcp::<server>::<tool>`
/// instead), and every runtime-owned/`Control` tool — those orchestrate the
/// session, and a script is not the session.
pub(super) fn unreachable(tool: &str, tools: &ToolRegistry) -> Option<String> {
    let owned = tool == RHAI_TOOL
        || tool == MCP_TOOL
        || capability::runtime_owned(tool).is_some()
        || crate::plan_tasks::is_state_tool(tool)
        || crate::tool_names::DISCOVERY_TOOLS.contains(&tool)
        || capability::capability_of(tool, tools) == Some([Capability::Control].as_slice());
    owned.then(|| format!("`{tool}` cannot be called from a script"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cache_keys_on_the_full_input() {
        let sc = ScriptCall::new(false, Posture::AskPerCommand, Instant::now());
        sc.approve("edit", r#"{"path":"a"}"#);
        assert!(sc.was_approved("edit", r#"{"path":"a"}"#));
        assert!(!sc.was_approved("edit", r#"{"path":"b"}"#));
        assert!(!sc.was_approved("write", r#"{"path":"a"}"#));
    }

    #[test]
    fn runtime_owned_tools_and_envelopes_are_unreachable() {
        let tools = ToolRegistry::new();
        for tool in [
            "rhai",
            "mcp",
            "agent",
            "agent_send",
            "poll",
            "ask_user",
            "propose_plan",
            "request_mode",
            "update_tasks",
            "list_mcp",
            "mcp_tools",
            "load_skill",
        ] {
            assert!(unreachable(tool, &tools).is_some(), "{tool}");
        }
        for tool in ["read", "bash", "http", "mcp__gh__search"] {
            assert!(unreachable(tool, &tools).is_none(), "{tool}");
        }
    }

    #[test]
    fn bash_and_call_keep_their_own_timeout() {
        let sc = ScriptCall::new(
            false,
            Posture::AskPerCommand,
            Instant::now() + Duration::from_secs(5),
        );
        let via = Via::Script(&sc);
        for exempt in ["bash", "call", "write", "edit", "apply_patch"] {
            assert!(via.budget(exempt).is_none(), "{exempt}");
        }
        assert!(via
            .budget("read")
            .is_some_and(|d| d <= Duration::from_secs(5)));
        assert!(Via::Model.budget("read").is_none());
    }
}
