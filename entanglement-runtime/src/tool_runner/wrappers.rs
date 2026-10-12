//! The pre-#724 positional executor constructors, kept for one release (0.7)
//! as deprecated shims over [`ToolExecutor::builder`] so an embedder upgrading
//! gets a warning pointing at the migration instead of a compile break.
//! Remove them in the release after 0.7.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

use entanglement_core::{Agent, AgentCatalog, Holly, PermissionProfile, SessionId};

use crate::hooks::Hooks;
use crate::plan_files::PlanFileRegistry;
use crate::policy::{GrantStore, PermissionResolver};
use crate::skills::SkillRegistry;
use crate::tools::{SharedRegistry, ToolRegistry};

use super::{DiscoverySurface, EscapeRoot, ToolExecutor};

/// Equivalent to
/// `ToolExecutor::builder(holly, tools.shared()).agents(agents).base(base).spawn()`.
/// Grants are in-memory only (#747).
#[deprecated(since = "0.7.0", note = "use ToolExecutor::builder")]
pub fn spawn_tool_executor(
    holly: &Holly,
    tools: ToolRegistry,
    agents: AgentCatalog,
    base: PermissionProfile,
) -> tokio::task::JoinHandle<()> {
    ToolExecutor::builder(holly, tools.shared())
        .agents(agents)
        .base(base)
        .spawn()
}

/// Equivalent to [`spawn_tool_executor`] plus `.hooks(hooks)`.
#[deprecated(since = "0.7.0", note = "use ToolExecutor::builder")]
pub fn spawn_tool_executor_with_hooks(
    holly: &Holly,
    tools: ToolRegistry,
    agents: AgentCatalog,
    base: PermissionProfile,
    hooks: Hooks,
) -> tokio::task::JoinHandle<()> {
    ToolExecutor::builder(holly, tools.shared())
        .agents(agents)
        .base(base)
        .hooks(hooks)
        .spawn()
}

/// Every seam spelled positionally; each argument maps 1:1 onto the
/// [`ToolExecutor::builder`] setter of the same name (`agents` →
/// `live_agents`, `skills` → `live_skills`, an `Option` → setter only when
/// `Some`).
#[deprecated(since = "0.7.0", note = "use ToolExecutor::builder")]
#[allow(clippy::too_many_arguments)]
pub fn spawn_tool_executor_with_policy(
    holly: &Holly,
    tools: SharedRegistry,
    jobs: crate::host::jobs::JobRegistry,
    retained: crate::retained_output::RetainedOutputRegistry,
    scripts: crate::script_ops::ScriptRegistry,
    agents: Arc<RwLock<AgentCatalog>>,
    skills: Arc<RwLock<Arc<SkillRegistry>>>,
    base: PermissionProfile,
    active: Arc<Mutex<HashMap<SessionId, Agent>>>,
    perm_modes: Arc<Mutex<HashMap<SessionId, String>>>,
    resolver: Arc<dyn PermissionResolver>,
    grants: Arc<dyn GrantStore>,
    hooks: Hooks,
    escape_root: Option<EscapeRoot>,
    mode_table: Arc<crate::mode::ModeTable>,
    plan_files: Arc<PlanFileRegistry>,
    mcp_scopes: Option<Arc<crate::mcp::McpScopes>>,
    catalog: Option<Arc<entanglement_core::Catalog>>,
    discovery: Option<DiscoverySurface>,
) -> tokio::task::JoinHandle<()> {
    let mut b = ToolExecutor::builder(holly, tools)
        .jobs(jobs)
        .retained(retained)
        .scripts(scripts)
        .live_agents(agents)
        .live_skills(skills)
        .base(base)
        .active(active)
        .perm_modes(perm_modes)
        .resolver(resolver)
        .grants(grants)
        .hooks(hooks)
        .mode_table(mode_table)
        .plan_files(plan_files)
        .discovery(discovery.unwrap_or_default());
    if let Some(er) = escape_root {
        b = b.escape_root(er);
    }
    if let Some(scopes) = mcp_scopes {
        b = b.mcp_scopes(scopes);
    }
    if let Some(catalog) = catalog {
        b = b.catalog(catalog);
    }
    b.spawn()
}
