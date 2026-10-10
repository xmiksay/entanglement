//! Dispatch of an engine-backed invocation to its head — `run` (explicit or
//! the implicit one-shot), `pipe`, `serve`, or the TUI — plus the engine-free
//! `sessions` listing.

use std::path::Path;

use anyhow::Result;
use entanglement_core::{Holly, IdKind, InMsg, SessionId};
use entanglement_provider::Catalog;

use crate::args::{Cli, Cmd};
use crate::config;
use crate::pipe::pipe;
use crate::run::run_one;
use crate::services::Head;
use crate::session_store::{self, LaunchMode};
use crate::tui::tui;

/// Run the head `cli.cmd` selects (`None` = bare `skutter`: the TUI with no
/// prompt, an implicit one-shot `run` with one). The engine-free subcommands
/// (`sessions`/`inspect`/`config`) are handled in `main` before the engine
/// boots and never reach here.
pub(crate) async fn dispatch(
    cli: Cli,
    holly: &Holly,
    head: Head,
    catalog: Catalog,
    user_config: &config::Config,
    cwd: &Path,
) -> Result<()> {
    match cli.cmd {
        Some(Cmd::Run {
            prompt,
            session,
            agent,
            format,
            resume,
        }) => {
            let fresh = resume.is_none();
            let session_id = if let Some(resume_id) = &resume {
                SessionId::new(resume_id.clone())
            } else {
                SessionId::new(session.unwrap_or_else(|| holly.next_id(IdKind::Session)))
            };

            // An explicit `--mode` wins over ADR-0215's read-only narrowing.
            let launch = LaunchMode::resolve(cli.mode, user_config.mode.clone());
            if let Some(resume_id) = resume {
                let resume_session_id = SessionId::new(resume_id);
                let known = known_agents(&head);
                let (records, check) = session_store::SessionStore::for_cwd(cwd)
                    .load_for_resume(&resume_session_id, |n| known.iter().any(|k| k == n))?;
                // A retired agent resumes under a fallback (ADR-0215); say so.
                for fallback in &check.fallbacks {
                    eprintln!("{}", fallback.note(launch.explicit));
                }

                // Forced modes ride the replay itself, so a re-offered
                // parked call is never graded under the wider mode first, and
                // the configured mode never cascades over them (ADR-0216).
                session_store::resume_with_mode(
                    holly,
                    &session_id,
                    &records,
                    &check,
                    launch.explicit,
                    launch.mode.clone(),
                )
                .await?;
            }

            // CLI `--agent` wins; else the user config's default agent (#172).
            // An agent is fixed for a session's whole life (ADR-0207 §9), so
            // this only ever applies to a fresh spawn below — warn rather than
            // silently ignoring an explicit `--agent` alongside `--resume`.
            let agent = agent.or_else(|| user_config.agent.clone());
            if fresh {
                let agent = agent
                    .clone()
                    .unwrap_or_else(|| entanglement_core::DEFAULT_AGENT.to_string());
                start_fresh(holly, &session_id, Some(agent), launch.mode).await?;
            } else if agent.is_some() {
                tracing::warn!(
                    "--agent is ignored with --resume: a resumed session's agent is fixed"
                );
            }
            let prompt = prompt.join(" ");
            run_one(holly, &session_id, &prompt, &format).await
        }
        Some(Cmd::Pipe { session }) => {
            let session_id =
                SessionId::new(session.unwrap_or_else(|| holly.next_id(IdKind::Session)));
            pipe(holly, &session_id).await
        }
        #[cfg(feature = "serve")]
        Some(Cmd::Serve { port, allow_origin }) => {
            // Runs until Ctrl-C; the executor/persistence teardown in `main`
            // then runs as for any other head. A fresh `Holly` clone keeps the
            // outer handle for that shutdown path.
            entanglement_runtime::serve::serve(holly.clone(), port, allow_origin).await
        }
        Some(Cmd::Tui { session, agent }) => {
            let session_id =
                SessionId::new(session.unwrap_or_else(|| holly.next_id(IdKind::Session)));
            // CLI `--agent` wins; else the user config's default agent (#172).
            let agent = agent.or_else(|| user_config.agent.clone());
            let launch = LaunchMode::resolve(cli.mode, user_config.mode.clone());
            launch_tui(
                holly,
                session_id,
                agent,
                launch,
                head,
                catalog,
                user_config,
                cwd,
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
            let session_id = SessionId::new(holly.next_id(IdKind::Session));
            let agent = user_config.agent.clone();
            let launch = LaunchMode::resolve(cli.mode, user_config.mode.clone());
            if cli.prompt.is_empty() {
                launch_tui(
                    holly,
                    session_id,
                    agent,
                    launch,
                    head,
                    catalog,
                    user_config,
                    cwd,
                )
                .await
            } else {
                start_fresh(holly, &session_id, agent, launch.mode).await?;
                let prompt = cli.prompt.join(" ");
                run_one(holly, &session_id, &prompt, "text").await
            }
        }
    }
}

/// Start a fresh session the same way on every head (#752): bind its agent —
/// core seeds the starting mode from that agent's `default_mode` (#763) —
/// then land the head's `--mode`/config `mode:` on top, so the precedence is
/// `--mode` > config `mode:` > agent `default_mode` > `DEFAULT_MODE`. The
/// bind goes first: a `SetMode` to a still-nonexistent id would lazily
/// create it under the default agent instead (see `holly.rs`'s
/// unknown-session-id path). `None` agent leaves that lazy path to create it.
async fn start_fresh(
    holly: &Holly,
    session_id: &SessionId,
    agent: Option<String>,
    mode: Option<String>,
) -> Result<()> {
    if let Some(agent) = agent {
        bind_agent(holly, session_id, agent).await?;
    }
    apply_mode(holly, session_id, mode).await
}

/// Bind a fresh session's identity with an empty-prompt `Spawn` — the agent
/// is fixed for the session's whole life from here (ADR-0207 §9); the real
/// first prompt arrives afterwards.
async fn bind_agent(holly: &Holly, session_id: &SessionId, agent: String) -> Result<()> {
    holly
        .send(InMsg::Spawn {
            session: session_id.clone(),
            parent: None,
            predecessor: None,
            agent,
            prompt: String::new(),
            user: None,
            model: None,
        })
        .await?;
    Ok(())
}

/// Every agent name the live catalog carries (ADR-0215's fallback check).
fn known_agents(head: &Head) -> Vec<String> {
    let catalog = head
        .live_profiles
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    catalog.iter().map(|a| a.name.clone()).collect()
}

/// `--mode` (ADR-0207 §11/§12) for a fresh session: `SetMode` is
/// trusted-only (a mode *is* the session's authority), so this direct
/// `Holly::send` — not the wire — is exactly the surface it's meant for.
/// Callers resolve `--mode` > `config.yml` `mode:`; `None` sends nothing,
/// leaving the session at its agent's `default_mode` (else the engine's
/// `DEFAULT_MODE`) as seeded at spawn. A resume goes
/// through `session_store::resume_with_mode` instead (ADR-0216).
async fn apply_mode(holly: &Holly, session_id: &SessionId, mode: Option<String>) -> Result<()> {
    if let Some(mode) = mode {
        holly
            .send(InMsg::SetMode {
                session: session_id.clone(),
                mode,
            })
            .await?;
    }
    Ok(())
}

/// Bootstrap `session_id` under `agent` (when set) and the launch mode, then
/// hand the terminal to the TUI, which applies the same launch mode to any
/// session it later resumes (#752).
#[allow(clippy::too_many_arguments)]
async fn launch_tui(
    holly: &Holly,
    session_id: SessionId,
    agent: Option<String>,
    launch: LaunchMode,
    head: Head,
    catalog: Catalog,
    user_config: &config::Config,
    cwd: &Path,
) -> Result<()> {
    // Subscribe *before* the bootstrap `Spawn` send below, matching the
    // "subscribe before send" convention `subagent.rs` follows for spawned
    // children — otherwise the session task can emit
    // `SessionStarted`/`AgentChanged` before `tui()` gets around to
    // subscribing, permanently desyncing the agent badge (#598).
    let holly_sub = holly.subscribe();
    start_fresh(holly, &session_id, agent, launch.mode.clone()).await?;
    tui(
        holly,
        holly_sub,
        session_id,
        head.model_info,
        head.provider_name,
        catalog,
        head.live_profiles,
        head.live_agent_models,
        head.live_agent_generation,
        head.live_aux_models,
        head.reload_rx,
        cwd.to_path_buf(),
        head.tool_names,
        head.http_client,
        user_config.editor.clone(),
        user_config.tui.clone(),
        head.grants,
        head.mcp,
        head.advertising_state,
        launch,
    )
    .await
}

/// Prints past root sessions for `cwd`, most-recently-active first.
pub(crate) fn print_sessions(cwd: &Path) -> Result<()> {
    let mut sessions = session_store::SessionStore::for_cwd(cwd).list_sessions()?;
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
        // A set display name beats the derived first-prompt snippet.
        let description = s
            .name
            .as_deref()
            .or(s.first_prompt.as_deref())
            .unwrap_or("");
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
    //! `start_fresh` is the one launch path `run`, the implicit one-shot and
    //! the TUI share (#752), so the starting-mode precedence (#763,
    //! ADR-0218) is proven here once for every head.
    use std::time::Duration;

    use entanglement_core::{EngineConfig, OutEvent};

    use super::*;

    /// Starts `sid` the way a head does, then returns the mode it settled in
    /// (the last `ModeChanged` before a barrier turn's `Done`).
    async fn settled_mode(agent: Option<&str>, launch: LaunchMode) -> String {
        let holly = Holly::spawn(EngineConfig {
            agents: entanglement_runtime::agents::built_in_registry().expect("built-ins"),
            ..EngineConfig::default()
        });
        let sid = SessionId::new("s1");
        let mut sub = holly.subscribe();
        start_fresh(&holly, &sid, agent.map(String::from), launch.mode)
            .await
            .expect("start");
        holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
        let mut last = None;
        loop {
            let ev = tokio::time::timeout(Duration::from_secs(5), sub.recv())
                .await
                .expect("turn never finished")
                .expect("broadcast closed");
            match ev {
                OutEvent::ModeChanged { session, mode } if session == sid => last = Some(mode),
                OutEvent::Done { session, .. } if session == sid => break,
                _ => {}
            }
        }
        last.expect("no mode announced")
    }

    #[tokio::test]
    async fn the_plan_agent_starts_in_plan_with_no_mode_given() {
        assert_eq!(
            settled_mode(Some("plan"), LaunchMode::default()).await,
            "plan"
        );
    }

    #[tokio::test]
    async fn an_explicit_mode_beats_the_agents_default() {
        let launch = LaunchMode::resolve(Some("build".into()), None);
        assert_eq!(settled_mode(Some("plan"), launch).await, "build");
    }

    #[tokio::test]
    async fn a_config_mode_beats_the_agents_default() {
        let launch = LaunchMode::resolve(None, Some("build".into()));
        assert_eq!(settled_mode(Some("plan"), launch).await, "build");
    }

    #[tokio::test]
    async fn a_mode_with_no_agent_reaches_the_lazily_created_root() {
        // The bare-TUI case: no `--agent`, but `--mode research`.
        let launch = LaunchMode::resolve(Some("research".into()), None);
        assert_eq!(settled_mode(None, launch).await, "research");
    }

    #[tokio::test]
    async fn no_mode_and_no_hint_is_the_engine_default() {
        let mode = settled_mode(Some("general"), LaunchMode::default()).await;
        assert_eq!(mode, entanglement_core::DEFAULT_MODE);
    }
}
