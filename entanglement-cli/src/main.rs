//! `skutter` — stdio head for the headless agent engine.
//!
//! Two modes, both driving [`entanglement_core::Holly`] directly (the ABI):
//! - `run` sends a prompt and streams events until `Done`. `--format json`
//!   emits raw NDJSON (like `opencode run --format json`); `--format text`
//!   renders human-friendly output.
//! - `pipe` is a bidirectional NDJSON relay: `InMsg` lines on stdin,
//!   `OutEvent` lines on stdout. For scripting / editor integration.

mod pipe;
mod run;
mod session_store;
mod tui;

use anyhow::Result;
use clap::{Parser, Subcommand};
use entanglement_core::{host_tools, BashTool, EngineConfig, Holly, InMsg, SessionId};

use pipe::pipe;
use run::run_one;
use tui::tui;

#[derive(Debug, Clone)]
pub struct ModelInfo {
    pub provider: String,
    pub model: String,
}

/// Default models per provider when its `<PROVIDER>_MODEL` env is unset.
const DEFAULT_ZAI_MODEL: &str = "glm-5.2";
const DEFAULT_OPENAI_MODEL: &str = "gpt-4o";
const DEFAULT_OLLAMA_MODEL: &str = "llama3.1";
const DEFAULT_ANTHROPIC_MODEL: &str = "claude-sonnet-4-5";

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
/// OpenAI-compatible client ([`entanglement_llm::openai_factory`]); Anthropic
/// has its own client.
///
/// The root-contained host quartet (`read`/`glob`/`grep`/`edit`) is always
/// registered, rooted at the current working directory, so the
/// `build`/`plan`/`explore` permission profiles gate something real out of the
/// box. `bash` is opt-in: set `ENTANGLEMENT_ENABLE_BASH=1` to register
/// `BashTool` — it runs unsandboxed with the engine's full privileges
/// (ADR-0009 / ADR-0010).
fn build_config() -> (EngineConfig, ModelInfo) {
    let (mut cfg, model_info) = select_provider();
    let root = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let mut tools = host_tools(root.clone());
    if std::env::var("ENTANGLEMENT_ENABLE_BASH").as_deref() == Ok("1") {
        tools.register(BashTool::new(root.clone()));
        eprintln!(
            "skutter: bash enabled (ENTANGLEMENT_ENABLE_BASH=1) — \
             runs unsandboxed with full privileges"
        );
    }
    cfg.tools = tools;
    (cfg, model_info)
}

fn select_provider() -> (EngineConfig, ModelInfo) {
    match std::env::var("ENTANGLEMENT_PROVIDER").ok().as_deref() {
        Some("zai") => {
            let (cfg, info) = zai_config().expect("ENTANGLEMENT_PROVIDER=zai requires ZAI_API_KEY");
            (cfg, info)
        }
        Some("openai") => {
            let (cfg, info) =
                openai_config().expect("ENTANGLEMENT_PROVIDER=openai requires OPENAI_API_KEY");
            (cfg, info)
        }
        Some("ollama") => ollama_config(),
        Some("anthropic") => {
            let (cfg, info) = anthropic_config()
                .expect("ENTANGLEMENT_PROVIDER=anthropic requires ANTHROPIC_API_KEY");
            (cfg, info)
        }
        Some("echo") => echo_config(),
        Some(other) => {
            eprintln!(
                "skutter: unknown ENTANGLEMENT_PROVIDER='{other}' (expected: zai|openai|ollama|anthropic|echo)"
            );
            std::process::exit(2);
        }
        None => {
            if let Some((c, info)) = zai_config() {
                return (c, info);
            }
            if let Some((c, info)) = openai_config() {
                return (c, info);
            }
            if let Some((c, info)) = anthropic_config() {
                return (c, info);
            }
            eprintln!(
                "skutter: no provider key set — using EchoLlm \
                 (set ENTANGLEMENT_PROVIDER=ollama for local, or a *_API_KEY, or echo)"
            );
            (
                EngineConfig::default(),
                ModelInfo {
                    provider: "echo".to_string(),
                    model: "echo".to_string(),
                },
            )
        }
    }
}

fn env_nonempty(name: &str) -> Option<String> {
    match std::env::var(name) {
        Ok(v) if !v.is_empty() => Some(v),
        _ => None,
    }
}

fn zai_config() -> Option<(EngineConfig, ModelInfo)> {
    let key = env_nonempty("ZAI_API_KEY")?;
    let model = std::env::var("ZAI_MODEL").unwrap_or_else(|_| DEFAULT_ZAI_MODEL.to_string());
    let base = std::env::var("ZAI_API_BASE")
        .unwrap_or_else(|_| entanglement_llm::ZAI_CODING_PLAN_BASE.to_string());
    eprintln!("skutter: provider=zai model={model} base={base}");
    Some((
        EngineConfig {
            llm_factory: entanglement_llm::openai_factory(base, Some(key), model.clone()),
            ..EngineConfig::default()
        },
        ModelInfo {
            provider: "zai".to_string(),
            model,
        },
    ))
}

fn openai_config() -> Option<(EngineConfig, ModelInfo)> {
    let key = env_nonempty("OPENAI_API_KEY")?;
    let model = std::env::var("OPENAI_MODEL").unwrap_or_else(|_| DEFAULT_OPENAI_MODEL.to_string());
    let base = std::env::var("OPENAI_API_BASE")
        .unwrap_or_else(|_| entanglement_llm::OPENAI_BASE.to_string());
    eprintln!("skutter: provider=openai model={model} base={base}");
    Some((
        EngineConfig {
            llm_factory: entanglement_llm::openai_factory(base, Some(key), model.clone()),
            ..EngineConfig::default()
        },
        ModelInfo {
            provider: "openai".to_string(),
            model,
        },
    ))
}

fn ollama_config() -> (EngineConfig, ModelInfo) {
    let model = std::env::var("OLLAMA_MODEL").unwrap_or_else(|_| DEFAULT_OLLAMA_MODEL.to_string());
    let base =
        std::env::var("OLLAMA_BASE").unwrap_or_else(|_| entanglement_llm::OLLAMA_BASE.to_string());
    eprintln!("skutter: provider=ollama model={model} base={base}");
    (
        EngineConfig {
            llm_factory: entanglement_llm::openai_factory(base, None, model.clone()),
            ..EngineConfig::default()
        },
        ModelInfo {
            provider: "ollama".to_string(),
            model,
        },
    )
}

fn anthropic_config() -> Option<(EngineConfig, ModelInfo)> {
    let key = env_nonempty("ANTHROPIC_API_KEY")?;
    let model =
        std::env::var("ANTHROPIC_MODEL").unwrap_or_else(|_| DEFAULT_ANTHROPIC_MODEL.to_string());
    eprintln!("skutter: provider=anthropic model={model}");
    Some((
        EngineConfig {
            llm_factory: entanglement_llm::anthropic_factory(key, model.clone()),
            ..EngineConfig::default()
        },
        ModelInfo {
            provider: "anthropic".to_string(),
            model,
        },
    ))
}

fn echo_config() -> (EngineConfig, ModelInfo) {
    eprintln!("skutter: provider=echo (history-debugging stub)");
    (
        EngineConfig::default(),
        ModelInfo {
            provider: "echo".to_string(),
            model: "echo".to_string(),
        },
    )
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
    /// Terminal UI mode.
    Tui {
        #[arg(long, default_value = "tui")]
        session: String,
        /// Agent profile to run under (build | plan | explore | custom).
        #[arg(long)]
        agent: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let filter = if cli.verbose { "debug" } else { "warn" };
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let (config, model_info) = build_config();
    let holly = Holly::spawn(config);

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
        Some(Cmd::Tui { session, agent }) => {
            let session_id = SessionId::new(session);
            if let Some(a) = agent {
                holly
                    .send(InMsg::SetAgent {
                        session: session_id.clone(),
                        agent: a.to_string(),
                    })
                    .await?;
            }
            tui(holly, session_id, model_info).await
        }
        None => {
            let prompt = cli.prompt.join(" ");
            run_one(&holly, &SessionId::new("run"), None, &prompt, "text").await
        }
    }
}
