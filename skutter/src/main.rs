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

// The heads (`run`/`pipe`/`tui`, dispatched by `heads`), the clap surface
// (`args`), and the startup wiring (`providers`/`factories` → `engine_setup`
// → `boot` → `services`). The crate-root `use`s below make each runtime
// module, and the `skutter` library's own, reachable as `crate::<name>`.
mod args;
mod boot;
mod config_keys;
mod engine_setup;
mod factories;
mod heads;
mod pipe;
mod provider_setup;
mod providers;
mod run;
mod services;
mod tui;
mod user_dirs;

use entanglement_runtime::{
    agents, arg_validate, capability, discover, exec, jobs, mcp, mode, path_grading, plan_files,
    plan_watch, policy, retained_output, script_ops, skills, subagent, system_prompt,
    system_prompt_mode, throttle, tool_advertising, tool_names, tool_runner, SharedRegistry,
    ToolRegistry,
};
use skutter::{
    aux_llm, config, file_backup, history, hooks, host, inspect, logging, narrate, persistence,
    serve, session_store, session_title, slash_commands, state, watch,
};

use anyhow::{Context, Result};
use clap::Parser;
use entanglement_provider::{Catalog, ModelInfo};

use args::{Cli, Cmd, ConfigCmd, InspectCmd};

#[tokio::main]
async fn main() -> Result<()> {
    logging::startup_mark("process start");
    let cli = Cli::parse();
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));

    // Load the layered user config (#172, ADR-0047) up front: embedded defaults <
    // user (`${config_dir}/entanglement/config.yml`) < project
    // (`.entanglement/config.yml`). A malformed file is a loud error. Loaded
    // before logging so its `verbose` can raise the level for every mode,
    // including the pre-engine `inspect`/`sessions` fast paths.
    let config::Resolved {
        config: mut user_config,
        warnings: config_warnings,
        ..
    } = config::Config::resolve(&cwd).context("loading user config")?;
    // The library passes `hooks:` through unparsed (#964); a typo in it stays
    // as loud as one anywhere else in the file, for every subcommand. Parsed
    // once here and handed to `inspect config` and the services.
    let hooks = hooks::Hooks::parse(&user_config.hooks).context("loading user config")?;

    // Session-log auto-prune on startup (Issue 4, Phase 4.2): best-effort,
    // scoped to this cwd's session dir only. A read-only data dir or a transient
    // error is logged and skipped — never fatal to startup. Runs before any head
    // so the TUI's resume modal and `sessions` both reflect the pruned set.
    match crate::user_dirs::session_store(&cwd).prune(user_config.session_retention_days) {
        Ok(removed) if removed > 0 => {
            tracing::info!(
                "startup prune removed {removed} session log(s) older than {} day(s)",
                user_config.session_retention_days
            );
        }
        Ok(_) => {}
        Err(e) => tracing::debug!("startup session prune skipped: {e:#}"),
    }

    // Shared endpoint-state auto-prune on startup (#551): a `.state`/`.lock`
    // pair left behind by a `/key` rotation, a catalog `base_url` edit, or a
    // decommissioned provider is never otherwise removed. Best-effort and
    // process-wide (unlike the session prune above, this isn't scoped to
    // `cwd` — the state dir has no per-project split) — an idle-but-empty
    // pair older than an hour is swept; anything still live is left alone.
    let removed = entanglement_provider::prune_stale(std::time::Duration::from_secs(3600));
    if removed > 0 {
        tracing::info!("startup prune removed {removed} orphaned shared endpoint-state file(s)");
    }

    // stdout is reserved for command output — the assembled prompt
    // (`inspect prompt`), NDJSON frames (`run --format json` / `pipe`) — so logs
    // go to stderr, except under the TUI (raw mode) where they'd corrupt the
    // screen and get a file sink instead. `RUST_LOG` overrides `--verbose`;
    // `--verbose` overrides the config's `verbose`.
    //
    // The TUI is entered via `skutter tui` *and* as the default head for a bare
    // `skutter` with no prompt (the `None` arm of `heads::dispatch`); both
    // hold the terminal in raw mode, so both must route logs to the file sink —
    // otherwise a mid-session log line corrupts the display.
    let launches_tui = args::launches_tui_head(&cli.cmd, &cli.prompt);
    logging::init(cli.verbose || user_config.verbose, launches_tui)?;
    logging::startup_mark("config+prune+logging");

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
        return heads::print_sessions(&cwd);
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
            InspectCmd::Config => inspect::inspect_config(&cwd, &hooks),
            InspectCmd::AuxModels => inspect::inspect_aux_models(),
            InspectCmd::McpTokens => inspect::inspect_mcp_tokens(),
            InspectCmd::Mcp => inspect::inspect_mcp(&cwd),
            InspectCmd::Session { id } => inspect::inspect_session(&cwd, id),
            InspectCmd::Modes { name } => inspect::inspect_modes(&cwd, name.as_deref()),
            InspectCmd::Commands { name } => inspect::inspect_commands(&cwd, name.as_deref()),
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
                config_keys::set_key(&catalog, provider, key.clone())
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

    // Config load findings, unknown `ENTANGLEMENT_DEBUG` tokens and MCP
    // servers dropped for a tool-name clash. After the `.env` load so a debug
    // value parked there is caught too; `eprintln!` like the scaffold notices
    // above, so it shows without `--verbose` and stays in the scrollback after
    // a TUI exit.
    let mcp_clashes = mcp::AvailableMcp::from_config(&user_config.mcp, Vec::new())
        .clash_warnings()
        .to_vec();
    for warning in config_warnings
        .iter()
        .cloned()
        .chain(mcp_clashes)
        .chain(unknown_debug_tokens(
            std::env::var(entanglement_provider::DEBUG_ENV)
                .ok()
                .as_deref(),
        ))
    {
        eprintln!("skutter: warning: {warning}");
    }

    // First-run onboarding (#732, ADR-0222): the TUI asks for a provider
    // before boot when none is usable; other heads hit `select_provider`'s
    // hint-and-exit instead of a silent echo fallback.
    if launches_tui && !provider_setup::provider_configured(&catalog, &user_config) {
        if let tui::onboarding::Next::Quit =
            tui::onboarding::run(&catalog, &mut user_config).await?
        {
            return Ok(());
        }
    }

    let engine = boot::boot(&cwd, &user_config, &catalog).await?;
    let (holly, head, services) = services::start(engine, &cwd, &user_config, hooks, &catalog)?;
    let result = heads::dispatch(cli, &holly, head, catalog, &user_config, &cwd).await;
    services.shutdown(&holly).await;
    result
}

/// One warning naming the `ENTANGLEMENT_DEBUG` tokens no flag reads, if any.
fn unknown_debug_tokens(raw: Option<&str>) -> Option<String> {
    let (_, unknown) = entanglement_provider::DebugFlags::parse(raw?);
    (!unknown.is_empty()).then(|| {
        format!(
            "{}: ignoring unknown token(s) {} — known: bodies, echo, no-shared-state",
            entanglement_provider::DEBUG_ENV,
            unknown.join(", ")
        )
    })
}

#[cfg(test)]
mod tests {
    use super::unknown_debug_tokens;

    #[test]
    fn unknown_debug_tokens_yield_one_warning() {
        assert_eq!(unknown_debug_tokens(None), None);
        assert_eq!(unknown_debug_tokens(Some("")), None);
        assert_eq!(unknown_debug_tokens(Some("bodies,echo")), None);
        let w = unknown_debug_tokens(Some("bodies,1,verbose")).expect("unknown tokens");
        assert!(w.contains("1, verbose"), "{w}");
    }
}
