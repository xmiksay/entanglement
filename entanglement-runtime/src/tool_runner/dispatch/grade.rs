//! The permission-grading primitives `dispatch` composes (issue #451, split
//! out of `tool_runner.rs`): [`grade_call`] composes a call's whole grade
//! (the mode grade, then an existing grant's `Ask`→`Allow`, then the
//! trusted-script write floor), and
//! [`resolve_effective`] walks a call's ancestor chain through the pluggable
//! [`PermissionResolver`]. `resolve_effective` is re-exported at
//! `tool_runner::resolve_effective` for `propose_plan`.

use std::path::Path;

use entanglement_core::SessionId;

use crate::capability::Capability;
use crate::permission::min_permission;
use crate::permission_path::grading_arg;
use crate::policy::{AskReason, Grade, GrantStore, PermissionResolver};
use crate::rhai_trust::Trust;
use crate::tools::ToolRegistry;

/// Everything a call's grade depends on besides the call itself — what
/// [`grade_call`] composes. Borrowed so `dispatch` builds it from its own
/// arguments; [`crate::tool_runner::call_grader::CallGrader`] (#808) owns
/// the same set and lends it out, so a redirect hop grades *exactly* like a
/// dispatched call.
pub(crate) struct GradeInputs<'a> {
    /// The registry the call dispatches against: its tools declare their
    /// permission arguments (#962).
    pub tools: &'a ToolRegistry,
    pub resolver: &'a dyn PermissionResolver,
    pub chain: &'a [SessionId],
    pub grants: &'a dyn GrantStore,
    pub session: &'a SessionId,
    pub mode: &'a str,
    pub root: Option<&'a Path>,
    /// The trusted folders and skill dirs plus the root a relative file-tool
    /// `path` resolves against — for the write floor (ADR-0228); `None`
    /// where the floor does not apply.
    pub floor: Option<(&'a Trust, &'a Path)>,
    /// Per-call capability override (ADR-0249): when `Some`, this is used
    /// instead of looking up the tool's capabilities from the registry.
    /// Set by [`Tool::call_grading`][crate::tools::Tool::call_grading].
    pub capability_override: Option<&'a [Capability]>,
}

/// The grade `(tool, input)` resolves to, plus the argument it was graded
/// on: the mode grade clamped across the ancestor chain — a `Deny` returned
/// as resolved, with its reason (#964) — then an `Ask` (the mode's, or a
/// resolver's reasoned one) upgraded to `Allow` by an existing mode-scoped
/// grant on the same `(tool, argument)`, or on the key the resolver named
/// for its ask ([`grant_key`]). Last, a write
/// ([`crate::rhai_trust::floored_write`]) of a `*.rhai` file inside a
/// `rhai.trusted` folder, or of a skill's `SKILL.md`/`lib.rhai`, is floored
/// at `Ask` whatever allowed it (ADR-0228: such a file later runs or loads
/// unasked, so authoring one is never unseen); a `Deny` stays `Deny`.
pub(crate) async fn grade_call(
    g: &GradeInputs<'_>,
    tool: &str,
    input: &str,
) -> (Grade, Option<String>) {
    let arg = grading_arg(g.tools, tool, input, g.root);
    let grade =
        match resolve_effective(g.resolver, g.chain, tool, input, g.capability_override).await {
            Grade::Ask(why) if granted(g, grant_key(why.as_ref(), tool, arg.as_deref())) => {
                Grade::Allow
            }
            graded => graded,
        };
    let floored = || {
        g.floor.is_some_and(|(trust, root)| {
            crate::rhai_trust::floored_write(g.tools, tool, input, root, trust)
        })
    };
    if grade == Grade::Allow && floored() {
        return (Grade::Ask(None), arg);
    }
    (grade, arg)
}

fn granted(g: &GradeInputs<'_>, (tool, arg): (&str, Option<&str>)) -> bool {
    g.grants.is_granted(g.session, tool, arg, g.mode)
}

/// The `(tool, argument)` a grant answering an `Ask` is looked up and
/// recorded under: the key the resolver named for its ask
/// ([`AskReason::grant`]), else the call's own.
pub(crate) fn grant_key<'a>(
    why: Option<&'a AskReason>,
    tool: &'a str,
    arg: Option<&'a str>,
) -> (&'a str, Option<&'a str>) {
    match why.and_then(|w| w.grant.as_ref()) {
        Some(key) => (&key.tool, Some(&key.arg)),
        None => (tool, arg),
    }
}

/// The name a call grades under: the target its tool's
/// [`call_grading`][crate::tools::Tool::call_grading] names (an envelope
/// tool's target, a script-only read variant's base tool), else its own —
/// so rules, grants and repeat tracking all key on one identity.
pub(crate) fn graded_name(tools: &ToolRegistry, tool: &str, input: &str) -> String {
    tools
        .get(tool)
        .and_then(|t| t.call_grading(input))
        .map_or_else(|| tool.to_string(), |cg| cg.name)
}

/// Least-privileged resolver grade across a call's ancestor chain — the sub-agent
/// privilege ceiling (ADR-0024) applied *on top of* whatever the pluggable
/// [`PermissionResolver`] returns, so a tenant rule can never widen a child
/// beyond its parent. For the default [`ModeResolver`][crate::policy::ModeResolver] each per-session
/// resolve already clamps to the config ceiling internally, so folding them
/// least-privilege here is the whole of it — no separate outer `clamp_to_base`
/// needed. An empty chain is impossible — the leaf session is always present —
/// but defaults to `Deny` if one ever arrives. A `Deny` keeps the reason of
/// the first session in the chain (leaf first) that denied (#964).
///
/// When `capability_override` is `Some` (ADR-0249 per-call grading), each
/// session is resolved via
/// [`resolve_with_capabilities`][PermissionResolver::resolve_with_capabilities]
/// so a resolver that grades by capability class sees the caller-supplied
/// ones instead of the registry's.
///
/// `pub(crate)` for `propose_plan`'s own gate.
pub(crate) async fn resolve_effective(
    resolver: &dyn PermissionResolver,
    chain: &[SessionId],
    tool: &str,
    input: &str,
    capability_override: Option<&[Capability]>,
) -> Grade {
    let mut folded = Grade::Allow;
    let mut any = false;
    for session in chain {
        let grade = match capability_override {
            Some(caps) => {
                resolver
                    .resolve_with_capabilities(session, tool, input, caps)
                    .await
            }
            None => resolver.resolve(session, tool, input).await,
        };
        // Only a strictly lower grade replaces the fold, so a later `Deny`
        // never overwrites the first one's reason — save that a reasoned
        // `Ask` replaces a plain one, so its card says why.
        let lower = min_permission(folded.permission(), grade.permission()) != folded.permission();
        let reasoned = matches!((&folded, &grade), (Grade::Ask(None), Grade::Ask(Some(_))));
        if lower || reasoned {
            folded = grade;
        }
        any = true;
    }
    if any {
        folded
    } else {
        Grade::Deny(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A resolver that answers a fixed grade per session id (default `Allow`),
    /// so a test can prove the executor's ancestor clamp (#311, ADR-0024) sits
    /// *on top of* the pluggable resolver.
    struct PerSessionResolver(std::collections::HashMap<SessionId, Grade>);

    #[async_trait::async_trait]
    impl PermissionResolver for PerSessionResolver {
        async fn resolve(&self, session: &SessionId, _tool: &str, _input: &str) -> Grade {
            self.0.get(session).cloned().unwrap_or(Grade::Allow)
        }
    }

    fn per_session(grades: &[(&SessionId, Grade)]) -> PerSessionResolver {
        PerSessionResolver(
            grades
                .iter()
                .map(|(s, g)| ((*s).clone(), g.clone()))
                .collect(),
        )
    }

    #[tokio::test]
    async fn resolve_effective_clamps_least_privilege_over_the_chain() {
        let child = SessionId::new("child");
        let parent = SessionId::new("parent");
        // The tenant rule *widens* the child to Allow, but its parent resolves
        // Ask — the chain min must clamp the child back to Ask, so a resolver can
        // never widen a sub-agent beyond its ancestor.
        let resolver = per_session(&[(&child, Grade::Allow), (&parent, Grade::Ask(None))]);
        let chain = vec![child.clone(), parent.clone()];
        assert_eq!(
            resolve_effective(&resolver, &chain, "bash", "{}", None).await,
            Grade::Ask(None)
        );
        // A root (single-element chain) resolves to its own grade unchanged.
        assert_eq!(
            resolve_effective(&resolver, std::slice::from_ref(&child), "bash", "{}", None).await,
            Grade::Allow
        );
        // A parent `Deny` floors the child regardless of the tenant's Allow,
        // and its reason comes along.
        let deny_parent = per_session(&[(&parent, Grade::deny("parent says no"))]);
        assert_eq!(
            resolve_effective(&deny_parent, &chain, "bash", "{}", None).await,
            Grade::deny("parent says no")
        );
    }

    /// #964: the first deny in the chain (leaf first) names the reason; a
    /// later one cannot overwrite it.
    #[tokio::test]
    async fn the_first_deny_in_the_chain_keeps_its_reason() {
        let child = SessionId::new("child");
        let parent = SessionId::new("parent");
        let chain = vec![child.clone(), parent.clone()];
        let both = per_session(&[
            (&child, Grade::deny("child")),
            (&parent, Grade::deny("parent")),
        ]);
        assert_eq!(
            resolve_effective(&both, &chain, "bash", "{}", None).await,
            Grade::deny("child")
        );
        let plain = per_session(&[
            (&child, Grade::Deny(None)),
            (&parent, Grade::deny("parent")),
        ]);
        assert_eq!(
            resolve_effective(&plain, &chain, "bash", "{}", None).await,
            Grade::Deny(None)
        );
    }

    /// `grade_call` of a `bash` call no registered tool describes.
    async fn grade_bash(
        resolver: &dyn PermissionResolver,
        grants: &dyn GrantStore,
        session: &SessionId,
    ) -> Grade {
        let tools = ToolRegistry::new();
        let chain = [session.clone()];
        let inputs = GradeInputs {
            tools: &tools,
            resolver,
            chain: &chain,
            grants,
            session,
            mode: "build",
            root: None,
            floor: None,
            capability_override: None,
        };
        grade_call(&inputs, "bash", "{}").await.0
    }

    /// #968 review: a resolver's keyed ask is answered by a grant on its
    /// key only — a grant on the call's own argument does not lift it —
    /// while a plain ask still keys on the argument.
    #[tokio::test]
    async fn a_keyed_ask_is_answered_by_its_own_grant_only() {
        use crate::policy::{AskGrant, InMemoryGrantStore};
        use entanglement_core::{ApprovalScope, CardReason};

        let key = AskGrant {
            tool: "bash#outside-root".into(),
            arg: r#"["/elsewhere","make"]"#.into(),
        };
        let session = SessionId::new("s");
        let keyed = per_session(&[(
            &session,
            Grade::ask_keyed("outside", CardReason::RootEscape, key.clone()),
        )]);
        let plain = per_session(&[(&session, Grade::Ask(None))]);
        let grants = InMemoryGrantStore::new();
        // No registered tool: the call's own key is `(bash, None)`.
        grants
            .record(&session, "bash", None, ApprovalScope::Session, "build")
            .await;
        let lifted = grade_bash(&keyed, &grants, &session).await;
        assert!(matches!(lifted, Grade::Ask(Some(_))), "{lifted:?}");
        assert_eq!(grade_bash(&plain, &grants, &session).await, Grade::Allow);
        grants
            .record(
                &session,
                &key.tool,
                Some(&key.arg),
                ApprovalScope::Session,
                "build",
            )
            .await;
        assert_eq!(grade_bash(&keyed, &grants, &session).await, Grade::Allow);
    }
}
