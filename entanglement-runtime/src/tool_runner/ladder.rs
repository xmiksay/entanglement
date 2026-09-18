//! The interception ladder (ADR-0207 §8's "the mask machinery is deleted" —
//! every route below grades from the session's permission mode instead):
//! [`Intercept::classify`] routes a `ToolExec` by tool name, and
//! [`route_tool_exec`] is the thin dispatcher that hands each route to its
//! handler — [`orchestration`] for the routes that bypass permission
//! entirely (`Capability::Control`, or a tool with its own unconditional
//! semantics) and [`graded`] for the two that resolve a grade first. Split
//! out of `tool_runner.rs` (issue #451) — the executor loop there folds
//! lifecycle events and calls into this module once per `ToolExec`;
//! everything about *which* handler a tool name reaches, and how that
//! handler is launched, lives in this module tree.
//!
//! [`LadderCtx`] bundles the executor's long-lived, cheaply-clonable shared
//! state (constructed once before the loop starts) so a handler takes one
//! reference instead of two dozen positional parameters. Only `spawn_guard`
//! (mutated by `SpawnGuard::try_spawn`) and `overlays` (mutated elsewhere in
//! the loop on `ModeChanged`/`ToolOverlayChanged`) stay outside it, passed
//! explicitly — both are loop-owned state a handler only ever reads
//! (`overlays`) or synchronously updates (`spawn_guard`), never holds across
//! an `.await`.

mod graded;
mod orchestration;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

use entanglement_core::{AgentCatalog, Catalog, Holly, PermissionProfile, SessionId, ToolEnvelope};

use crate::agent_registry::AgentRegistry;
use crate::cancel::CancelRegistry;
use crate::hooks::Hooks;
use crate::mcp::{ActiveServers, AvailableMcp};
use crate::mode::ModeTable;
use crate::plan_files::PlanFileRegistry;
use crate::policy::{GrantStore, PermissionResolver};
use crate::questions::OpenQuestions;
use crate::skills::SkillRegistry;
use crate::subagent::SpawnGuard;
use crate::tool_advertising::SharedAdvertisingState;
#[cfg(feature = "rhai")]
use crate::tool_names::RHAI_TOOL;
use crate::tool_names::{
    AGENT_SEND_TOOL, AGENT_TOOL, ASK_USER_TOOL, DESCRIBE_TOOL, EXPLORE_TOOL, POLL_TOOL,
    PROPOSE_PLAN_TOOL, RESPONSES_TOOL_SEARCH_TOOL,
};
use crate::tools::SharedRegistry;

use super::EscapeRoot;

/// How the executor routes a `ToolExec`. The tool mask (#116, ADR-0038) that
/// used to precede this classification is retired (ADR-0207 §8) — every
/// route below grades from the session's permission mode instead.
///
/// Classification is a **pure function of the tool name** ([`Intercept::classify`]),
/// and the routes are a `match` (mutually exclusive) instead of a fall-through
/// chain of `if tool == X { … }` branches, so a newly added route can't be
/// silently mis-ordered. Adding a tool means adding a variant here and its
/// `match` arm in [`route_tool_exec`] — both checked by the compiler's
/// exhaustiveness rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Intercept {
    /// `agent`: session orchestration only (touches no host resource), gated by
    /// the per-profile spawn control, not per-tool approval (#60/#119/#120;
    /// #606, ADR-0161 §1). Blocks for the answer by default; the parsed
    /// `background` flag picks the non-blocking launch instead — one guard
    /// path, two return shapes.
    Spawn,
    /// `agent_send`: sends a follow-up prompt to a sub-agent already launched
    /// with `agent` — steer a running child, or follow up a finished one
    /// (#609, ADR-0162; ADR-0207 §7 retires the `propose_plan` sponsored
    /// build this once also re-engaged). Session orchestration only, like
    /// `Spawn` — gated by
    /// [`crate::agent_registry::AgentRegistry::begin_send`]'s ownership +
    /// lifecycle check instead of per-tool approval.
    AgentSend,
    /// `poll`: joins a background `bash` job or a launched sub-agent (#605,
    /// ADR-0161 §1-4, replacing `bash_output`/`agent_poll`) — it reads
    /// accumulated job/spawn state, starting no session and touching no host.
    Poll,
    /// `ask_user`: a runtime-owned prompt tool (#90, ADR-0027) that surfaces a
    /// question to the head instead of running against the registry.
    AskUser,
    /// `propose_plan`: the plan agent's finalize step (#141, ADR-0042),
    /// force-parked on the `Ask` path since user approval *is* its semantics.
    ProposePlan,
    /// `explore`/`describe` (#560, ADR-0196 §4) plus the reserved
    /// `responses_tool_search` call name (P7, ADR-0196 §3 — a streamed
    /// client-executed `tool_search_call` from the OpenAI Responses wire,
    /// never a name the model chose from an advertised schema): the
    /// always-on discovery trio — read-only catalog introspection, starting
    /// nothing and touching no host resource. Exempt from the
    /// `Allow`/`Ask`/`Deny` ladder like every other runtime-owned
    /// orchestration tool (`is_non_maskable` still marks these two for the
    /// TUI's own tool-state rendering, `tool_state.rs` — unrelated to
    /// dispatch since ADR-0207).
    Discover,
    /// `rhai`: a sandboxed script tool (#122, ADR-0046) that resolves its own
    /// permission live against the loop's profile snapshot inside the script task.
    /// Behind the `rhai` feature (#502, ADR-0135) — a lean build without it
    /// never registers the tool, so a call named `rhai` falls through to the
    /// generic `Permission` route below and is refused there as unknown.
    #[cfg(feature = "rhai")]
    Rhai,
    /// Every other host tool: the generic `Allow | Ask | Deny` dispatch.
    Permission,
}

impl Intercept {
    /// Route an (already-unmasked) tool by name.
    pub(super) fn classify(tool: &str) -> Self {
        match tool {
            AGENT_TOOL => Self::Spawn,
            AGENT_SEND_TOOL => Self::AgentSend,
            POLL_TOOL => Self::Poll,
            ASK_USER_TOOL => Self::AskUser,
            PROPOSE_PLAN_TOOL => Self::ProposePlan,
            EXPLORE_TOOL | DESCRIBE_TOOL | RESPONSES_TOOL_SEARCH_TOOL => Self::Discover,
            #[cfg(feature = "rhai")]
            RHAI_TOOL => Self::Rhai,
            _ => Self::Permission,
        }
    }

    /// Whether this route skips the per-tool `Allow | Ask | Deny` decision. The
    /// spawn/poll/prompt/plan/discover routes touch no host resource, so
    /// permission does not apply; `Rhai` resolves permission itself inside the
    /// script task; the generic `Permission` route *is* the permission
    /// decision.
    pub(super) fn bypasses_permission(self) -> bool {
        matches!(
            self,
            Self::Spawn
                | Self::AgentSend
                | Self::Poll
                | Self::AskUser
                | Self::ProposePlan
                | Self::Discover
        )
    }
}

/// The executor's long-lived, cheaply-clonable shared state (issue #451):
/// built once, right before the loop starts, from exactly the parameters/
/// loop-locals [`route_tool_exec`]'s handlers already cloned per call — this
/// is the same cloning, just off a struct field instead of a bare loop-local
/// variable. Owned (not `&'a` borrows) so a `LadderCtx` needs no lifetime
/// parameter and outlives every detached task the ladder spawns.
pub(super) struct LadderCtx {
    pub holly: Holly,
    pub registry: AgentRegistry,
    pub retained: crate::retained_output::RetainedOutputRegistry,
    pub jobs: crate::host::jobs::JobRegistry,
    pub scripts: crate::script_ops::ScriptRegistry,
    pub pending: crate::pending::PendingDecisions,
    pub open_questions: OpenQuestions,
    pub resolver: Arc<dyn PermissionResolver>,
    pub agents: Arc<RwLock<AgentCatalog>>,
    pub perm_modes: Arc<Mutex<HashMap<SessionId, String>>>,
    pub mode_table: Arc<ModeTable>,
    pub plan_files: Arc<PlanFileRegistry>,
    pub plan_root: std::path::PathBuf,
    pub cancels: CancelRegistry,
    pub tools: SharedRegistry,
    pub skills: Arc<std::sync::RwLock<Arc<SkillRegistry>>>,
    pub mcp_avail: Arc<AvailableMcp>,
    pub mcp_active: ActiveServers,
    pub mcp_scopes: Option<Arc<crate::mcp::McpScopes>>,
    pub advertising: SharedAdvertisingState,
    pub escape_root: Option<EscapeRoot>,
    pub base: PermissionProfile,
    pub grants: Arc<dyn GrantStore>,
    pub mcp_http: Option<entanglement_core::HttpClient>,
    pub active_skill: Arc<Mutex<std::collections::HashSet<SessionId>>>,
    pub hooks: Arc<Hooks>,
    pub validation: Arc<crate::arg_validate::LoopBreaker>,
    pub denials: Arc<crate::run_limits::DenialTracker>,
    /// The active provider/model catalog (#560 P12, ADR-0207 §12): read by
    /// `explore`/`describe`'s `models` kind and by `agent`'s `model`
    /// parameter validation. `None` — every lean/test wrapper with no
    /// catalog wired — degrades both to "no catalog configured" rather than
    /// panicking.
    pub catalog: Option<Arc<Catalog>>,
}

/// Dispatch one `ToolExec` per its [`Intercept`] route. `spawn_guard` and
/// `overlays` are the executor loop's own mutable state — see the module doc
/// for why they stay outside [`LadderCtx`].
#[allow(clippy::too_many_arguments)]
pub(super) async fn route_tool_exec(
    ctx: &LadderCtx,
    spawn_guard: &mut SpawnGuard,
    overlays: &HashMap<SessionId, Vec<entanglement_core::ToolOverlayEntry>>,
    route: Intercept,
    // The call exactly as the model emitted it, when core unwrapped an
    // `invoke` envelope (ADR-0204). Only the `Permission` route's schema
    // validation needs it (`graded::permission`'s duplicate-key re-scan) —
    // every other route ignores it, same as `tool`/`input` themselves are
    // ignored by routes that never dispatch a call.
    envelope: Option<ToolEnvelope>,
    session: SessionId,
    request_id: String,
    tool: String,
    input: String,
) {
    match route {
        Intercept::Spawn => {
            orchestration::spawn(ctx, spawn_guard, session, request_id, input).await;
        }
        Intercept::AgentSend => {
            orchestration::agent_send(ctx, session, request_id, input).await;
        }
        Intercept::Poll => {
            orchestration::poll(ctx, session, request_id, input).await;
        }
        Intercept::AskUser => {
            orchestration::ask_user(ctx, session, request_id, input).await;
        }
        Intercept::ProposePlan => {
            orchestration::propose_plan(ctx, spawn_guard, session, request_id, input).await;
        }
        Intercept::Discover => {
            orchestration::discover(ctx, tool, session, request_id, input).await;
        }
        #[cfg(feature = "rhai")]
        Intercept::Rhai => {
            graded::rhai(ctx, spawn_guard, overlays, tool, session, request_id, input).await;
        }
        Intercept::Permission => {
            graded::permission(
                ctx,
                spawn_guard,
                overlays,
                tool,
                envelope,
                session,
                request_id,
                input,
            )
            .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "rhai")]
    use crate::tool_names::RHAI_TOOL;
    use crate::tool_names::{
        AGENT_SEND_TOOL, AGENT_TOOL, ASK_USER_TOOL, DESCRIBE_TOOL, EXPLORE_TOOL, POLL_TOOL,
        PROPOSE_PLAN_TOOL, RESPONSES_TOOL_SEARCH_TOOL,
    };

    #[test]
    fn classify_maps_each_orchestration_tool_to_its_route() {
        assert_eq!(Intercept::classify(AGENT_TOOL), Intercept::Spawn);
        assert_eq!(Intercept::classify(AGENT_SEND_TOOL), Intercept::AgentSend);
        assert_eq!(Intercept::classify(POLL_TOOL), Intercept::Poll);
        assert_eq!(Intercept::classify(ASK_USER_TOOL), Intercept::AskUser);
        assert_eq!(
            Intercept::classify(PROPOSE_PLAN_TOOL),
            Intercept::ProposePlan
        );
        assert_eq!(Intercept::classify(EXPLORE_TOOL), Intercept::Discover);
        assert_eq!(Intercept::classify(DESCRIBE_TOOL), Intercept::Discover);
        assert_eq!(
            Intercept::classify(RESPONSES_TOOL_SEARCH_TOOL),
            Intercept::Discover
        );
        #[cfg(feature = "rhai")]
        assert_eq!(Intercept::classify(RHAI_TOOL), Intercept::Rhai);
    }

    #[test]
    fn classify_routes_every_other_tool_to_permission() {
        // Host-registry tools and runtime state tools take the generic path.
        for tool in [
            "read",
            "write",
            "edit",
            "bash",
            "call",
            crate::tool_names::UPDATE_TASKS_TOOL,
            "",
        ] {
            assert_eq!(
                Intercept::classify(tool),
                Intercept::Permission,
                "`{tool}` should fall through to the permission dispatch"
            );
        }
    }

    #[test]
    fn only_orchestration_routes_bypass_permission() {
        // The spawn/poll/prompt/plan routes touch no host resource; `rhai`
        // resolves permission itself and the generic path *is* the decision.
        assert!(Intercept::Spawn.bypasses_permission());
        assert!(Intercept::AgentSend.bypasses_permission());
        assert!(Intercept::Poll.bypasses_permission());
        assert!(Intercept::AskUser.bypasses_permission());
        assert!(Intercept::ProposePlan.bypasses_permission());
        assert!(Intercept::Discover.bypasses_permission());
        #[cfg(feature = "rhai")]
        assert!(!Intercept::Rhai.bypasses_permission());
        assert!(!Intercept::Permission.bypasses_permission());
    }
}
