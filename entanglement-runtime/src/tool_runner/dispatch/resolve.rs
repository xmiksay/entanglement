//! The registry a call runs against (ADR-0201/0226): the scoped session's
//! own MCP tools, then a lazy connect for an unregistered `mcp__*` name. A
//! script's call waits on that connect no longer than its remaining budget
//! (#831); a model's call waits the connect's own ceiling.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use entanglement_core::{HttpClient, SessionId};

use crate::mcp::available::{
    disabled_decline, resolve_unregistered, try_lazy_connect, unknown_server_decline,
    LazyConnectOutcome, McpTier,
};
use crate::mcp::scoped::ScopedConnect;
use crate::mcp::{ActiveServers, AvailableMcp, McpScope, McpScopes};
use crate::tool_advertising;
use crate::tools::{SharedRegistry, ToolRegistry};

use super::{Dispatcher, Via};

/// Below this much budget a script's call never starts a connect: it could
/// only be cut at once, leaving a connect behind for nothing.
const BUDGET_FLOOR: Duration = Duration::from_millis(50);

/// What resolution reads, owned.
struct ToolSource {
    scopes: Option<Arc<McpScopes>>,
    session: SessionId,
    tools: ToolRegistry,
    registry: SharedRegistry,
    avail: Arc<AvailableMcp>,
    active: ActiveServers,
    http: Option<HttpClient>,
}

impl ToolSource {
    async fn resolve(self, tool: &str) -> Result<ToolRegistry, String> {
        let tools = match &self.scopes {
            Some(scopes) => {
                let tools = scopes
                    .overlay_registry_for_call(&self.session, self.tools, tool)
                    .await?;
                if let Some(decline) = scopes.scope_decline(&self.session, &tools, tool) {
                    return Err(decline);
                }
                tools
            }
            None => self.tools,
        };
        if tools.contains(tool)
            || crate::plan_tasks::is_state_tool(tool)
            || tool == crate::tool_names::REQUEST_MODE_TOOL
        {
            return Ok(tools);
        }
        if tool.starts_with("mcp__") {
            return resolve_unregistered(
                &self.avail,
                tool,
                &self.registry,
                &self.active,
                self.http.as_ref(),
            )
            .await;
        }
        Err(tool_advertising::unknown_tool_reply(&tools, tool))
    }
}

/// A server a script's call must connect before it can resolve.
enum Target {
    Global(String),
    Scoped(Arc<McpScopes>, McpScope, String),
}

type Connect = Pin<Box<dyn Future<Output = Result<(), String>> + Send>>;

impl Dispatcher {
    /// Resolve `tool`'s registry, connecting its MCP server lazily if need
    /// be. `Err` is the refusal text.
    pub(super) async fn resolve_tools(
        &self,
        tool: &str,
        via: Via<'_>,
    ) -> Result<ToolRegistry, String> {
        if let Some(budget) = via.budget(tool).filter(|_| tool.starts_with("mcp__")) {
            if let Some(target) = self.connect_target(tool) {
                self.await_connect(target, budget).await?;
            }
        }
        ToolSource {
            scopes: self.mcp_scopes.clone(),
            session: self.session.clone(),
            tools: self.tools.clone(),
            registry: self.registry.clone(),
            avail: self.mcp_avail.clone(),
            active: self.mcp_active.clone(),
            http: self.mcp_http.clone(),
        }
        .resolve(tool)
        .await
    }

    /// The not-yet-connected server `tool` belongs to, or `None` when
    /// resolving it connects nothing.
    fn connect_target(&self, tool: &str) -> Option<Target> {
        if let Some(scopes) = &self.mcp_scopes {
            match scopes.connect_needed(&self.session, tool) {
                ScopedConnect::Unscoped => {}
                ScopedConnect::Ready => return None,
                ScopedConnect::Needed { scope, server } => {
                    return Some(Target::Scoped(scopes.clone(), scope, server))
                }
            }
        }
        if self.tools.contains(tool) {
            return None;
        }
        let server = self.mcp_avail.server_for_tool(tool)?;
        let connected = self
            .mcp_active
            .lock()
            .expect("MCP active-server mutex poisoned")
            .contains_key(&server);
        let available = self.mcp_avail.tier_of(&server) == McpTier::Available;
        (available && !connected).then_some(Target::Global(server))
    }

    /// Wait on `target`'s one detached connect (started here if none runs)
    /// for at most `budget`. Detached rather than dropped on expiry: whoever
    /// queued behind it — the model, a later script — gets the finished
    /// connection instead of a fresh attempt.
    async fn await_connect(&self, target: Target, budget: Duration) -> Result<(), String> {
        let server = match &target {
            Target::Global(server) | Target::Scoped(_, _, server) => server.clone(),
        };
        if budget < BUDGET_FLOOR {
            return Err(format!(
                "mcp server `{server}` is not connected and the script's time budget is spent \
                 — no connect was started"
            ));
        }
        let connect: Connect = match target {
            Target::Scoped(scopes, scope, server) => Box::pin(scopes.script_connect(scope, server)),
            Target::Global(server) => {
                let (avail, registry, active, http) = (
                    self.mcp_avail.clone(),
                    self.registry.clone(),
                    self.mcp_active.clone(),
                    self.mcp_http.clone(),
                );
                let key = server.clone();
                Box::pin(
                    self.mcp_avail
                        .script_connects()
                        .join(key, move || async move {
                            match try_lazy_connect(
                                &avail,
                                &server,
                                &registry,
                                &active,
                                http.as_ref(),
                            )
                            .await
                            {
                                LazyConnectOutcome::Connected => Ok(()),
                                LazyConnectOutcome::Unknown => Err(unknown_server_decline(&server)),
                                LazyConnectOutcome::Disabled => Err(disabled_decline(&server)),
                                LazyConnectOutcome::Failed(why) => Err(why),
                            }
                        }),
                )
            }
        };
        match tokio::time::timeout(budget, connect).await {
            Ok(settled) => settled,
            Err(_) => Err(connect_cut(&server)),
        }
    }
}

/// The catchable error a script gets when its budget ran out mid-connect.
fn connect_cut(server: &str) -> String {
    format!(
        "mcp server `{server}` was still connecting when the script's time budget ran out — \
         the connect continues in the background; a later call reuses it once it is up"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cut_names_the_server_and_the_budget() {
        let msg = connect_cut("slow");
        assert!(msg.contains("`slow`"), "{msg}");
        assert!(msg.contains("time budget ran out"), "{msg}");
    }
}
