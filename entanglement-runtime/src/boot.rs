//! Engine bring-up shared by every engine-backed head: definitions discovery,
//! the shared HTTP client, the engine config with its resolvers, and the
//! spawned [`Holly`]. The runtime services wired around it live in
//! [`crate::services`].

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, RwLock};

use anyhow::{Context, Result};
use entanglement_core::{AgentCatalog, Holly};
use entanglement_provider::{Catalog, HttpClient, ModelInfo};

use crate::engine_setup::build_config;
use crate::factories::build_model_resolver;
use crate::providers::web_search_config;
use crate::tool_runner::EscapeRoot;
use crate::{
    agents, config, endpoint, host, logging, mcp, mode, policy, retained_output, script_ops,
    skills, subagent, system_prompt, system_prompt_mode, tool_advertising, SharedRegistry,
};

/// Everything [`boot`] produced that the services and heads still need, past
/// the point where `engine_config` moved into [`Holly::spawn`].
pub(crate) struct Engine {
    pub(crate) holly: Holly,
    pub(crate) model_info: ModelInfo,
    pub(crate) provider_name: String,
    pub(crate) http_client: HttpClient,
    pub(crate) tools: SharedRegistry,
    pub(crate) tool_names: Vec<String>,
    pub(crate) mcp_active: mcp::ActiveServers,
    pub(crate) mcp_available: Arc<mcp::AvailableMcp>,
    pub(crate) mcp_configs: mcp::ServerConfigs,
    pub(crate) mcp_capabilities: HashMap<String, Vec<String>>,
    pub(crate) escape_root: EscapeRoot,
    pub(crate) sandbox_config: policy::SandboxConfig,
    pub(crate) jobs: host::JobRegistry,
    pub(crate) retained: retained_output::RetainedOutputRegistry,
    pub(crate) scripts: script_ops::ScriptRegistry,
    pub(crate) live_skills: Arc<RwLock<Arc<skills::SkillRegistry>>>,
    pub(crate) live_profiles: Arc<RwLock<AgentCatalog>>,
    pub(crate) live_agent_models: Arc<Mutex<config::agent_models::AgentModelStore>>,
    pub(crate) live_agent_generation: Arc<Mutex<config::agent_generation::AgentGenerationStore>>,
    pub(crate) live_aux_models: Arc<Mutex<config::aux_models::AuxModelStore>>,
    pub(crate) aux_registry: crate::aux_llm::AuxLlmRegistry,
    pub(crate) advertising_state: Arc<tool_advertising::AdvertisingState>,
    pub(crate) advertising_inputs: Arc<tool_advertising::AdvertisingInputs>,
}

/// Discover definitions, build the engine config and spawn the engine.
pub(crate) async fn boot(
    cwd: &Path,
    user_config: &config::Config,
    catalog: &Catalog,
) -> Result<Engine> {
    // Discover file-based agent definitions (#112): embedded built-ins, then the
    // user dir, then the project dir. A malformed file is a loud error. Each
    // agent body is composed with the shared preamble, project brief, env block,
    // and skill index into its final system prompt (#113) as it is loaded.
    // Discover skills (#114): embedded stock skills, then user, then project. A
    // malformed SKILL.md is a loud error. Only `name` + `description` reach the
    // model, folded into the assembled system prompt as a tier-1 disclosure list
    // (user_only skills withheld) — selection stays the model's own reasoning.
    let skill_registry =
        std::sync::Arc::new(skills::load_registry(cwd).context("loading skill definitions")?);
    // The runtime's own live-reloadable mirror (#329, ADR-0084): a definitions
    // watcher swaps this for a freshly-loaded registry on a debounced dir
    // change. `load_skill` (below) and the watcher both share this handle;
    // core's own copy (built into `engine_config` below) has no such seam.
    let live_skills: Arc<RwLock<Arc<skills::SkillRegistry>>> =
        Arc::new(RwLock::new(skill_registry.clone()));
    let mut prompt_ctx = system_prompt::PromptContext::load(cwd);
    prompt_ctx.skills = skill_registry.disclosures();
    // The MCP capability index (#426): config-side `capabilities:` hints on
    // `user_config.mcp` *and* every catalog-bundled server (e.g. z.ai's
    // `web_search_prime`, which never joins `user_config.mcp` — #542),
    // folded into any bare `read`/`write`/`call` permission key alongside the
    // fixed built-in set — computed once here (like the ceiling permission
    // below) rather than re-derived on every reload.
    let mut mcp_capabilities = mcp::capability_index_with_catalog(catalog, &user_config.mcp)
        .context("resolving MCP capability hints")?;
    // Every declared endpoint tool joins the same data-driven `call` index
    // (#560 P8), unconditionally — see `endpoint::call_capability_names`.
    mcp_capabilities
        .entry("call".to_string())
        .or_default()
        .extend(endpoint::call_capability_names(&user_config.endpoints));
    // The skill registry also resolves per-agent `skills:` preload bodies (#117),
    // orthogonal to the tier-1 disclosures above and to the `load_skill` mask.
    let mut profiles = agents::load_registry(cwd, &prompt_ctx, &skill_registry, &mcp_capabilities)
        .context("loading agent definitions")?;
    // Per-agent model pins (#323, ADR-0081): overlay the persisted
    // `agent-models.yml` onto the freshly-loaded profiles *before* the engine
    // builds — a persisted pin wins over frontmatter. The store is then threaded
    // into the TUI so a `/model` choice under an active profile persists back.
    let agent_models = config::agent_models::AgentModelStore::load();
    agent_models.apply(&mut profiles);
    // Shared with the definitions watcher (#329): a reload re-reads the managed
    // file and re-applies it onto the freshly-loaded profiles the same way.
    let live_agent_models = Arc::new(Mutex::new(agent_models));
    logging::startup_mark("skills+agents");
    // Per-agent generation-parameter overrides (#374, ADR-0094): unlike the model
    // pin above, this doesn't overlay onto `profiles` (`GenerationParams` isn't
    // `Eq`, so it can't join `Agent`'s derive) — instead it's wrapped in a
    // `GenerationResolver` closure threaded onto `EngineConfig` below, resolved
    // by profile name at session start / `SetAgent`. Also threaded straight into
    // the TUI (#376), the only surface that writes to it (`/set`'s
    // persist-on-confirmation).
    let live_agent_generation = Arc::new(Mutex::new(
        config::agent_generation::AgentGenerationStore::load(),
    ));

    // Per-endpoint in-flight cap: the client default (3) unless the catalog
    // entry sets `concurrency:` (catalog data only, ADR-0213). Bounds how many
    // requests (incl. streaming sub-agents) hit one provider at once, so a
    // spawn-storm queues instead of 429-storming (ADR-0111).
    let http_client = HttpClient::new()?;
    // The skill registry is shared: its tier-1 disclosures fed the system prompt
    // above, and `load_skill` (#115) resolves tier-2 bodies against it at runtime.
    let (
        mut engine_config,
        model_info,
        provider_name,
        tools,
        tool_names,
        mcp_active,
        mcp_available,
        escape_root,
        sandbox_config,
        jobs,
        retained,
        scripts,
    ) = build_config(
        catalog,
        &http_client,
        profiles,
        live_skills.clone(),
        user_config,
    )
    .await;
    logging::startup_mark("tools+mcp connect");
    // Per-agent generation-parameter overrides (#374, ADR-0094): resolved by
    // profile name at session start / `SetAgent`, same precedence tier the model
    // pin's persisted file occupies (persisted store > profile/catalog default).
    engine_config.generation_resolver = Some(
        config::agent_generation::AgentGenerationStore::resolver(live_agent_generation.clone()),
    );
    // `explore`/`describe`'s shared state (#560, ADR-0196 §2-4): the pinned
    // session→mode map plus the `client_side`-encoding discovered-tool set.
    // Constructed once, here, so the *same* `Arc` is visible to the system-
    // prompt resolver just below, the tool-spec resolver further down, and
    // the tool executor (`services::start`) — Phase P1's loop-local-only map
    // is gone.
    let advertising_state = std::sync::Arc::new(tool_advertising::AdvertisingState::new());
    // Per-session system prompt (#566, #560): folds the `<env>` date-freshness
    // patch with the ADR-0196 §5 `ToolSearch`-mode prompt slimming into the
    // one `SystemPromptResolver` slot — consulted once per turn, a no-op for
    // a `Full`-mode session except on the date's actual rollover, always-Some
    // for a `ToolSearch`-mode one (the slimmed prompt, byte-stable per
    // session since the mode never changes mid-session).
    engine_config.system_prompt_resolver =
        Some(system_prompt_mode::resolver(advertising_state.clone()));
    // The static "what modes exist" preamble (ADR-0207 §9/§12): the runtime
    // owns the mode table, so it supplies this text; core folds it once into
    // the cached system prompt. Must stay static across every session — see
    // `mode::describe::modes_preamble`'s own doc for why.
    engine_config.modes_preamble = Some(mode::describe::modes_preamble());
    // Per-purpose aux-model pins (Issue 5): a managed `aux-models.yml` sibling
    // of `agent-models.yml`, consulted by the `AuxLlmRegistry` to route a side
    // transformation (session-title generation today; compaction summary once
    // wired) to a separate provider/model, falling back to the primary when a
    // purpose is unset or its pin won't resolve. Threaded into the TUI so
    // `/aux-model` persists back to disk.
    let live_aux_models = Arc::new(Mutex::new(config::aux_models::AuxModelStore::load()));
    // The registry reuses the same catalog resolver the engine calls on
    // `SetModel` (capturing the warm per-endpoint pool) + the primary
    // `LlmFactory` as the no-pin fallback, so an unset purpose is byte-identical
    // to "use the main model". The catalog + the primary model's own effective
    // concurrency cap (#589) back `AuxLlmRegistry::concurrency_cap`, so a caller
    // firing an aux call alongside a live primary-turn call (the session-title
    // generator) can tell whether it would contend for the same per-model
    // permit before doing so.
    let primary_concurrency = catalog.effective_concurrency(&provider_name, &model_info.id);
    let aux_registry = crate::aux_llm::AuxLlmRegistry::new(
        live_aux_models.clone(),
        build_model_resolver(
            catalog.clone(),
            http_client.clone(),
            web_search_config(user_config),
        ),
        engine_config.llm_factory.clone(),
        catalog.clone(),
        primary_concurrency,
        (provider_name.clone(), model_info.id.clone()),
    );
    // Route session compaction (both `/compact` and the auto-summarize overflow
    // path) through the `summarize` pin when one is set — core calls this with
    // the purpose string and falls back to the session's own backend on `None`.
    engine_config.aux_llm_resolver = Some(aux_registry.clone().resolver());
    // Dynamic `ToolRegistry` (#372, ADR-0096) behind the per-session resolver
    // core consults every round (ADR-0076) — shaped per the session's pinned
    // advertising mode/encoding/discovery (ADR-0196, ADR-0204). Lives in the
    // library (`tool_advertising::surface`) so tests run the same resolver.
    // The resolver pins each session at its first resolution from the model
    // core hands it — the startup backend when none is bound (ADR-0204).
    let advertising_inputs = Arc::new(
        tool_advertising::AdvertisingInputs::new(
            Arc::new(user_config.clone()),
            Some(Arc::new(catalog.clone())),
        )
        .with_default_model(provider_name.clone(), model_info.id.clone()),
    );
    engine_config.tool_spec_resolver = Some(tool_advertising::surface::tool_spec_resolver(
        tool_advertising::surface::SurfaceSources {
            tools: tools.clone(),
            avail: mcp_available.clone(),
            advertising: advertising_state.clone(),
            inputs: advertising_inputs.clone(),
            agent_specs: subagent::agent_specs(&engine_config.agents),
        },
    ));
    // Live MCP server management (#375): `ActiveServers` was seeded by
    // `build_config` from the servers it actually connected; `ServerConfigs`
    // starts from the *whole* user-configured set (including a disabled/failed
    // one) — the wider map `save_mcp` must round-trip so a live add/remove
    // never drops an unrelated entry. Catalog-bundled servers (#542) are
    // deliberately absent: they never persist to `config.yml`.
    let mcp_configs: mcp::ServerConfigs = Arc::new(Mutex::new(user_config.mcp.clone()));
    // Fail fast on a malformed config (e.g. a profile registry without `build`)
    // rather than leaning on the supervisor's synthesized fallback.
    if let Err(e) = engine_config.validate() {
        eprintln!("skutter: invalid engine configuration: {e}");
        std::process::exit(2);
    }
    // The runtime keeps its own live-reloadable mirror of the profile registry
    // (#329, ADR-0084) to resolve permissions (#59) and drive the TUI picker;
    // the engine gets its own (immutable-for-the-process-lifetime) copy via
    // `engine_config`, captured here *before* it moves into `Holly::spawn`.
    let live_profiles: Arc<RwLock<AgentCatalog>> =
        Arc::new(RwLock::new(engine_config.agents.clone()));
    let holly = Holly::spawn(engine_config);
    Ok(Engine {
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
    })
}
