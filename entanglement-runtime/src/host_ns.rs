//! What an embedder adds to a `rhai` script — the library's own engine binds
//! no file or process function (#933).
//!
//! - Host script namespaces (ADR-0249): an embedder registers a namespace
//!   (e.g. `infra`) bound to one of its tools (e.g. `invoke`), plus a
//!   per-session resolver listing the callable function names for that
//!   session. Each `rhai` run generates `<ns>::<fn>(#{...})` from the
//!   resolver, and a call dispatches to the bound tool with
//!   `{"tool": "<fn>", "args": {...}}`, graded per the tool's
//!   [`call_grading`][crate::tools::Tool::call_grading].
//! - Script bindings (#933): a registered tool that returns a
//!   [`ScriptBinding`] from [`Tool::script_binding`][crate::tools::Tool::script_binding]
//!   is callable as a plain function under its own name (`read(path)`), its
//!   positional arguments marshalled into the tool's JSON input.
//!
//! Either way a call climbs the same dispatch seam as a model-issued one.

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

/// A registered tool's positional script function (#933): `name(a, b)` runs
/// the tool with `{"<key a>": a, "<key b>": b}`, through the same dispatch
/// seam as a model-issued call. Bound under the tool's name, which must be a
/// Rhai identifier.
#[derive(Debug, Clone, Copy)]
pub struct ScriptBinding {
    /// One parameter list per overload: each parameter's input key and type.
    pub overloads: &'static [&'static [(&'static str, ScriptArg)]],
    /// Parse the tool's reply as JSON into a Rhai value instead of returning
    /// its text; a reply that is not JSON throws.
    pub json: bool,
    /// An input key set to the run's remaining wall-clock budget in whole
    /// seconds (at least 1), so a tool that bounds itself cannot outlive the
    /// script — the engine's own interrupt cannot reach a call in flight.
    pub budget_key: Option<&'static str>,
    /// The `rhai` spec's reference line(s) for this function: its
    /// signatures and what it returns.
    pub doc: &'static str,
}

/// A script binding parameter's type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScriptArg {
    Str,
    Int,
    Bool,
    /// An array, sent as an array of strings.
    StrList,
}
