//! The permission-grading primitives `dispatch` composes (issue #451, split
//! out of `tool_runner.rs`): [`apply_grant`] upgrades a resolved `Ask` to
//! `Allow` off an existing grant, [`resolve_effective`] walks a call's
//! ancestor chain through the pluggable [`PermissionResolver`]. Both are
//! `pub(crate)` and re-exported at `tool_runner::{apply_grant,
//! resolve_effective}` — `ladder::graded` and (for `resolve_effective`
//! alone) `propose_plan`/`script::binding_policy` call them by that path.

use entanglement_core::{Permission, SessionId};

use crate::permission::min_permission;
use crate::policy::{GrantStore, PermissionResolver};

/// Upgrade a resolved `Ask` to `Allow` when `(session, tool, arg)` is already
/// granted under `mode` (#174, ADR-0207 §8: a grant matches only the mode it
/// was earned in): a session-scoped or persisted "always allow" grant lets an
/// *identical* later call in the *same mode* skip the prompt. Only `Ask` is
/// widened — a `Deny` (a hard policy floor) and an outright `Allow` pass
/// through untouched.
pub(crate) fn apply_grant(
    grants: &dyn GrantStore,
    session: &SessionId,
    tool: &str,
    arg: Option<&str>,
    perm: Permission,
    mode: &str,
) -> Permission {
    if perm == Permission::Ask && grants.is_granted(session, tool, arg, mode) {
        Permission::Allow
    } else {
        perm
    }
}

/// Least-privileged resolver grade across a call's ancestor chain — the sub-agent
/// privilege ceiling (ADR-0024) applied *on top of* whatever the pluggable
/// [`PermissionResolver`] returns, so a tenant rule can never widen a child
/// beyond its parent. For the default [`ModeResolver`][crate::policy::ModeResolver] each per-session
/// resolve already clamps to the config ceiling internally, so folding them
/// least-privilege here is the whole of it — no separate outer `clamp_to_base`
/// needed. An empty chain is impossible — the leaf session is always present —
/// but defaults to `Deny` if one ever arrives.
///
/// `pub(crate)`: also the grading primitive [`crate::script::BindingPolicy`]
/// reuses (ADR-0207 stage 4b) so a `rhai` binding — and `rhai`'s own
/// dispatch, `ladder::Intercept::Rhai` — resolve through the *exact* same
/// pluggable resolver + chain walk a direct tool call does, instead of a
/// second `Agent`-chain path.
pub(crate) async fn resolve_effective(
    resolver: &dyn PermissionResolver,
    chain: &[SessionId],
    tool: &str,
    input: &str,
) -> Permission {
    let mut perm = Permission::Allow;
    let mut any = false;
    for session in chain {
        perm = min_permission(perm, resolver.resolve(session, tool, input).await);
        any = true;
    }
    if any {
        perm
    } else {
        Permission::Deny
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A resolver that answers a fixed grade per session id (default `Allow`),
    /// so a test can prove the executor's ancestor clamp (#311, ADR-0024) sits
    /// *on top of* the pluggable resolver.
    struct PerSessionResolver(std::collections::HashMap<SessionId, Permission>);

    #[async_trait::async_trait]
    impl PermissionResolver for PerSessionResolver {
        async fn resolve(&self, session: &SessionId, _tool: &str, _input: &str) -> Permission {
            self.0.get(session).copied().unwrap_or(Permission::Allow)
        }
    }

    #[tokio::test]
    async fn resolve_effective_clamps_least_privilege_over_the_chain() {
        let child = SessionId::new("child");
        let parent = SessionId::new("parent");
        // The tenant rule *widens* the child to Allow, but its parent resolves
        // Ask — the chain min must clamp the child back to Ask, so a resolver can
        // never widen a sub-agent beyond its ancestor.
        let resolver = PerSessionResolver(
            [
                (child.clone(), Permission::Allow),
                (parent.clone(), Permission::Ask),
            ]
            .into_iter()
            .collect(),
        );
        let chain = vec![child.clone(), parent.clone()];
        assert_eq!(
            resolve_effective(&resolver, &chain, "bash", "{}").await,
            Permission::Ask
        );
        // A root (single-element chain) resolves to its own grade unchanged.
        assert_eq!(
            resolve_effective(&resolver, std::slice::from_ref(&child), "bash", "{}").await,
            Permission::Allow
        );
        // A parent `Deny` floors the child regardless of the tenant's Allow.
        let deny_parent =
            PerSessionResolver([(parent.clone(), Permission::Deny)].into_iter().collect());
        assert_eq!(
            resolve_effective(&deny_parent, &chain, "bash", "{}").await,
            Permission::Deny
        );
    }
}
