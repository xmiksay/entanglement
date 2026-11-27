//! `pre_tool_use` as a [`PermissionResolver`] wrapper (#964): the library's
//! pluggable grade is the one check every call — a script's nested calls
//! included — passes before it runs, so a veto there needs no hook seam in
//! the executor.

use std::sync::Arc;

use async_trait::async_trait;
use entanglement_core::{Permission, SessionId};
use entanglement_runtime::capability::Capability;
use entanglement_runtime::policy::PermissionResolver;

use super::Hooks;

/// Grades a call `Deny` when a `pre_tool_use` hook vetoes it, else as
/// `inner` does. The executor asks a resolver once per session in a call's
/// ancestor chain, so a sub-agent's call runs the hooks once per ancestor
/// (each payload naming that session); a veto from any of them denies.
pub struct HookResolver {
    inner: Arc<dyn PermissionResolver>,
    hooks: Arc<Hooks>,
}

impl HookResolver {
    pub fn new(inner: Arc<dyn PermissionResolver>, hooks: Arc<Hooks>) -> Self {
        Self { inner, hooks }
    }

    async fn vetoed(&self, session: &SessionId, tool: &str, input: &str) -> bool {
        let Some(reason) = self.hooks.run_pre_tool_use(session, tool, input).await else {
            return false;
        };
        // The grade carries no reason, so the model sees the plain deny;
        // the hook's own output is kept here.
        tracing::warn!(%session, %tool, %reason, "pre_tool_use hook denied the call");
        true
    }
}

#[async_trait]
impl PermissionResolver for HookResolver {
    async fn resolve(&self, session: &SessionId, tool: &str, input: &str) -> Permission {
        if self.vetoed(session, tool, input).await {
            return Permission::Deny;
        }
        self.inner.resolve(session, tool, input).await
    }

    async fn resolve_with_capabilities(
        &self,
        session: &SessionId,
        tool: &str,
        input: &str,
        capabilities: &[Capability],
    ) -> Permission {
        if self.vetoed(session, tool, input).await {
            return Permission::Deny;
        }
        self.inner
            .resolve_with_capabilities(session, tool, input, capabilities)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::HookSpec;

    struct Fixed(Permission);

    #[async_trait]
    impl PermissionResolver for Fixed {
        async fn resolve(&self, _: &SessionId, _: &str, _: &str) -> Permission {
            self.0
        }
    }

    fn resolver(command: &str, tools: &[&str], inner: Permission) -> HookResolver {
        let hooks = Hooks {
            pre_tool_use: vec![HookSpec {
                command: command.into(),
                tools: tools.iter().map(|t| t.to_string()).collect(),
                timeout_secs: 30,
            }],
            ..Default::default()
        };
        HookResolver::new(Arc::new(Fixed(inner)), Arc::new(hooks))
    }

    #[tokio::test]
    async fn a_veto_denies_whatever_the_inner_grade() {
        let r = resolver("exit 1", &[], Permission::Allow);
        let s = SessionId::new("s");
        assert_eq!(r.resolve(&s, "bash", "{}").await, Permission::Deny);
        assert_eq!(
            r.resolve_with_capabilities(&s, "bash", "{}", &[]).await,
            Permission::Deny
        );
    }

    #[tokio::test]
    async fn a_cleared_or_unmatched_hook_keeps_the_inner_grade() {
        let s = SessionId::new("s");
        let cleared = resolver("exit 0", &[], Permission::Ask);
        assert_eq!(cleared.resolve(&s, "bash", "{}").await, Permission::Ask);
        let scoped = resolver("exit 1", &["edit"], Permission::Allow);
        assert_eq!(scoped.resolve(&s, "bash", "{}").await, Permission::Allow);
        assert_eq!(scoped.resolve(&s, "edit", "{}").await, Permission::Deny);
    }
}
