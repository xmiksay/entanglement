//! Pluggable policy seams for the runtime tool executor (#311).
//!
//! [`spawn_tool_executor_with_policy`][crate::tool_runner::spawn_tool_executor_with_policy]
//! hard-codes nothing about *where* an allow/deny/ask decision or an "always
//! allow" grant comes from: it drives two trait objects, a [`PermissionResolver`]
//! and a [`GrantStore`]. The single-user CLI plugs in the defaults below — the
//! agent-profile chain clamped by the config ceiling ([`ProfileResolver`]) and
//! the managed grants file ([`DefaultGrantStore`]) — so its behavior is
//! byte-identical. A multi-tenant embedder that stores rules per user in its own
//! DB swaps both without forking the ~350-line executor, keeping the shared
//! interception ladder, spawn/mask gating, hooks, rhai, and plan/tasks tools.
//!
//! ## Where the seams sit in the ladder
//!
//! The executor asks the resolver for the grade of a *single* session, then takes
//! the least-privileged grade across the session's ancestor chain
//! ([`ancestor_chain`][crate::permission::ancestor_chain]) — so the sub-agent
//! privilege ceiling (ADR-0024) and spawn/mask gating stay in the ladder **on top
//! of** the resolver result. A tenant rule can widen or narrow a session's own
//! grade, but can never widen a child beyond its parent. The `GrantStore` only
//! ever upgrades a resolved `Ask` to `Allow`; a multi-tenant store's "always
//! allow" write lands in its own DB and surfaces on the *next* call through its
//! resolver, so the trait's read side is deliberately the resolver's job — the
//! store's own [`is_granted`][GrantStore::is_granted] covers only the default
//! file/session grants the CLI needs.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use entanglement_core::{AgentProfile, ApprovalScope, Permission, PermissionProfile, SessionId};

use crate::bash_live::LiveBashState;
use crate::grants::FileGrantStore;
use crate::host::SandboxPolicy;
use crate::permission::{clamp_to_base, permission_for, permission_workdir};
use crate::permission_path::grading_arg;

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
/// resolved `Ask` to `Allow`. The write side ([`record`][GrantStore::record])
/// is async because an [`ApprovalScope::Always`] grant may hit a DB; the read
/// side ([`is_granted`][GrantStore::is_granted]) is a fast in-memory/cached check
/// the executor consults synchronously before prompting. A multi-tenant store
/// writes an "always" rule to its DB and resolves later reads through its
/// [`PermissionResolver`] instead, so its `is_granted` can simply return `false`.
#[async_trait]
pub trait GrantStore: Send + Sync {
    /// Whether `(tool, arg)` from `session` is already granted (session or
    /// always), upgrading a resolved `Ask` to `Allow`.
    fn is_granted(&self, session: &SessionId, tool: &str, arg: Option<&str>) -> bool;
    /// Record an approval per its scope. `Once` records nothing; `Session` is
    /// in-memory; `Always` persists (a file for the default, a DB row for a
    /// multi-tenant store).
    async fn record(
        &self,
        session: &SessionId,
        tool: &str,
        arg: Option<&str>,
        scope: ApprovalScope,
    );
    /// Release a session's in-memory grants when it ends.
    fn forget_session(&self, session: &SessionId);

    /// Grant an explicit directory to `session`, covering the read-only triad
    /// (`read`/`grep`/`glob`) for the rest of the session (#486, ADR-0126) —
    /// the TUI `/allow <path>` command's entry point. Synchronous and never
    /// persisted (unlike `Always` scope above), so no DB round-trip is
    /// needed. Default no-op that just echoes `dir` back unnormalized, so an
    /// embedder's custom `GrantStore` (`tests/policy_seam.rs`) keeps
    /// compiling without wiring directory grants; only `DefaultGrantStore`
    /// (the TUI's store) overrides it for real.
    fn grant_session_dir(&self, session: &SessionId, dir: &str) -> String {
        let _ = session;
        dir.to_string()
    }
}

/// The single-user CLI resolver: the executor's live active-profile map plus the
/// config permission ceiling (#172). Resolves a session's *own* profile grade
/// clamped by the base ceiling; the executor mins this across the ancestor chain
/// for the sub-agent clamp, so the pair reproduces `effective_permission` +
/// `clamp_to_base` exactly (the clamp is monotonic, so min-of-clamped equals
/// clamp-of-min). Shares the same `Arc<Mutex<..>>` the executor folds lifecycle
/// events into, so it always reads the current profile view. `root` (#485,
/// ADR-0125) is the project root a path-arg tool's argument is normalized
/// relative to before matching an arg-scoped rule — `None` (the test-only
/// executor wrappers) keeps the pre-#485 verbatim match. `live_bash` (#498,
/// ADR-0133) is `None` for a caller that never wires live bash enablement
/// (byte-identical to pre-#498 behavior); when `Some`, a live grade overrides
/// the session's own profile for `bash`/`bash_output` specifically — see
/// [`resolve`][Self::resolve].
pub struct ProfileResolver {
    active: Arc<Mutex<HashMap<SessionId, AgentProfile>>>,
    base: PermissionProfile,
    root: Option<PathBuf>,
    live_bash: Option<Arc<LiveBashState>>,
}

impl ProfileResolver {
    pub fn new(
        active: Arc<Mutex<HashMap<SessionId, AgentProfile>>>,
        base: PermissionProfile,
        root: Option<PathBuf>,
    ) -> Self {
        Self {
            active,
            base,
            root,
            live_bash: None,
        }
    }

    /// Wire in the live bash enablement state (#498) so `bash`/`bash_output`
    /// calls consult its grade, when one is set, ahead of the session's own
    /// profile. Chainable at construction, mirroring a builder-style opt-in.
    pub fn with_live_bash(mut self, live_bash: Arc<LiveBashState>) -> Self {
        self.live_bash = Some(live_bash);
        self
    }
}

#[async_trait]
impl PermissionResolver for ProfileResolver {
    async fn resolve(&self, session: &SessionId, tool: &str, input: &str) -> Permission {
        let arg = grading_arg(tool, input, self.root.as_deref());
        let workdir = permission_workdir(tool, input);
        // A live bash enablement (#498) overrides the session's own profile
        // for `bash`/`bash_output` specifically — a profile authored before
        // bash was live-enabled has no real opinion on it. `grade()` is `None`
        // when bash was never live-enabled (including the startup-only
        // `ENTANGLEMENT_ENABLE_BASH` path), which falls through to ordinary
        // per-profile resolution below, unchanged from pre-#498 behavior.
        let live_grade = if matches!(tool, "bash" | "bash_output") {
            self.live_bash.as_ref().and_then(|s| s.grade())
        } else {
            None
        };
        // Read the folded profile view without holding the lock across an await
        // (there is none here) — the executor's single-threaded loop is the sole
        // writer, so this brief lock never contends.
        let own = match live_grade {
            Some(grade) => crate::bash_live::grade_profile(&grade).resolve_scoped(
                tool,
                arg.as_deref(),
                workdir.as_deref(),
            ),
            None => {
                let active = self.active.lock().unwrap();
                permission_for(&active, session, tool, arg.as_deref(), workdir.as_deref())
            }
        };
        clamp_to_base(own, &self.base, tool, arg.as_deref(), workdir.as_deref())
    }
}

/// Resolve the confinement policy `bash`/`call` run a session's commands under
/// (#479, ADR-0104 amendment). Sync and infallible — unlike permission there is
/// no `Ask` round-trip and no DB lookup a real embedder would need to await; a
/// tenant that wants per-tenant sandboxing swaps this the same way it would
/// [`PermissionResolver`]. `session: None` is the plain [`crate::tools::Tool::run`]
/// path (no live session to resolve against — standalone use, most unit tests).
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

/// The single-user CLI resolver: reads the executor's live per-session
/// confinement cache, folded from lifecycle events exactly like
/// [`ProfileResolver`] folds `active` (`tool_runner`'s dispatch loop is the
/// sole writer of both `own`/`floor` below). `own` is a session's own profile
/// resolved against `default_policy` (the process-global `ENTANGLEMENT_SANDBOX`
/// default); `floor` is the ancestor clamp (#479, ADR-0104 amendment) — the
/// most-confined effective policy across the session's ancestor chain at the
/// moment it was spawned, mirroring ADR-0024's privilege ceiling for
/// confinement instead of permission grade. Kept as two maps rather than one
/// pre-combined value so a later `AgentChanged`/`SetAgent` on this exact
/// session can recompute `own` without losing the frozen ancestor floor (#479).
/// An unseen session (never folded — e.g. a direct `.run()` call with no live
/// session) falls back to `default_policy` alone: sandboxing is defense in
/// depth on top of the permission gate, not the gate itself, so this does not
/// fail-closed to maximum confinement the way `permission_for` fails closed to
/// `Deny`.
pub struct ProfileSandboxResolver {
    own: Arc<Mutex<HashMap<SessionId, SandboxPolicy>>>,
    floor: Arc<Mutex<HashMap<SessionId, SandboxPolicy>>>,
    default_policy: SandboxPolicy,
}

impl ProfileSandboxResolver {
    pub fn new(
        own: Arc<Mutex<HashMap<SessionId, SandboxPolicy>>>,
        floor: Arc<Mutex<HashMap<SessionId, SandboxPolicy>>>,
        default_policy: SandboxPolicy,
    ) -> Self {
        Self {
            own,
            floor,
            default_policy,
        }
    }
}

impl SandboxResolver for ProfileSandboxResolver {
    fn resolve(&self, session: Option<&SessionId>) -> SandboxPolicy {
        match session {
            Some(session) => resolve_sandbox(&self.own, &self.floor, session, self.default_policy),
            None => self.default_policy,
        }
    }
}

/// A session's effective confinement: its own resolved policy clamped by the
/// frozen ancestor floor (#479, ADR-0104 amendment). Shared by
/// [`ProfileSandboxResolver::resolve`] and `tool_runner`'s dispatch loop (which
/// computes a *new* session's floor from its parent's already-folded effective
/// value at `SessionStarted`) so the two never drift.
pub(crate) fn resolve_sandbox(
    own: &Arc<Mutex<HashMap<SessionId, SandboxPolicy>>>,
    floor: &Arc<Mutex<HashMap<SessionId, SandboxPolicy>>>,
    session: &SessionId,
    default_policy: SandboxPolicy,
) -> SandboxPolicy {
    let own = own
        .lock()
        .unwrap()
        .get(session)
        .copied()
        .unwrap_or(default_policy);
    let floor = floor
        .lock()
        .unwrap()
        .get(session)
        .copied()
        .unwrap_or_else(SandboxPolicy::none);
    own.most_confined(floor)
}

/// Resolve `session`'s own policy from its (possibly just-switched) profile and
/// record it in `own` (#479). Used at `SessionStarted`, `AgentChanged`, and the
/// `ToolExec` self-heal — every point `tool_runner` (re)resolves a session's
/// active profile. Never touches `floor`: the ancestor clamp is frozen once at
/// spawn ([`record_session_sandbox`]), not re-derived on a later profile
/// switch, so a mid-session `SetAgent` can relax/tighten its own confinement
/// without losing the floor its parent imposed.
pub(crate) fn record_own_sandbox(
    own: &Arc<Mutex<HashMap<SessionId, SandboxPolicy>>>,
    session: &SessionId,
    profile_sandbox: Option<&str>,
    default_policy: SandboxPolicy,
) {
    own.lock().unwrap().insert(
        session.clone(),
        default_policy.resolve_profile_override(profile_sandbox),
    );
}

/// Fold a newly-started session's own policy plus its frozen ancestor floor
/// into the shared maps (#479, ADR-0104 amendment): the floor is the parent's
/// *already-resolved* effective confinement (its own policy clamped by its own
/// floor), so the clamp composes down an arbitrarily deep spawn chain exactly
/// like ADR-0024's permission ceiling. A root session (`parent: None`) gets the
/// unconfined identity element (`SandboxPolicy::none()`, the lowest
/// confinement rank), so `own.most_confined(floor)` reduces to `own` alone.
pub(crate) fn record_session_sandbox(
    own: &Arc<Mutex<HashMap<SessionId, SandboxPolicy>>>,
    floor: &Arc<Mutex<HashMap<SessionId, SandboxPolicy>>>,
    session: &SessionId,
    parent: Option<&SessionId>,
    profile_sandbox: Option<&str>,
    default_policy: SandboxPolicy,
) {
    record_own_sandbox(own, session, profile_sandbox, default_policy);
    let parent_floor = parent
        .map(|p| resolve_sandbox(own, floor, p, default_policy))
        .unwrap_or_else(SandboxPolicy::none);
    floor.lock().unwrap().insert(session.clone(), parent_floor);
}

/// Bundled per-process sandbox state (#479, ADR-0104 amendment): the shared
/// maps `tool_runner`'s dispatch loop folds lifecycle events into (mirroring
/// `active`'s sharing with [`ProfileResolver`]) plus the process-global
/// default an unseen session falls back to. Grouped into one value so a
/// caller that doesn't care about per-profile sandboxing — every test helper,
/// the `embedded` example — passes a single [`SandboxConfig::none`] instead of
/// three positional args.
#[derive(Clone)]
pub struct SandboxConfig {
    pub base: SandboxPolicy,
    pub own: Arc<Mutex<HashMap<SessionId, SandboxPolicy>>>,
    pub floor: Arc<Mutex<HashMap<SessionId, SandboxPolicy>>>,
}

impl SandboxConfig {
    /// Every call unsandboxed, no per-profile overrides — byte-identical to
    /// pre-#479 behavior.
    pub fn none() -> Self {
        Self {
            base: SandboxPolicy::none(),
            own: Arc::new(Mutex::new(HashMap::new())),
            floor: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Read from the process-global `ENTANGLEMENT_SANDBOX`/`ENTANGLEMENT_SANDBOX_NETWORK`
    /// env vars, with fresh empty per-session maps.
    pub fn from_env() -> Self {
        Self {
            base: SandboxPolicy::from_env(),
            ..Self::none()
        }
    }

    /// The resolver `BashTool`/`CallTool` consult per call (#479).
    pub fn resolver(&self) -> Arc<dyn SandboxResolver> {
        Arc::new(ProfileSandboxResolver::new(
            self.own.clone(),
            self.floor.clone(),
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

    /// Re-read the persisted `Always` grants from disk (#329) — the watcher's
    /// hook for picking up a grant another skutter instance recorded, without
    /// disturbing this process's in-memory `Session`-scoped grants.
    pub fn reload(&self) {
        self.inner.lock().unwrap().reload();
    }
}

#[async_trait]
impl GrantStore for DefaultGrantStore {
    fn is_granted(&self, session: &SessionId, tool: &str, arg: Option<&str>) -> bool {
        self.inner.lock().unwrap().is_granted(session, tool, arg)
    }

    async fn record(
        &self,
        session: &SessionId,
        tool: &str,
        arg: Option<&str>,
        scope: ApprovalScope,
    ) {
        self.inner.lock().unwrap().record(session, tool, arg, scope);
    }

    fn forget_session(&self, session: &SessionId) {
        self.inner.lock().unwrap().forget_session(session);
    }

    fn grant_session_dir(&self, session: &SessionId, dir: &str) -> String {
        self.inner.lock().unwrap().grant_session_dir(session, dir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use entanglement_core::AgentMode;

    fn build_profile_with_scoped_read() -> AgentProfile {
        AgentProfile {
            name: "build".into(),
            description: String::new(),
            mode: AgentMode::Primary,
            system_prompt: String::new(),
            model: None,
            provider: None,
            permission: PermissionProfile::new(Permission::Ask)
                .with("read(src/*)", Permission::Allow),
            tools: None,
            disallowed_tools: Vec::new(),
            can_spawn: None,
            spawnable_agents: None,
            sandbox: None,
        }
    }

    /// #485, ADR-0125: an absolute path resolving inside a wired `root` must
    /// grade identically to its root-relative spelling — regression pin for the
    /// bug (an arg-scoped rule authored root-relative silently fell through to
    /// the profile default for the absolute form).
    #[tokio::test]
    async fn resolve_matches_an_absolute_in_root_path_when_root_is_wired() {
        let session = SessionId::new("s1");
        let active = Arc::new(Mutex::new(HashMap::from([(
            session.clone(),
            build_profile_with_scoped_read(),
        )])));
        let resolver = ProfileResolver::new(
            active,
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
        let active = Arc::new(Mutex::new(HashMap::from([(
            session.clone(),
            build_profile_with_scoped_read(),
        )])));
        let resolver =
            ProfileResolver::new(active, PermissionProfile::new(Permission::Allow), None);
        assert_eq!(
            resolver
                .resolve(&session, "read", r#"{"path":"/r/src/main.rs"}"#)
                .await,
            Permission::Ask
        );
    }

    /// #498: with no live bash state wired, `bash` resolves through the
    /// session's own profile exactly as before — the opt-in `with_live_bash`
    /// changes nothing for a caller that never calls it.
    #[tokio::test]
    async fn bash_resolves_through_the_profile_without_live_bash_wired() {
        let session = SessionId::new("s1");
        let profile = AgentProfile {
            permission: PermissionProfile::new(Permission::Deny),
            ..build_profile_with_scoped_read()
        };
        let active = Arc::new(Mutex::new(HashMap::from([(session.clone(), profile)])));
        let resolver =
            ProfileResolver::new(active, PermissionProfile::new(Permission::Allow), None);
        assert_eq!(
            resolver
                .resolve(&session, "bash", r#"{"command":"git status"}"#)
                .await,
            Permission::Deny
        );
    }

    /// #498: a live bash grade overrides the session's own profile for
    /// `bash`/`bash_output`, but the config ceiling still clamps the result —
    /// a live `Allow` never bypasses a `bash: deny` base.
    #[tokio::test]
    async fn live_bash_grade_overrides_the_profile_but_not_the_ceiling() {
        let session = SessionId::new("s1");
        // The session's own profile denies bash outright — a live grade must
        // still be able to override this (bash didn't exist for this profile
        // to have a real opinion on until it was live-enabled).
        let profile = AgentProfile {
            permission: PermissionProfile::new(Permission::Deny),
            ..build_profile_with_scoped_read()
        };
        let active = Arc::new(Mutex::new(HashMap::from([(session.clone(), profile)])));
        let live_bash = crate::bash_live::LiveBashState::new(false);

        // Allow-all ceiling: the live grade governs outright.
        let resolver = ProfileResolver::new(
            active.clone(),
            PermissionProfile::new(Permission::Allow),
            None,
        )
        .with_live_bash(live_bash.clone());
        crate::bash_live::bash_enable(
            &Arc::new(std::sync::RwLock::new(crate::tools::ToolRegistry::new())),
            &live_bash,
            &crate::bash_live::BashToolConfig {
                root: PathBuf::from("."),
                extra_roots: None,
                secret_env: Vec::new(),
                sandbox_resolver: Arc::new(crate::host::SandboxPolicy::none()),
            },
            entanglement_core::BashGrade::Allow { pattern: None },
        );
        assert_eq!(
            resolver
                .resolve(&session, "bash", r#"{"command":"rm -rf /"}"#)
                .await,
            Permission::Allow
        );
        assert_eq!(
            resolver.resolve(&session, "bash_output", "{}").await,
            Permission::Allow
        );

        // A `bash: deny` ceiling still wins over the live `Allow`.
        let strict_ceiling = ProfileResolver::new(
            active,
            PermissionProfile::new(Permission::Allow).with("bash", Permission::Deny),
            None,
        )
        .with_live_bash(live_bash);
        assert_eq!(
            strict_ceiling
                .resolve(&session, "bash", r#"{"command":"git status"}"#)
                .await,
            Permission::Deny
        );
    }

    /// #479: an unseen session (never folded from a lifecycle event) falls back
    /// to the process-global default — unlike permission's fail-closed `Deny`,
    /// sandboxing is defense in depth, not the gate itself.
    #[test]
    fn sandbox_resolver_falls_back_to_default_for_an_unseen_session() {
        let confined = SandboxPolicy {
            backend: crate::host::SandboxBackend::Bubblewrap,
            network: false,
        };
        let cfg = SandboxConfig {
            base: confined,
            ..SandboxConfig::none()
        };
        let resolver = cfg.resolver();
        assert_eq!(resolver.resolve(Some(&SessionId::new("ghost"))), confined);
    }

    /// #479: a profile's own override wins when no ancestor floor clamps it.
    #[test]
    fn sandbox_resolver_reads_the_session_own_override() {
        let cfg = SandboxConfig::none();
        let session = SessionId::new("s1");
        let confined = SandboxPolicy {
            backend: crate::host::SandboxBackend::Bubblewrap,
            network: false,
        };
        cfg.own.lock().unwrap().insert(session.clone(), confined);
        assert_eq!(cfg.resolver().resolve(Some(&session)), confined);
    }

    /// #479, ADR-0104 amendment: a confined parent's floor clamps a child whose
    /// own profile would otherwise run unsandboxed.
    #[test]
    fn sandbox_resolver_clamps_to_the_ancestor_floor() {
        let cfg = SandboxConfig::none();
        let child = SessionId::new("child");
        let confined = SandboxPolicy {
            backend: crate::host::SandboxBackend::Bubblewrap,
            network: false,
        };
        // Child's own profile is unsandboxed, but its recorded floor (the
        // parent's effective policy at spawn time) is confined.
        cfg.own
            .lock()
            .unwrap()
            .insert(child.clone(), SandboxPolicy::none());
        cfg.floor.lock().unwrap().insert(child.clone(), confined);
        assert_eq!(cfg.resolver().resolve(Some(&child)), confined);
    }

    /// #479, ADR-0104 amendment: `record_session_sandbox` is the exact
    /// computation `tool_runner`'s `SessionStarted` handler performs — this
    /// pins the spawn-chain clamp end to end (population, not just resolution)
    /// without spinning up the full engine: a confined parent's child inherits
    /// its confinement as a floor even though the child's own profile is
    /// unsandboxed, and a grandchild inherits the same floor transitively.
    #[test]
    fn record_session_sandbox_clamps_a_multi_level_spawn_chain() {
        let cfg = SandboxConfig::none();
        let confined = SandboxPolicy {
            backend: crate::host::SandboxBackend::Bubblewrap,
            network: false,
        };
        let parent = SessionId::new("parent");
        let child = SessionId::new("child");
        let grandchild = SessionId::new("grandchild");

        // Root: confined by its own profile, no ancestor.
        record_session_sandbox(&cfg.own, &cfg.floor, &parent, None, Some("bwrap"), cfg.base);
        assert_eq!(cfg.resolver().resolve(Some(&parent)), confined);

        // Child: unsandboxed profile (`sandbox: none`), but spawned under the
        // confined parent — the floor clamps it confined anyway.
        record_session_sandbox(
            &cfg.own,
            &cfg.floor,
            &child,
            Some(&parent),
            Some("none"),
            cfg.base,
        );
        assert_eq!(cfg.resolver().resolve(Some(&child)), confined);

        // Grandchild: no override at all (inherits the process default, which
        // is unsandboxed here) — still clamps to the same confined floor,
        // proving the clamp composes transitively down the chain.
        record_session_sandbox(
            &cfg.own,
            &cfg.floor,
            &grandchild,
            Some(&child),
            None,
            cfg.base,
        );
        assert_eq!(cfg.resolver().resolve(Some(&grandchild)), confined);
    }

    /// #479: an unsandboxed parent imposes no floor, so a confined child's own
    /// (stricter) override still wins — the clamp only ever tightens, never
    /// loosens a child below what its own profile already asked for.
    #[test]
    fn record_session_sandbox_never_loosens_a_childs_own_stricter_choice() {
        let cfg = SandboxConfig::none();
        let confined = SandboxPolicy {
            backend: crate::host::SandboxBackend::Bubblewrap,
            network: false,
        };
        let parent = SessionId::new("parent");
        let child = SessionId::new("child");
        record_session_sandbox(&cfg.own, &cfg.floor, &parent, None, None, cfg.base);
        record_session_sandbox(
            &cfg.own,
            &cfg.floor,
            &child,
            Some(&parent),
            Some("bwrap"),
            cfg.base,
        );
        assert_eq!(cfg.resolver().resolve(Some(&child)), confined);
    }
}
