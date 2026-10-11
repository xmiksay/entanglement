//! The runtime services wired around a booted engine — tool executor,
//! responders, persistence, watchers — plus the handles every head needs and
//! the ordered shutdown that lets persistence drain.

use std::path::Path;
use std::sync::{Arc, Mutex, RwLock};

use anyhow::{Context, Result};
use entanglement_core::{AgentCatalog, Holly};
use entanglement_provider::{Catalog, HttpClient, ModelInfo};
use tokio::task::JoinHandle;

use crate::boot::Engine;
use crate::policy::DefaultGrantStore;
use crate::tool_runner::DiscoverySurface;
use crate::{
    capability, config, history, logging, mcp, mode, persistence, plan_files, plan_watch, throttle,
    tool_advertising, tool_runner, watch,
};

/// What the interactive heads (the TUI in particular) consume once the
/// services are running.
pub(crate) struct Head {
    pub(crate) model_info: ModelInfo,
    pub(crate) provider_name: String,
    pub(crate) live_profiles: Arc<RwLock<AgentCatalog>>,
    /// User slash commands (#733), live-reloaded by the definitions watcher.
    pub(crate) live_commands: crate::slash_commands::LiveCommands,
    pub(crate) live_agent_models: Arc<Mutex<config::agent_models::AgentModelStore>>,
    pub(crate) live_agent_generation: Arc<Mutex<config::agent_generation::AgentGenerationStore>>,
    pub(crate) live_aux_models: Arc<Mutex<config::aux_models::AuxModelStore>>,
    pub(crate) reload_rx: tokio::sync::mpsc::UnboundedReceiver<String>,
    pub(crate) tool_names: Vec<String>,
    pub(crate) http_client: HttpClient,
    pub(crate) grants: Arc<DefaultGrantStore>,
    pub(crate) mcp: mcp::McpHandles,
    pub(crate) advertising_state: Arc<tool_advertising::AdvertisingState>,
}

/// The background tasks [`Services::shutdown`] tears down.
pub(crate) struct Services {
    tool_executor: JoinHandle<()>,
    mcp_responder: JoinHandle<()>,
    throttle: JoinHandle<()>,
    session_title: JoinHandle<()>,
    narrate: JoinHandle<()>,
    history: JoinHandle<()>,
    watcher: Option<JoinHandle<()>>,
    plans_watcher: Option<JoinHandle<()>>,
    persistence: JoinHandle<()>,
}

/// Start every runtime service around `engine`, returning the engine handle,
/// the head-facing handles, and the services to shut down afterwards.
pub(crate) fn start(
    engine: Engine,
    cwd: &Path,
    user_config: &config::Config,
    catalog: &Catalog,
) -> Result<(Holly, Head, Services)> {
    let Engine {
        holly,
        model_info,
        provider_name,
        http_client,
        tools,
        tool_names,
        mcp_active,
        mcp_available,
        mcp_configs,
        mcp_capabilities,
        escape_root,
        sandbox_config,
        jobs,
        retained,
        scripts,
        live_skills,
        live_profiles,
        live_agent_models,
        live_agent_generation,
        live_aux_models,
        aux_registry,
        advertising_state,
        advertising_inputs,
    } = engine;
    // The TUI heads need their own handle too (`/tools`' status column,
    // ADR-0199 part 3) — cloned here since the executor construction below
    // moves `advertising_state` itself into its `DiscoverySurface`.
    let advertising_state_for_tui = advertising_state.clone();

    // Runtime owns tool execution (#58) and permission dispatch + approval (#59):
    // answer the engine's ToolExec round-trip, gating each call on
    // `live_profiles`. The user config's `permissions` section is the global
    // ceiling clamped over every resolved grade (#172), and its `hooks` section
    // wires the lifecycle hooks (#199) around tool dispatch and prompt ingress.
    // Per-session permission mode (ADR-0207 stage 4), folded from
    // `OutEvent::ModeChanged` the same way `active` folds `AgentChanged` —
    // `ModeResolver` grades every call from this map, not from `active`.
    // `perm_modes` is the *same* map `sandbox_config` was built from in
    // `build_config` (ADR-0207 §6, stage 5b) — sandboxing must see exactly
    // the mode *names* dispatch sees, not a second copy that can drift.
    //
    // `mode_table` is deliberately **not** `sandbox_config.table` here
    // (ADR-0207 stage 6c): that early table has to exist before the tool
    // registry does (`register_default_tools` bakes its sandbox resolver in
    // at construction), so it's built-in-only and carries no `config.yml`
    // `modes:` tuning — harmless for sandboxing, since `ModeTuning` has no
    // `sandbox`/limits fields to tune (only rule grades). Permission
    // *grading* does need the tuned rules, and tuning's own guard (ADR-0207
    // §5) needs a real capability resolver to validate against — both only
    // available now that `tools` is fully populated.
    let perm_modes = sandbox_config.modes.clone();
    let mode_table = Arc::new({
        let registry = tools.read().expect("tool registry lock poisoned");
        mode::build_table(&user_config.modes, &|name| {
            capability::capability_of(name, &registry)
        })
        .context("building the permission mode table from config.yml `modes:`")?
    });
    // `grants` is built here rather than left to the builder's in-memory
    // default (#747/#775): the head persists `Always` grants to the managed
    // `grants.yml`, and the same store feeds the definitions watcher's
    // `LiveDefinitions` below (#329), so a grant another skutter instance
    // recorded is visible on the next reload.
    let grants = Arc::new(DefaultGrantStore::load());
    // Per-session plan-file staleness tracking (#513): constructed here, not
    // inside the executor, so the dedicated plans-folder watch (#627, below)
    // can share this exact instance — its out-of-band notice and the
    // executor's staleness guard must agree on what the agent last knew.
    let plan_files = Arc::new(plan_files::PlanFileRegistry::new());
    // Cloned before `escape_root` moves into the executor below — reused for
    // the plans watcher's own root (#627).
    let plan_root = escape_root.root.clone();
    // The resolver is the builder's default `ModeResolver` over exactly
    // these `perm_modes`/`mode_table`/`tools`/ceiling, with root-relative arg
    // normalization off `escape_root` (#485, ADR-0125).
    let tool_executor = tool_runner::ToolExecutor::builder(&holly, tools.clone())
        .jobs(jobs)
        .retained(retained)
        .scripts(scripts)
        .live_agents(live_profiles.clone())
        .live_skills(live_skills.clone())
        .base(user_config.permissions.clone())
        .perm_modes(perm_modes)
        .grants(grants.clone())
        .hooks(user_config.hooks.clone())
        // Escape-root approval (ADR-0109): the same store the host tools read.
        .escape_root(escape_root)
        // `mode_table` carries the `config.yml` `modes:` tuning; sandboxing
        // reads `perm_modes` directly, independent of this executor.
        .mode_table(mode_table)
        .plan_files(plan_files.clone())
        // The same advertising inputs the resolver pins from; the executor
        // only logs a `SetModel` onto a model preferring another mode.
        .advertising_inputs(advertising_inputs)
        // `explore`/`describe`'s shared state (#560, ADR-0196 §4): the same
        // `advertising_state` the tool-spec/system-prompt resolvers `boot`
        // wired read, plus the MCP three-state roster `explore` lists.
        .discovery(DiscoverySurface {
            advertising: advertising_state,
            mcp_avail: mcp_available.clone(),
            mcp_active: mcp_active.clone(),
            validation: std::sync::Arc::new(crate::arg_validate::LoopBreaker::new()),
            // ADR-0201: the same shared endpoint-pool client a dispatch-time
            // lazy MCP re-enable rides.
            http: Some(http_client.clone()),
        })
        .spawn();

    // Live MCP server management (#375): a runtime service answering
    // `McpList`/`McpAdd`/`McpRemove` off the inbound fan-out, since it alone
    // holds `tools` + `mcp_active` + `mcp_configs` — mirrors
    // `history::spawn_history_responder`'s answer to `ReplayFrom`.
    let mcp_responder_handle = mcp::spawn_mcp_responder(
        &holly,
        tools.clone(),
        mcp_active.clone(),
        mcp_configs,
        mcp_available.clone(),
        catalog.key_envs(),
        http_client.clone(),
    );

    // Wire-visible LLM-endpoint throttle transitions (#517, ADR-0141): polls
    // the shared `HttpClient` (the same one injected into the TUI at
    // `tui::mod::run`) and emits `OutEvent::Throttle` so stdio/WS heads see a
    // 429 cool-down or pacing slowdown too, not just the TUI's direct poll.
    let throttle_handle = throttle::spawn_throttle_responder(&holly, http_client.clone());

    // Auto session-title generator (Issue 5): on the first prompt of an
    // unnamed session, spawns a background task that asks the aux
    // `session_title` LLM for a short title and sets it via `SetSessionMeta`.
    // Purely runtime-side; best-effort (a failure is logged + dropped). Aborted
    // at shutdown alongside the other responders.
    let session_title_handle =
        crate::session_title::spawn_session_title_generator(&holly, aux_registry.clone());
    // Live action narrator (#635): on every tool call, spawns a background
    // task that asks the aux `narrate` LLM for a short "what the agent is
    // doing" phrase and sets it via `SetSessionMeta`. Same purely-runtime-side,
    // best-effort shape as the title generator above.
    let narrate_handle = crate::narrate::spawn_action_narrator(&holly, aux_registry);

    // Spawn the persistence subscriber to log all inbound + outbound frames.
    // One store for both: the log the tap writes is the one `ReplayFrom` reads.
    let sessions = crate::session_store::SessionStore::for_cwd(cwd);
    let persistence_handle = persistence::spawn_persistence_subscriber(&holly, sessions.clone());
    // Answer `ReplayFrom` late-subscriber history queries from that same log
    // (#160). The handle is kept so it can be aborted at shutdown — like the
    // tool executor and watcher, it holds a `Holly` clone (an inbound
    // subscriber + event emitter) that keeps the broadcast channels open until
    // every clone is dropped. Aborting it is what lets the persistence
    // subscriber see `RecvError::Closed` and flush.
    let history_handle = history::spawn_history_responder(&holly, sessions);

    // Live definitions watcher (#329, ADR-0084): inotify on the resolved
    // agent/skill dirs + managed files, debounced, reloading straight into the
    // runtime-held mirrors `boot` built — never core's `EngineConfig`, which stays
    // pinned to what was loaded at process start (ADR-0081 precedent: live
    // registry mutation in core is a rejected design). `reload_rx` only matters
    // to the TUI (a status line); every other head lets its messages drop.
    // User slash commands (#733): head-side prompt templates, loaded here
    // rather than in `boot` because the engine never sees them — a malformed
    // native command file is as loud at startup as a malformed skill.
    let live_commands: crate::slash_commands::LiveCommands = Arc::new(RwLock::new(Arc::new(
        crate::slash_commands::load_registry(cwd).context("loading slash commands")?,
    )));
    let live = watch::LiveDefinitions {
        agents: live_profiles.clone(),
        skills: live_skills.clone(),
        commands: live_commands.clone(),
        agent_models: live_agent_models.clone(),
        grants: grants.clone(),
        mcp_capabilities: mcp_capabilities.clone(),
    };
    let (reload_tx, reload_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let watcher_handle = watch::spawn_watcher(cwd.to_path_buf(), live, Some(reload_tx));

    // Dedicated plans-folder watch (#627, ADR-0145 "Consequences"): notifies a
    // plan session's transcript the moment its bound file changes out of band
    // (an editor save, not this session's own tool calls) instead of only at
    // the next `propose_plan(path=...)` call. Deliberately its own watcher,
    // not folded into the definitions watcher above — that one reloads
    // agent/skill/config *definitions*, an unrelated reload action; this
    // shares only the `spawn_debounced_watcher` primitive. `plan_files` is the
    // exact instance the tool executor above reads/writes, so the two stay in
    // sync (a self-heal here is the same fact the staleness guard would
    // otherwise re-discover on the next `propose_plan` call).
    let plans_watcher_handle =
        plan_watch::spawn_plans_watcher(&holly, plan_root, plan_files.clone());
    logging::startup_mark("engine+responders ready");
    let head = Head {
        model_info,
        provider_name,
        live_profiles,
        live_commands,
        live_agent_models,
        live_agent_generation,
        live_aux_models,
        reload_rx,
        tool_names,
        http_client,
        grants,
        mcp: mcp::McpHandles {
            avail: mcp_available,
            registry: tools,
            active: mcp_active,
        },
        advertising_state: advertising_state_for_tui,
    };
    let services = Services {
        tool_executor,
        mcp_responder: mcp_responder_handle,
        throttle: throttle_handle,
        session_title: session_title_handle,
        narrate: narrate_handle,
        history: history_handle,
        watcher: watcher_handle,
        plans_watcher: plans_watcher_handle,
        persistence: persistence_handle,
    };
    Ok((holly, head, services))
}

impl Services {
    /// Shut the engine down and let the persistence task flush before exit: a
    /// one-shot `run` ends the instant the turn does, and the detached
    /// subscriber still holds broadcast-buffered events it hasn't written.
    /// `Holly::shutdown` closes the outbox however many clones survive (#700) —
    /// a `serve` connection or a detached task can no longer hold the drain
    /// open. The runtime services are still aborted so their own work (a
    /// `JoinSet` local to each future, the tool executor's `CancelAllOnDrop`
    /// guard, #545) stops too.
    pub(crate) async fn shutdown(self, holly: &Holly) {
        self.tool_executor.abort();
        self.mcp_responder.abort();
        self.throttle.abort();
        self.session_title.abort();
        self.narrate.abort();
        self.history.abort();
        if let Some(h) = self.watcher {
            h.abort();
        }
        if let Some(h) = self.plans_watcher {
            h.abort();
        }
        holly.shutdown().await;
        // Backstop (#545): the drain above should now complete promptly, but a
        // future oversight in a detached task (or a sink write that never
        // returns) must not hang the process forever — a bounded wait beats an
        // unkillable `skutter run` again.
        if tokio::time::timeout(std::time::Duration::from_secs(5), self.persistence)
            .await
            .is_err()
        {
            tracing::warn!(
                "persistence subscriber did not shut down within 5s; exiting without a full drain"
            );
        }
    }
}
