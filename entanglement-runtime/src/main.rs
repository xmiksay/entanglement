//! `skutter` — terminal head for the headless agent engine.
//!
//! The default invocation — bare `skutter` with no subcommand and no prompt —
//! launches the TUI. A positional prompt (`skutter "do the thing"`) is an
//! implicit one-shot `run`. Explicit subcommands:
//! - `run` sends a prompt and streams events until `Done`. `--format json`
//!   emits raw NDJSON (like `opencode run --format json`); `--format text`
//!   renders human-friendly output.
//! - `pipe` is a bidirectional NDJSON relay: `InMsg` lines on stdin,
//!   `OutEvent` lines on stdout. For scripting / editor integration.

// Only the bin-specific heads stay `mod` here. The reusable library modules
// live in `lib.rs`; importing them (rather than re-declaring `mod`) stops the
// library source being compiled a second time and removes the hand-sync that
// let a bin-only `mod` slip past `check-lean` (issue #208). The crate-root
// `use` below makes each library module reachable as `crate::<name>` from the
// bin submodules too, matching the existing `use entanglement_provider::…`.
mod pipe;
mod run;
mod tui;

use entanglement_runtime::{
    agents, ask_user, config, extra_roots, history, host, inspect, logging, mcp, persistence,
    plan_tasks, policy, propose_plan, script, session_store, skills, subagent, system_prompt,
    tool_names, tool_runner, watch, ToolRegistry,
};
use tool_runner::EscapeRoot;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use entanglement_core::{EngineConfig, Holly, InMsg, ProfileRegistry, SessionId};
use entanglement_provider::{
    Catalog, GenerationParams, HttpClient, LlmFactory, ModelInfo, ModelPricing, ModelResolver,
    ProviderEntry, ResolvedModel, WebSearchConfig, Wire,
};
use policy::{DefaultGrantStore, PermissionResolver, ProfileResolver};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

use host::{BashTool, CallTool, ReadRawTool, SandboxPolicy};
use pipe::pipe;
use run::run_one;
use session_store::{integrity_gap, list_sessions, pair_records, read};
use skills::LoadSkillTool;
use tui::tui;

/// Pick a provider and build the engine config.
///
/// Selection order:
/// 1. `ENTANGLEMENT_PROVIDER` env, one of `zai | openai | ollama | anthropic | echo`
///    (explicit; errors loudly if the matching key is missing).
/// 2. Auto-detect by key presence, z.ai first (the project's primary), then
///    OpenAI, then Anthropic.
/// 3. Fall back to `EchoLlm` so `skutter` always runs end-to-end and history
///    propagation is observable.
///
/// Set `ENTANGLEMENT_PROVIDER=ollama` to use a local keyless Ollama (it has no key to
/// auto-detect on). Set `ENTANGLEMENT_PROVIDER=echo` to use the EchoLlm stub,
/// which returns a summary of the messages it received (useful for debugging
/// history propagation without a real provider). z.ai/OpenAI/Ollama share one
/// OpenAI-compatible client ([`entanglement_provider::openai_factory`]); Anthropic
/// has its own client.
///
/// The root-contained host quintet (`read`/`glob`/`grep`/`edit`/`write`), `call`
/// (argv exec, no shell), and `load_skill` are always registered, rooted at the
/// current working directory, so the `build`/`plan`/`explore` permission
/// profiles gate something real out of the box. `bash` (shell) stays opt-in:
/// set `ENTANGLEMENT_ENABLE_BASH=1` to register `BashTool` and its
/// `BashOutputTool` poller — they run unsandboxed with the engine's full
/// privileges by default (ADR-0009 / ADR-0010 / ADR-0045). `call` runs with the
/// same full-privilege, unsandboxed-by-default execution but no shell means no
/// injection surface, so its *registration* no longer rides `bash`'s opt-in gate
/// (ADR-0094); per-profile permission (`Allow`/`Ask`/`Deny`) remains the actual
/// dispatch gate, same as any other tool. Both may instead run confined under
/// bubblewrap — set `ENTANGLEMENT_SANDBOX=bwrap` (`ENTANGLEMENT_SANDBOX_NETWORK=1`
/// to keep network access) — fail-closed: a sandboxed spawn that can't enter
/// the sandbox errors rather than falling back to unsandboxed (#399, ADR-0104).
///
/// Core no longer executes tools (#58): it only advertises their schemas
/// (`cfg.tool_specs`, later kept live via `cfg.tool_spec_resolver`, #372). The
/// returned [`ToolRegistry`] stays in the runtime — wrapped into a
/// [`entanglement_runtime::SharedRegistry`] by the caller — and is handed to
/// [`tool_runner::spawn_tool_executor_with_policy`], which answers the
/// [`entanglement_core::OutEvent::ToolExec`] round-trip.
async fn build_config(
    catalog: &Catalog,
    http_client: &HttpClient,
    profiles: ProfileRegistry,
    skills: Arc<RwLock<Arc<skills::SkillRegistry>>>,
    user_config: &config::Config,
) -> (
    EngineConfig,
    ModelInfo,
    String,
    ToolRegistry,
    Vec<String>,
    HashMap<String, mcp::ActiveServer>,
    EscapeRoot,
) {
    let (mut cfg, model_info, provider_name) = select_provider(catalog, http_client, user_config);
    // Realtime model/provider switch (#218): give the engine a resolver so a
    // session can re-bind its LLM from the catalog with no restart. Captures the
    // catalog + the warm per-endpoint HTTP client (#217).
    cfg.model_resolver = Some(build_model_resolver(
        catalog.clone(),
        http_client.clone(),
        web_search_config(user_config),
    ));
    // File-based agent definitions (#112) replace core's hardcoded fallback trio.
    cfg.profiles = profiles;
    // Thread the resolved model's context window into the engine (#178) so each
    // session budgets its history against the real window (128k for GLM-5.2, not
    // a fixed 180k). `None` (unknown model / echo) keeps core's flat fallback.
    cfg.context_window = model_info.context_window.map(|w| w as usize);
    // User-configurable turn cap (#177): config file → engine, falling back to
    // the `EngineConfig::default()` (200) when unset.
    if let Some(max) = user_config.max_turns {
        cfg.max_turns = max;
    }
    // Idle-TTL auto-hibernation (#401, ADR-0090): config file → engine, `None`
    // (unset) keeps the sweep disabled — the pre-#401 default for every head.
    cfg.idle_ttl = user_config.idle_ttl;
    // Canonicalize the working root once at startup (#163, ADR-0054): host-tool
    // containment checks against this, so a symlinked cwd must resolve to its
    // real path here or every resolved target would look like an escape.
    let root = std::env::current_dir()
        .and_then(|p| p.canonicalize())
        .unwrap_or_else(|_| std::path::PathBuf::from("."));
    let secret_env = catalog.key_envs();
    let bash_enabled = std::env::var("ENTANGLEMENT_ENABLE_BASH").as_deref() == Ok("1");
    // Optional bubblewrap confinement for bash/call (#399, ADR-0104). Off by
    // default — `bash_enabled` alone still means unsandboxed, full-privilege
    // execution, matching every release before this.
    let sandbox = SandboxPolicy::from_env();
    // Per-project scratch dir for default `call` artifacts — outside the repo,
    // so a routine `call` neither pollutes the workdir nor re-triggers the
    // definitions watcher. Best-effort: if the data dir is unavailable, `call`
    // falls back to its legacy in-repo `.entanglement/tmp` location.
    let scratch_base = session_store::scratch_dir(&root).ok();
    // Escape-root approval store (ADR-0109): shared by the host tools (which
    // consult it to relax containment for an approved out-of-root path) and the
    // tool executor (which forces an approval prompt on a first out-of-root
    // access and records the grant here).
    let extra_root_store = Arc::new(extra_roots::ExtraRootStore::load());
    let mut tools = register_default_tools(
        root.clone(),
        scratch_base,
        Some(extra_root_store.clone()),
        secret_env,
        bash_enabled,
        sandbox,
    );
    let escape_root = EscapeRoot {
        root: root.clone(),
        store: extra_root_store,
    };
    if bash_enabled && !sandbox.is_sandboxed() {
        eprintln!(
            "skutter: bash enabled (ENTANGLEMENT_ENABLE_BASH=1) — \
             run unsandboxed with full privileges"
        );
    }
    // `call` is always registered (ADR-0093), so the sandbox notice fires
    // independent of `bash_enabled`.
    if sandbox.is_sandboxed() {
        eprintln!(
            "skutter: bash/call sandboxed via bubblewrap (ENTANGLEMENT_SANDBOX=bwrap, \
             network: {})",
            if sandbox.network { "allowed" } else { "cut" }
        );
    }
    // `load_skill` is tier-2 progressive disclosure (#115): a real host tool (it
    // reads the filesystem), so it is registered here and goes through the *same*
    // per-call permission gate as `read` — no runtime-executor interception.
    tools.register(LoadSkillTool::new(skills));
    // External MCP tool servers (#198): spawn each configured server, discover its
    // `tools/list`, and register every tool into the same registry as a
    // runtime-side provider. They then ride `tool_specs` (schemas) and the
    // `ToolExec` round-trip (execution) with no core change — governed by the same
    // permission profiles as any host tool. A server that fails to connect is
    // logged and skipped, never fatal.
    let initial_mcp = mcp::connect(&user_config.mcp, &mut tools).await;
    cfg.tool_specs = tools.specs();
    // `read_raw` (rhai-only, see `script.rs`'s `parse_json`/`parse_yaml`)
    // registers *after* the specs snapshot above: present in `tools` for
    // execution (the rhai bridge routes through the same `ToolRegistry`), but
    // never advertised as a standalone model-callable tool.
    tools.register(ReadRawTool::new(root.clone()));
    // The `agent_*` family is orchestration, not registry tools (#60, #120): the
    // runtime executor handles them directly, so they only need advertising to
    // the model. Per-profile spawn control (#119, ADR-0040) makes the family
    // *per-profile* — each profile's roster + target enum is scoped to who it may
    // spawn, and a non-spawning profile gets nothing — so it lives in
    // `profile_tool_specs` (appended by core for the active profile), not the
    // shared `tool_specs`. Empty entries are simply omitted.
    // Plan authorship (#231, ADR-0049): `update_plan` and the `propose_plan`
    // finalize step are advertised only to a profile that *explicitly* allowlists
    // them — the default-closed gate that replaces the old `owns_plan` flag, so
    // they never leak to an inherit-all profile. They ride the same per-profile
    // seam as the spawn family; core's #116 mask filters them again at turn time.
    let profile_tool_specs = cfg
        .profiles
        .iter()
        .filter_map(|p| {
            let mut specs = subagent::spawn_specs_for(p, &cfg.profiles);
            specs.extend(propose_plan::specs_for(p));
            specs.extend(plan_tasks::plan_specs_for(p));
            (!specs.is_empty()).then(|| (p.name.clone(), specs))
        })
        .collect();
    cfg.profile_tool_specs = profile_tool_specs;
    // `update_tasks` is a runtime state tool (#231): general progress bookkeeping,
    // no cross-agent authority, so it rides the shared specs (a read-only profile
    // masks it out via its allowlist + permission). The runtime executor
    // intercepts it — and `update_plan` — to emit the `Plan`/`TaskList` snapshot.
    cfg.tool_specs.push(plan_tasks::update_tasks_spec());
    // `ask_user` is likewise runtime-owned (#90) but not a spawn tool: every
    // profile may surface a decision prompt, so it stays in the shared specs.
    cfg.tool_specs.push(ask_user::ask_user_spec());
    // `rhai` is a runtime-owned sandboxed script tool (#122, ADR-0046). Its
    // bindings are exactly the root-contained quintet, so it is no more
    // privileged than the always-registered tools and rides the shared specs
    // (registered by default; a profile masks it like any tool via its
    // allowlist). The executor intercepts it before permission resolution.
    cfg.tool_specs.push(script::rhai_spec());
    // The `/agent` picker's tools-checklist dialog (#330) offers every advertised
    // tool name — captured here (before `cfg` is moved into `Holly::spawn`), not
    // via the `ToolRegistry` alone, so it also includes the runtime-owned specs
    // appended above (`update_tasks`/`ask_user`/`rhai`) that aren't registry
    // tools but are still maskable via a profile's `tools`/`disallowed_tools`.
    let mut tool_names: Vec<String> = cfg.tool_specs.iter().map(|s| s.name.clone()).collect();
    tool_names.sort();
    tool_names.dedup();
    (
        cfg,
        model_info,
        provider_name,
        tools,
        tool_names,
        initial_mcp,
        escape_root,
    )
}

/// Assemble the tool registry: the root-contained quintet plus `call`
/// (registered unconditionally — argv exec, no shell, ADR-0094) and, only when
/// `bash_enabled`, the opt-in `bash`/`bash_output` pair sharing one job
/// registry (ADR-0010/#170). `secret_env` (the catalog's provider API-key env
/// vars, #164) is scrubbed from both exec tools' children. `sandbox` (#399,
/// ADR-0104) optionally confines both `bash` and `call` via bubblewrap —
/// `SandboxPolicy::none()` leaves their spawn behavior unchanged.
fn register_default_tools(
    root: std::path::PathBuf,
    scratch_base: Option<std::path::PathBuf>,
    extra_roots: Option<Arc<extra_roots::ExtraRootStore>>,
    secret_env: Vec<String>,
    bash_enabled: bool,
    sandbox: SandboxPolicy,
) -> ToolRegistry {
    let mut tools = host::host_tools_with_extra_roots(root.clone(), extra_roots.clone());
    let mut call = CallTool::new(root.clone())
        .with_secret_env(secret_env.clone())
        .with_sandbox(sandbox);
    if let Some(base) = scratch_base {
        call = call.with_scratch_base(base);
    }
    if let Some(e) = &extra_roots {
        call = call.with_extra_roots(e.clone());
    }
    tools.register(call);
    if bash_enabled {
        let jobs = host::JobRegistry::new();
        let mut bash = BashTool::new(root.clone())
            .with_secret_env(secret_env.clone())
            .with_jobs(jobs.clone())
            .with_sandbox(sandbox);
        if let Some(e) = &extra_roots {
            bash = bash.with_extra_roots(e.clone());
        }
        tools.register(bash);
        tools.register(host::BashOutputTool::new(jobs));
    }
    tools
}

/// Resolve the active provider from the catalog:
///
/// - `ENTANGLEMENT_PROVIDER=<name>`, else the user config's `provider`, looks
///   `<name>` up **in the catalog** (so user-defined providers work); `echo`
///   stays a built-in stub. A missing key for the named provider exits cleanly.
/// - neither set → auto-detect by iterating catalog order and picking the first
///   provider whose `key_env` is set and non-empty (keyless Ollama is skipped),
///   else fall back to `EchoLlm`.
///
/// Precedence is env > config > auto-detect, mirroring every other setting.
fn select_provider(
    catalog: &Catalog,
    http_client: &HttpClient,
    user_config: &config::Config,
) -> (EngineConfig, ModelInfo, String) {
    let selected = std::env::var("ENTANGLEMENT_PROVIDER")
        .ok()
        .or_else(|| user_config.provider.clone());
    match selected.as_deref() {
        Some("echo") => {
            let (cfg, info) = echo_config();
            (cfg, info, "echo".to_string())
        }
        Some(name) => {
            let Some(entry) = catalog.provider(name) else {
                eprintln!(
                    "skutter: unknown provider='{name}' \
                     (not in catalog: {}, or echo)",
                    catalog_names(catalog)
                );
                std::process::exit(2);
            };
            let (cfg, info) =
                wire_config(entry, http_client, catalog, user_config).unwrap_or_else(|| {
                    exit_missing_key(
                        &entry.name,
                        entry.key_env.as_deref().unwrap_or("its API key"),
                    )
                });
            (cfg, info, entry.name.clone())
        }
        None => {
            for entry in &catalog.providers {
                // Auto-detect only over keyed providers (keyless Ollama can't be
                // sniffed and stays opt-in), first one with a key present wins.
                if entry.key_env.as_deref().and_then(env_nonempty).is_some() {
                    if let Some((cfg, info)) = wire_config(entry, http_client, catalog, user_config)
                    {
                        return (cfg, info, entry.name.clone());
                    }
                }
            }
            eprintln!(
                "skutter: no provider key set — using EchoLlm \
                 (set ENTANGLEMENT_PROVIDER=ollama for local, or a *_API_KEY, or echo)"
            );
            let (cfg, info) = echo_config();
            (cfg, info, "echo".to_string())
        }
    }
}

/// Comma-joined provider names, for the unknown-provider diagnostic.
fn catalog_names(catalog: &Catalog) -> String {
    catalog
        .providers
        .iter()
        .map(|p| p.name.as_str())
        .collect::<Vec<_>>()
        .join("|")
}

/// Explicit `ENTANGLEMENT_PROVIDER` set but its key env var is absent: exit
/// cleanly (like the unknown-provider branch) instead of panicking on `.expect`.
fn exit_missing_key(provider: &str, key_env: &str) -> ! {
    eprintln!("skutter: ENTANGLEMENT_PROVIDER={provider} requires {key_env} to be set");
    std::process::exit(2);
}

fn env_nonempty(name: &str) -> Option<String> {
    match std::env::var(name) {
        Ok(v) if !v.is_empty() => Some(v),
        _ => None,
    }
}

/// Build the engine config for a catalog provider, dispatching on its wire.
/// Returns `None` when a required key env is absent (keyed provider, no key).
fn wire_config(
    entry: &ProviderEntry,
    http_client: &HttpClient,
    catalog: &Catalog,
    user_config: &config::Config,
) -> Option<(EngineConfig, ModelInfo)> {
    match entry.wire {
        Wire::Openai => openai_wire_config(entry, http_client, catalog, user_config),
        Wire::Anthropic => anthropic_wire_config(entry, http_client, catalog, user_config),
        Wire::Gemini => gemini_wire_config(entry, http_client, catalog, user_config),
    }
}

/// Model id from `{NAME}_MODEL` env, else the user config's `model`, else the
/// entry's `default_model` (env > config > catalog default).
fn resolve_model(entry: &ProviderEntry, user_config: &config::Config) -> String {
    let name = entry.name.to_uppercase();
    env_nonempty(&format!("{name}_MODEL"))
        .or_else(|| user_config.model.clone())
        .unwrap_or_else(|| entry.default_model.clone())
}

/// The opt-in provider-side web search settings to bind onto the LLM client
/// (#305), or `None` when the config leaves it disabled — the disabled case makes
/// the client request no web-search tool, exactly as before. Cloned so both
/// startup and the live `/model` switch resolver bind identically.
fn web_search_config(user_config: &config::Config) -> Option<WebSearchConfig> {
    user_config
        .web_search
        .enabled
        .then(|| user_config.web_search.clone())
}

/// Per-minute request budget from `{NAME}_RPM` env, else the catalog entry's
/// `rpm` (env > catalog; `None` → the client's built-in default). Mirrors the
/// rest of the catalog's precedence for this per-endpoint bucket size (#241).
fn resolve_rpm(entry: &ProviderEntry) -> Option<u32> {
    let name = entry.name.to_uppercase();
    env_nonempty(&format!("{name}_RPM"))
        .and_then(|v| v.parse::<u32>().ok())
        .or(entry.rpm)
}

/// Summarize the chosen model against the catalog (context window, display name).
fn model_info_for(entry: &ProviderEntry, model: &str, catalog: &Catalog) -> ModelInfo {
    ModelInfo::from_catalog(catalog.model(&entry.name, model), model)
}

/// Resolve the generation knobs for `model` from its catalog capability metadata
/// (#191): temperature only when `supports_temperature`, thinking budget only when
/// `supports_thinking`, plus `max_output_tokens`. `None` when the chosen model is
/// absent from the catalog (an env-typed id) — the client then uses its defaults.
fn generation_for(
    entry: &ProviderEntry,
    model: &str,
    catalog: &Catalog,
) -> Option<GenerationParams> {
    catalog
        .model(&entry.name, model)
        .map(|m| m.generation_params())
}

/// Per-model USD pricing keyed by model id, flattened across every provider in
/// the catalog (#192). The engine looks a turn's effective model up here to price
/// its reported usage; a model with no `pricing` block is simply absent (unknown
/// cost). Later providers win on a duplicate id, matching the catalog's own
/// `model_by_id` precedence.
fn pricing_map(catalog: &Catalog) -> HashMap<String, ModelPricing> {
    let mut map = HashMap::new();
    for provider in &catalog.providers {
        for model in &provider.models {
            if let Some(pricing) = model.pricing {
                map.insert(model.id.clone(), pricing);
            }
        }
    }
    map
}

/// OpenAI-compatible provider (z.ai/OpenAI/Ollama/any proxy). Key from
/// `entry.key_env` (absent → `None` = skip a keyed provider); base from
/// `{NAME}_API_BASE` else `{NAME}_BASE` env else `entry.base_url`.
fn openai_wire_config(
    entry: &ProviderEntry,
    http_client: &HttpClient,
    catalog: &Catalog,
    user_config: &config::Config,
) -> Option<(EngineConfig, ModelInfo)> {
    let model = resolve_model(entry, user_config);
    // Reuse the mid-session builder so startup and a live switch resolve the
    // wire/base/key identically (#218); a keyed provider missing its key → skip.
    let llm_factory =
        openai_factory_for(entry, &model, http_client, web_search_config(user_config)).ok()?;
    eprintln!("skutter: provider={} model={model}", entry.name);
    Some((
        EngineConfig {
            llm_factory,
            default_model: Some(model.clone()),
            generation: generation_for(entry, &model, catalog),
            pricing: pricing_map(catalog),
            ..EngineConfig::default()
        },
        model_info_for(entry, &model, catalog),
    ))
}

/// Anthropic-wire provider. Always keyed; base is the client's own default.
fn anthropic_wire_config(
    entry: &ProviderEntry,
    http_client: &HttpClient,
    catalog: &Catalog,
    user_config: &config::Config,
) -> Option<(EngineConfig, ModelInfo)> {
    let model = resolve_model(entry, user_config);
    let llm_factory =
        anthropic_factory_for(entry, &model, http_client, web_search_config(user_config)).ok()?;
    eprintln!("skutter: provider={} model={model}", entry.name);
    Some((
        EngineConfig {
            llm_factory,
            default_model: Some(model.clone()),
            generation: generation_for(entry, &model, catalog),
            pricing: pricing_map(catalog),
            ..EngineConfig::default()
        },
        model_info_for(entry, &model, catalog),
    ))
}

/// Gemini-wire provider (#309). Always keyed; base is the client's own default
/// (`GEMINI_BASE`) unless overridden. Gemini has no provider-side web-search knob,
/// so — unlike the OpenAI/Anthropic arms — none is threaded here.
fn gemini_wire_config(
    entry: &ProviderEntry,
    http_client: &HttpClient,
    catalog: &Catalog,
    user_config: &config::Config,
) -> Option<(EngineConfig, ModelInfo)> {
    let model = resolve_model(entry, user_config);
    let llm_factory = gemini_factory_for(entry, &model, http_client).ok()?;
    eprintln!("skutter: provider={} model={model}", entry.name);
    Some((
        EngineConfig {
            llm_factory,
            default_model: Some(model.clone()),
            generation: generation_for(entry, &model, catalog),
            pricing: pricing_map(catalog),
            ..EngineConfig::default()
        },
        model_info_for(entry, &model, catalog),
    ))
}

/// Build a Gemini-wire [`LlmFactory`] for an explicit `(entry, model)`. Shared by
/// startup and the live-switch resolver (#218). Always keyed; `Err(message)` when
/// the key env is absent/unset. Base from `{NAME}_API_BASE`/`{NAME}_BASE` env else
/// `entry.base_url` else the client's [`GEMINI_BASE`] default.
fn gemini_factory_for(
    entry: &ProviderEntry,
    model: &str,
    http_client: &HttpClient,
) -> Result<LlmFactory, String> {
    let key_env = entry
        .key_env
        .as_deref()
        .ok_or_else(|| format!("provider `{}` has no API key env", entry.name))?;
    let key = env_nonempty(key_env).ok_or_else(|| format!("{key_env} is not set"))?;
    let name = entry.name.to_uppercase();
    let base = env_nonempty(&format!("{name}_API_BASE"))
        .or_else(|| env_nonempty(&format!("{name}_BASE")))
        .or_else(|| entry.base_url.clone())
        .unwrap_or_else(|| entanglement_provider::GEMINI_BASE.to_string());
    Ok(entanglement_provider::gemini_factory(
        base,
        key,
        model.to_string(),
        resolve_rpm(entry),
        http_client.clone(),
    ))
}

/// Build an OpenAI-compat [`LlmFactory`] for an explicit `(entry, model)`.
/// Shared by startup ([`openai_wire_config`]) and the live-switch resolver
/// ([`build_model_resolver`], #218). `Err(message)` when a keyed provider's API
/// key is unset — startup maps it to a skip, the switch surfaces it to the head.
fn openai_factory_for(
    entry: &ProviderEntry,
    model: &str,
    http_client: &HttpClient,
    web_search: Option<WebSearchConfig>,
) -> Result<LlmFactory, String> {
    let key = match &entry.key_env {
        Some(k) => Some(
            env_nonempty(k)
                .ok_or_else(|| format!("{k} is not set for provider `{}`", entry.name))?,
        ),
        None => None, // keyless (Ollama)
    };
    let name = entry.name.to_uppercase();
    let base = env_nonempty(&format!("{name}_API_BASE"))
        .or_else(|| env_nonempty(&format!("{name}_BASE")))
        .or_else(|| entry.base_url.clone())
        .unwrap_or_else(|| entanglement_provider::OPENAI_BASE.to_string());
    Ok(entanglement_provider::openai_factory(
        base,
        key,
        model.to_string(),
        resolve_rpm(entry),
        web_search,
        http_client.clone(),
    ))
}

/// Build an Anthropic-wire [`LlmFactory`] for an explicit `(entry, model)`.
/// Shared by startup and the live-switch resolver (#218). Always keyed;
/// `Err(message)` when the key env is absent/unset.
fn anthropic_factory_for(
    entry: &ProviderEntry,
    model: &str,
    http_client: &HttpClient,
    web_search: Option<WebSearchConfig>,
) -> Result<LlmFactory, String> {
    let key_env = entry
        .key_env
        .as_deref()
        .ok_or_else(|| format!("provider `{}` has no API key env", entry.name))?;
    let key = env_nonempty(key_env).ok_or_else(|| format!("{key_env} is not set"))?;
    Ok(entanglement_provider::anthropic_factory(
        key,
        model.to_string(),
        resolve_rpm(entry),
        web_search,
        http_client.clone(),
    ))
}

/// Build the live model/provider resolver the engine calls on `SetModel` (#218).
/// Captures the catalog + the per-endpoint HTTP client (clients stay warm across
/// switches, #217) and re-runs the same wire/base/key resolution as startup for
/// an explicit `(provider, model)` — so a mid-session switch binds exactly like a
/// fresh launch would, minus the model-default fallback (the model is chosen by
/// the head). An unknown provider / missing key comes back as `Err` for the head
/// to surface. The opt-in [`WebSearchConfig`] (#305) is captured too, so a live
/// switch binds provider-side web search exactly as startup did.
fn build_model_resolver(
    catalog: Catalog,
    http_client: HttpClient,
    web_search: Option<WebSearchConfig>,
) -> ModelResolver {
    Arc::new(move |provider: &str, model: &str| {
        let entry = catalog
            .provider(provider)
            .ok_or_else(|| format!("unknown provider `{provider}`"))?;
        let llm_factory = match entry.wire {
            Wire::Openai => openai_factory_for(entry, model, &http_client, web_search.clone())?,
            Wire::Anthropic => {
                anthropic_factory_for(entry, model, &http_client, web_search.clone())?
            }
            Wire::Gemini => gemini_factory_for(entry, model, &http_client)?,
        };
        Ok(ResolvedModel {
            provider: entry.name.clone(),
            model: model.to_string(),
            llm_factory,
            generation: catalog
                .model(provider, model)
                .map(|m| m.generation_params()),
            context_window: catalog
                .model(provider, model)
                .and_then(|m| m.context_window)
                .map(|w| w as usize),
        })
    })
}

fn echo_model_info() -> ModelInfo {
    ModelInfo {
        id: "echo".to_string(),
        display_name: "Echo (debug)".to_string(),
        context_window: None,
    }
}

fn echo_config() -> (EngineConfig, ModelInfo) {
    eprintln!("skutter: provider=echo (history-debugging stub)");
    (EngineConfig::default(), echo_model_info())
}

#[derive(Parser)]
#[command(
    name = "skutter",
    version,
    about = "Terminal head for the entanglement agent engine (TUI is the default)"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
    /// Positional prompt for the implicit one-shot run. Bare `skutter` with no
    /// prompt *and* no subcommand launches the TUI instead; a prompt runs one
    /// turn (implicit `run`). Use an explicit `run`/`tui`/`pipe` subcommand to
    /// disambiguate.
    prompt: Vec<String>,
    /// Log at `debug` (unless `RUST_LOG` is set, which always wins). Global, so
    /// it may follow the subcommand: `skutter run … --verbose`.
    #[arg(long, global = true)]
    verbose: bool,
}

#[derive(Subcommand)]
enum Cmd {
    /// One-shot: send a prompt and stream the turn.
    Run {
        /// Prompt text.
        prompt: Vec<String>,
        /// Session id to use (generates UUID if not specified).
        #[arg(long)]
        session: Option<String>,
        /// Agent profile to run under (build | plan | explore | custom).
        #[arg(long)]
        agent: Option<String>,
        /// Output format.
        #[arg(long, value_name = "text|json", default_value = "text")]
        format: String,
        /// Resume a session from log records.
        #[arg(long)]
        resume: Option<String>,
    },
    /// Bidirectional NDJSON relay (stdin: InMsg, stdout: OutEvent).
    Pipe {
        #[arg(long)]
        session: Option<String>,
    },
    /// Terminal UI mode.
    Tui {
        #[arg(long)]
        session: Option<String>,
        /// Agent profile to run under (build | plan | explore | custom).
        #[arg(long)]
        agent: Option<String>,
    },
    /// Local WebSocket server head (loopback-bound; browser twin of the TUI).
    #[cfg(feature = "serve")]
    Serve {
        /// Port to bind on `127.0.0.1` (loopback-only by design — ADR-0048).
        #[arg(long, default_value_t = 4517)]
        port: u16,
        /// Opt-in `Origin` allowlist for browser clients. Unset accepts every
        /// origin (raw local clients send none) — never mandatory (ADR-0048).
        #[arg(long)]
        allow_origin: Option<String>,
    },
    /// List past root sessions for the current directory.
    Sessions,
    /// Inspect resolved runtime state without spawning the engine.
    Inspect {
        #[command(subcommand)]
        what: InspectCmd,
    },
    /// Manage the managed user configuration (provider keys, …).
    Config {
        #[command(subcommand)]
        cmd: ConfigCmd,
    },
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// Persist a provider's API key to the managed env file (#304). The value
    /// comes from `--key`, a hidden prompt, or piped stdin — never echoed.
    SetKey {
        /// Provider name (catalog key: zai | openai | anthropic | …).
        provider: String,
        /// The key value. Omit to be prompted (hidden) or to read piped stdin.
        #[arg(long)]
        key: Option<String>,
    },
}

#[derive(Subcommand)]
enum InspectCmd {
    /// Print an agent's assembled system prompt (preamble + body + brief + env
    /// + skill index + preloaded skills), exactly as it ships to the model.
    Prompt {
        /// Agent profile to resolve (build | plan | explore | custom).
        #[arg(long)]
        agent: String,
        /// Break the prompt into its component parts, each with its source path.
        #[arg(long)]
        parts: bool,
    },
    /// List resolved agents with their winning layer + provenance (no `name`), or
    /// print one agent's full resolved profile (permission/mask/spawn/plan +
    /// prompt length) and what lower layers it overrode.
    Agents {
        /// Agent to detail (build | plan | explore | custom). Omit for the table.
        name: Option<String>,
    },
    /// List resolved skills with their winning layer + `root_dir` (no `name`),
    /// print the exact tier-1 disclosure block the model gets (`--disclosures`),
    /// or dry-run the `load_skill` path substitution for one skill (a `name`).
    Skills {
        /// Skill to dry-run through the `load_skill` path substitution. Omit for
        /// the table.
        name: Option<String>,
        /// Print the exact tier-1 `disclosures()` block the model receives.
        #[arg(long)]
        disclosures: bool,
    },
    /// Print the resolved user config (#172): merged settings with their winning
    /// layer (default < user < project) and the permission ceiling.
    Config,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));

    // Load the layered user config (#172, ADR-0047) up front: embedded defaults <
    // user (`${config_dir}/entanglement/config.yml`) < project
    // (`.entanglement/config.yml`). A malformed file is a loud error. Loaded
    // before logging so its `verbose` can raise the level for every mode,
    // including the pre-engine `inspect`/`sessions` fast paths.
    let user_config = config::Config::load(&cwd).context("loading user config")?;

    // stdout is reserved for command output — the assembled prompt
    // (`inspect prompt`), NDJSON frames (`run --format json` / `pipe`) — so logs
    // go to stderr, except under the TUI (raw mode) where they'd corrupt the
    // screen and get a file sink instead. `RUST_LOG` overrides `--verbose`;
    // `--verbose` overrides the config's `verbose`.
    logging::init(
        cli.verbose || user_config.verbose,
        matches!(cli.cmd, Some(Cmd::Tui { .. })),
    )?;

    // First run: drop a commented starter config so `${config_dir}/entanglement/`
    // is a discoverable starting point rather than an empty dir (#219). The
    // template is fully commented — it changes nothing until edited — so this is
    // best-effort: a write failure (read-only home, race) is logged, never fatal.
    // Announced via `eprintln!` (not just `tracing::info!`, which the default
    // `warn` filter swallows — a one-time "here's your config file" notice must
    // not require `--verbose`/`RUST_LOG` to be seen at all).
    match config::scaffold_if_missing() {
        Ok(Some(path)) => {
            eprintln!("skutter: wrote starter config to {}", path.display());
            tracing::info!("wrote starter config to {}", path.display());
        }
        Ok(None) => {}
        Err(e) => tracing::debug!("could not scaffold default user config: {e:#}"),
    }

    // `sessions` only reads the log store — handle it before spinning up a
    // provider/engine so it stays cheap and prints nothing about providers.
    if matches!(cli.cmd, Some(Cmd::Sessions)) {
        return print_sessions(&cwd);
    }

    // `inspect` re-runs prompt/registry discovery only — no provider or engine —
    // so it too is handled before startup (and stays silent about providers).
    if let Some(Cmd::Inspect { what }) = &cli.cmd {
        return match what {
            InspectCmd::Prompt { agent, parts } => inspect::inspect_prompt(&cwd, agent, *parts),
            InspectCmd::Agents { name } => inspect::inspect_agents(&cwd, name.as_deref()),
            InspectCmd::Skills { name, disclosures } => {
                inspect::inspect_skills(&cwd, name.as_deref(), *disclosures)
            }
            InspectCmd::Config => inspect::inspect_config(&cwd),
        };
    }

    // Load the provider/model catalog once (embedded defaults + user override).
    // A malformed user file is a loud error, never a silent fallback.
    let catalog = Catalog::load().context("loading provider catalog")?;

    // `config set-key` writes a provider key to the managed env file (#304) — a
    // pre-engine fast path like `inspect`/`sessions`: it needs the catalog (to
    // map provider → key env) but no provider/engine.
    if let Some(Cmd::Config { cmd }) = &cli.cmd {
        return match cmd {
            ConfigCmd::SetKey { provider, key } => {
                config::keys::set_key(&catalog, provider, key.clone())
            }
        };
    }

    // Managed provider-key env file (#220): scaffold a commented template listing
    // the catalog's known key vars on first run, then load `KEY=VALUE` pairs into
    // the process env *without overriding* anything the real env already set (env
    // > file). Both are best-effort — a read-only home or malformed line is logged,
    // never fatal — and run before `select_provider` reads any API key below.
    // Same visibility fix as the config scaffold above: `eprintln!` so the
    // one-time notice isn't gated behind `--verbose`/`RUST_LOG`.
    match config::env_file::scaffold_if_missing(&catalog.key_envs()) {
        Ok(Some(path)) => {
            eprintln!("skutter: wrote provider env file to {}", path.display());
            tracing::info!("wrote provider env file to {}", path.display());
        }
        Ok(None) => {}
        Err(e) => tracing::debug!("could not scaffold provider env file: {e:#}"),
    }
    match config::env_file::load() {
        Ok(Some((path, set))) if set > 0 => {
            tracing::info!("loaded {set} provider key(s) from {}", path.display())
        }
        Ok(_) => {}
        Err(e) => tracing::debug!("could not load provider env file: {e:#}"),
    }

    // Discover file-based agent definitions (#112): embedded built-ins, then the
    // user dir, then the project dir. A malformed file is a loud error. Each
    // agent body is composed with the shared preamble, project brief, env block,
    // and skill index into its final system prompt (#113) as it is loaded.
    // Discover skills (#114): embedded stock skills, then user, then project. A
    // malformed SKILL.md is a loud error. Only `name` + `description` reach the
    // model, folded into the assembled system prompt as a tier-1 disclosure list
    // (user_only skills withheld) — selection stays the model's own reasoning.
    let skill_registry =
        std::sync::Arc::new(skills::load_registry(&cwd).context("loading skill definitions")?);
    // The runtime's own live-reloadable mirror (#329, ADR-0084): a definitions
    // watcher swaps this for a freshly-loaded registry on a debounced dir
    // change. `load_skill` (below) and the watcher both share this handle;
    // core's own copy (built into `engine_config` below) has no such seam.
    let live_skills: Arc<RwLock<Arc<skills::SkillRegistry>>> =
        Arc::new(RwLock::new(skill_registry.clone()));
    let mut prompt_ctx = system_prompt::PromptContext::load(&cwd);
    prompt_ctx.skills = skill_registry.disclosures();
    // The skill registry also resolves per-agent `skills:` preload bodies (#117),
    // orthogonal to the tier-1 disclosures above and to the `load_skill` mask.
    let mut profiles = agents::load_registry(&cwd, &prompt_ctx, &skill_registry)
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
    // Per-agent generation-parameter overrides (#374, ADR-0094): unlike the model
    // pin above, this doesn't overlay onto `profiles` (`GenerationParams` isn't
    // `Eq`, so it can't join `AgentProfile`'s derive) — instead it's wrapped in a
    // `GenerationResolver` closure threaded onto `EngineConfig` below, resolved
    // by profile name at session start / `SetAgent`. Also threaded straight into
    // the TUI (#376), the only surface that writes to it (`/set`'s
    // persist-on-confirmation).
    let live_agent_generation = Arc::new(Mutex::new(
        config::agent_generation::AgentGenerationStore::load(),
    ));

    let http_client = HttpClient::new();
    // The skill registry is shared: its tier-1 disclosures fed the system prompt
    // above, and `load_skill` (#115) resolves tier-2 bodies against it at runtime.
    let (mut engine_config, model_info, provider_name, tools, tool_names, initial_mcp, escape_root) =
        build_config(
            &catalog,
            &http_client,
            profiles,
            live_skills.clone(),
            &user_config,
        )
        .await;
    // Per-agent generation-parameter overrides (#374, ADR-0094): resolved by
    // profile name at session start / `SetAgent`, same precedence tier the model
    // pin's persisted file occupies (persisted store > profile/catalog default).
    engine_config.generation_resolver = Some(
        config::agent_generation::AgentGenerationStore::resolver(live_agent_generation.clone()),
    );
    // Dynamic `ToolRegistry` (#372, ADR-0096): shared mutably so a live
    // registration change (MCP add/remove, #375) is visible without a restart.
    // `engine_config.tool_specs` stays the static snapshot baked above (still
    // useful as the tools-checklist roster below); `tool_spec_resolver` is the
    // seam core actually consults every turn (ADR-0076) — reproducing that same
    // snapshot (registry tools + the runtime-owned pseudo-tools that aren't
    // registry entries) keeps this change behavior-neutral today, while making
    // every *future* registry mutation land on the next turn for free.
    let tools = tools.shared();
    {
        let tools = tools.clone();
        let runtime_owned_specs = [
            plan_tasks::update_tasks_spec(),
            ask_user::ask_user_spec(),
            script::rhai_spec(),
        ];
        engine_config.tool_spec_resolver = Some(Arc::new(move |_session: &SessionId| {
            // `read_raw` lives in the same shared registry as every other tool
            // (rhai's bridge needs to `execute()` it) but must never reach the
            // model directly — it's read-only for `parse_json`/`parse_yaml`
            // (ADR-0098) and is graded/masked as an alias of `read`, which only
            // holds if a profile author never sees it to configure separately.
            let mut specs: Vec<_> = tools
                .read()
                .unwrap()
                .specs()
                .into_iter()
                .filter(|s| s.name != "read_raw")
                .collect();
            specs.extend(runtime_owned_specs.iter().cloned());
            specs
        }));
    }
    // Live MCP server management (#375): `ActiveServers` starts seeded from the
    // servers `build_config` actually connected; `ServerConfigs` starts from
    // the *whole* configured set (including a disabled/failed one) — the wider
    // map `save_mcp` must round-trip so a live add/remove never drops an
    // unrelated entry.
    let mcp_active: mcp::ActiveServers = Arc::new(Mutex::new(initial_mcp));
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
    let live_profiles: Arc<RwLock<ProfileRegistry>> =
        Arc::new(RwLock::new(engine_config.profiles.clone()));
    let holly = Holly::spawn(engine_config);

    // Runtime owns tool execution (#58) and permission dispatch + approval (#59):
    // answer the engine's ToolExec round-trip, gating each call on
    // `live_profiles`. The user config's `permissions` section is the global
    // ceiling clamped over every resolved grade (#172), and its `hooks` section
    // wires the lifecycle hooks (#199) around tool dispatch and prompt ingress.
    // Built directly against `spawn_tool_executor_with_policy` (#311) rather
    // than the `_with_hooks` convenience wrapper (which owns a plain
    // `ProfileRegistry`) because the head needs its own handles on `active` and
    // `grants` — `grants` feeds the definitions watcher's `LiveDefinitions`
    // below (#329), so a persisted "always allow" grant another skutter
    // instance recorded is visible on the next reload.
    let active = Arc::new(Mutex::new(HashMap::new()));
    let resolver: Arc<dyn PermissionResolver> = Arc::new(ProfileResolver::new(
        active.clone(),
        user_config.permissions.clone(),
    ));
    let grants = Arc::new(DefaultGrantStore::load());
    let tool_executor = tool_runner::spawn_tool_executor_with_policy(
        &holly,
        tools.clone(),
        live_profiles.clone(),
        live_skills.clone(),
        user_config.permissions.clone(),
        active,
        resolver,
        grants.clone(),
        user_config.hooks.clone(),
        // Escape-root approval (ADR-0109): the same store the host tools read.
        Some(escape_root),
    );

    // Live MCP server management (#375): a runtime service answering
    // `McpList`/`McpAdd`/`McpRemove` off the inbound fan-out, since it alone
    // holds `tools` + `mcp_active` + `mcp_configs` — mirrors
    // `history::spawn_history_responder`'s answer to `ReplayFrom`.
    let mcp_responder_handle =
        mcp::spawn_mcp_responder(&holly, tools.clone(), mcp_active, mcp_configs);

    // Spawn the persistence subscriber to log all inbound + outbound frames.
    let persistence_handle = persistence::spawn_persistence_subscriber(&holly, cwd.clone());
    // Answer `ReplayFrom` late-subscriber history queries from that same log
    // (#160). The handle is kept so it can be aborted at shutdown — like the
    // tool executor and watcher, it holds a `Holly` clone (an inbound
    // subscriber + event emitter) that keeps the broadcast channels open until
    // every clone is dropped. Aborting it is what lets the persistence
    // subscriber see `RecvError::Closed` and flush.
    let history_handle = history::spawn_history_responder(&holly, cwd.clone());

    // Live definitions watcher (#329, ADR-0084): inotify on the resolved
    // agent/skill dirs + managed files, debounced, reloading straight into the
    // runtime-held mirrors above — never core's `EngineConfig`, which stays
    // pinned to what was loaded at process start (ADR-0081 precedent: live
    // registry mutation in core is a rejected design). `reload_rx` only matters
    // to the TUI (a status line); every other head lets its messages drop.
    let live = watch::LiveDefinitions {
        profiles: live_profiles.clone(),
        skills: live_skills.clone(),
        agent_models: live_agent_models.clone(),
        grants: grants.clone(),
    };
    let (reload_tx, reload_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let watcher_handle = watch::spawn_watcher(cwd.clone(), live, Some(reload_tx));

    let result = match cli.cmd {
        Some(Cmd::Run {
            prompt,
            session,
            agent,
            format,
            resume,
        }) => {
            let session_id = if let Some(resume_id) = &resume {
                SessionId::new(resume_id.clone())
            } else {
                SessionId::new(session.unwrap_or_else(|| SessionId::new_uuid().0))
            };

            if let Some(resume_id) = resume {
                let resume_session_id = SessionId::new(resume_id);
                let records = read(&cwd, &resume_session_id).with_context(|| {
                    format!("Failed to read session records for {}", resume_session_id)
                })?;

                if let Some(dropped) = integrity_gap(&records) {
                    anyhow::bail!(
                        "Refusing to resume {resume_session_id}: its session log is missing \
                         {dropped} record(s) dropped during recording, so replay would \
                         reconstruct an incomplete conversation. Start a fresh session instead."
                    );
                }

                holly
                    .resume(session_id.clone(), pair_records(&records))
                    .await?;
            }

            // CLI `--agent` wins; else the user config's default agent (#172).
            let agent = agent.or_else(|| user_config.agent.clone());
            if let Some(ref a) = agent {
                holly
                    .send(InMsg::SetAgent {
                        session: session_id.clone(),
                        agent: a.to_string(),
                    })
                    .await?;
            }
            let prompt = prompt.join(" ");
            run_one(&holly, &session_id, agent.as_deref(), &prompt, &format).await
        }
        Some(Cmd::Pipe { session }) => {
            let session_id = SessionId::new(session.unwrap_or_else(|| SessionId::new_uuid().0));
            pipe(&holly, &session_id).await
        }
        #[cfg(feature = "serve")]
        Some(Cmd::Serve { port, allow_origin }) => {
            // Runs until Ctrl-C; the executor/persistence teardown below then runs
            // as for any other head. A fresh `Holly` clone keeps the outer handle
            // for that shutdown path.
            entanglement_runtime::serve::serve(holly.clone(), port, allow_origin).await
        }
        Some(Cmd::Tui { session, agent }) => {
            let session_id = SessionId::new(session.unwrap_or_else(|| SessionId::new_uuid().0));
            // CLI `--agent` wins; else the user config's default agent (#172).
            let agent = agent.or_else(|| user_config.agent.clone());
            if let Some(a) = agent {
                holly
                    .send(InMsg::SetAgent {
                        session: session_id.clone(),
                        agent: a.to_string(),
                    })
                    .await?;
            }
            let bash_enabled = std::env::var("ENTANGLEMENT_ENABLE_BASH").as_deref() == Ok("1");
            tui(
                &holly,
                session_id,
                model_info,
                provider_name,
                catalog,
                live_profiles,
                live_agent_models,
                live_agent_generation,
                reload_rx,
                cwd.clone(),
                bash_enabled,
                tool_names,
            )
            .await
        }
        Some(Cmd::Sessions) => unreachable!("sessions is handled before engine setup"),
        Some(Cmd::Inspect { .. }) => unreachable!("inspect is handled before engine setup"),
        Some(Cmd::Config { .. }) => unreachable!("config is handled before engine setup"),
        None => {
            // No subcommand:
            // - bare `skutter` (no prompt) → launch the TUI (the default head);
            // - `skutter "<prompt>"` → one implicit `run` turn, as before.
            if cli.prompt.is_empty() {
                let session_id = SessionId::new_uuid();
                let agent = user_config.agent.clone();
                if let Some(ref a) = agent {
                    holly
                        .send(InMsg::SetAgent {
                            session: session_id.clone(),
                            agent: a.to_string(),
                        })
                        .await?;
                }
                let bash_enabled = std::env::var("ENTANGLEMENT_ENABLE_BASH").as_deref() == Ok("1");
                tui(
                    &holly,
                    session_id,
                    model_info,
                    provider_name,
                    catalog,
                    live_profiles,
                    live_agent_models,
                    live_agent_generation,
                    reload_rx,
                    cwd.clone(),
                    bash_enabled,
                    tool_names,
                )
                .await
            } else {
                let session_id = SessionId::new_uuid();
                let agent = user_config.agent.clone();
                if let Some(ref a) = agent {
                    holly
                        .send(InMsg::SetAgent {
                            session: session_id.clone(),
                            agent: a.to_string(),
                        })
                        .await?;
                }
                let prompt = cli.prompt.join(" ");
                run_one(&holly, &session_id, agent.as_deref(), &prompt, "text").await
            }
        }
    };

    // Shut the engine down and let the persistence task flush before exit: a
    // one-shot `run` ends the instant the turn does, and the detached subscriber
    // still holds broadcast-buffered events it hasn't written. The tool executor
    // and history responder each hold a `Holly` clone (an inbox + event sender),
    // so aborting them is required for the channels to actually close and the
    // persistence subscriber to drain + exit.
    tool_executor.abort();
    mcp_responder_handle.abort();
    history_handle.abort();
    if let Some(h) = watcher_handle {
        h.abort();
    }
    drop(holly);
    let _ = persistence_handle.await;

    result
}

/// Prints past root sessions for `cwd`, most-recently-active first.
fn print_sessions(cwd: &std::path::Path) -> Result<()> {
    let mut sessions = list_sessions(cwd)?;
    sessions.retain(|s| s.root);
    sessions.sort_by_key(|s| std::cmp::Reverse(s.last_active));

    if sessions.is_empty() {
        println!("No sessions found for this directory.");
        println!("Start one with:  skutter run --session <id> \"<prompt>\"");
        println!("Then resume it:  skutter run --resume <id> \"<prompt>\"");
        return Ok(());
    }

    println!(
        "{:<28}  {:<10}  {:<16}  {:<14}  DESCRIPTION",
        "ID", "AGENT", "MODEL", "LAST ACTIVE"
    );
    for s in &sessions {
        let model = s.model.as_deref().unwrap_or("default");
        let description = s.first_prompt.as_deref().unwrap_or("");
        println!(
            "{:<28}  {:<10}  {:<16}  {:<14}  {}",
            s.id.0,
            s.agent,
            model,
            format_relative(s.last_active),
            description
        );
    }
    Ok(())
}

/// Formats a Unix-ms timestamp as a compact "time ago" relative to now
/// (clamped to `0s ago` if the record's timestamp is ahead of the clock).
fn format_relative(ts_ms: u64) -> String {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let secs = now_ms.saturating_sub(ts_ms) / 1000;
    match secs {
        0..=59 => format!("{secs}s ago"),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86399 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86400),
    }
}

#[cfg(test)]
mod tests {
    use super::register_default_tools;
    use crate::host::SandboxPolicy;

    fn tool_names(bash_enabled: bool) -> Vec<String> {
        let root = std::env::temp_dir();
        register_default_tools(
            root,
            None,
            None,
            Vec::new(),
            bash_enabled,
            SandboxPolicy::none(),
        )
        .specs()
        .into_iter()
        .map(|s| s.name.to_string())
        .collect()
    }

    #[test]
    fn call_is_registered_unconditionally() {
        let names = tool_names(false);
        assert!(names.contains(&"call".to_string()), "{names:?}");
    }

    #[test]
    fn bash_and_bash_output_stay_opt_in() {
        let names = tool_names(false);
        assert!(!names.contains(&"bash".to_string()), "{names:?}");
        assert!(!names.contains(&"bash_output".to_string()), "{names:?}");
    }

    #[test]
    fn bash_enabled_registers_bash_pair_and_call() {
        let names = tool_names(true);
        assert!(names.contains(&"call".to_string()), "{names:?}");
        assert!(names.contains(&"bash".to_string()), "{names:?}");
        assert!(names.contains(&"bash_output".to_string()), "{names:?}");
    }
}
