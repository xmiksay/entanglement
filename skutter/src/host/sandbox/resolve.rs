//! Which [`SandboxPolicy`] a session's `bash` runs under (ADR-0207 §6, the
//! stage 5b amendment of ADR-0104): a mode fact, tightened by the env. Only
//! `bash` reads it, so it lives with the sandbox, not in the permission
//! pipeline.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use entanglement_core::SessionId;

use super::{SandboxBackend, SandboxPolicy};
use crate::mode::Mode;

/// A mode's confinement policy for `bash`: `sandbox`/`sandbox_network`
/// translated into a [`SandboxPolicy`]. `ENTANGLEMENT_SANDBOX`/
/// `ENTANGLEMENT_SANDBOX_NETWORK` are then layered on top via
/// [`SandboxPolicy::most_confined`] — env may only tighten what the mode
/// declares, never loosen it, which `most_confined` gives for free since
/// it always picks the more-confined of the two.
pub fn mode_policy(mode: &Mode) -> SandboxPolicy {
    let backend = match mode.sandbox.as_deref() {
        Some("bwrap") | Some("bubblewrap") => SandboxBackend::Bubblewrap,
        _ => SandboxBackend::None,
    };
    SandboxPolicy {
        backend,
        network: mode.sandbox_network,
    }
}

/// Resolve the confinement policy `bash` runs a session's commands under
/// (ADR-0207 §6, stage 5b amendment of ADR-0104). Sync and infallible —
/// unlike permission there is no `Ask` round-trip and no DB lookup a real
/// embedder would need to await; a tenant that wants per-tenant sandboxing
/// swaps this the same way it would
/// [`crate::policy::PermissionResolver`]. `session: None`
/// is the plain [`crate::tools::Tool::run`] path (no live session to resolve
/// against — standalone use, most unit tests).
pub trait SandboxResolver: Send + Sync {
    fn resolve(&self, session: Option<&SessionId>) -> SandboxPolicy;
}

/// A fixed policy is trivially its own resolver — the `.with_sandbox(policy)`
/// builder `BashTool` already had keeps working unchanged, now
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
/// Reads the same session→mode map [`crate::policy::ModeResolver`] grades permission
/// from, resolves it against `table` exactly like `ModeResolver` does, and
/// derives the mode's [`SandboxPolicy`] via
/// [`mode_policy`] — then layers `base` (the
/// process-global `ENTANGLEMENT_SANDBOX`/`ENTANGLEMENT_SANDBOX_NETWORK`
/// env default) on top via `most_confined`, so the env may only **tighten**
/// what the mode declares, never loosen it. An unseen session (never folded,
/// or an unknown mode name) falls back to `base` alone: sandboxing is defense
/// in depth on top of the permission gate, not the gate itself, so this does
/// not fail-closed to maximum confinement the way
/// [`crate::policy::ModeResolver`]
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
        mode_policy(mode).most_confined(self.base)
    }
}

/// Bundled per-process sandbox state (ADR-0207 §6, stage 5b): the same
/// session→mode map and [`crate::mode::ModeTable`] the executor's
/// `ModeResolver` grades permission from, plus the process-global default
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
    /// mode `table` the caller's `ModeResolver` uses — sandboxing must see
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

    /// The resolver `BashTool` consults per call (#479, ADR-0207
    /// §6).
    pub fn resolver(&self) -> Arc<dyn SandboxResolver> {
        Arc::new(ModeSandboxResolver::new(
            self.modes.clone(),
            self.table.clone(),
            self.base,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mode::{Limits, ModeTable, Rules};
    use entanglement_core::Permission;

    fn modes_map(session: &SessionId) -> Arc<Mutex<HashMap<SessionId, String>>> {
        Arc::new(Mutex::new(HashMap::from([(
            session.clone(),
            "test".to_string(),
        )])))
    }

    /// A `Mode` fixture with a given sandbox posture, otherwise a bare
    /// pass-through — the sandbox tests below only ever read `sandbox`/
    /// `sandbox_network` through [`mode_policy`].
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
        backend: SandboxBackend::Bubblewrap,
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
        assert_eq!(resolved.backend, SandboxBackend::Bubblewrap);
        assert!(resolved.network, "sandbox_network: true shares the network");
    }
}
