//! The permission-grading primitives `dispatch` composes (issue #451, split
//! out of `tool_runner.rs`): [`grade_call`] composes a call's whole grade
//! (the mode grade, then [`apply_grant`]'s `Ask`→`Allow` off an
//! existing grant, then the trusted-script write floor), and
//! [`resolve_effective`] walks a call's ancestor chain through the pluggable
//! [`PermissionResolver`]. `resolve_effective` is re-exported at
//! `tool_runner::resolve_effective` for `propose_plan`.

use std::path::Path;

use entanglement_core::{Permission, SessionId};

use crate::capability::Capability;
use crate::permission::min_permission;
use crate::permission_path::grading_arg;
use crate::policy::{Grade, GrantStore, PermissionResolver};
use crate::rhai_trust::Trust;

/// Everything a call's grade depends on besides the call itself — what
/// [`grade_call`] composes. Borrowed so `dispatch` builds it from its own
/// arguments; [`crate::tool_runner::call_grader::CallGrader`] (#808) owns
/// the same set and lends it out, so a redirect hop grades *exactly* like a
/// dispatched call.
pub(crate) struct GradeInputs<'a> {
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
/// as resolved, with its reason (#964) — then
/// upgraded `Ask`→`Allow` by an existing mode-scoped grant. Last, a
/// `write`/`edit`/`apply_patch` of a `*.rhai` file inside a `rhai.trusted`
/// folder, or of a skill's `SKILL.md`/`lib.rhai`, is floored at `Ask`
/// whatever allowed it (ADR-0228: such a file later runs or loads unasked,
/// so authoring one is never unseen); a `Deny` stays `Deny`. The
/// escape-root gate stays with the caller.
pub(crate) async fn grade_call(
    g: &GradeInputs<'_>,
    tool: &str,
    input: &str,
) -> (Grade, Option<String>) {
    let arg = grading_arg(tool, input, g.root);
    let base = resolve_effective(g.resolver, g.chain, tool, input, g.capability_override).await;
    if let Grade::Deny(_) = base {
        return (base, arg);
    }
    let perm = apply_grant(
        g.grants,
        g.session,
        tool,
        arg.as_deref(),
        base.permission(),
        g.mode,
    );
    let perm = if perm == Permission::Allow
        && g.floor
            .is_some_and(|(trust, root)| crate::rhai_trust::floored_write(tool, input, root, trust))
    {
        Permission::Ask
    } else {
        perm
    };
    (perm.into(), arg)
}

/// The grade identity of a call: a script-facing read variant grades as the
/// tool it rides on — `read_raw` → `read` (ADR-0098), `glob_json`/`grep_json`
/// → `glob`/`grep` (ADR-0206) — so a structured-output escape hatch is never
/// a permission escape hatch. Everything else grades under its own name.
pub(crate) fn graded_name(tool: &str) -> &str {
    match tool {
        "read_raw" => "read",
        "glob_json" => "glob",
        "grep_json" => "grep",
        other => other,
    }
}

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
        // never overwrites the first one's reason.
        if min_permission(folded.permission(), grade.permission()) != folded.permission() {
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
        let resolver = per_session(&[(&child, Grade::Allow), (&parent, Grade::Ask)]);
        let chain = vec![child.clone(), parent.clone()];
        assert_eq!(
            resolve_effective(&resolver, &chain, "bash", "{}", None).await,
            Grade::Ask
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
}
