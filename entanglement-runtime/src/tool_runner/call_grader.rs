//! A dispatched call's own grading context, reachable from inside the tool
//! while it runs (#808, ADR-0225). One tool call can reach targets beyond
//! its argument — `web_fetch` follows redirects — and each such target must
//! grade exactly like a fresh call would. The executor scopes a
//! [`CallGrader`] around every graded dispatch as a tokio task-local (the
//! same mechanism `file_backup::with_exec_seq` uses), so the `Tool` trait's
//! signature stays untouched for the many tools that never need it.
//!
//! Outside an executor (a tool called directly, an embedder's own loop)
//! there is no grader and [`grade_in_scope`] returns `None`: the initial
//! call wasn't graded there either, so nothing is lost.

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

use entanglement_core::{Permission, PermissionProfile, SessionId, ToolOverlayEntry};

use super::dispatch::grade::{grade_call, GradeInputs};
use crate::policy::{GrantStore, PermissionResolver};
use crate::tools::ToolRegistry;

/// The owned twin of [`GradeInputs`], plus the overlay *deny* the
/// dispatcher checks before grading.
pub(crate) struct CallGrader {
    pub tools: ToolRegistry,
    pub resolver: Arc<dyn PermissionResolver>,
    pub chain: Vec<SessionId>,
    pub grants: Arc<dyn GrantStore>,
    pub session: SessionId,
    pub mode: String,
    pub overlay_entry: Option<ToolOverlayEntry>,
    pub overlay_denied: bool,
    pub ceiling: PermissionProfile,
    pub root: Option<PathBuf>,
    /// The call is a nested call of a trusted or **Run all** script
    /// (ADR-0228): a target it reaches mid-call (a redirect hop) passes an
    /// `Ask` exactly as the call itself did. Denies still win.
    pub run_all: bool,
}

impl CallGrader {
    /// Grade `(tool, input)` as `dispatch` would grade the same call, minus
    /// what can't apply mid-call (hooks, the escape-root gate, parking).
    pub(crate) async fn grade(&self, tool: &str, input: &str) -> Permission {
        if self.overlay_denied {
            return Permission::Deny;
        }
        let inputs = GradeInputs {
            tools: &self.tools,
            resolver: &*self.resolver,
            chain: &self.chain,
            grants: &*self.grants,
            session: &self.session,
            mode: &self.mode,
            overlay_entry: self.overlay_entry.as_ref(),
            ceiling: &self.ceiling,
            root: self.root.as_deref(),
            // A mid-call target is a fetch, never a file write.
            floor: None,
        };
        match grade_call(&inputs, tool, input).await.0 {
            Permission::Ask if self.run_all => Permission::Allow,
            grade => grade,
        }
    }

    pub(crate) fn mode(&self) -> &str {
        &self.mode
    }
}

tokio::task_local! {
    static CALL_GRADER: Arc<CallGrader>;
}

/// Run `f` with `grader` reachable via [`grade_in_scope`].
pub(crate) async fn scope<F: Future>(grader: CallGrader, f: F) -> F::Output {
    CALL_GRADER.scope(Arc::new(grader), f).await
}

/// Grade `(tool, input)` against the running call's context, with the
/// session's mode name for messages; `None` when no executor scoped one.
pub async fn grade_in_scope(tool: &str, input: &str) -> Option<(Permission, String)> {
    let grader = CALL_GRADER.try_with(Arc::clone).ok()?;
    let grade = grader.grade(tool, input).await;
    Some((grade, grader.mode().to_string()))
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    /// A grader over a single-mode table, for tests that drive a tool
    /// directly but need the in-call grading seam.
    pub(crate) fn grader_for(mode: crate::mode::Mode, tools: ToolRegistry) -> CallGrader {
        let name = mode.name.clone();
        let table = Arc::new(crate::mode::ModeTable::new(vec![mode]).expect("one mode"));
        let session = SessionId::new("s");
        let modes = Arc::new(std::sync::Mutex::new(
            [(session.clone(), name.clone())].into_iter().collect(),
        ));
        CallGrader {
            resolver: Arc::new(crate::policy::ModeResolver::new(
                modes,
                table,
                tools.clone().shared(),
                PermissionProfile::new(Permission::Allow),
                None,
            )),
            tools,
            chain: vec![session.clone()],
            grants: Arc::new(crate::policy::DefaultGrantStore::load_from(None)),
            session,
            mode: name,
            overlay_entry: None,
            overlay_denied: false,
            ceiling: PermissionProfile::new(Permission::Allow),
            root: None,
            run_all: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::grader_for;
    use super::*;
    use crate::mode::{Limits, Mode, Rules};

    fn mode() -> Mode {
        Mode {
            name: "research".into(),
            default: Permission::Ask,
            rules: Rules::from_lists(&["bash(rm*)".into()], &["bash(ls*)".into()], &[]),
            limits: Limits::default(),
            sandbox: None,
            sandbox_network: false,
        }
    }

    async fn grade(cmd: &str) -> Option<(Permission, String)> {
        grade_in_scope("bash", &format!(r#"{{"command":"{cmd}"}}"#)).await
    }

    #[tokio::test]
    async fn grades_like_dispatch_inside_a_scope_and_is_absent_outside() {
        assert_eq!(grade("ls").await, None, "no executor scope");
        let graded = scope(grader_for(mode(), ToolRegistry::new()), async {
            (
                grade("rm -rf x").await,
                grade("ls -la").await,
                grade("make").await,
            )
        })
        .await;
        let research = || "research".to_string();
        assert_eq!(graded.0, Some((Permission::Deny, research())));
        assert_eq!(graded.1, Some((Permission::Allow, research())));
        assert_eq!(graded.2, Some((Permission::Ask, research())));
    }

    /// A trusted / **Run all** script's call passes an `Ask` hop like the
    /// call itself did; a `Deny` and an overlay deny still stop it.
    #[tokio::test]
    async fn run_all_passes_an_ask_but_never_a_deny() {
        let mut grader = grader_for(mode(), ToolRegistry::new());
        grader.run_all = true;
        let graded = scope(grader, async { (grade("make").await, grade("rm x").await) }).await;
        assert_eq!(graded.0.map(|g| g.0), Some(Permission::Allow));
        assert_eq!(graded.1.map(|g| g.0), Some(Permission::Deny));
        let mut grader = grader_for(mode(), ToolRegistry::new());
        (grader.run_all, grader.overlay_denied) = (true, true);
        let graded = scope(grader, grade("make")).await;
        assert_eq!(graded.map(|g| g.0), Some(Permission::Deny));
    }

    #[tokio::test]
    async fn an_overlay_deny_refuses_every_target() {
        let mut grader = grader_for(mode(), ToolRegistry::new());
        grader.overlay_denied = true;
        let graded = scope(grader, grade("ls")).await;
        assert_eq!(graded.map(|g| g.0), Some(Permission::Deny));
    }
}
