//! `skutter` — stdio head for the headless agent engine.
//!
//! Two modes, both driving [`entanglement_core::Holly`] directly (the ABI):
//! - `run` sends a prompt and streams events until `Done`. `--format json`
//!   emits raw NDJSON (like `opencode run --format json`); `--format text`
//!   renders human-friendly output.
//! - `pipe` is a bidirectional NDJSON relay: `InMsg` lines on stdin,
//!   `OutEvent` lines on stdout. For scripting / editor integration.

use std::io::Write;
use std::time::Duration;

use anyhow::Result;
use clap::{Parser, Subcommand};
use entanglement_core::{
    host_tools, AgentState, EngineConfig, Holly, InMsg, OutEvent, SessionId, TaskStatus,
};
use tokio::io::AsyncBufReadExt;

/// Default models per provider when its `<PROVIDER>_MODEL` env is unset.
const DEFAULT_ZAI_MODEL: &str = "glm-5.2";
const DEFAULT_OPENAI_MODEL: &str = "gpt-4o";
const DEFAULT_OLLAMA_MODEL: &str = "llama3.1";
const DEFAULT_ANTHROPIC_MODEL: &str = "claude-sonnet-4-5";

/// Pick a provider and build the engine config.
///
/// Selection order:
/// 1. `ENTANGLEMENT_PROVIDER` env, one of `zai | openai | ollama | anthropic`
///    (explicit; errors loudly if the matching key is missing).
/// 2. Auto-detect by key presence, z.ai first (the project's primary), then
///    OpenAI, then Anthropic.
/// 3. Fall back to `DummyLlm` so `skutter` always runs end-to-end.
///
/// Set `ENTANGLEMENT_PROVIDER=ollama` to use a local keyless Ollama (it has no key to
/// auto-detect on). z.ai/OpenAI/Ollama share one OpenAI-compatible client
/// ([`entanglement_llm::openai_factory`]); Anthropic has its own client.
///
/// The read-only host trio (`read`, `glob`, `grep`) is always registered,
/// rooted at the current working directory, so the `build`/`plan`/`explore`
/// permission profiles gate something real out of the box.
fn build_config() -> EngineConfig {
    let mut cfg = select_provider();
    let root = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    cfg.tools = host_tools(root);
    cfg
}

fn select_provider() -> EngineConfig {
    match std::env::var("ENTANGLEMENT_PROVIDER").ok().as_deref() {
        Some("zai") => require(
            zai_config(),
            "ENTANGLEMENT_PROVIDER=zai requires ZAI_API_KEY",
        ),
        Some("openai") => require(
            openai_config(),
            "ENTANGLEMENT_PROVIDER=openai requires OPENAI_API_KEY",
        ),
        Some("ollama") => ollama_config(),
        Some("anthropic") => require(
            anthropic_config(),
            "ENTANGLEMENT_PROVIDER=anthropic requires ANTHROPIC_API_KEY",
        ),
        Some(other) => {
            eprintln!(
                "skutter: unknown ENTANGLEMENT_PROVIDER='{other}' (expected: zai|openai|ollama|anthropic)"
            );
            std::process::exit(2);
        }
        None => {
            if let Some(c) = zai_config() {
                return c;
            }
            if let Some(c) = openai_config() {
                return c;
            }
            if let Some(c) = anthropic_config() {
                return c;
            }
            eprintln!(
                "skutter: no provider key set — using DummyLlm \
                 (set ENTANGLEMENT_PROVIDER=ollama for local, or a *_API_KEY)"
            );
            EngineConfig::default()
        }
    }
}

fn env_nonempty(name: &str) -> Option<String> {
    match std::env::var(name) {
        Ok(v) if !v.is_empty() => Some(v),
        _ => None,
    }
}

fn require(cfg: Option<EngineConfig>, msg: &str) -> EngineConfig {
    match cfg {
        Some(c) => c,
        None => {
            eprintln!("skutter: {msg}");
            std::process::exit(2);
        }
    }
}

fn zai_config() -> Option<EngineConfig> {
    let key = env_nonempty("ZAI_API_KEY")?;
    let model = std::env::var("ZAI_MODEL").unwrap_or_else(|_| DEFAULT_ZAI_MODEL.to_string());
    let base = std::env::var("ZAI_API_BASE")
        .unwrap_or_else(|_| entanglement_llm::ZAI_CODING_PLAN_BASE.to_string());
    eprintln!("skutter: provider=zai model={model} base={base}");
    Some(EngineConfig {
        llm_factory: entanglement_llm::openai_factory(base, Some(key), model),
        ..EngineConfig::default()
    })
}

fn openai_config() -> Option<EngineConfig> {
    let key = env_nonempty("OPENAI_API_KEY")?;
    let model = std::env::var("OPENAI_MODEL").unwrap_or_else(|_| DEFAULT_OPENAI_MODEL.to_string());
    let base = std::env::var("OPENAI_API_BASE")
        .unwrap_or_else(|_| entanglement_llm::OPENAI_BASE.to_string());
    eprintln!("skutter: provider=openai model={model} base={base}");
    Some(EngineConfig {
        llm_factory: entanglement_llm::openai_factory(base, Some(key), model),
        ..EngineConfig::default()
    })
}

fn ollama_config() -> EngineConfig {
    let model = std::env::var("OLLAMA_MODEL").unwrap_or_else(|_| DEFAULT_OLLAMA_MODEL.to_string());
    let base =
        std::env::var("OLLAMA_BASE").unwrap_or_else(|_| entanglement_llm::OLLAMA_BASE.to_string());
    eprintln!("skutter: provider=ollama model={model} base={base}");
    EngineConfig {
        llm_factory: entanglement_llm::openai_factory(base, None, model),
        ..EngineConfig::default()
    }
}

fn anthropic_config() -> Option<EngineConfig> {
    let key = env_nonempty("ANTHROPIC_API_KEY")?;
    let model =
        std::env::var("ANTHROPIC_MODEL").unwrap_or_else(|_| DEFAULT_ANTHROPIC_MODEL.to_string());
    eprintln!("skutter: provider=anthropic model={model}");
    Some(EngineConfig {
        llm_factory: entanglement_llm::anthropic_factory(key, model),
        ..EngineConfig::default()
    })
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

    let holly = Holly::spawn(build_config());

    match cli.cmd {
        Some(Cmd::Run {
            prompt,
            session,
            agent,
            format,
        }) => {
            let prompt = prompt.join(" ");
            run_one(
                &holly,
                &SessionId::new(session),
                agent.as_deref(),
                &prompt,
                &format,
            )
            .await
        }
        Some(Cmd::Pipe { session }) => pipe(&holly, &SessionId::new(session)).await,
        None => {
            let prompt = cli.prompt.join(" ");
            run_one(&holly, &SessionId::new("run"), None, &prompt, "text").await
        }
    }
}

/// Send one prompt and stream events until `Done` (or timeout).
async fn run_one(
    holly: &Holly,
    session: &SessionId,
    agent: Option<&str>,
    prompt: &str,
    format: &str,
) -> Result<()> {
    let json = format == "json";
    let mut sub = holly.subscribe();

    if let Some(a) = agent {
        holly
            .send(InMsg::SetAgent {
                session: session.clone(),
                agent: a.to_string(),
            })
            .await?;
    }
    holly
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
async fn pipe(holly: &Holly, default_session: &SessionId) -> Result<()> {
    let mut sub = holly.subscribe();
    let (done_tx, mut done_rx) = tokio::sync::oneshot::channel::<()>();

    // stdin reader → InMsg
    let holly2 = holly.clone();
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
                    if holly2.send(msg).await.is_err() {
                        break;
                    }
                }
                Err(e) => {
                    let _ = holly2
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
