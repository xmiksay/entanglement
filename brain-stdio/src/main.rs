//! `brain` — stdio head for the headless agent engine.
//!
//! Two modes, both driving [`brain_core::Brain`] directly (the ABI):
//! - `run` sends a prompt and streams events until `Done`. `--format json`
//!   emits raw NDJSON (like `opencode run --format json`); `--format text`
//!   renders human-friendly output.
//! - `pipe` is a bidirectional NDJSON relay: `InMsg` lines on stdin,
//!   `OutEvent` lines on stdout. For scripting / editor integration.

use std::io::Write;
use std::time::Duration;

use anyhow::Result;
use brain_core::{AgentState, Brain, EngineConfig, InMsg, OutEvent, SessionId, TaskStatus};
use clap::{Parser, Subcommand};
use tokio::io::AsyncBufReadExt;

#[derive(Parser)]
#[command(
    name = "brain",
    version,
    about = "Headless AI coding agent — stdio head"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
    /// Default subcommand equivalent to `run` with a prompt.
    #[arg(default_value = "Hello, brain!")]
    prompt: Vec<String>,
    #[arg(long)]
    verbose: bool,
}

#[derive(Subcommand)]
enum Cmd {
    /// One-shot: send a prompt and stream the turn.
    Run {
        /// Prompt text.
        prompt: Vec<String>,
        /// Session id to use.
        #[arg(long, default_value = "run")]
        session: String,
        /// Agent profile to run under (build | plan | explore | custom).
        #[arg(long)]
        agent: Option<String>,
        /// Output format.
        #[arg(long, value_name = "text|json", default_value = "text")]
        format: String,
    },
    /// Bidirectional NDJSON relay (stdin: InMsg, stdout: OutEvent).
    Pipe {
        #[arg(long, default_value = "pipe")]
        session: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let filter = if cli.verbose { "debug" } else { "warn" };
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let brain = Brain::spawn(EngineConfig::default());

    match cli.cmd {
        Some(Cmd::Run {
            prompt,
            session,
            agent,
            format,
        }) => {
            let prompt = prompt.join(" ");
            run_one(
                &brain,
                &SessionId::new(session),
                agent.as_deref(),
                &prompt,
                &format,
            )
            .await
        }
        Some(Cmd::Pipe { session }) => pipe(&brain, &SessionId::new(session)).await,
        None => {
            let prompt = cli.prompt.join(" ");
            run_one(&brain, &SessionId::new("run"), None, &prompt, "text").await
        }
    }
}

/// Send one prompt and stream events until `Done` (or timeout).
async fn run_one(
    brain: &Brain,
    session: &SessionId,
    agent: Option<&str>,
    prompt: &str,
    format: &str,
) -> Result<()> {
    let json = format == "json";
    let mut sub = brain.subscribe();

    if let Some(a) = agent {
        brain
            .send(InMsg::SetAgent {
                session: session.clone(),
                agent: a.to_string(),
            })
            .await?;
    }
    brain
        .send(InMsg::Prompt {
            session: session.clone(),
            text: prompt.to_string(),
        })
        .await?;

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    loop {
        let ev = match tokio::time::timeout(Duration::from_secs(60), sub.recv()).await {
            Ok(Ok(ev)) => ev,
            Ok(Err(_)) => break, // engine dropped
            Err(_) => anyhow::bail!("timed out waiting for engine event"),
        };
        if ev.session() != session {
            continue;
        }
        if json {
            writeln!(out, "{}", serde_json::to_string(&ev)?)?;
        } else {
            render_text(&mut out, &ev)?;
        }
        out.flush()?;
        if matches!(ev, OutEvent::Done { .. }) {
            break;
        }
    }
    Ok(())
}

/// Continuous NDJSON relay.
async fn pipe(brain: &Brain, default_session: &SessionId) -> Result<()> {
    let mut sub = brain.subscribe();
    let (done_tx, mut done_rx) = tokio::sync::oneshot::channel::<()>();

    // stdin reader → InMsg
    let brain2 = brain.clone();
    let default_session = default_session.clone();
    tokio::spawn(async move {
        let stdin = tokio::io::stdin();
        let mut lines = tokio::io::BufReader::new(stdin).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            match serde_json::from_str::<InMsg>(trimmed) {
                Ok(msg) => {
                    if brain2.send(msg).await.is_err() {
                        break;
                    }
                }
                Err(e) => {
                    let _ = brain2
                        .send(InMsg::Prompt {
                            session: default_session.clone(),
                            text: trimmed.to_string(),
                        })
                        .await;
                    eprintln!("note: treated line as prompt for default session ({e})");
                }
            }
        }
        let _ = done_tx.send(());
    });

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    loop {
        tokio::select! {
            ev = sub.recv() => match ev {
                Ok(ev) => {
                    writeln!(out, "{}", serde_json::to_string(&ev)?)?;
                    out.flush()?;
                }
                Err(_) => break,
            },
            _ = &mut done_rx => {
                // stdin closed; flush any trailing events then stop.
                while let Ok(ev) = tokio::time::timeout(Duration::from_millis(200), sub.recv()).await {
                    if let Ok(ev) = ev {
                        writeln!(out, "{}", serde_json::to_string(&ev)?)?;
                    }
                }
                break;
            }
        }
    }
    Ok(())
}

/// Human-friendly rendering of a single event.
fn render_text<W: Write>(out: &mut W, ev: &OutEvent) -> Result<()> {
    match ev {
        OutEvent::Status { state, .. } => match state {
            AgentState::Thinking => writeln!(out, "… thinking")?,
            AgentState::WaitingApproval => writeln!(out, "… waiting for approval")?,
            AgentState::Error => writeln!(out, "! turn ended in error")?,
            _ => {}
        },
        OutEvent::AgentChanged { agent, .. } => writeln!(out, "# agent: {agent}")?,
        OutEvent::Plan { content, .. } => writeln!(out, "▸ plan:\n{content}")?,
        OutEvent::TextDelta { text, .. } => writeln!(out, "> {text}")?,
        OutEvent::ToolRequest { tool, input, .. } => writeln!(out, "? {tool}: {input}")?,
        OutEvent::ToolOutput { output, .. } => writeln!(out, "= {output}")?,
        OutEvent::TaskList { tasks, .. } => {
            writeln!(out, "▢ tasks:")?;
            for t in tasks {
                writeln!(out, "  [{}] {}", task_symbol(t.status), t.content)?;
            }
        }
        OutEvent::Error { message, .. } => writeln!(out, "! {message}")?,
        OutEvent::Done { .. } => writeln!(out, "✓ done")?,
    }
    Ok(())
}

fn task_symbol(s: TaskStatus) -> &'static str {
    match s {
        TaskStatus::Pending => "○",
        TaskStatus::InProgress => "▶",
        TaskStatus::Completed => "✓",
        TaskStatus::Cancelled => "✗",
    }
}
