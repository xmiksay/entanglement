//! The permission-grading primitives `dispatch` composes (issue #451, split
//! out of `tool_runner.rs`): [`grade_call`] composes a call's whole grade
//! (overlay entry or mode grade, then [`apply_grant`]'s `Ask`→`Allow` off an
//! existing grant, then the trusted-script write floor), and
//! [`resolve_effective`] walks a call's ancestor chain through the pluggable
//! [`PermissionResolver`]. `resolve_effective` is re-exported at
//! `tool_runner::resolve_effective` for `propose_plan`.

use std::path::Path;

use entanglement_core::{Permission, PermissionProfile, SessionId, ToolOverlayEntry};

use crate::permission::{clamp_to_base, min_permission};
use crate::permission_path::grading_arg;
use crate::policy::{GrantStore, PermissionResolver};
use crate::rhai_trust::Trust;
use crate::tools::ToolRegistry;

/// Everything a call's grade depends on besides the call itself — what
/// [`grade_call`] composes. Borrowed so `dispatch` builds it from its own
/// arguments; [`crate::tool_runner::call_grader::CallGrader`] (#808) owns
/// the same set and lends it out, so a redirect hop grades *exactly* like a
/// dispatched call.
pub(crate) struct GradeInputs<'a> {
    pub tools: &'a ToolRegistry,
    pub resolver: &'a dyn PermissionResolver,
    pub chain: &'a [SessionId],
    pub grants: &'a dyn GrantStore,
    pub session: &'a SessionId,
    pub mode: &'a str,
    pub overlay_entry: Option<&'a ToolOverlayEntry>,
    pub ceiling: &'a PermissionProfile,
    pub root: Option<&'a Path>,
    /// The trusted folders and skill dirs plus the root a relative file-tool
    /// `path` resolves against — for the write floor (ADR-0228); `None`
    /// where the floor does not apply.
    pub floor: Option<(&'a Trust, &'a Path)>,
}

/// The grade `(tool, input)` resolves to, plus the argument it was graded
/// on: the nearest overlay enable entry's grade (ceiling-clamped) when one
/// matched, else the mode grade clamped across the ancestor chain — then
/// upgraded `Ask`→`Allow` by an existing mode-scoped grant. Last, a
/// `write`/`edit`/`apply_patch` of a `*.rhai` file inside a `rhai.trusted`
/// folder, or of a skill's `SKILL.md`/`lib.rhai`, is floored at `Ask`
/// whatever allowed it (ADR-0228: such a file later runs or loads unasked,
/// so authoring one is never unseen); a `Deny` stays `Deny`. Hooks, the escape-root gate and the overlay *deny* stay
/// with the caller.
pub(crate) async fn grade_call(
    g: &GradeInputs<'_>,
    tool: &str,
    input: &str,
) -> (Permission, Option<String>) {
    let arg = grading_arg(tool, input, g.root);
    let workdir = crate::permission::permission_workdir(tool, input);
    let base = match g.overlay_entry {
        Some(entry) => {
            let overlay_profile = crate::permission::overlay_entry_grade(tool, entry);
            let grade = crate::permission_bash::resolve_scoped_bash_aware(
                &overlay_profile,
                tool,
                arg.as_deref(),
                workdir.as_deref(),
            );
            let capabilities = crate::capability::capability_of(tool, g.tools).unwrap_or(&[]);
            clamp_to_base(
                grade,
                g.ceiling,
                tool,
                capabilities,
                arg.as_deref(),
                workdir.as_deref(),
            )
        }
        None => resolve_effective(g.resolver, g.chain, tool, input).await,
    };
    let perm = apply_grant(g.grants, g.session, tool, arg.as_deref(), base, g.mode);
    let perm = if perm == Permission::Allow
        && g.floor
            .is_some_and(|(trust, root)| crate::rhai_trust::floored_write(tool, input, root, trust))
    {
        Permission::Ask
    } else {
        perm
    };
    (perm, arg)
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
/// but defaults to `Deny` if one ever arrives.
///
/// `pub(crate)` for `propose_plan`'s own gate.
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
