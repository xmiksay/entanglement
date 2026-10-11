//! Dispatch of an engine-backed invocation to its head — `run` (explicit or
//! the implicit one-shot), `pipe`, `serve`, or the TUI — plus the engine-free
//! `sessions` listing.

use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use entanglement_core::{Holly, IdKind, InMsg, OutEvent, SessionId};
use entanglement_provider::Catalog;

use crate::args::{Cli, Cmd};
use crate::pipe::pipe;
use crate::run::run_one;
use crate::services::Head;
use crate::session_store::{self, LaunchMode};
use crate::tui::tui;
use crate::{config, slash_commands};

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
            let launch = launch_mode(cli.mode, user_config, cwd)?;
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
            // `/name args` expands a user slash command, as in the TUI (#733).
            let prompt = slash_commands::expand_argv_or_literal(&head.live_commands, &prompt);
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
            let launch = launch_mode(cli.mode, user_config, cwd)?;
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
            let launch = launch_mode(cli.mode, user_config, cwd)?;
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
                let prompt =
                    slash_commands::expand_argv_or_literal(&head.live_commands, &cli.prompt);
                run_one(holly, &session_id, &prompt, "text").await
            }
        }
    }
}

/// Resolve the head's launch mode, naming the config file that set a bad
/// `mode:` so the error says what to fix (ADR-0218).
fn launch_mode(
    cli: Option<String>,
    user_config: &config::Config,
    cwd: &Path,
) -> Result<LaunchMode> {
    LaunchMode::resolve(cli, user_config.mode.clone(), || {
        config::Config::resolve(cwd)
            .ok()
            .and_then(|r| {
                let layer = r.provenance.iter().find(|(k, _)| k == "mode")?.1;
                r.layers
                    .into_iter()
                    .find(|(l, _)| *l == layer)
                    .map(|(_, s)| s)
            })
            .unwrap_or_else(|| "config.yml".to_string())
    })
}

/// Start a fresh session the same way on every head (#752): one
/// empty-prompt `Spawn` binds its agent — fixed for the session's whole life
/// (ADR-0207 §9) — and carries the head's `--mode`/config `mode:`, so the
/// session announces exactly one starting mode, with no follow-up `SetMode`
/// transition for the model to read as a change. With no launch mode, core
/// starts it in the agent's `default_mode` (clamped to `DEFAULT_MODE`), else
/// `DEFAULT_MODE`: precedence `--mode` > config `mode:` > agent hint >
/// `build` (#763, ADR-0218). The bind must precede the first prompt, which
/// would otherwise lazily create the id under the default agent (see
/// `holly.rs`'s unknown-session-id path). With neither an agent nor a mode
/// there is nothing to bind, so that lazy path creates it on first use.
async fn start_fresh(
    holly: &Holly,
    session_id: &SessionId,
    agent: Option<String>,
    mode: Option<String>,
) -> Result<()> {
    if agent.is_none() && mode.is_none() {
        return Ok(());
    }
    // Subscribed before the send: a refusal is a seq-0 `Error` emitted
    // before any caller listens. It must stop the launch — a prompt sent
    // after it would lazily create a `general`/`build` root instead
    // (ADR-0210), silently dropping the agent and the narrower mode.
    let mut events = holly.subscribe();
    holly
        .send(InMsg::Spawn {
            session: session_id.clone(),
            parent: None,
            predecessor: None,
            agent: agent.unwrap_or_else(|| entanglement_core::DEFAULT_AGENT.to_string()),
            prompt: String::new(),
            user: None,
            model: None,
            mode,
            workdir: None,
        })
        .await?;
    await_started(holly, &mut events, session_id).await
}

/// How long a fresh session's `Spawn` may take to start or be refused.
const SPAWN_ACK_TIMEOUT: Duration = Duration::from_secs(10);

/// Wait for `session_id`'s `SessionStarted`, failing on its supervisor
/// `Error` (a refused `Spawn`) or on no answer within [`SPAWN_ACK_TIMEOUT`].
async fn await_started(
    holly: &Holly,
    events: &mut tokio::sync::broadcast::Receiver<OutEvent>,
    session_id: &SessionId,
) -> Result<()> {
    use tokio::sync::broadcast::error::RecvError;
    let deadline = tokio::time::Instant::now() + SPAWN_ACK_TIMEOUT;
    loop {
        let event = tokio::time::timeout_at(deadline, events.recv())
            .await
            .with_context(|| format!("session {session_id} did not start in time"))?;
        match event {
            Ok(OutEvent::SessionStarted { session, .. }) if &session == session_id => return Ok(()),
            Ok(OutEvent::Error {
                session, message, ..
            }) if &session == session_id => bail!("cannot start session {session_id}: {message}"),
            // The answer may have been among the dropped events; a live
            // session means it started (a refusal never registers one).
            Err(RecvError::Lagged(_)) if holly.is_live(session_id) => return Ok(()),
            Ok(_) | Err(RecvError::Lagged(_)) => {}
            Err(RecvError::Closed) => bail!("engine stopped before session {session_id} started"),
        }
    }
}

/// Every agent name the live catalog carries (ADR-0215's fallback check).
fn known_agents(head: &Head) -> Vec<String> {
    let catalog = head
        .live_profiles
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    catalog.iter().map(|a| a.name.clone()).collect()
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
        head.live_commands,
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
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use async_trait::async_trait;
    use entanglement_core::{
        stream_from_response, EngineConfig, Llm, LlmRequest, LlmResponse, LlmStream, OutEvent,
    };

    use super::*;

    /// Records each request's model-visible mode notice.
    struct NoticeLlm(Arc<Mutex<Vec<String>>>);
    #[async_trait]
    impl Llm for NoticeLlm {
        async fn stream(&mut self, req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
            let notice = req.trailing_notice.clone().unwrap_or_default();
            self.0.lock().unwrap().push(notice);
            Ok(stream_from_response(LlmResponse {
                text: "done".into(),
                ..Default::default()
            }))
        }
    }

    /// Starts `sid` the way a head does and runs one barrier turn. Asserts
    /// it announced exactly one starting mode (no follow-up switch) and that
    /// the model never read a "changed from" transition; returns that mode.
    async fn settled_mode(agent: Option<&str>, launch: LaunchMode) -> String {
        let notices = Arc::new(Mutex::new(Vec::new()));
        let seen = notices.clone();
        let holly = Holly::spawn(EngineConfig {
            agents: entanglement_runtime::agents::built_in_registry().expect("built-ins"),
            mode_ladder: entanglement_runtime::mode::BUILTIN_LADDER
                .map(String::from)
                .to_vec(),
            llm_factory: Arc::new(move || Box::new(NoticeLlm(seen.clone())) as Box<dyn Llm>),
            ..EngineConfig::default()
        });
        let sid = SessionId::new("s1");
        let mut sub = holly.subscribe();
        start_fresh(&holly, &sid, agent.map(String::from), launch.mode)
            .await
            .expect("start");
        holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
        let mut modes = Vec::new();
        loop {
            let ev = tokio::time::timeout(Duration::from_secs(5), sub.recv())
                .await
                .expect("turn never finished")
                .expect("broadcast closed");
            match ev {
                OutEvent::ModeChanged { session, mode } if session == sid => modes.push(mode),
                OutEvent::Done { session, .. } if session == sid => break,
                _ => {}
            }
        }
        assert_eq!(modes.len(), 1, "one starting mode, no switch: {modes:?}");
        let notices = notices.lock().unwrap();
        assert!(
            notices.iter().all(|n| !n.contains("changed from")),
            "{notices:?}"
        );
        modes.remove(0)
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
        let launch = LaunchMode::resolve(Some("build".into()), None, String::new).unwrap();
        assert_eq!(settled_mode(Some("plan"), launch).await, "build");
    }

    #[tokio::test]
    async fn a_config_mode_beats_the_agents_default() {
        let launch = LaunchMode::resolve(None, Some("build".into()), String::new).unwrap();
        assert_eq!(settled_mode(Some("plan"), launch).await, "build");
    }

    #[tokio::test]
    async fn a_mode_with_no_agent_reaches_the_lazily_created_root() {
        // The bare-TUI case: no `--agent`, but `--mode research`.
        let launch = LaunchMode::resolve(Some("research".into()), None, String::new).unwrap();
        assert_eq!(settled_mode(None, launch).await, "research");
    }

    #[tokio::test]
    async fn no_mode_and_no_hint_is_the_engine_default() {
        let mode = settled_mode(Some("general"), LaunchMode::default()).await;
        assert_eq!(mode, entanglement_core::DEFAULT_MODE);
    }

    #[tokio::test]
    async fn a_refused_spawn_stops_the_launch() {
        // A mode the engine refuses must fail `start_fresh` itself, so no
        // prompt ever follows into a lazily created `general`/`build` root.
        let calls = Arc::new(Mutex::new(Vec::new()));
        let seen = calls.clone();
        let holly = Holly::spawn(EngineConfig {
            agents: entanglement_runtime::agents::built_in_registry().expect("built-ins"),
            mode_ladder: entanglement_runtime::mode::BUILTIN_LADDER
                .map(String::from)
                .to_vec(),
            llm_factory: Arc::new(move || Box::new(NoticeLlm(seen.clone())) as Box<dyn Llm>),
            ..EngineConfig::default()
        });
        let sid = SessionId::new("s1");
        let err = start_fresh(&holly, &sid, Some("plan".into()), Some("reserch".into()))
            .await
            .expect_err("a refused spawn must fail the launch");
        assert!(
            format!("{err:#}").contains("unknown mode `reserch`"),
            "{err:#}"
        );
        assert!(!holly.is_live(&sid));
        assert!(calls.lock().unwrap().is_empty(), "no LLM request was made");
    }
}
