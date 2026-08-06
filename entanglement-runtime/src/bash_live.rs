//! Lazily-registrable built-in tools (#611, ADR-0163): a closed table of tool
//! names a session's **enable** tool-overlay entry (#539, ADR-0149,
//! `InMsg::SetToolOverlay`) may register into the shared registry on demand
//! — the counterpart to what the bespoke `InMsg::BashEnable` did before this
//! ADR folded live bash enablement into the generic overlay. `bash` is the
//! only member at introduction: it is the one host tool gated behind the
//! startup-only `ENTANGLEMENT_ENABLE_BASH` env var (ADR-0010) and absent
//! from the [`SharedRegistry`] otherwise, so an overlay entry alone cannot
//! conjure it into existence. `poll` (#605) needs no matching step here — it
//! is always registered, dispatching on whatever handle it is given.
//!
//! The overlay's *grade* for an enabled tool (flat `Ask`/`Allow`, or —
//! #611/ADR-0163 — an `arg_pattern`-narrowed `Allow`) is a session-scoped
//! concern resolved by [`crate::permission::overlay_entry_grade`] inside
//! `tool_runner`'s dispatch ladder; this module only ever mutates the
//! process-global [`SharedRegistry`], mirroring the MCP live-management seam
//! (#372/#375, `crate::mcp::live`/`crate::mcp::responder`).

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use entanglement_core::{Holly, OutEvent, ToolOverlayEntry};
use tokio::sync::broadcast::error::RecvError;

use crate::builtin_visibility::BuiltinVisibility;
use crate::extra_roots::ExtraRootStore;
use crate::host::{BashTool, JobRegistry};
use crate::policy::SandboxResolver;
use crate::tools::SharedRegistry;

/// Whether `bash` is currently registered in the shared tool registry — via
/// the startup `ENTANGLEMENT_ENABLE_BASH` env var or a later tool-overlay
/// enable (ADR-0163 §2), either way. A dedicated flag rather than querying
/// the [`SharedRegistry`] directly because `CallTool`'s shape-check hint
/// (#554) is built during `main.rs`'s by-value tool-registration phase,
/// before the registry exists in its shared `Arc<RwLock<..>>` form — this
/// flag is seeded from the same startup bool and flipped by
/// [`register_lazy_builtin`] the instant `bash` lands in the registry, so
/// every reader observes one source of truth. Unlike the pre-ADR-0163
/// `LiveBashState` this superseded, it carries no grade — that's a
/// per-session overlay concern now ([`crate::permission::overlay_entry_grade`]).
pub struct BashRegistered(AtomicBool);

impl BashRegistered {
    pub fn new(registered_at_startup: bool) -> Arc<Self> {
        Arc::new(Self(AtomicBool::new(registered_at_startup)))
    }

    pub fn get(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    fn set(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// The closed table of lazily-registrable built-ins (ADR-0163 §2): a tool
/// name here may be registered on demand by a session tool-overlay enable
/// entry matching it, even though it is absent from the registry at
/// startup. Deliberately not "any name the runtime recognises" — teaching
/// the overlay to *instantiate* a tool (rather than only reveal one already
/// registered) is a genuine widening of a trusted frame's power, so the set
/// stays short and reviewed.
pub(crate) const LAZY_BUILTINS: &[&str] = &["bash"];

/// Everything a lazy `bash` registration needs to build a fresh `BashTool`,
/// mirroring `register_default_tools`'s bash arm in `main.rs` — captured
/// once at startup and handed to [`spawn_lazy_builtin_responder`], since it
/// has no other way to reach these values. `sandbox_resolver` (#479) is the
/// same per-profile resolver `register_default_tools` wires into the
/// startup-registered tool, so a live-enabled `bash` respects a profile's
/// `sandbox:` override exactly like one registered at startup. `jobs` (#605)
/// is the *same* [`JobRegistry`] `poll` was wired up with at startup — one
/// long-lived registry reused across enable/disable cycles (ADR-0163 §3)
/// rather than minted fresh, which is what keeps background-job ids stable
/// across an `/enable tool bash` + `/disable tool bash` and fixes #616's
/// job-orphaning as a side effect.
#[derive(Clone)]
pub struct BashToolConfig {
    pub root: PathBuf,
    pub extra_roots: Option<Arc<ExtraRootStore>>,
    pub secret_env: Vec<String>,
    pub sandbox_resolver: Arc<dyn SandboxResolver>,
    pub jobs: JobRegistry,
}

/// Register `name` into `registry` if it names a lazily-registrable built-in
/// (`LAZY_BUILTINS`) not already present — a no-op otherwise (unknown name,
/// or already registered, so a repeated enable never double-registers).
/// Flips `registered` when `bash` lands in the registry, so `CallTool`'s
/// shape-check hint (#554) and the TUI `!bash` gate observe it immediately.
fn register_lazy_builtin(
    registry: &SharedRegistry,
    config: &BashToolConfig,
    registered: &BashRegistered,
    name: &str,
) {
    if !LAZY_BUILTINS.contains(&name) {
        return;
    }
    {
        let mut reg = registry.write().unwrap();
        if reg.contains(name) {
            return;
        }
        match name {
            "bash" => {
                let mut bash = BashTool::new(config.root.clone())
                    .with_secret_env(config.secret_env.clone())
                    .with_jobs(config.jobs.clone())
                    .with_sandbox_resolver(config.sandbox_resolver.clone());
                if let Some(e) = &config.extra_roots {
                    bash = bash.with_extra_roots(e.clone());
                }
                reg.register(bash);
            }
            other => unreachable!("LAZY_BUILTINS entry {other:?} has no registration arm"),
        }
    }
    if name == "bash" {
        registered.set();
    }
}

/// Register every lazily-registrable built-in `entries` newly enables
/// (ADR-0163 §2) — called on every `OutEvent::ToolOverlayChanged`, for any
/// session, since a built-in's registration is process-global even though
/// the overlay that triggered it is per-session.
fn register_lazy_builtins(
    registry: &SharedRegistry,
    config: &BashToolConfig,
    registered: &BashRegistered,
    entries: &[ToolOverlayEntry],
) {
    for name in LAZY_BUILTINS {
        if ToolOverlayEntry::disposition(entries, name) == Some(true) {
            register_lazy_builtin(registry, config, registered, name);
        }
    }
}

/// Spawns a subscriber that watches the outbound broadcast for
/// `OutEvent::ToolOverlayChanged` (#539, ADR-0149) and lazily registers any
/// closed-table built-in a newly-enabled entry names (§2) — the runtime-side
/// half of ADR-0163's fold-in, mirroring `mcp::spawn_mcp_responder`'s shape
/// though this responder mutates state rather than answering a request.
pub fn spawn_lazy_builtin_responder(
    holly: &Holly,
    registry: SharedRegistry,
    config: BashToolConfig,
    registered: Arc<BashRegistered>,
    visibility: Arc<BuiltinVisibility>,
) -> tokio::task::JoinHandle<()> {
    let mut events = holly.subscribe();

    tokio::spawn(async move {
        loop {
            match events.recv().await {
                Ok(OutEvent::ToolOverlayChanged { session, entries }) => {
                    register_lazy_builtins(&registry, &config, &registered, &entries);
                    // Advertisement scoping (ADR-0178): the same overlay list
                    // that may have just registered the tool also decides
                    // which session gets to *see* it. Resume-safe: core
                    // replays `ToolOverlayChanged` on resume, so a resumed
                    // session re-folds its marks here — unlike the lazy MCP
                    // enablement map, which is never replayed.
                    visibility.fold_overlay(&session, &entries);
                }
                Ok(OutEvent::SessionEnded { session, .. }) => {
                    visibility.forget_session(&session);
                }
                Ok(_) => {}
                // A dropped broadcast frame under lag can only delay a lazy
                // registration until the next matching overlay change — the
                // overlay itself is unaffected (core owns that state), so
                // this is a bounded, self-correcting miss, not data loss.
                Err(RecvError::Lagged(n)) => {
                    tracing::warn!("lazy-builtin responder lagged, skipped {n} outbound events");
                }
                Err(RecvError::Closed) => break,
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use std::sync::RwLock as StdRwLock;

    use entanglement_core::{EngineConfig, SessionId};

    use super::*;
    use crate::tools::ToolRegistry;

    fn empty_engine() -> Holly {
        Holly::spawn(EngineConfig::default())
    }

    fn test_config() -> BashToolConfig {
        BashToolConfig {
            root: PathBuf::from("."),
            extra_roots: None,
            secret_env: Vec::new(),
            sandbox_resolver: Arc::new(crate::host::SandboxPolicy::none()),
            jobs: JobRegistry::new(),
        }
    }

    #[test]
    fn register_lazy_builtins_registers_bash_on_an_enable_entry() {
        let registry: SharedRegistry = Arc::new(StdRwLock::new(ToolRegistry::new()));
        let registered = BashRegistered::new(false);
        register_lazy_builtins(
            &registry,
            &test_config(),
            &registered,
            &[ToolOverlayEntry::ask("bash")],
        );
        assert!(registry.read().unwrap().contains("bash"));
        assert!(registered.get());
    }

    #[test]
    fn register_lazy_builtins_ignores_a_deny_entry() {
        let registry: SharedRegistry = Arc::new(StdRwLock::new(ToolRegistry::new()));
        register_lazy_builtins(
            &registry,
            &test_config(),
            &BashRegistered::new(false),
            &[ToolOverlayEntry::deny("bash")],
        );
        assert!(!registry.read().unwrap().contains("bash"));
    }

    #[test]
    fn register_lazy_builtins_ignores_a_name_outside_the_closed_table() {
        let registry: SharedRegistry = Arc::new(StdRwLock::new(ToolRegistry::new()));
        register_lazy_builtins(
            &registry,
            &test_config(),
            &BashRegistered::new(false),
            &[ToolOverlayEntry::ask("mcp__chessbase__evaluate")],
        );
        assert!(!registry.read().unwrap().contains("bash"));
        assert!(!registry
            .read()
            .unwrap()
            .contains("mcp__chessbase__evaluate"));
    }

    #[test]
    fn register_lazy_builtins_is_idempotent() {
        let registry: SharedRegistry = Arc::new(StdRwLock::new(ToolRegistry::new()));
        let config = test_config();
        let registered = BashRegistered::new(false);
        register_lazy_builtins(
            &registry,
            &config,
            &registered,
            &[ToolOverlayEntry::ask("bash")],
        );
        register_lazy_builtins(
            &registry,
            &config,
            &registered,
            &[ToolOverlayEntry::ask("bash")],
        );
        // Still exactly one `bash` entry — a repeated enable never
        // double-registers.
        assert!(registry.read().unwrap().contains("bash"));
    }

    #[tokio::test]
    async fn spawn_lazy_builtin_responder_registers_bash_on_tool_overlay_changed() {
        let holly = empty_engine();
        let registry: SharedRegistry = Arc::new(StdRwLock::new(ToolRegistry::new()));
        let registered = BashRegistered::new(false);
        let visibility = BuiltinVisibility::new(None);
        let handle = spawn_lazy_builtin_responder(
            &holly,
            registry.clone(),
            test_config(),
            registered.clone(),
            visibility.clone(),
        );

        // A runtime-authored event, as core's session task would emit in
        // reply to a trusted head's `InMsg::SetToolOverlay`.
        holly
            .send(entanglement_core::InMsg::SetToolOverlay {
                session: SessionId::new("s1"),
                entries: vec![ToolOverlayEntry::ask("bash")],
            })
            .await
            .unwrap();

        // Poll until the responder has had a chance to fold the resulting
        // `ToolOverlayChanged` — no direct signal to await otherwise.
        for _ in 0..50 {
            if registry.read().unwrap().contains("bash") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(registry.read().unwrap().contains("bash"));
        assert!(registered.get());
        // Advertisement scoping (ADR-0178): the enabling session sees it, an
        // unrelated one does not — even though the registration is global.
        let avail = crate::mcp::AvailableMcp::default();
        assert!(visibility.visible("bash", &SessionId::new("s1"), &avail));
        assert!(!visibility.visible("bash", &SessionId::new("s2"), &avail));
        handle.abort();
    }

    #[tokio::test]
    async fn spawn_lazy_builtin_responder_forgets_visibility_on_session_ended() {
        let holly = empty_engine();
        let registry: SharedRegistry = Arc::new(StdRwLock::new(ToolRegistry::new()));
        let visibility = BuiltinVisibility::new(None);
        let handle = spawn_lazy_builtin_responder(
            &holly,
            registry.clone(),
            test_config(),
            BashRegistered::new(false),
            visibility.clone(),
        );

        let avail = crate::mcp::AvailableMcp::default();
        let s1 = SessionId::new("s1");
        holly
            .send(entanglement_core::InMsg::SetToolOverlay {
                session: s1.clone(),
                entries: vec![ToolOverlayEntry::ask("bash")],
            })
            .await
            .unwrap();
        for _ in 0..50 {
            if visibility.visible("bash", &s1, &avail) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(visibility.visible("bash", &s1, &avail));

        holly
            .send(entanglement_core::InMsg::CloseSession {
                session: s1.clone(),
            })
            .await
            .unwrap();
        for _ in 0..50 {
            if !visibility.visible("bash", &s1, &avail) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(!visibility.visible("bash", &s1, &avail));
        handle.abort();
    }

    /// #605 (fixes #616 as a side effect): a lazily-registered `bash` reuses
    /// the `JobRegistry` the config was built with — the same one `poll` was
    /// wired up with at startup — instead of minting a fresh, disconnected
    /// one. A job started right after enabling must be pollable through that
    /// shared registry.
    #[tokio::test]
    async fn register_lazy_builtins_shares_the_configured_job_registry() {
        let registry: SharedRegistry = Arc::new(StdRwLock::new(ToolRegistry::new()));
        let dir = tempfile::tempdir().unwrap();
        let jobs = JobRegistry::new();
        let config = BashToolConfig {
            root: dir.path().to_path_buf(),
            extra_roots: None,
            secret_env: Vec::new(),
            sandbox_resolver: Arc::new(crate::host::SandboxPolicy::none()),
            jobs: jobs.clone(),
        };
        register_lazy_builtins(
            &registry,
            &config,
            &BashRegistered::new(false),
            &[ToolOverlayEntry::ask("bash")],
        );

        let session = entanglement_core::SessionId::new("s1");
        let call = entanglement_core::ToolCall {
            id: "r1".to_string(),
            name: "bash".to_string(),
            input: r#"{"command":"true","background":true}"#.to_string(),
            provider_meta: None,
        };
        let snapshot = registry.read().unwrap().clone();
        let started = snapshot.execute(&call, &session).await;
        let text = entanglement_core::content_text(&started.content);
        let id = text
            .split_whitespace()
            .find(|w| w.starts_with("j-"))
            .unwrap()
            .trim_end_matches(|c: char| !c.is_ascii_alphanumeric() && c != '-')
            .to_string();

        // The registry `register_lazy_builtins` was given sees the job the
        // lazily-registered `bash` spawned — they are the same instance, not
        // two independent ones.
        assert!(jobs.poll(&id, &session, false, 5).await.is_some());
    }
}
