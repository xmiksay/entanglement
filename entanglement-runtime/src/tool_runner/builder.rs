//! [`ToolExecutor::builder`] (#724): the one way to construct the runtime tool
//! executor. Replaces the ~19-positional-parameter
//! `spawn_tool_executor_with_policy`, whose every new seam broke every
//! embedder's call site — here a new seam is one more setter with a default,
//! so existing builder chains keep compiling.
//!
//! Required inputs (the engine handle and the tool registry) go into
//! [`ToolExecutor::builder`] itself, so a half-configured executor can't be
//! spawned. Every other seam is optional; its default is documented on its
//! setter and summarized in `docs/embedding/tool-execution.md`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

use entanglement_core::{Agent, AgentCatalog, Holly, Permission, PermissionProfile, SessionId};

use crate::hooks::Hooks;
use crate::host::jobs::JobRegistry;
use crate::mcp::McpScopes;
use crate::mode::ModeTable;
use crate::plan_files::PlanFileRegistry;
use crate::policy::{GrantStore, PermissionResolver};
use crate::retained_output::RetainedOutputRegistry;
use crate::script_ops::ScriptRegistry;
use crate::skills::SkillRegistry;
use crate::tools::SharedRegistry;

use super::{DiscoverySurface, EscapeRoot};

/// Namespace for [`ToolExecutor::builder`]; the spawned executor itself is
/// just its `JoinHandle` (abort it to shut the executor down).
pub struct ToolExecutor;

impl ToolExecutor {
    /// Start configuring the per-engine tool executor over `holly`'s outbox,
    /// dispatching against `tools` (a [`SharedRegistry`] so a live MCP
    /// add/remove is visible without a restart, ADR-0096 — an owned
    /// [`crate::tools::ToolRegistry`] converts with `.shared()`).
    pub fn builder(holly: &Holly, tools: SharedRegistry) -> ToolExecutorBuilder {
        ToolExecutorBuilder {
            holly: holly.clone(),
            tools,
            jobs: JobRegistry::new(),
            retained: RetainedOutputRegistry::new(),
            scripts: ScriptRegistry::new(),
            agents: Arc::new(RwLock::new(AgentCatalog::new())),
            skills: Arc::new(RwLock::new(Arc::new(SkillRegistry::default()))),
            base: PermissionProfile::new(Permission::Allow),
            active: Arc::new(Mutex::new(HashMap::new())),
            perm_modes: Arc::new(Mutex::new(HashMap::new())),
            resolver: None,
            grants: None,
            hooks: Hooks::default(),
            escape_root: None,
            mode_table: None,
            plan_files: Arc::new(PlanFileRegistry::new()),
            mcp_scopes: None,
            catalog: None,
            discovery: DiscoverySurface::default(),
            workdirs: crate::workdir::SessionWorkdirs::default(),
            http_gate: None,
            rhai_trusted: Default::default(),
            skill_dirs: Vec::new(),
            module_dirs: Vec::new(),
            attendance: crate::attendance::Attendance::new(),
        }
    }
}

/// Configures and spawns the tool executor; see [`ToolExecutor::builder`].
/// Fields are `pub(super)` so `tool_runner::spawn_executor` can destructure
/// them — the builder is the executor's whole input set.
#[must_use = "a ToolExecutorBuilder does nothing until `.spawn()` is called"]
pub struct ToolExecutorBuilder {
    pub(super) holly: Holly,
    pub(super) tools: SharedRegistry,
    pub(super) jobs: JobRegistry,
    pub(super) retained: RetainedOutputRegistry,
    pub(super) scripts: ScriptRegistry,
    pub(super) agents: Arc<RwLock<AgentCatalog>>,
    pub(super) skills: Arc<RwLock<Arc<SkillRegistry>>>,
    pub(super) base: PermissionProfile,
    pub(super) active: Arc<Mutex<HashMap<SessionId, Agent>>>,
    pub(super) perm_modes: Arc<Mutex<HashMap<SessionId, String>>>,
    pub(super) resolver: Option<Arc<dyn PermissionResolver>>,
    pub(super) grants: Option<Arc<dyn GrantStore>>,
    pub(super) hooks: Hooks,
    pub(super) escape_root: Option<EscapeRoot>,
    pub(super) mode_table: Option<Arc<ModeTable>>,
    pub(super) plan_files: Arc<PlanFileRegistry>,
    pub(super) mcp_scopes: Option<Arc<McpScopes>>,
    pub(super) catalog: Option<Arc<entanglement_core::Catalog>>,
    pub(super) discovery: DiscoverySurface,
    pub(super) workdirs: crate::workdir::SessionWorkdirs,
    pub(super) http_gate: Option<Arc<crate::http_gate::HttpGate>>,
    pub(super) rhai_trusted: crate::rhai_trust::TrustedFolders,
    pub(super) skill_dirs: Vec<std::path::PathBuf>,
    pub(super) module_dirs: Vec<std::path::PathBuf>,
    pub(super) attendance: crate::attendance::Attendance,
}

impl ToolExecutorBuilder {
    /// The per-session workdir view (#764, ADR-0224) this executor writes
    /// from each `ToolExec` — share it with the `glob`/`grep` tools so they
    /// print paths relative to the session's workdir. Default: private.
    pub fn workdirs(mut self, workdirs: crate::workdir::SessionWorkdirs) -> Self {
        self.workdirs = workdirs;
        self
    }

    /// The background-job registry (#605) the caller's `BashTool` writes, so
    /// `poll`'s job-handle path reaches the jobs `bash` actually spawned.
    /// Default: a private registry — fine when no external `BashTool` shares it.
    pub fn jobs(mut self, jobs: JobRegistry) -> Self {
        self.jobs = jobs;
        self
    }

    /// The retained-output registry (#608) the caller's `CallTool` writes a
    /// truncated result into, read back by `poll`. Default: private.
    pub fn retained(mut self, retained: RetainedOutputRegistry) -> Self {
        self.retained = retained;
        self
    }

    /// The background-`rhai` script registry (#637, ADR-0185). Default:
    /// private — still reachable, since the executor's own `rhai` arm writes
    /// it and its own `poll` arm reads it.
    pub fn scripts(mut self, scripts: ScriptRegistry) -> Self {
        self.scripts = scripts;
        self
    }

    /// A fixed agent catalog (the runtime's read-only copy of the engine's).
    /// Default: [`AgentCatalog::new`] — the built-in profiles only.
    pub fn agents(self, agents: AgentCatalog) -> Self {
        self.live_agents(Arc::new(RwLock::new(agents)))
    }

    /// A live-swappable agent catalog (#329): a definitions watcher replaces
    /// the inner value and the executor sees it on its next lookup.
    pub fn live_agents(mut self, agents: Arc<RwLock<AgentCatalog>>) -> Self {
        self.agents = agents;
        self
    }

    /// The live skill registry `load_skill` resolves against (#400) — the
    /// executor emits `OutEvent::SkillActive` off it. Default: empty.
    pub fn live_skills(mut self, skills: Arc<RwLock<Arc<SkillRegistry>>>) -> Self {
        self.skills = skills;
        self
    }

    /// The global permission ceiling (#172, `config.yml` `permissions:`):
    /// every resolved grade is clamped least-privilege against it, and the
    /// default resolver is built over it. Default: allow-all, i.e. no clamp —
    /// the same profile an absent `permissions:` section yields.
    pub fn base(mut self, base: PermissionProfile) -> Self {
        self.base = base;
        self
    }

    /// The shared session→[`Agent`] map the executor folds lifecycle events
    /// into (spawn gating). Default: a private map;
    /// pass one only if the caller reads it too.
    pub fn active(mut self, active: Arc<Mutex<HashMap<SessionId, Agent>>>) -> Self {
        self.active = active;
        self
    }

    /// The session→permission-mode map (ADR-0207) folded from
    /// `OutEvent::ModeChanged`; the default resolver grades from it, and a
    /// mode-aware sandbox resolver must read this exact map. Default: private.
    ///
    /// A custom [`Self::resolver`] (or sandbox resolver) that reads a mode map
    /// **must** be handed the same `Arc` here: otherwise the executor folds
    /// `ModeChanged` into its private map and the resolver silently grades
    /// from a map that never changes.
    pub fn perm_modes(mut self, perm_modes: Arc<Mutex<HashMap<SessionId, String>>>) -> Self {
        self.perm_modes = perm_modes;
        self
    }

    /// The `Allow | Ask | Deny` decider (#311) — a multi-tenant embedder's
    /// own rules. Default: a [`crate::policy::ModeResolver`] over this
    /// builder's final `perm_modes`, `mode_table`, `tools` and `base`, with
    /// root-relative path normalization when [`Self::escape_root`] is set.
    /// A custom resolver that grades off a session→mode map must share that
    /// map via [`Self::perm_modes`], or it never sees a mode change.
    pub fn resolver(mut self, resolver: Arc<dyn PermissionResolver>) -> Self {
        self.resolver = Some(resolver);
        self
    }

    /// Where "always allow" grants are recorded (#311). Default: **in-memory
    /// only** (#747/#775) — an `Always` approval never reads or writes the
    /// managed `grants.yml`. A production single-user head wanting persisted
    /// grants passes `Arc::new(DefaultGrantStore::load())` explicitly.
    pub fn grants(mut self, grants: Arc<dyn GrantStore>) -> Self {
        self.grants = Some(grants);
        self
    }

    /// Lifecycle hooks (#199, ADR-0066). Default: none.
    pub fn hooks(mut self, hooks: Hooks) -> Self {
        self.hooks = hooks;
        self
    }

    /// Approval-gated escape-root access (ADR-0109); its `root` is also the
    /// `propose_plan` project root. Default: strict containment — an
    /// out-of-root path is a hard error, and plans resolve against the cwd.
    pub fn escape_root(mut self, escape_root: EscapeRoot) -> Self {
        self.escape_root = Some(escape_root);
        self
    }

    /// The mode table `perm_modes` names resolve against (ADR-0207) — the
    /// default resolver's rules and a spawn's `max_depth`/`max_agents`.
    /// Default: [`ModeTable::builtin`] (no `config.yml` `modes:` tuning).
    pub fn mode_table(mut self, mode_table: Arc<ModeTable>) -> Self {
        self.mode_table = Some(mode_table);
        self
    }

    /// Plan-file staleness tracking (#513), shared with a plans-folder watch
    /// (`plan_watch::spawn_plans_watcher`, #627). Default: private.
    pub fn plan_files(mut self, plan_files: Arc<PlanFileRegistry>) -> Self {
        self.plan_files = plan_files;
        self
    }

    /// Session-keyed per-user MCP scopes (#684, ADR-0188). Default: none —
    /// single-user, byte-identical to skutter.
    pub fn mcp_scopes(mut self, scopes: Arc<McpScopes>) -> Self {
        self.mcp_scopes = Some(scopes);
        self
    }

    /// The provider/model catalog `list_models` lists and `agent`'s `model`
    /// parameter is validated against. Default: none — both report "no
    /// catalog configured".
    pub fn catalog(mut self, catalog: Arc<entanglement_core::Catalog>) -> Self {
        self.catalog = Some(catalog);
        self
    }

    /// Enable the rhai `http()` binding (ADR-0227) over `gate`'s `http_auth`
    /// credentials: `spawn` registers the `http` tool with this same gate, so
    /// injection and the forced per-host approval can never come from two
    /// different gates. Default: no `http` tool at all.
    pub fn http_gate(mut self, gate: Arc<crate::http_gate::HttpGate>) -> Self {
        self.http_gate = Some(gate);
        self
    }

    /// `config.yml` `rhai.trusted` folders, canonical (ADR-0228): a
    /// `rhai(path)` inside one runs with no launch card and every nested call
    /// force-allowed (explicit denies still win). A plain `Vec` is trusted
    /// unconditionally, like the project layer. Default: none.
    pub fn rhai_trusted(mut self, folders: impl Into<crate::rhai_trust::TrustedFolders>) -> Self {
        self.rhai_trusted = folders.into();
        self
    }

    /// The user and project skill layer dirs ([`crate::skills::skill_dirs`]):
    /// a `write`/`edit`/`apply_patch` of a `SKILL.md`/`lib.rhai` under one
    /// always asks (ADR-0228). Default: none.
    pub fn skill_dirs(mut self, dirs: Vec<std::path::PathBuf>) -> Self {
        self.skill_dirs = dirs;
        self
    }

    /// The user and project rhai module dirs ([`crate::skills::module_dirs`]):
    /// a `write`/`edit`/`apply_patch` of a `*.rhai` under one always asks
    /// (ADR-0248). Default: none.
    pub fn module_dirs(mut self, dirs: Vec<std::path::PathBuf>) -> Self {
        self.module_dirs = dirs;
        self
    }

    /// The discovery tools' shared state (ADR-0226), shared with the
    /// caller's tool-spec resolver. Default: [`DiscoverySurface::default`] —
    /// private state and an empty MCP roster.
    pub fn discovery(mut self, discovery: DiscoverySurface) -> Self {
        self.discovery = discovery;
        self
    }

    /// The attendance registry the heads register their watchers in
    /// (ADR-0247): a watched session — or a child of one — parks an `Ask`
    /// instead of collapsing it, and its cards and questions never time
    /// out. Default: private, so nobody watches and a mode's
    /// `question_timeout` alone decides.
    pub fn attendance(mut self, attendance: crate::attendance::Attendance) -> Self {
        self.attendance = attendance;
        self
    }

    /// Spawn the executor. Subscribes to the engine synchronously, before
    /// returning, so no `ToolExec`/`Prompt` sent right after is missed; runs
    /// until the engine's outbox closes.
    pub fn spawn(self) -> tokio::task::JoinHandle<()> {
        super::spawn_executor(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn builder(holly: &Holly) -> ToolExecutorBuilder {
        ToolExecutor::builder(holly, crate::tools::ToolRegistry::new().shared())
    }

    /// The defaults embedders rely on (and `docs/embedding/tool-execution.md` documents):
    /// no clamp, no persisted grants, no custom policy, strict containment,
    /// single-user.
    #[tokio::test]
    async fn bare_builder_carries_the_documented_defaults() {
        let holly = Holly::spawn(entanglement_core::EngineConfig::default());
        let b = builder(&holly);
        assert_eq!(b.base, PermissionProfile::new(Permission::Allow));
        assert!(
            b.resolver.is_none(),
            "resolver defaults to ModeResolver at spawn"
        );
        assert!(b.grants.is_none(), "grants default to in-memory at spawn");
        assert!(
            b.mode_table.is_none(),
            "mode table defaults to built-in at spawn"
        );
        assert!(b.escape_root.is_none());
        assert!(b.mcp_scopes.is_none());
        assert!(b.catalog.is_none());
        assert!(b.hooks.is_empty());
        assert!(b.active.lock().unwrap().is_empty());
        assert!(b.perm_modes.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn setters_replace_their_default() {
        let holly = Holly::spawn(entanglement_core::EngineConfig::default());
        let base = PermissionProfile::new(Permission::Ask);
        let table = Arc::new(ModeTable::builtin().expect("built-in modes parse"));
        let modes = Arc::new(Mutex::new(HashMap::new()));
        let b = builder(&holly)
            .base(base.clone())
            .mode_table(table.clone())
            .perm_modes(modes.clone())
            .grants(Arc::new(crate::policy::DefaultGrantStore::load_from(None)));
        assert_eq!(b.base, base);
        assert!(Arc::ptr_eq(b.mode_table.as_ref().unwrap(), &table));
        assert!(Arc::ptr_eq(&b.perm_modes, &modes));
        assert!(b.grants.is_some());
    }
}
