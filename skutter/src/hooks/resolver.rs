//! `pre_tool_use` as a [`PermissionResolver`] wrapper (#964): the library's
//! pluggable grade is the one check every call — a script's nested calls
//! included — passes before it runs, so a veto there needs no hook seam in
//! the executor.

use std::sync::Arc;

use async_trait::async_trait;
use entanglement_core::SessionId;
use entanglement_runtime::capability::Capability;
use entanglement_runtime::policy::{Grade, PermissionResolver};

use super::Hooks;

/// Grades a call `Deny` when a `pre_tool_use` hook vetoes it — carrying the
/// hook's reason, which the model receives as the call's result — else as
/// `inner` does. The executor asks a resolver once per session in a call's
/// ancestor chain, so a sub-agent's call runs the hooks once per ancestor
/// (each payload naming that session; #936 makes it one run naming the
/// calling session); a veto from any of them denies.
pub struct HookResolver {
    inner: Arc<dyn PermissionResolver>,
    hooks: Arc<Hooks>,
}

impl HookResolver {
    pub fn new(inner: Arc<dyn PermissionResolver>, hooks: Arc<Hooks>) -> Self {
        Self { inner, hooks }
    }

    async fn veto(&self, session: &SessionId, tool: &str, input: &str) -> Option<Grade> {
        let reason = self.hooks.run_pre_tool_use(session, tool, input).await?;
        tracing::warn!(%session, %tool, %reason, "pre_tool_use hook denied the call");
        Some(Grade::deny(reason))
    }
}

#[async_trait]
impl PermissionResolver for HookResolver {
    async fn resolve(&self, session: &SessionId, tool: &str, input: &str) -> Grade {
        if let Some(veto) = self.veto(session, tool, input).await {
            return veto;
        }
        self.inner.resolve(session, tool, input).await
    }

    async fn resolve_with_capabilities(
        &self,
        session: &SessionId,
        tool: &str,
        input: &str,
        capabilities: &[Capability],
    ) -> Grade {
        if let Some(veto) = self.veto(session, tool, input).await {
            return veto;
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

    struct Fixed(Grade);

    #[async_trait]
    impl PermissionResolver for Fixed {
        async fn resolve(&self, _: &SessionId, _: &str, _: &str) -> Grade {
            self.0.clone()
        }
    }

    fn resolver(command: &str, tools: &[&str], inner: Grade) -> HookResolver {
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
    async fn a_veto_denies_with_the_hooks_reason_whatever_the_inner_grade() {
        let r = resolver("echo no-rm >&2; exit 1", &[], Grade::Allow);
        let s = SessionId::new("s");
        let reason = |g: Grade| match g {
            Grade::Deny(Some(reason)) => reason,
            other => panic!("expected a reasoned deny, got {other:?}"),
        };
        let plain = reason(r.resolve(&s, "bash", "{}").await);
        assert!(
            plain.starts_with("tool `bash` blocked by pre_tool_use hook"),
            "{plain}"
        );
        assert!(plain.contains("no-rm"), "{plain}");
        let with_caps = reason(r.resolve_with_capabilities(&s, "bash", "{}", &[]).await);
        assert_eq!(with_caps, plain);
    }

    #[tokio::test]
    async fn a_cleared_or_unmatched_hook_keeps_the_inner_grade() {
        let s = SessionId::new("s");
        let cleared = resolver("exit 0", &[], Grade::Ask);
        assert_eq!(cleared.resolve(&s, "bash", "{}").await, Grade::Ask);
        let scoped = resolver("exit 1", &["edit"], Grade::Allow);
        assert_eq!(scoped.resolve(&s, "bash", "{}").await, Grade::Allow);
        assert!(matches!(
            scoped.resolve(&s, "edit", "{}").await,
            Grade::Deny(Some(_))
        ));
    }
}
