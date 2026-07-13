//! `skutter` — stdio head for the headless agent engine.
//!
//! Two modes, both driving [`entanglement_core::Holly`] directly (the ABI):
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
    agents, ask_user, config, host, inspect, logging, persistence, plan_tasks, propose_plan,
    script, session_store, skills, subagent, system_prompt, tool_runner,
};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use entanglement_core::{EngineConfig, Holly, InMsg, ProfileRegistry, SessionId, ToolRegistry};
use entanglement_provider::{Catalog, HttpClient, ModelInfo, ModelPricing, ProviderEntry, Wire};
use std::collections::HashMap;

use host::{host_tools, BashTool, CallTool};
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
/// The root-contained host quintet (`read`/`glob`/`grep`/`edit`/`write`) plus
/// `load_skill` are always registered, rooted at the current working directory,
/// so the `build`/`plan`/`explore` permission profiles gate something real out
/// of the box. The exec pair is opt-in: set `ENTANGLEMENT_ENABLE_BASH=1` to register
/// `BashTool` (shell) and `CallTool` (argv, no shell) — they run unsandboxed
/// with the engine's full privileges (ADR-0009 / ADR-0010 / ADR-0045).
///
/// Core no longer executes tools (#58): it only advertises their schemas
/// (`cfg.tool_specs`). The returned [`ToolRegistry`] stays in the runtime and
/// is handed to [`tool_runner::spawn_tool_executor`], which answers the
/// [`entanglement_core::OutEvent::ToolExec`] round-trip.
fn build_config(
    catalog: &Catalog,
    http_client: &HttpClient,
    profiles: ProfileRegistry,
    skills: std::sync::Arc<skills::SkillRegistry>,
    user_config: &config::Config,
) -> (EngineConfig, ModelInfo, ToolRegistry) {
    let (mut cfg, model_info) = select_provider(catalog, http_client, user_config);
    // File-based agent definitions (#112) replace core's hardcoded fallback trio.
    cfg.profiles = profiles;
    // Canonicalize the working root once at startup (#163, ADR-0054): host-tool
    // containment checks against this, so a symlinked cwd must resolve to its
    // real path here or every resolved target would look like an escape.
    let root = std::env::current_dir()
        .and_then(|p| p.canonicalize())
        .unwrap_or_else(|_| std::path::PathBuf::from("."));
    let mut tools = host_tools(root.clone());
    if std::env::var("ENTANGLEMENT_ENABLE_BASH").as_deref() == Ok("1") {
        // The opt-in gate enables the whole exec pair (ADR-0010/ADR-0045):
        // `bash` (shell) and `call` (argv, no shell). Profiles differentiate
        // dispatch — e.g. a profile may `Allow` `call` while asking on `bash`.
        // Both scrub the catalog's provider API-key env vars before spawn so a
        // model-authored command can't exfiltrate the credentials (#164).
        let secret_env = catalog.key_envs();
        tools.register(BashTool::new(root.clone()).with_secret_env(secret_env.clone()));
        tools.register(CallTool::new(root.clone()).with_secret_env(secret_env));
        eprintln!(
            "skutter: bash + call enabled (ENTANGLEMENT_ENABLE_BASH=1) — \
             run unsandboxed with full privileges"
        );
    }
    // `load_skill` is tier-2 progressive disclosure (#115): a real host tool (it
    // reads the filesystem), so it is registered here and goes through the *same*
    // per-call permission gate as `read` — no runtime-executor interception.
    tools.register(LoadSkillTool::new(skills));
    cfg.tool_specs = tools.specs();
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
    (cfg, model_info, tools)
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
) -> (EngineConfig, ModelInfo) {
    let selected = std::env::var("ENTANGLEMENT_PROVIDER")
        .ok()
        .or_else(|| user_config.provider.clone());
    match selected.as_deref() {
        Some("echo") => echo_config(),
        Some(name) => {
            let Some(entry) = catalog.provider(name) else {
                eprintln!(
                    "skutter: unknown provider='{name}' \
                     (not in catalog: {}, or echo)",
                    catalog_names(catalog)
                );
                std::process::exit(2);
            };
            wire_config(entry, http_client, catalog, user_config).unwrap_or_else(|| {
                exit_missing_key(
                    &entry.name,
                    entry.key_env.as_deref().unwrap_or("its API key"),
                )
            })
        }
        None => {
            for entry in &catalog.providers {
                // Auto-detect only over keyed providers (keyless Ollama can't be
                // sniffed and stays opt-in), first one with a key present wins.
                if entry.key_env.as_deref().and_then(env_nonempty).is_some() {
                    if let Some(cfg) = wire_config(entry, http_client, catalog, user_config) {
                        return cfg;
                    }
                }
            }
            eprintln!(
                "skutter: no provider key set — using EchoLlm \
                 (set ENTANGLEMENT_PROVIDER=ollama for local, or a *_API_KEY, or echo)"
            );
            (EngineConfig::default(), echo_model_info())
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
    let key = match &entry.key_env {
        Some(k) => Some(env_nonempty(k)?), // keyed provider missing its key: skip
        None => None,                      // keyless (Ollama)
    };
    let name = entry.name.to_uppercase();
    let model = resolve_model(entry, user_config);
    let base = env_nonempty(&format!("{name}_API_BASE"))
        .or_else(|| env_nonempty(&format!("{name}_BASE")))
        .or_else(|| entry.base_url.clone())
        .unwrap_or_else(|| entanglement_provider::OPENAI_BASE.to_string());
    eprintln!("skutter: provider={} model={model} base={base}", entry.name);
    Some((
        EngineConfig {
            llm_factory: entanglement_provider::openai_factory(
                base,
                key,
                model.clone(),
                resolve_rpm(entry),
                http_client.clone(),
            ),
            default_model: Some(model.clone()),
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
    let key = env_nonempty(entry.key_env.as_deref()?)?;
    let model = resolve_model(entry, user_config);
    eprintln!("skutter: provider={} model={model}", entry.name);
    Some((
        EngineConfig {
            llm_factory: entanglement_provider::anthropic_factory(
                key,
                model.clone(),
                resolve_rpm(entry),
                http_client.clone(),
            ),
            default_model: Some(model.clone()),
            pricing: pricing_map(catalog),
            ..EngineConfig::default()
        },
        model_info_for(entry, &model, catalog),
    ))
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
    about = "Terminal head for the entanglement agent engine"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
    /// Default subcommand equivalent to `run` with a prompt.
    #[arg(default_value = "Hello, Holly!")]
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
    /// List past root sessions for the current directory.
    Sessions,
    /// Inspect resolved runtime state without spawning the engine.
    Inspect {
        #[command(subcommand)]
        what: InspectCmd,
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
    match config::scaffold_if_missing() {
        Ok(Some(path)) => tracing::info!("wrote starter config to {}", path.display()),
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

    // Managed provider-key env file (#220): scaffold a commented template listing
    // the catalog's known key vars on first run, then load `KEY=VALUE` pairs into
    // the process env *without overriding* anything the real env already set (env
    // > file). Both are best-effort — a read-only home or malformed line is logged,
    // never fatal — and run before `select_provider` reads any API key below.
    match config::env_file::scaffold_if_missing(&catalog.key_envs()) {
        Ok(Some(path)) => tracing::info!("wrote provider env file to {}", path.display()),
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
    let mut prompt_ctx = system_prompt::PromptContext::load(&cwd);
    prompt_ctx.skills = skill_registry.disclosures();
    // The skill registry also resolves per-agent `skills:` preload bodies (#117),
    // orthogonal to the tier-1 disclosures above and to the `load_skill` mask.
    let profiles = agents::load_registry(&cwd, &prompt_ctx, &skill_registry)
        .context("loading agent definitions")?;

    let http_client = HttpClient::new();
    // The skill registry is shared: its tier-1 disclosures fed the system prompt
    // above, and `load_skill` (#115) resolves tier-2 bodies against it at runtime.
    let (engine_config, model_info, tools) = build_config(
        &catalog,
        &http_client,
        profiles,
        skill_registry,
        &user_config,
    );
    // Fail fast on a malformed config (e.g. a profile registry without `build`)
    // rather than leaning on the supervisor's synthesized fallback.
    if let Err(e) = engine_config.validate() {
        eprintln!("skutter: invalid engine configuration: {e}");
        std::process::exit(2);
    }
    // The runtime keeps its own copy of the profile registry to resolve
    // permissions (#59); the engine gets the same shape via `engine_config`.
    let profiles = engine_config.profiles.clone();
    let holly = Holly::spawn(engine_config);

    // Runtime owns tool execution (#58) and permission dispatch + approval (#59):
    // answer the engine's ToolExec round-trip, gating each call on `profiles`.
    // The user config's `permissions` section is the global ceiling clamped over
    // every resolved grade (#172). The TUI also needs the registry (its
    // entry-agent picker is registry-driven, #119), so hand the executor a clone
    // and keep `profiles` for the head below.
    let tool_executor = tool_runner::spawn_tool_executor(
        &holly,
        tools,
        profiles.clone(),
        user_config.permissions.clone(),
    );

    // Spawn the persistence subscriber to log all inbound + outbound frames.
    let persistence_handle = persistence::spawn_persistence_subscriber(&holly, cwd.clone());

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
                catalog,
                profiles,
                cwd.clone(),
                bash_enabled,
            )
            .await
        }
        Some(Cmd::Sessions) => unreachable!("sessions is handled before engine setup"),
        Some(Cmd::Inspect { .. }) => unreachable!("inspect is handled before engine setup"),
        None => {
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
    };

    // Shut the engine down and let the persistence task flush before exit: a
    // one-shot `run` ends the instant the turn does, and the detached subscriber
    // still holds broadcast-buffered events it hasn't written. The tool executor
    // holds a `Holly` clone (an inbox + event sender), so aborting it is required
    // for the channels to actually close.
    tool_executor.abort();
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
        "{:<28}  {:<10}  {:<20}  LAST ACTIVE",
        "ID", "AGENT", "MODEL"
    );
    for s in &sessions {
        let model = s.model.as_deref().unwrap_or("default");
        println!(
            "{:<28}  {:<10}  {:<20}  {}",
            s.id.0,
            s.agent,
            model,
            format_relative(s.last_active)
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
