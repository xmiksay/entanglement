//! Startup assembly of the engine config and the tool registry: provider
//! selection, host/exec tool registration, skill/endpoint/MCP tools, and the
//! runtime-owned specs advertised alongside them.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

use entanglement_core::{AgentCatalog, EngineConfig};
use entanglement_provider::{Catalog, HttpClient, ModelInfo};

use crate::factories::build_model_resolver;
use crate::host::{BashTool, CallTool, GlobJsonTool, GrepJsonTool, ReadRawTool};
use crate::mode::ModeTable;
use crate::providers::{select_provider, web_search_config};
#[cfg(feature = "rhai")]
use crate::script;
use crate::skills::LoadSkillTool;
use crate::tool_runner::EscapeRoot;
use crate::{
    ask_user, config, endpoint, extra_roots, file_backup, host, mcp, plan_tasks, policy, poll,
    propose_plan, request_mode, retained_output, script_ops, session_store, skills, subagent,
    workdir, SharedRegistry, ToolRegistry,
};

/// Pick a provider and build the engine config.
///
/// Selection order:
/// 1. `ENTANGLEMENT_PROVIDER` env, one of `zai | openai | ollama | anthropic | echo`
///    (explicit; errors loudly if the matching key is missing).
/// 2. Auto-detect by key presence, z.ai first (the project's primary), then
///    OpenAI, then Anthropic.
/// 3. Nothing configured: the TUI onboards first (or the user picks echo);
///    every other head prints a hint and exits 2 — no silent `EchoLlm`
///    fallback (#732, ADR-0222).
///
/// Set `ENTANGLEMENT_PROVIDER=ollama` to use a local keyless Ollama (it has no key to
/// auto-detect on). Set `ENTANGLEMENT_PROVIDER=echo` to use the EchoLlm stub,
/// which returns a summary of the messages it received (useful for debugging
/// history propagation without a real provider). z.ai/OpenAI/Ollama share one
/// OpenAI-compatible client ([`entanglement_provider::openai_factory`]); Anthropic
/// has its own client.
///
/// The root-contained host quintet (`read`/`glob`/`grep`/`edit`/`write`), `call`
/// (argv exec, no shell), `load_skill`, and `bash` are always registered, rooted
/// at the current working directory, so the `build`/`plan`/`explore` permission
/// profiles gate something real out of the box (ADR-0195 retired the old
/// `ENTANGLEMENT_ENABLE_BASH` opt-in — registration is not where `bash`'s
/// security story lives; the permission profiles and the config ceiling are).
/// Both run unsandboxed with the engine's full privileges by default
/// (ADR-0009 / ADR-0045). `call` runs with the same full-privilege,
/// unsandboxed-by-default execution but no shell means no injection surface
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
/// [`tool_runner::ToolExecutor::builder`], which answers the
/// [`entanglement_core::OutEvent::ToolExec`] round-trip.
pub(crate) async fn build_config(
    catalog: &Catalog,
    http_client: &HttpClient,
    agents: AgentCatalog,
    skills: Arc<RwLock<Arc<skills::SkillRegistry>>>,
    user_config: &config::Config,
    file_backups: file_backup::FileBackups,
) -> (
    EngineConfig,
    ModelInfo,
    String,
    SharedRegistry,
    Vec<String>,
    mcp::ActiveServers,
    Arc<mcp::AvailableMcp>,
    EscapeRoot,
    policy::SandboxConfig,
    host::JobRegistry,
    retained_output::RetainedOutputRegistry,
    script_ops::ScriptRegistry,
    workdir::SessionWorkdirs,
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
    cfg.agents = agents;
    // Lets a spawned child's agent `default_mode` narrow it, never widen (#763).
    cfg.mode_ladder = crate::mode::BUILTIN_LADDER.map(String::from).to_vec();
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
    // Auto-summarize on context overflow (ADR-0103): config file → engine,
    // unset keeps the engine default (`true`).
    if let Some(auto) = user_config.auto_compact {
        cfg.auto_compact = auto;
    }
    // Canonicalize the working root once at startup (#163, ADR-0054): host-tool
    // containment checks against this, so a symlinked cwd must resolve to its
    // real path here or every resolved target would look like an escape.
    let root = std::env::current_dir()
        .and_then(|p| p.canonicalize())
        .unwrap_or_else(|_| std::path::PathBuf::from("."));
    let secret_env = catalog.key_envs();
    // Per-session permission mode (ADR-0207 stage 4), folded from
    // `OutEvent::ModeChanged` by the tool executor — constructed here,
    // *before* `register_default_tools`, so `bash`/`call`'s sandbox resolver
    // (below) and the executor's `ModeResolver` (wired much later, once
    // `Holly` exists) share the exact same map and mode table instead of two
    // copies that could drift. `skutter` always runs the four built-in modes
    // — code, not configuration (ADR-0207 §2).
    let perm_modes = Arc::new(Mutex::new(HashMap::new()));
    let mode_table = Arc::new(ModeTable::builtin().expect("built-in permission modes must parse"));
    // Sandbox confinement is a **mode** fact now (ADR-0207 §6, stage 5b):
    // `SandboxConfig` reads `perm_modes` directly and layers
    // `ENTANGLEMENT_SANDBOX`/`ENTANGLEMENT_SANDBOX_NETWORK` on top as a
    // tighten-only env override (`SandboxPolicy::most_confined`). Off by
    // default — an unset `ENTANGLEMENT_SANDBOX` and a mode with no
    // `sandbox:` key mean unsandboxed, full-privilege execution.
    let sandbox_config = policy::SandboxConfig::new(perm_modes.clone(), mode_table.clone());
    // Per-project scratch dir (#524, ADR-0142) — a pre-trusted write location
    // the model is steered toward over `/tmp` (see `system_prompt`'s env
    // block). `call`'s own default-output artifact used to live here too;
    // since #608 that output is retained in memory instead (`retained`
    // below), so this is purely the general-purpose scratch carve-out now.
    let scratch_base = session_store::SessionStore::for_cwd(&root)
        .scratch_dir()
        .ok();
    // Escape-root approval store (ADR-0109): shared by the host tools (which
    // consult it to relax containment for an approved out-of-root path) and the
    // tool executor (which forces an approval prompt on a first out-of-root
    // access and records the grant here). The scratch dir itself is
    // pre-trusted (#524, ADR-0142): every path under it is durably allowed with
    // no grant, for every tool, in every profile.
    let mut extra_root_store = extra_roots::ExtraRootStore::load();
    if let Some(scratch) = &scratch_base {
        extra_root_store = extra_root_store.with_scratch(scratch.clone());
    }
    let extra_root_store = Arc::new(extra_root_store);
    // Session working directories (#764, ADR-0224): core validates a
    // `/cd` through this seam (same root + escape-root store the host tools
    // check), and the executor's per-session view feeds the search tools.
    cfg.workdir_resolver = Some(workdir::resolver(
        root.clone(),
        Some(extra_root_store.clone()),
    ));
    let workdirs = workdir::SessionWorkdirs::default();
    // The one `JobRegistry` for this process's whole lifetime (#605): shared by
    // `bash` (registered at startup alongside every other built-in, ADR-0195)
    // and `poll`'s job-handle path, so a background job is always pollable
    // through whichever `BashTool` actually spawned it. This also closes #616's
    // job-orphaning — there is only ever one registry.
    let jobs = host::JobRegistry::new();
    // The one `RetainedOutputRegistry` for this process's whole lifetime
    // (#608): shared by `call` (which writes a truncated result's full text
    // there) and `poll`'s retained-output-handle path (which reads it back) —
    // the same shared-instance shape `jobs` above uses.
    let retained = retained_output::RetainedOutputRegistry::new();
    // The one `ScriptRegistry` for this process's whole lifetime (#637,
    // ADR-0185): the executor's `rhai` arm writes a `background: true`
    // script's handle/output there, `poll`'s `x-` path reads it back.
    let scripts = script_ops::ScriptRegistry::new();
    // Undo/rewind (ADR-0212): the file tools back up what they replace, and
    // core's `RevertFiles` reaches the same store through this seam.
    cfg.file_reverter = Some(file_backups.reverter());
    let mut tools = register_default_tools(
        root.clone(),
        Some(extra_root_store.clone()),
        secret_env.clone(),
        sandbox_config.resolver(),
        jobs.clone(),
        retained.clone(),
        Some(file_backups),
        workdirs.clone(),
    );
    let escape_root = EscapeRoot {
        root: root.clone(),
        store: extra_root_store,
    };
    // `call` and `bash` are both always registered (ADR-0093/ADR-0195), so the
    // sandbox notice fires whenever confinement is on.
    if sandbox_config.base.is_sandboxed() {
        eprintln!(
            "skutter: bash/call sandboxed via bubblewrap (ENTANGLEMENT_SANDBOX=bwrap, \
             network: {})",
            if sandbox_config.base.network {
                "allowed"
            } else {
                "cut"
            }
        );
    }
    // Skill-declared tools (#560 P8): a native/strict-layer `SKILL.md`'s
    // `tools:` frontmatter — endpoint refs, rhai-backed tools, and aliases —
    // registered once at skill-discovery time (this same startup pass, not
    // gated behind `load_skill`), namespaced `skill__<skill>__<name>`. See
    // `skills::tools`'s module doc for why discovery-time beats load-time.
    // Snapshotted from the live wrapper *before* it's moved into
    // `LoadSkillTool::new` below (cheap — an `Arc` clone under a brief read
    // lock).
    let skill_registry_snapshot = skills.read().unwrap().clone();
    skills::tools::register_skill_tools(&mut tools, &skill_registry_snapshot, http_client);
    // `load_skill` is tier-2 progressive disclosure (#115): a real host tool (it
    // reads the filesystem), so it is registered here and goes through the *same*
    // per-call permission gate as `read` — no runtime-executor interception.
    tools.register(LoadSkillTool::new(skills));
    // Definition-driven HTTP endpoint tools (#560 P8): `config.yml`'s
    // `endpoints:` map, each registered as `endpoint__<name>` — startup-only,
    // see `endpoint`'s module doc for why live-reload isn't wired.
    endpoint::register_endpoints(&mut tools, &user_config.endpoints, http_client);
    // External MCP tool servers (#198): spawn each configured server, discover its
    // `tools/list`, and register every tool into the same registry as a
    // runtime-side provider. They then ride `tool_specs` (schemas) and the
    // `ToolExec` round-trip (execution) with no core change — governed by the same
    // permission profiles as any host tool. A server that fails to connect is
    // logged and skipped, never fatal.
    // The same provider-key scrub as the exec tools (#164): an MCP stdio server
    // is an arbitrary external process and must not inherit the engine's keys.
    // Three-state split (#542): catalog-bundled servers (key-gated) overlay
    // the user `mcp:` map; only the `enabled` slice connects at startup —
    // `allowed` servers wait in `mcp_available` for a per-session `/enable`
    // or the `mcp_enable` tool below.
    let (startup_mcp, mcp_available) =
        mcp::AvailableMcp::partition(catalog, &user_config.mcp, secret_env.clone());
    let mcp_available = Arc::new(mcp_available);
    let initial_mcp = mcp::connect(&startup_mcp, &mut tools, &secret_env, http_client).await;
    let mcp_active: mcp::ActiveServers = Arc::new(Mutex::new(initial_mcp));
    // Shared from here on (#372): `mcp_enable` needs the registry handle to
    // register a lazily-connected server's tools at enable time, so the
    // by-value registration phase ends here. Registered *before* the specs
    // snapshot so it rides the static roster like any host tool.
    let tools = tools.shared();
    tools.write().unwrap().register(mcp::McpEnableTool::new(
        mcp_available.clone(),
        &tools,
        mcp_active.clone(),
        http_client.clone(),
    ));
    cfg.tool_specs = tools.read().unwrap().specs();
    // `read_raw` and the script-facing search variants (rhai-only — see
    // `script.rs`'s `parse_json`/`parse_yaml` and the `glob_json`/`grep_json`
    // bindings, ADR-0206) register *after* the specs snapshot above: present
    // in `tools` for execution (the rhai bridge routes through the same
    // `ToolRegistry`), but never advertised as standalone model-callable
    // tools.
    tools
        .write()
        .unwrap()
        .register(ReadRawTool::new(root.clone()));
    tools
        .write()
        .unwrap()
        .register(GlobJsonTool::new(root.clone()).with_workdirs(workdirs.clone()));
    tools
        .write()
        .unwrap()
        .register(GrepJsonTool::new(root.clone()).with_workdirs(workdirs.clone()));
    // The `agent_*` family is orchestration, not registry tools (#60, #120): the
    // runtime executor handles them directly, so they only need advertising to
    // the model. Spawning is unconditional now (ADR-0207 §6/§9) — the roster
    // is a constant, not scoped per spawning profile — so it joins the
    // shared `tool_specs` like `ask_user`/`update_tasks` below, keeping the
    // advertised array identical across `SetAgent` (the whole point of the
    // constant roster).
    cfg.tool_specs.extend(subagent::agent_specs(&cfg.agents));
    // `update_tasks` is a runtime state tool (#231): general progress bookkeeping,
    // no cross-agent authority, so it rides the shared specs (a read-only
    // session's permission mode declines the call at dispatch). The runtime
    // executor intercepts it to emit the `TaskList` snapshot.
    cfg.tool_specs.push(plan_tasks::update_tasks_spec());
    // `ask_user` is likewise runtime-owned (#90) but not a spawn tool: every
    // profile may surface a decision prompt, so it stays in the shared specs.
    cfg.tool_specs.push(ask_user::ask_user_spec());
    // `propose_plan` (#231, ADR-0049; #513, ADR-0145) is advertised
    // unconditionally now (ADR-0207 §7: the old default-closed per-profile
    // allowlist gate is retired along with the mask it read) — it joins the
    // shared specs like `update_tasks`; the force-park on `Ask` is the only
    // gate left.
    cfg.tool_specs.push(propose_plan::propose_plan_spec());
    // `request_mode` (#560, ADR-0207 §10) is likewise unconditional — a
    // blocked model must always be able to ask for more authority, in every
    // mode including the one that would deny the ask itself.
    cfg.tool_specs.push(request_mode::request_mode_spec());
    // `poll` (#605, ADR-0161) is runtime-owned like `ask_user` but not a spawn
    // tool either — it joins both background `bash` jobs and sub-agents, so it
    // rides the shared specs rather than the per-profile spawn family.
    cfg.tool_specs.push(poll::poll_spec());
    // `rhai` is a runtime-owned sandboxed script tool (#122, ADR-0046). Its
    // bindings are exactly the root-contained quintet, so it is no more
    // privileged than the always-registered tools and rides the shared specs
    // (registered by default; graded like any tool through the session's
    // permission mode). The executor intercepts it before permission
    // resolution. Behind the `rhai` feature (#502, ADR-0135) — absent
    // entirely from a lean build that opts out of it.
    #[cfg(feature = "rhai")]
    cfg.tool_specs.push(script::rhai_spec());
    // `/tools` and the bare `/enable` checklist offer every advertised tool
    // name — captured here (before `cfg` is moved into `Holly::spawn`), not
    // via the `ToolRegistry` alone, so it also includes the runtime-owned
    // specs appended above (`update_tasks`/`ask_user`/`rhai`) that aren't
    // registry tools.
    let mut tool_names: Vec<String> = cfg.tool_specs.iter().map(|s| s.name.clone()).collect();
    tool_names.sort();
    tool_names.dedup();
    (
        cfg,
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
        workdirs,
    )
}

/// Assemble the tool registry: the root-contained sextet plus the exec pair —
/// `call` (registered unconditionally since ADR-0094) and `bash` (registered
/// unconditionally since ADR-0195, which retired the `ENTANGLEMENT_ENABLE_BASH`
/// opt-in: registration is not where either tool's security story lives, the
/// permission profiles + config ceiling are). `jobs` (#605; #606) is the one
/// process-lifetime `JobRegistry` `poll` was wired up with — shared with both
/// exec tools (not minted fresh) so a `call background=true`/`bash`
/// background job is the same one `poll` can see. `retained` (#608) is
/// likewise the one process-lifetime `RetainedOutputRegistry` `poll` was
/// wired up with, shared with `call` so a truncated blocking result's handle
/// is pollable. `secret_env` (the catalog's provider API-key env vars, #164)
/// is scrubbed from both exec tools' children. `sandbox_resolver`
/// (#399/ADR-0104, #479) resolves both `bash` and `call`'s bubblewrap
/// confinement per session/profile — a resolver that always returns
/// `SandboxPolicy::none()` leaves their spawn behavior unchanged.
#[allow(clippy::too_many_arguments)]
pub(crate) fn register_default_tools(
    root: std::path::PathBuf,
    extra_roots: Option<Arc<extra_roots::ExtraRootStore>>,
    secret_env: Vec<String>,
    sandbox_resolver: Arc<dyn policy::SandboxResolver>,
    jobs: host::JobRegistry,
    retained: retained_output::RetainedOutputRegistry,
    file_backups: Option<file_backup::FileBackups>,
    workdirs: workdir::SessionWorkdirs,
) -> ToolRegistry {
    let mut tools = host::host_tools_with_backups(
        root.clone(),
        extra_roots.clone(),
        file_backups,
        Some(workdirs),
    );
    let mut call = CallTool::new(root.clone())
        .with_secret_env(secret_env.clone())
        .with_sandbox_resolver(sandbox_resolver.clone())
        .with_jobs(jobs.clone())
        .with_retained_output(retained);
    if let Some(e) = &extra_roots {
        call = call.with_extra_roots(e.clone());
    }
    tools.register(call);
    let mut bash = BashTool::new(root.clone())
        .with_secret_env(secret_env.clone())
        .with_jobs(jobs)
        .with_sandbox_resolver(sandbox_resolver);
    if let Some(e) = &extra_roots {
        bash = bash.with_extra_roots(e.clone());
    }
    tools.register(bash);
    // `web_fetch` (#736) is not in the tool-search kernel: under
    // `tool_search` it surfaces through `explore`/`describe` like any other
    // registered tool, so registering it never changes the advertised array.
    #[cfg(feature = "web-fetch")]
    match host::WebFetchTool::new() {
        Ok(tool) => tools.register(tool),
        Err(e) => tracing::warn!("web_fetch unavailable: {e:#}"),
    }
    tools
}

#[cfg(test)]
mod tests {
    use super::register_default_tools;
    use crate::host::SandboxPolicy;
    use crate::retained_output;

    /// The advertised roster `register_default_tools` produces with no env var
    /// or opt-in of any kind — the ADR-0195 posture: both exec tools are
    /// registered unconditionally at startup, exactly like the sextet.
    fn tool_names() -> Vec<String> {
        let root = std::env::temp_dir();
        register_default_tools(
            root,
            None,
            Vec::new(),
            std::sync::Arc::new(SandboxPolicy::none()),
            crate::host::JobRegistry::new(),
            retained_output::RetainedOutputRegistry::new(),
            None,
            crate::workdir::SessionWorkdirs::default(),
        )
        .specs()
        .into_iter()
        .map(|s| s.name.to_string())
        .collect()
    }

    #[test]
    fn call_and_bash_are_registered_unconditionally_at_startup() {
        // ADR-0195: `bash` joins `call` (ADR-0093) as an always-registered
        // built-in — no env var, no `/enable` needed. What a run may do with
        // it is the permission profiles' + the config ceiling's job, not the
        // registry's.
        let names = tool_names();
        assert!(names.contains(&"call".to_string()), "{names:?}");
        assert!(names.contains(&"bash".to_string()), "{names:?}");
        #[cfg(feature = "web-fetch")]
        assert!(names.contains(&"web_fetch".to_string()), "{names:?}");
        // The sextet is still there alongside them.
        for expected in ["read", "glob", "grep", "edit", "write", "apply_patch"] {
            assert!(names.contains(&expected.to_string()), "{names:?}");
        }
    }
}
