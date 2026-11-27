//! Pluggable policy seams for the runtime tool executor (#311).
//!
//! [`ToolExecutor::builder`][crate::tool_runner::ToolExecutor::builder]
//! hard-codes nothing about *where* an allow/deny/ask decision or an "always
//! allow" grant comes from: it drives two trait objects, a [`PermissionResolver`]
//! and a [`GrantStore`]. The defaults are the session's permission **mode**
//! clamped by the config ceiling ([`ModeResolver`], ADR-0207 stage 4) and
//! grants held in memory ([`InMemoryGrantStore`]); nothing here touches the
//! filesystem (#966) — `skutter` persists `Always` grants in its own store. A
//! multi-tenant embedder that stores rules per user in its own DB swaps both
//! without forking the executor, keeping the shared interception ladder, spawn
//! gating, rhai, and plan/tasks tools.
//!
//! ## Where the seams sit in the ladder
//!
//! The executor asks the resolver for the grade of a *single* session, then
//! takes the least-privileged grade across the session's ancestor chain
//! ([`ancestor_chain`][crate::permission::ancestor_chain]) — the sub-agent
//! privilege ceiling (ADR-0024) stays in the ladder on top of the resolver
//! result. The `GrantStore` only ever upgrades a resolved `Ask` to `Allow`,
//! and only within the mode it was earned in (ADR-0207 §8) — a multi-tenant
//! store's "always allow" write lands in its own DB and surfaces on the
//! *next* call through its resolver, so [`is_granted`][GrantStore::is_granted]
//! covers only the session/always grants a single-user head records.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use entanglement_core::{ApprovalScope, Permission, PermissionProfile, SessionId};

use crate::capability;
use crate::mode::ModeTable;
use crate::permission::{clamp_to_base, permission_scope, permission_workdir};
use crate::permission_path::grading_arg;
use crate::tools::SharedRegistry;

mod grade;
mod memory;
pub(crate) use grade::deny_text;
pub use grade::Grade;
pub use memory::InMemoryGrantStore;

/// Decide the `Allow | Ask | Deny` grade for one concrete tool call. `session`
/// lets a multi-tenant embedder derive the tenant; `input` (the raw JSON tool
/// input) enables argument-scoped rules. A [`Grade::Deny`] may carry the
/// reason the model is told instead of the mode-deny text (#964). Called
/// once per session in a call's ancestor chain — the executor clamps the
/// results least-privilege, so a resolver need only decide a single
/// session's own grade. Async because a real embedder hits a DB; the ladder
/// already runs in a detached task.
#[async_trait]
pub trait PermissionResolver: Send + Sync {
    async fn resolve(&self, session: &SessionId, tool: &str, input: &str) -> Grade;

    /// Resolve with caller-supplied capabilities (ADR-0249 per-call grading):
    /// the same grade as [`resolve`][Self::resolve], but the tool's capability
    /// comes from the caller instead of the registry. The default delegates to
    /// `resolve` unchanged — only a resolver that grades by capability (e.g.
    /// `ModeResolver`) overrides this.
    async fn resolve_with_capabilities(
        &self,
        session: &SessionId,
        tool: &str,
        input: &str,
        _capabilities: &[crate::capability::Capability],
    ) -> Grade {
        self.resolve(session, tool, input).await
    }
}

/// Persist and read "always allow" grants (#174). A grant only ever upgrades a
/// resolved `Ask` to `Allow`, and only within the **mode** it was earned in
/// (ADR-0207 §8: a grant from `build` must not fire in `research`) — `mode` is
/// matched exactly, so the caller passes the session's *current* mode on every
/// call. The write side ([`record`][GrantStore::record]) is async because an
/// [`ApprovalScope::Always`] grant may hit a DB; the read side
/// ([`is_granted`][GrantStore::is_granted]) is a fast in-memory/cached check
/// the executor consults synchronously before prompting. A multi-tenant store
/// writes an "always" rule to its DB and resolves later reads through its
/// [`PermissionResolver`] instead, so its `is_granted` can simply return `false`.
#[async_trait]
pub trait GrantStore: Send + Sync {
    /// Whether `(tool, arg)` from `session`, earned under `mode`, is already
    /// granted (session or always), upgrading a resolved `Ask` to `Allow`.
    fn is_granted(&self, session: &SessionId, tool: &str, arg: Option<&str>, mode: &str) -> bool;
    /// Record an approval per its scope, tagged with the mode it was earned
    /// in. `Once` records nothing; `Session` is in-memory; `Always` lasts as
    /// long as the store keeps it (memory for the default, a file for
    /// `skutter`, a DB row for a multi-tenant store).
    async fn record(
        &self,
        session: &SessionId,
        tool: &str,
        arg: Option<&str>,
        scope: ApprovalScope,
        mode: &str,
    );
    /// Release a session's in-memory grants when it ends.
    fn forget_session(&self, session: &SessionId);
}

/// The single-user CLI resolver (ADR-0207 stage 4): grades a call from the
/// session's **mode**, not its agent profile — `Agent` no longer
/// carries any permission fact. Looks up the mode name in the folded `modes`
/// map (mirrors `OutEvent::ModeChanged` the way `active` mirrors
/// `AgentChanged`), resolves it against `table`, reads the tool's declared
/// [`capability::Capability`] set from the live registry, and calls
/// [`Mode::resolve`][crate::mode::Mode::resolve] with the call's
/// argument/workdir — then clamps to the config permission ceiling (#172),
/// unchanged from before this stage. `table` is always
/// [`ModeTable::builtin`] for `skutter`; an embedder supplies its own via
/// [`ModeTable::new`]. Config `modes:` tuning is a later stage's wiring.
///
/// An **unseen session fails closed** (`Permission::Deny`, #156): a session
/// whose `ModeChanged` broadcast was dropped under overload must never
/// resolve to allow-all. `root` (#485, ADR-0125) is the project root a
/// path-arg tool's argument is normalized relative to before matching an
/// arg-scoped rule — `None` (the test-only executor wrappers) keeps the
/// pre-#485 verbatim match.
pub struct ModeResolver {
    modes: Arc<Mutex<HashMap<SessionId, String>>>,
    table: Arc<ModeTable>,
    registry: SharedRegistry,
    base: PermissionProfile,
    root: Option<PathBuf>,
}

impl ModeResolver {
    pub fn new(
        modes: Arc<Mutex<HashMap<SessionId, String>>>,
        table: Arc<ModeTable>,
        registry: SharedRegistry,
        base: PermissionProfile,
        root: Option<PathBuf>,
    ) -> Self {
        Self {
            modes,
            table,
            registry,
            base,
            root,
        }
    }
}

impl ModeResolver {
    fn grade(
        &self,
        session: &SessionId,
        tool: &str,
        input: &str,
        caps: &[capability::Capability],
    ) -> Permission {
        let (arg, kind, workdir) = {
            let registry = self.registry.read().expect("tool registry lock poisoned");
            (
                grading_arg(&registry, tool, input, self.root.as_deref()),
                permission_scope(&registry, tool).arg,
                permission_workdir(&registry, tool, input),
            )
        };
        // Fail-closed (#156, carried into ADR-0207): a session whose
        // `ModeChanged` broadcast was dropped is unseen here, and an unseen
        // session must never resolve to allow-all.
        let mode_name = {
            let modes = self.modes.lock().expect("mode mutex poisoned");
            modes.get(session).cloned()
        };
        let Some(mode_name) = mode_name else {
            return Permission::Deny;
        };
        let Some(mode) = self.table.get(&mode_name) else {
            // Defense in depth, not a reachable path for a built-in table:
            // `InMsg::SetMode` is the only writer of a session's mode, and a
            // real head validates it against the same table before sending.
            tracing::warn!(%session, mode = %mode_name, "unknown permission mode; denying");
            return Permission::Deny;
        };
        let own = mode.resolve(tool, caps, arg.as_deref(), kind, workdir.as_deref());
        clamp_to_base(
            own,
            &self.base,
            tool,
            caps,
            arg.as_deref(),
            kind,
            workdir.as_deref(),
        )
    }
}

#[async_trait]
impl PermissionResolver for ModeResolver {
    async fn resolve(&self, session: &SessionId, tool: &str, input: &str) -> Grade {
        let caps = {
            let registry = self.registry.read().expect("tool registry lock poisoned");
            capability::capability_of(tool, &registry).unwrap_or(&[])
        };
        self.grade(session, tool, input, caps).into()
    }

    async fn resolve_with_capabilities(
        &self,
        session: &SessionId,
        tool: &str,
        input: &str,
        capabilities: &[capability::Capability],
    ) -> Grade {
        self.grade(session, tool, input, capabilities).into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mode::{Limits, Mode, Rules};
    use crate::tools::ToolRegistry;
    use std::sync::RwLock;

    /// A single-mode table carrying a `read(src/*)` scoped rule, the mode
    /// resolver's counterpart of the old `build_profile_with_scoped_read`
    /// `Agent` fixture — `ModeResolver` grades from the session's
    /// mode now, so the fixture is a `Mode`, not a profile.
    fn table_with_scoped_read() -> Arc<ModeTable> {
        let mode = Mode {
            name: "test".to_string(),
            default: Permission::Ask,
            rules: Rules::from_lists(&[], &["peek(src/*)".to_string()], &[]),
            limits: Limits::default(),
            sandbox: None,
            sandbox_network: false,
        };
        Arc::new(ModeTable::new(vec![mode]).expect("single-mode table is valid"))
    }

    fn modes_map(session: &SessionId) -> Arc<Mutex<HashMap<SessionId, String>>> {
        Arc::new(Mutex::new(HashMap::from([(
            session.clone(),
            "test".to_string(),
        )])))
    }

    fn empty_registry() -> SharedRegistry {
        Arc::new(RwLock::new(ToolRegistry::new()))
    }

    fn host_registry() -> SharedRegistry {
        crate::test_tools::stub_tools().shared()
    }

    /// #485, ADR-0125: an absolute path resolving inside a wired `root` must
    /// grade identically to its root-relative spelling — regression pin for the
    /// bug (an arg-scoped rule authored root-relative silently fell through to
    /// the profile default for the absolute form).
    #[tokio::test]
    async fn resolve_matches_an_absolute_in_root_path_when_root_is_wired() {
        let session = SessionId::new("s1");
        let resolver = ModeResolver::new(
            modes_map(&session),
            table_with_scoped_read(),
            host_registry(),
            PermissionProfile::new(Permission::Allow),
            Some(PathBuf::from("/r")),
        );
        assert_eq!(
            resolver
                .resolve(&session, "peek", r#"{"path":"/r/src/main.rs"}"#)
                .await,
            Grade::Allow
        );
        // The relative spelling already worked pre-#485 — must stay identical.
        assert_eq!(
            resolver
                .resolve(&session, "peek", r#"{"path":"src/main.rs"}"#)
                .await,
            Grade::Allow
        );
    }

    /// With no root wired (the test-only executor wrappers), the absolute
    /// spelling stays verbatim and therefore falls through to the profile
    /// default — byte-identical to pre-#485 behavior.
    #[tokio::test]
    async fn resolve_does_not_relativize_without_a_wired_root() {
        let session = SessionId::new("s1");
        let resolver = ModeResolver::new(
            modes_map(&session),
            table_with_scoped_read(),
            host_registry(),
            PermissionProfile::new(Permission::Allow),
            None,
        );
        assert_eq!(
            resolver
                .resolve(&session, "peek", r#"{"path":"/r/src/main.rs"}"#)
                .await,
            Grade::Ask
        );
    }

    /// ADR-0207 stage 4: a session whose mode was never folded (a dropped
    /// `ModeChanged`) fails closed — mirroring the pre-stage-4 unseen-profile
    /// behavior, never allow-all.
    #[tokio::test]
    async fn resolve_denies_an_unseen_session() {
        let session = SessionId::new("s1");
        let resolver = ModeResolver::new(
            Arc::new(Mutex::new(HashMap::new())),
            table_with_scoped_read(),
            empty_registry(),
            PermissionProfile::new(Permission::Allow),
            None,
        );
        assert_eq!(
            resolver.resolve(&session, "peek", r#"{"path":"x"}"#).await,
            Grade::Deny(None)
        );
    }

    /// #962: an embedder tool that declares a path argument is the target
    /// of `tool(path)` rules, graded root-relative like a host tool — no
    /// name list to join.
    #[tokio::test]
    async fn an_embedder_path_tool_is_the_target_of_scoped_rules() {
        use crate::tools::{ArgKind, PermissionScope, Tool};
        struct Doc;
        #[async_trait]
        impl Tool for Doc {
            fn name(&self) -> std::borrow::Cow<'static, str> {
                "doc".into()
            }
            async fn run(&self, _input: &str) -> anyhow::Result<String> {
                Ok(String::new())
            }
            fn permission_scope(&self) -> PermissionScope {
                PermissionScope::arg(ArgKind::Path)
            }
            fn permission_arg(&self, input: &str) -> Option<String> {
                crate::path_grading::str_arg(input, "file")
            }
        }
        let mode = Mode {
            name: "test".to_string(),
            default: Permission::Ask,
            rules: Rules::from_lists(&[], &["doc(notes/*)".to_string()], &[]),
            limits: Limits::default(),
            sandbox: None,
            sandbox_network: false,
        };
        let table = Arc::new(ModeTable::new(vec![mode]).expect("single-mode table is valid"));
        let mut registry = ToolRegistry::new();
        registry.register(Doc);
        let session = SessionId::new("s1");
        let resolver = ModeResolver::new(
            modes_map(&session),
            table,
            registry.shared(),
            PermissionProfile::new(Permission::Allow),
            Some(PathBuf::from("/r")),
        );
        let grade = |input: &'static str| resolver.resolve(&session, "doc", input);
        assert_eq!(grade(r#"{"file":"/r/notes/a.md"}"#).await, Grade::Allow);
        assert_eq!(grade(r#"{"file":"src/a.rs"}"#).await, Grade::Ask);
    }
}
