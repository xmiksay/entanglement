//! The escape-root check (ADR-0109, moved out of the library executor by
//! #968): a [`PermissionResolver`] wrapper that asks before a call reaches a
//! path outside the project root.
//!
//! The library keeps no filesystem-root logic. `skutter` wraps its resolver
//! in an [`EscapeResolver`]: when the inner grade is not a `Deny` and the
//! call's path — the registered tool's [`Tool::escape_target`], or a `rhai`
//! script `path` — resolves outside the root, the grade becomes an `Ask`
//! that carries why ([`CardReason::RootEscape`], or
//! [`CardReason::LaunchEscape`] for a script launch). An approval is then an
//! ordinary grant on the call's `(tool, argument)`, scoped to the mode it
//! was earned in, and an unattended session treats the ask like any other
//! (ADR-0207 §8/§11). A durable [`ExtraRootStore`] entry — a `/cd` into an
//! out-of-root directory, the trusted scratch dir — skips the ask.
//!
//! [`Tool::escape_target`]: crate::tools::Tool::escape_target

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use entanglement_core::{CardReason, SessionId};

use super::containment::escaping_path;
use crate::capability::Capability;
use crate::path_grading::str_arg;
use crate::policy::{Grade, PermissionResolver};
use crate::state::ExtraRootStore;
use crate::tool_names::RHAI_TOOL;
use crate::tools::SharedRegistry;

/// The canonical project root and its durable escape-root grants — what
/// `skutter` builds its [`EscapeResolver`] and `/cd` from.
#[derive(Clone)]
pub struct EscapeRoot {
    pub root: PathBuf,
    pub store: Arc<ExtraRootStore>,
}

/// Asks before a call reaches outside `root`; see the module docs.
pub struct EscapeResolver {
    inner: Arc<dyn PermissionResolver>,
    root: PathBuf,
    store: Arc<ExtraRootStore>,
    tools: SharedRegistry,
}

impl EscapeResolver {
    /// Wrap `inner` for the canonical project `root`, the live `tools` (each
    /// names the path its calls touch) and the durable grants in `store`.
    pub fn new(
        inner: Arc<dyn PermissionResolver>,
        root: PathBuf,
        store: Arc<ExtraRootStore>,
        tools: SharedRegistry,
    ) -> Self {
        Self {
            inner,
            root,
            store,
            tools,
        }
    }

    /// The out-of-root path a call to `tool` with `input` reaches and no
    /// durable grant covers.
    fn escaping(&self, tool: &str, input: &str) -> Option<PathBuf> {
        let target = if tool == RHAI_TOOL {
            str_arg(input, "path")
        } else {
            let tools = self.tools.read().expect("tool registry lock poisoned");
            tools.get(tool)?.escape_target(input)
        }?;
        let abs = escaping_path(&self.root, &target)?;
        (!self.store.is_durably_allowed(tool, &abs)).then_some(abs)
    }

    /// `grade`, turned into a reasoned `Ask` when the call escapes the root.
    fn check(&self, grade: Grade, tool: &str, input: &str) -> Grade {
        if matches!(grade, Grade::Deny(_)) {
            return grade;
        }
        match self.escaping(tool, input) {
            None => grade,
            Some(abs) => ask(tool, &abs),
        }
    }
}

/// The `Ask` for a call reaching `abs`.
fn ask(tool: &str, abs: &Path) -> Grade {
    if tool == RHAI_TOOL {
        let text = format!("runs a script outside the project root: {}", abs.display());
        Grade::ask(text, CardReason::LaunchEscape)
    } else {
        let text = format!(
            "accesses a path outside the project root: {}",
            abs.display()
        );
        Grade::ask(text, CardReason::RootEscape)
    }
}

#[async_trait]
impl PermissionResolver for EscapeResolver {
    async fn resolve(&self, session: &SessionId, tool: &str, input: &str) -> Grade {
        let grade = self.inner.resolve(session, tool, input).await;
        self.check(grade, tool, input)
    }

    async fn resolve_with_capabilities(
        &self,
        session: &SessionId,
        tool: &str,
        input: &str,
        capabilities: &[Capability],
    ) -> Grade {
        let grade = self
            .inner
            .resolve_with_capabilities(session, tool, input, capabilities)
            .await;
        self.check(grade, tool, input)
    }
}

#[cfg(test)]
mod tests;
