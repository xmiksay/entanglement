//! Live bash enablement (#498, ADR-0133): register the `bash`/`bash_output`
//! pair in a running process, graded by a [`BashGrade`] rather than a bare
//! on/off — mirrors the `SharedRegistry` live-MCP-management seam (#372/#375,
//! `crate::mcp::live`/`crate::mcp::responder`).
//!
//! [`LiveBashState`] is the shared handle [`crate::policy::ProfileResolver`]
//! consults and [`spawn_bash_responder`] mutates off the inbound fan-out.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use entanglement_core::{BashGrade, Holly, InMsg, Permission, PermissionProfile};
use tokio::sync::broadcast::error::RecvError;

use crate::extra_roots::ExtraRootStore;
use crate::host::{BashOutputTool, BashTool, JobRegistry};
use crate::policy::SandboxResolver;
use crate::tools::SharedRegistry;

/// Shared state a live bash enablement mutates (#498). `registered` is seeded
/// from the startup `ENTANGLEMENT_ENABLE_BASH` env var too, so the TUI
/// `!bash` passthrough gate (which just asks [`is_enabled`][Self::is_enabled])
/// reflects both paths uniformly; `grade` stays `None` for that startup path,
/// so [`ProfileResolver`][crate::policy::ProfileResolver] falls through to
/// ordinary per-profile permission resolution there — byte-identical to
/// pre-#498 behavior whenever bash was never live-enabled.
pub struct LiveBashState {
    registered: AtomicBool,
    grade: RwLock<Option<BashGrade>>,
}

impl LiveBashState {
    pub fn new(registered_at_startup: bool) -> Arc<Self> {
        Arc::new(Self {
            registered: AtomicBool::new(registered_at_startup),
            grade: RwLock::new(None),
        })
    }

    /// Whether `bash`/`bash_output` are currently registered — via the startup
    /// env var or a live [`InMsg::BashEnable`], either way.
    pub fn is_enabled(&self) -> bool {
        self.registered.load(Ordering::SeqCst)
    }

    /// The live permission override in effect, or `None` when bash was never
    /// live-enabled (a startup-registered pair resolves through the session's
    /// own profile, exactly as before #498).
    pub fn grade(&self) -> Option<BashGrade> {
        self.grade.read().unwrap().clone()
    }

    fn set(&self, grade: BashGrade) {
        *self.grade.write().unwrap() = Some(grade);
        self.registered.store(true, Ordering::SeqCst);
    }

    fn clear(&self) {
        *self.grade.write().unwrap() = None;
        self.registered.store(false, Ordering::SeqCst);
    }
}

/// The [`PermissionProfile`] a [`BashGrade`] materializes into (#498):
/// [`BashGrade::Ask`] is a flat `Ask` default; [`BashGrade::Allow`] with no
/// pattern is a flat `Allow`; with a command pattern it stays `Ask` by default
/// and adds an argument-scoped `bash(pattern): allow` rule (the existing
/// `tool(pattern)` syntax, #173) so only matching commands are pre-approved —
/// `bash_output` has no command to match, so it falls through to that same
/// `Ask` default in the narrowed case.
pub fn grade_profile(grade: &BashGrade) -> PermissionProfile {
    match grade {
        BashGrade::Ask => PermissionProfile::new(Permission::Ask),
        BashGrade::Allow { pattern: None } => PermissionProfile::new(Permission::Allow),
        BashGrade::Allow { pattern: Some(p) } => {
            PermissionProfile::new(Permission::Ask).with(format!("bash({p})"), Permission::Allow)
        }
    }
}

/// Everything [`bash_enable`] needs to build a fresh `BashTool`/`BashOutputTool`
/// pair, mirroring `register_default_tools`'s bash arm in `main.rs` — captured
/// once at startup and handed to the bash responder, since it has no other way
/// to reach these values. `sandbox_resolver` (#479) is the same per-profile
/// resolver `register_default_tools` wires into the startup-registered pair,
/// so a live-enabled `bash` respects a profile's `sandbox:` override exactly
/// like one registered at startup.
#[derive(Clone)]
pub struct BashToolConfig {
    pub root: PathBuf,
    pub extra_roots: Option<Arc<ExtraRootStore>>,
    pub secret_env: Vec<String>,
    pub sandbox_resolver: Arc<dyn SandboxResolver>,
}

/// Register `bash`/`bash_output` into `registry` (a no-op if already
/// present — idempotent, so a repeated `/bash on` just updates the grade) and
/// install `grade` as the live permission override. Mirrors
/// `mcp::live::mcp_add`'s "mutate the registry, then record the new state"
/// shape.
pub fn bash_enable(
    registry: &SharedRegistry,
    state: &Arc<LiveBashState>,
    config: &BashToolConfig,
    grade: BashGrade,
) {
    {
        let mut reg = registry.write().unwrap();
        if !reg.contains("bash") {
            let jobs = JobRegistry::new();
            let mut bash = BashTool::new(config.root.clone())
                .with_secret_env(config.secret_env.clone())
                .with_jobs(jobs.clone())
                .with_sandbox_resolver(config.sandbox_resolver.clone());
            if let Some(e) = &config.extra_roots {
                bash = bash.with_extra_roots(e.clone());
            }
            reg.register(bash);
            reg.register(BashOutputTool::new(jobs));
        }
    }
    state.set(grade);
}

/// Unregister `bash`/`bash_output` and clear the live grade override. Mirrors
/// `mcp::live::mcp_remove`.
pub fn bash_disable(registry: &SharedRegistry, state: &Arc<LiveBashState>) {
    {
        let mut reg = registry.write().unwrap();
        reg.unregister("bash");
        reg.unregister("bash_output");
    }
    state.clear();
}

/// Spawns a subscriber that answers `InMsg::BashEnable`/`BashDisable` off the
/// inbound fan-out (#498) — mirrors `mcp::spawn_mcp_responder`. Neither op can
/// fail (registration is infallible, unlike an MCP connect), so every request
/// replies with `OutEvent::BashChanged`.
pub fn spawn_bash_responder(
    holly: &Holly,
    registry: SharedRegistry,
    state: Arc<LiveBashState>,
    config: BashToolConfig,
) -> tokio::task::JoinHandle<()> {
    let emitter = holly.clone();
    let mut inbound = holly.subscribe_inbound();

    tokio::spawn(async move {
        loop {
            match inbound.recv().await {
                Ok(InMsg::BashEnable { grade }) => {
                    bash_enable(&registry, &state, &config, grade.clone());
                    tracing::info!(?grade, "bash: live-enabled");
                    emitter.emit_bash_changed(true, Some(grade));
                }
                Ok(InMsg::BashDisable) => {
                    bash_disable(&registry, &state);
                    tracing::info!("bash: live-disabled");
                    emitter.emit_bash_changed(false, None);
                }
                Ok(_) => {}
                // A dropped inbound frame under lag can only lose a command —
                // the head times out and re-asks; keep serving.
                Err(RecvError::Lagged(n)) => {
                    tracing::warn!("bash responder lagged, skipped {n} inbound messages");
                }
                Err(RecvError::Closed) => break,
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use std::sync::RwLock as StdRwLock;

    use entanglement_core::{EngineConfig, OutEvent};

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
        }
    }

    #[test]
    fn grade_profile_ask_is_flat_ask() {
        let p = grade_profile(&BashGrade::Ask);
        assert_eq!(p.resolve("bash", Some("git status")), Permission::Ask);
        assert_eq!(p.resolve("bash_output", None), Permission::Ask);
    }

    #[test]
    fn grade_profile_blanket_allow_is_flat_allow() {
        let p = grade_profile(&BashGrade::Allow { pattern: None });
        assert_eq!(p.resolve("bash", Some("rm -rf /")), Permission::Allow);
        assert_eq!(p.resolve("bash_output", None), Permission::Allow);
    }

    #[test]
    fn grade_profile_narrowed_allow_only_matches_the_pattern() {
        let p = grade_profile(&BashGrade::Allow {
            pattern: Some("git *".to_string()),
        });
        assert_eq!(p.resolve("bash", Some("git status")), Permission::Allow);
        assert_eq!(p.resolve("bash", Some("rm -rf /")), Permission::Ask);
        // `bash_output` has no command to match the pattern against, so it
        // falls through to the narrowed grade's `Ask` default.
        assert_eq!(p.resolve("bash_output", None), Permission::Ask);
    }

    #[test]
    fn bash_enable_registers_the_pair_and_records_the_grade() {
        let registry: SharedRegistry = Arc::new(StdRwLock::new(ToolRegistry::new()));
        let state = LiveBashState::new(false);
        assert!(!state.is_enabled());
        bash_enable(&registry, &state, &test_config(), BashGrade::Ask);
        assert!(registry.read().unwrap().contains("bash"));
        assert!(registry.read().unwrap().contains("bash_output"));
        assert!(state.is_enabled());
        assert_eq!(state.grade(), Some(BashGrade::Ask));
    }

    #[test]
    fn bash_enable_is_idempotent_but_still_updates_the_grade() {
        let registry: SharedRegistry = Arc::new(StdRwLock::new(ToolRegistry::new()));
        let state = LiveBashState::new(false);
        bash_enable(&registry, &state, &test_config(), BashGrade::Ask);
        bash_enable(
            &registry,
            &state,
            &test_config(),
            BashGrade::Allow { pattern: None },
        );
        assert_eq!(state.grade(), Some(BashGrade::Allow { pattern: None }));
        // Still exactly one `bash` entry — re-enabling never double-registers.
        assert!(registry.read().unwrap().contains("bash"));
    }

    #[test]
    fn bash_disable_unregisters_and_clears_the_grade() {
        let registry: SharedRegistry = Arc::new(StdRwLock::new(ToolRegistry::new()));
        let state = LiveBashState::new(false);
        bash_enable(&registry, &state, &test_config(), BashGrade::Ask);
        bash_disable(&registry, &state);
        assert!(!registry.read().unwrap().contains("bash"));
        assert!(!registry.read().unwrap().contains("bash_output"));
        assert!(!state.is_enabled());
        assert_eq!(state.grade(), None);
    }

    #[tokio::test]
    async fn bash_enable_replies_with_bash_changed() {
        let holly = empty_engine();
        let mut sub = holly.subscribe();
        let registry: SharedRegistry = Arc::new(StdRwLock::new(ToolRegistry::new()));
        let state = LiveBashState::new(false);
        let handle = spawn_bash_responder(&holly, registry.clone(), state.clone(), test_config());

        holly
            .send(InMsg::BashEnable {
                grade: BashGrade::Allow { pattern: None },
            })
            .await
            .unwrap();

        let ev = tokio::time::timeout(std::time::Duration::from_secs(2), sub.recv())
            .await
            .expect("timed out waiting for BashChanged")
            .unwrap();
        match ev {
            OutEvent::BashChanged { enabled, grade } => {
                assert!(enabled);
                assert_eq!(grade, Some(BashGrade::Allow { pattern: None }));
            }
            other => panic!("expected BashChanged, got {other:?}"),
        }
        assert!(registry.read().unwrap().contains("bash"));
        handle.abort();
    }

    #[tokio::test]
    async fn bash_disable_replies_with_bash_changed() {
        let holly = empty_engine();
        let mut sub = holly.subscribe();
        let registry: SharedRegistry = Arc::new(StdRwLock::new(ToolRegistry::new()));
        let state = LiveBashState::new(true);
        bash_enable(&registry, &state, &test_config(), BashGrade::Ask);
        let handle = spawn_bash_responder(&holly, registry.clone(), state.clone(), test_config());

        holly.send(InMsg::BashDisable).await.unwrap();

        let ev = tokio::time::timeout(std::time::Duration::from_secs(2), sub.recv())
            .await
            .expect("timed out waiting for BashChanged")
            .unwrap();
        match ev {
            OutEvent::BashChanged { enabled, grade } => {
                assert!(!enabled);
                assert_eq!(grade, None);
            }
            other => panic!("expected BashChanged, got {other:?}"),
        }
        assert!(!registry.read().unwrap().contains("bash"));
        handle.abort();
    }
}
