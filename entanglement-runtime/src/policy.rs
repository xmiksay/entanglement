//! Pluggable policy seams for the runtime tool executor (#311).
//!
//! [`spawn_tool_executor_with_policy`][crate::tool_runner::spawn_tool_executor_with_policy]
//! hard-codes nothing about *where* an allow/deny/ask decision or an "always
//! allow" grant comes from: it drives two trait objects, a [`PermissionResolver`]
//! and a [`GrantStore`]. The single-user CLI plugs in the defaults below — the
//! session's permission **mode** clamped by the config ceiling
//! ([`ProfileResolver`], ADR-0207 stage 4) and the managed grants file
//! ([`DefaultGrantStore`]). A multi-tenant embedder that stores rules per user
//! in its own DB swaps both without forking the executor, keeping the shared
//! interception ladder, spawn gating, hooks, rhai, and plan/tasks tools.
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
//! covers only the default file/session grants the CLI needs.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use entanglement_core::{ApprovalScope, Permission, PermissionProfile, SessionId};

use crate::capability;
use crate::grants::FileGrantStore;
use crate::host::SandboxPolicy;
use crate::mode::ModeTable;
use crate::permission::{clamp_to_base, permission_workdir};
use crate::permission_path::grading_arg;
use crate::tools::SharedRegistry;

/// Decide the `Allow | Ask | Deny` grade for one concrete tool call. `session`
/// lets a multi-tenant embedder derive the tenant; `input` (the raw JSON tool
/// input) enables argument-scoped rules. Called once per session in a call's
/// ancestor chain — the executor clamps the results least-privilege, so a
/// resolver need only decide a single session's own grade. Async because a real
/// embedder hits a DB; the ladder already runs in a detached task.
#[async_trait]
pub trait PermissionResolver: Send + Sync {
    async fn resolve(&self, session: &SessionId, tool: &str, input: &str) -> Permission;
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
    /// in. `Once` records nothing; `Session` is in-memory; `Always` persists
    /// (a file for the default, a DB row for a multi-tenant store).
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

    /// Grant an explicit directory to `session`, covering the read-only triad
    /// (`read`/`grep`/`glob`) for the rest of the session under `mode` only
    /// (#486, ADR-0126; mode-scoped by #634) — the TUI `/allow <path>`
    /// command's entry point. Synchronous and never persisted (unlike
    /// `Always` scope above), so no DB round-trip is needed. Default no-op
    /// that just echoes `dir` back unnormalized, so an embedder's custom
    /// `GrantStore` (`tests/policy_seam.rs`) keeps compiling without wiring
    /// directory grants; only `DefaultGrantStore` (the TUI's store) overrides
    /// it for real.
    fn grant_session_dir(&self, session: &SessionId, dir: &str, mode: &str) -> String {
        let _ = (session, mode);
        dir.to_string()
    }
}

/// The single-user CLI resolver (ADR-0207 stage 4): grades a call from the
/// session's **mode**, not its agent profile — `AgentProfile` no longer
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
pub struct ProfileResolver {
    modes: Arc<Mutex<HashMap<SessionId, String>>>,
    table: Arc<ModeTable>,
    registry: SharedRegistry,
    base: PermissionProfile,
    root: Option<PathBuf>,
}

impl ProfileResolver {
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

#[async_trait]
impl PermissionResolver for ProfileResolver {
    async fn resolve(&self, session: &SessionId, tool: &str, input: &str) -> Permission {
        let arg = grading_arg(tool, input, self.root.as_deref());
        let workdir = permission_workdir(tool, input);
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
        let capabilities = {
            let registry = self.registry.read().expect("tool registry lock poisoned");
            capability::capability_of(tool, &registry).unwrap_or(&[])
        };
        let own = mode.resolve(tool, capabilities, arg.as_deref(), workdir.as_deref());
        clamp_to_base(
            own,
            &self.base,
            tool,
            capabilities,
            arg.as_deref(),
            workdir.as_deref(),
        )
    }
}

/// Resolve the confinement policy `bash`/`call` run a session's commands under
/// (ADR-0207 §6, stage 5b amendment of ADR-0104). Sync and infallible —
/// unlike permission there is no `Ask` round-trip and no DB lookup a real
/// embedder would need to await; a tenant that wants per-tenant sandboxing
/// swaps this the same way it would [`PermissionResolver`]. `session: None`
/// is the plain [`crate::tools::Tool::run`] path (no live session to resolve
/// against — standalone use, most unit tests).
pub trait SandboxResolver: Send + Sync {
    fn resolve(&self, session: Option<&SessionId>) -> SandboxPolicy;
}

/// A fixed policy is trivially its own resolver — the `.with_sandbox(policy)`
/// builder `BashTool`/`CallTool` already had keeps working unchanged, now
/// backed by `Arc<dyn SandboxResolver>` internally (#479).
impl SandboxResolver for SandboxPolicy {
    fn resolve(&self, _session: Option<&SessionId>) -> SandboxPolicy {
        *self
    }
}

/// The single-user CLI resolver (ADR-0207 §6, stage 5b): sandboxing is a
/// **mode** fact now, not a per-profile one, and a mode applies to its whole
/// spawn sub-tree — so there is no more per-session ancestor floor to freeze
/// at spawn (ADR-0104's amendment retired ADR-0134's scoping entirely).
/// Reads the same session→mode map [`ProfileResolver`] grades permission
/// from, resolves it against `table` exactly like `ProfileResolver` does, and
/// derives the mode's [`SandboxPolicy`][crate::host::SandboxPolicy] via
/// [`crate::mode::Mode::sandbox_policy`] — then layers `base` (the
/// process-global `ENTANGLEMENT_SANDBOX`/`ENTANGLEMENT_SANDBOX_NETWORK`
/// env default) on top via `most_confined`, so the env may only **tighten**
/// what the mode declares, never loosen it. An unseen session (never folded,
/// or an unknown mode name) falls back to `base` alone: sandboxing is defense
/// in depth on top of the permission gate, not the gate itself, so this does
/// not fail-closed to maximum confinement the way [`ProfileResolver::resolve`]
/// fails closed to `Deny`.
pub struct ModeSandboxResolver {
    modes: Arc<Mutex<HashMap<SessionId, String>>>,
    table: Arc<crate::mode::ModeTable>,
    base: SandboxPolicy,
}

impl ModeSandboxResolver {
    pub fn new(
        modes: Arc<Mutex<HashMap<SessionId, String>>>,
        table: Arc<crate::mode::ModeTable>,
        base: SandboxPolicy,
    ) -> Self {
        Self { modes, table, base }
    }
}

impl SandboxResolver for ModeSandboxResolver {
    fn resolve(&self, session: Option<&SessionId>) -> SandboxPolicy {
        let Some(session) = session else {
            return self.base;
        };
        let mode_name = {
            let modes = self.modes.lock().expect("mode mutex poisoned");
            modes.get(session).cloned()
        };
        let Some(mode) = mode_name.as_deref().and_then(|n| self.table.get(n)) else {
            return self.base;
        };
        mode.sandbox_policy().most_confined(self.base)
    }
}

/// Bundled per-process sandbox state (ADR-0207 §6, stage 5b): the same
/// session→mode map and [`crate::mode::ModeTable`] the executor's
/// `ProfileResolver` grades permission from, plus the process-global default
/// an unseen session falls back to. Grouped into one value so a caller that
/// doesn't care about mode-scoped sandboxing — every test helper, the
/// `embedded` example — passes a single [`SandboxConfig::none`].
#[derive(Clone)]
pub struct SandboxConfig {
    pub base: SandboxPolicy,
    pub modes: Arc<Mutex<HashMap<SessionId, String>>>,
    pub table: Arc<crate::mode::ModeTable>,
}

impl SandboxConfig {
    /// Every call unsandboxed, no per-mode overrides — an empty mode table
    /// means every lookup falls back to `base` (unconfined).
    pub fn none() -> Self {
        Self {
            base: SandboxPolicy::none(),
            modes: Arc::new(Mutex::new(HashMap::new())),
            table: Arc::new(crate::mode::ModeTable::new(Vec::new()).expect("empty table is valid")),
        }
    }

    /// The real single-user wiring: `base` from `ENTANGLEMENT_SANDBOX`/
    /// `ENTANGLEMENT_SANDBOX_NETWORK`, sharing the *same* `modes` map and
    /// mode `table` the caller's `ProfileResolver` uses — sandboxing must see
    /// exactly the mode permission dispatch sees, not a second copy that can
    /// drift.
    pub fn new(
        modes: Arc<Mutex<HashMap<SessionId, String>>>,
        table: Arc<crate::mode::ModeTable>,
    ) -> Self {
        Self {
            base: SandboxPolicy::from_env(),
            modes,
            table,
        }
    }

    /// The resolver `BashTool`/`CallTool` consult per call (#479, ADR-0207
    /// §6).
    pub fn resolver(&self) -> Arc<dyn SandboxResolver> {
        Arc::new(ModeSandboxResolver::new(
            self.modes.clone(),
            self.table.clone(),
            self.base,
        ))
    }
}

/// The single-user CLI grant store: the managed [`FileGrantStore`] behind a
/// `Mutex` so the shared trait object can record and read grants. An `Always`
/// grant persists to `${config_dir}/entanglement/grants.yml`.
pub struct DefaultGrantStore {
    inner: Mutex<FileGrantStore>,
}

impl DefaultGrantStore {
    /// Load the persisted `Always` grants from the managed file.
    pub fn load() -> Self {
        Self {
            inner: Mutex::new(FileGrantStore::load()),
        }
    }

    fn grants(&self) -> std::sync::MutexGuard<'_, FileGrantStore> {
        self.inner.lock().expect("grants mutex poisoned")
    }

    /// Re-read the persisted `Always` grants from disk (#329) — the watcher's
    /// hook for picking up a grant another skutter instance recorded, without
    /// disturbing this process's in-memory `Session`-scoped grants.
    pub fn reload(&self) {
        self.grants().reload();
    }
}

#[async_trait]
impl GrantStore for DefaultGrantStore {
    fn is_granted(&self, session: &SessionId, tool: &str, arg: Option<&str>, mode: &str) -> bool {
        self.grants().is_granted(session, tool, arg, mode)
    }

    async fn record(
        &self,
        session: &SessionId,
        tool: &str,
        arg: Option<&str>,
        scope: ApprovalScope,
        mode: &str,
    ) {
        self.grants().record(session, tool, arg, scope, mode);
    }

    fn forget_session(&self, session: &SessionId) {
        self.grants().forget_session(session);
    }

    fn grant_session_dir(&self, session: &SessionId, dir: &str, mode: &str) -> String {
        self.grants().grant_session_dir(session, dir, mode)
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
    /// `AgentProfile` fixture — `ProfileResolver` grades from the session's
    /// mode now, so the fixture is a `Mode`, not a profile.
    fn table_with_scoped_read() -> Arc<ModeTable> {
        let mode = Mode {
            name: "test".to_string(),
            default: Permission::Ask,
            rules: Rules::from_lists(&[], &["read(src/*)".to_string()], &[]),
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

    /// #485, ADR-0125: an absolute path resolving inside a wired `root` must
    /// grade identically to its root-relative spelling — regression pin for the
    /// bug (an arg-scoped rule authored root-relative silently fell through to
    /// the profile default for the absolute form).
    #[tokio::test]
    async fn resolve_matches_an_absolute_in_root_path_when_root_is_wired() {
        let session = SessionId::new("s1");
        let resolver = ProfileResolver::new(
            modes_map(&session),
            table_with_scoped_read(),
            empty_registry(),
            PermissionProfile::new(Permission::Allow),
            Some(PathBuf::from("/r")),
        );
        assert_eq!(
            resolver
                .resolve(&session, "read", r#"{"path":"/r/src/main.rs"}"#)
                .await,
            Permission::Allow
        );
        // The relative spelling already worked pre-#485 — must stay identical.
        assert_eq!(
            resolver
                .resolve(&session, "read", r#"{"path":"src/main.rs"}"#)
                .await,
            Permission::Allow
        );
    }

    /// With no root wired (the test-only executor wrappers), the absolute
    /// spelling stays verbatim and therefore falls through to the profile
    /// default — byte-identical to pre-#485 behavior.
    #[tokio::test]
    async fn resolve_does_not_relativize_without_a_wired_root() {
        let session = SessionId::new("s1");
        let resolver = ProfileResolver::new(
            modes_map(&session),
            table_with_scoped_read(),
            empty_registry(),
            PermissionProfile::new(Permission::Allow),
            None,
        );
        assert_eq!(
            resolver
                .resolve(&session, "read", r#"{"path":"/r/src/main.rs"}"#)
                .await,
            Permission::Ask
        );
    }

    /// ADR-0207 stage 4: a session whose mode was never folded (a dropped
    /// `ModeChanged`) fails closed — mirroring the pre-stage-4 unseen-profile
    /// behavior, never allow-all.
    #[tokio::test]
    async fn resolve_denies_an_unseen_session() {
        let session = SessionId::new("s1");
        let resolver = ProfileResolver::new(
            Arc::new(Mutex::new(HashMap::new())),
            table_with_scoped_read(),
            empty_registry(),
            PermissionProfile::new(Permission::Allow),
            None,
        );
        assert_eq!(
            resolver.resolve(&session, "read", r#"{"path":"x"}"#).await,
            Permission::Deny
        );
    }

    /// A `Mode` fixture with a given sandbox posture, otherwise a bare
    /// pass-through — the sandbox tests below only ever read `sandbox`/
    /// `sandbox_network` through `Mode::sandbox_policy`.
    fn mode_with_sandbox(sandbox: Option<&str>, sandbox_network: bool) -> Mode {
        Mode {
            name: "test".to_string(),
            default: Permission::Allow,
            rules: Rules::default(),
            limits: Limits::default(),
            sandbox: sandbox.map(str::to_string),
            sandbox_network,
        }
    }

    fn sandbox_cfg(mode: Mode, base: SandboxPolicy, session: &SessionId) -> SandboxConfig {
        SandboxConfig {
            base,
            modes: modes_map(session),
            table: Arc::new(ModeTable::new(vec![mode]).expect("single-mode table is valid")),
        }
    }

    const CONFINED: SandboxPolicy = SandboxPolicy {
        backend: crate::host::SandboxBackend::Bubblewrap,
        network: false,
    };

    /// #479: an unseen session (never folded from a lifecycle event, or an
    /// unknown mode name) falls back to the process-global default — unlike
    /// permission's fail-closed `Deny`, sandboxing is defense in depth, not
    /// the gate itself.
    #[test]
    fn sandbox_resolver_falls_back_to_base_for_an_unseen_session() {
        let cfg = SandboxConfig {
            base: CONFINED,
            ..SandboxConfig::none()
        };
        let resolver = cfg.resolver();
        assert_eq!(resolver.resolve(Some(&SessionId::new("ghost"))), CONFINED);
    }

    /// ADR-0207 §6, stage 5b: the session's mode declares the sandbox
    /// posture — a `bwrap` mode confines even when `ENTANGLEMENT_SANDBOX` is
    /// unset (`base` unconfined).
    #[test]
    fn sandbox_resolver_reads_the_session_mode() {
        let session = SessionId::new("s1");
        let cfg = sandbox_cfg(
            mode_with_sandbox(Some("bwrap"), false),
            SandboxPolicy::none(),
            &session,
        );
        assert_eq!(cfg.resolver().resolve(Some(&session)), CONFINED);
    }

    /// ADR-0207 §6, stage 5b: env may only *tighten* what the mode declares,
    /// never loosen it — a `bwrap` mode stays confined even if
    /// `ENTANGLEMENT_SANDBOX` is unset, and an unconfined mode picks up a
    /// confined env base (env tightening an otherwise-open mode).
    #[test]
    fn env_base_only_tightens_never_loosens_the_mode() {
        let session = SessionId::new("s1");
        // Mode confines, env base is unconfined: still confined.
        let confining_mode = sandbox_cfg(
            mode_with_sandbox(Some("bwrap"), false),
            SandboxPolicy::none(),
            &session,
        );
        assert_eq!(confining_mode.resolver().resolve(Some(&session)), CONFINED);

        // Mode is unconfined, env base confines: still confined (env tightens).
        let open_mode = sandbox_cfg(mode_with_sandbox(None, false), CONFINED, &session);
        assert_eq!(open_mode.resolver().resolve(Some(&session)), CONFINED);
    }

    /// ADR-0207 §6, stage 5b: a mode's own `sandbox_network: true` shares the
    /// host network namespace under confinement — the network-sharing rank
    /// sits strictly between unconfined and network-cut confinement.
    #[test]
    fn mode_sandbox_network_shares_the_host_namespace() {
        let session = SessionId::new("s1");
        let cfg = sandbox_cfg(
            mode_with_sandbox(Some("bwrap"), true),
            SandboxPolicy::none(),
            &session,
        );
        let resolved = cfg.resolver().resolve(Some(&session));
        assert_eq!(resolved.backend, crate::host::SandboxBackend::Bubblewrap);
        assert!(resolved.network, "sandbox_network: true shares the network");
    }
}
