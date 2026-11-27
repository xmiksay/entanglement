//! The escape-root check (ADR-0109, moved out of the library executor by
//! #968): a [`PermissionResolver`] wrapper that asks before a call reaches a
//! path outside the project root.
//!
//! The library keeps no filesystem-root logic. `skutter` wraps its resolver
//! in an [`EscapeResolver`]: when the inner grade is not a `Deny` and the
//! call's path — the registered tool's [`Tool::escape_target`], or a `rhai`
//! script `path` — resolves outside the root, the grade becomes an `Ask`
//! that carries why ([`CardReason::RootEscape`], or
//! [`CardReason::LaunchEscape`] for a script launch). The ask names its own
//! grant key ([`AskGrant`]): the tool's name with an [`ESCAPE_SUFFIX`], and
//! the escaped path — plus the permission argument when that is not the
//! path (`bash`'s command), so a grant on `bash(cargo build)` never runs it
//! in another directory and approving one out-of-root run covers that
//! command in that directory only. The grant is scoped to the mode it was
//! earned in, and an unattended session treats the ask like any other
//! (ADR-0207 §8/§11). A durable [`ExtraRootStore`] entry — a `/cd` into an
//! out-of-root directory, the trusted scratch dir — skips the ask.
//!
//! [`Tool::escape_target`]: crate::tools::Tool::escape_target

use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError};

use async_trait::async_trait;
use entanglement_core::{CardReason, SessionId};

use super::containment::escaping_path;
use crate::capability::Capability;
use crate::path_grading::str_arg;
use crate::policy::{AskGrant, Grade, PermissionResolver};
use crate::state::ExtraRootStore;
use crate::tool_names::RHAI_TOOL;
use crate::tools::{ArgKind, SharedRegistry};

/// Appended to a tool's name in the grant key of an escape ask — a name no
/// registered tool has, so the key never meets an ordinary call's grant.
pub const ESCAPE_SUFFIX: &str = "#outside-root";

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
    /// durable grant covers, with the call's permission argument when that
    /// names something other than the path.
    fn escaping(&self, tool: &str, input: &str) -> Option<(PathBuf, Option<String>)> {
        let (target, arg) = if tool == RHAI_TOOL {
            (str_arg(input, "path")?, None)
        } else {
            // Read through poison like the library executor: a panicked
            // registry writer must not take the permission path down with it.
            let tools = self.tools.read().unwrap_or_else(PoisonError::into_inner);
            let t = tools.get(tool)?;
            let arg = match t.permission_scope().arg {
                ArgKind::Path => None,
                _ => t.permission_arg(input),
            };
            (t.escape_target(input)?, arg)
        };
        let abs = escaping_path(&self.root, &target)?;
        (!self.store.is_durably_allowed(tool, &abs)).then_some((abs, arg))
    }

    /// `grade`, turned into a reasoned `Ask` when the call escapes the root.
    fn check(&self, grade: Grade, tool: &str, input: &str) -> Grade {
        if matches!(grade, Grade::Deny(_)) {
            return grade;
        }
        match self.escaping(tool, input) {
            None => grade,
            Some((abs, arg)) => ask(tool, &abs, arg),
        }
    }
}

/// The `Ask` for a call reaching `abs`, keyed on [`escape_grant`].
fn ask(tool: &str, abs: &Path, arg: Option<String>) -> Grade {
    let grant = escape_grant(tool, abs, arg);
    if tool == RHAI_TOOL {
        let text = format!("runs a script outside the project root: {}", abs.display());
        Grade::ask_keyed(text, CardReason::LaunchEscape, grant)
    } else {
        let text = format!(
            "accesses a path outside the project root: {}",
            abs.display()
        );
        Grade::ask_keyed(text, CardReason::RootEscape, grant)
    }
}

/// The grant that answers an escape ask: `<tool>#outside-root` on the
/// escaped path, or on `[path, argument]` (JSON, so no path or command can
/// forge another's key) when the argument is not the path.
pub fn escape_grant(tool: &str, abs: &Path, arg: Option<String>) -> AskGrant {
    let path = abs.to_string_lossy().into_owned();
    AskGrant {
        tool: format!("{tool}{ESCAPE_SUFFIX}"),
        arg: match arg {
            None => path,
            Some(arg) => serde_json::json!([path, arg]).to_string(),
        },
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
