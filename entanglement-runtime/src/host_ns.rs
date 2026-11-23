//! Host script namespaces (ADR-0249): an embedder registers a namespace
//! (e.g. `infra`) bound to one of its tools (e.g. `invoke`), plus a
//! per-session resolver listing the callable function names for that session.
//! Each `rhai` run generates `<ns>::<fn>(#{...})` from the resolver, and a
//! call dispatches to the bound tool with `{"tool": "<fn>", "args": {...}}`,
//! graded per the tool's [`call_grading`][crate::tools::Tool::call_grading].

use std::sync::Arc;

use entanglement_core::SessionId;

/// One function a session may call through a host namespace.
#[derive(Debug, Clone)]
pub struct HostFn {
    /// The function name the model and scripts see (e.g. `page_edit`).
    pub name: String,
    /// One-line description shown in `mcp_tools`-style listings.
    pub description: String,
}

/// Per-session function resolver: returns the functions `session` may call
/// through this namespace, or `None` to hide the namespace entirely.
pub type HostFnResolver = Arc<dyn Fn(&SessionId) -> Option<Vec<HostFn>> + Send + Sync>;

/// One host namespace: a name bound to a registered tool, with a per-session
/// resolver deciding which function names are callable.
///
/// Registration: [`ToolExecutorBuilder::host_namespace`]. The backing tool
/// must implement [`call_grading`][crate::tools::Tool::call_grading] so each
/// function call is graded as its target.
#[derive(Clone)]
pub struct HostNamespace {
    /// The namespace identifier (e.g. `infra`). Becomes the rhai module path
    /// `<ns>::<fn>`.
    pub name: String,
    /// The registered tool name calls are dispatched to (e.g. `invoke`).
    /// The tool's input receives `{"tool": "<fn>", "args": {...}}`.
    pub tool: String,
    /// Per-session function set.
    pub resolver: HostFnResolver,
}

impl std::fmt::Debug for HostNamespace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostNamespace")
            .field("name", &self.name)
            .field("tool", &self.tool)
            .finish()
    }
}
