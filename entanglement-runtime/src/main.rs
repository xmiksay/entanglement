//! `skutter` — stdio head for the headless agent engine.
//!
//! Two modes, both driving [`entanglement_core::Holly`] directly (the ABI):
//! - `run` sends a prompt and streams events until `Done`. `--format json`
//!   emits raw NDJSON (like `opencode run --format json`); `--format text`
//!   renders human-friendly output.
//! - `pipe` is a bidirectional NDJSON relay: `InMsg` lines on stdin,
//!   `OutEvent` lines on stdout. For scripting / editor integration.

mod agent_poll;
mod ask_user;
mod host;
mod permission;
mod persistence;
mod pipe;
mod run;
mod session_store;
mod subagent;
mod tool_runner;
mod tui;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use entanglement_core::{EngineConfig, Holly, InMsg, OutEvent, SessionId, ToolRegistry};
use entanglement_provider::{models_for, HttpClient, ModelInfo};

use host::{host_tools, BashTool};
use pipe::pipe;
use run::run_one;
use session_store::{read, LogPayload};
use tui::tui;

/// Provider name for model selection.
const PROVIDER_ZAI: &str = "zai";
const PROVIDER_OPENAI: &str = "openai";
const PROVIDER_OLLAMA: &str = "ollama";
const PROVIDER_ANTHROPIC: &str = "anthropic";

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
/// OpenAI-compatible client ([`entanglement_provider::openai_factory`]); Anthropic
/// has its own client.
///
/// The root-contained host quartet (`read`/`glob`/`grep`/`edit`) is always
/// registered, rooted at the current working directory, so the
/// `build`/`plan`/`explore` permission profiles gate something real out of the
/// box. `bash` is opt-in: set `ENTANGLEMENT_ENABLE_BASH=1` to register
/// `BashTool` — it runs unsandboxed with the engine's full privileges
/// (ADR-0009 / ADR-0010).
///
/// Core no longer executes tools (#58): it only advertises their schemas
/// (`cfg.tool_specs`). The returned [`ToolRegistry`] stays in the runtime and
/// is handed to [`tool_runner::spawn_tool_executor`], which answers the
/// [`entanglement_core::OutEvent::ToolExec`] round-trip.
fn build_config(http_client: &HttpClient) -> (EngineConfig, ModelInfo, ToolRegistry) {
    let (mut cfg, model_info) = select_provider(http_client);
    let root = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let mut tools = host_tools(root.clone());
    if std::env::var("ENTANGLEMENT_ENABLE_BASH").as_deref() == Ok("1") {
        tools.register(BashTool::new(root.clone()));
        eprintln!(
            "skutter: bash enabled (ENTANGLEMENT_ENABLE_BASH=1) — \
             runs unsandboxed with full privileges"
        );
    }
    cfg.tool_specs = tools.specs();
    // `spawn_agent` is orchestration, not a registry tool (#60): the runtime
    // executor handles it directly, so it only needs advertising to the model.
    cfg.tool_specs.push(subagent::spawn_agent_spec());
    // `agent_poll` is the join half of non-blocking spawn (#89): it awaits a
    // launched sub-agent's answer. Runtime-owned like `spawn_agent`.
    cfg.tool_specs.push(agent_poll::agent_poll_spec());
    // `ask_user` is likewise runtime-owned (#90): the executor intercepts it to
    // surface a decision prompt to the head, not a host-tool call.
    cfg.tool_specs.push(ask_user::ask_user_spec());
    (cfg, model_info, tools)
}

fn select_provider(http_client: &HttpClient) -> (EngineConfig, ModelInfo) {
    match std::env::var("ENTANGLEMENT_PROVIDER").ok().as_deref() {
        Some("zai") => {
            let (cfg, info) =
                zai_config(http_client).expect("ENTANGLEMENT_PROVIDER=zai requires ZAI_API_KEY");
            (cfg, info)
        }
        Some("openai") => {
            let (cfg, info) = openai_config(http_client)
                .expect("ENTANGLEMENT_PROVIDER=openai requires OPENAI_API_KEY");
            (cfg, info)
        }
        Some("ollama") => ollama_config(http_client),
        Some("anthropic") => {
            let (cfg, info) = anthropic_config(http_client)
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
            if let Some((c, info)) = zai_config(http_client) {
                return (c, info);
            }
            if let Some((c, info)) = openai_config(http_client) {
                return (c, info);
            }
            if let Some((c, info)) = anthropic_config(http_client) {
                return (c, info);
            }
            eprintln!(
                "skutter: no provider key set — using EchoLlm \
                 (set ENTANGLEMENT_PROVIDER=ollama for local, or a *_API_KEY, or echo)"
            );
            (
                EngineConfig::default(),
                ModelInfo {
                    id: "echo".to_string(),
                    display_name: "Echo (debug)".to_string(),
                    context_window: None,
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

fn zai_config(http_client: &HttpClient) -> Option<(EngineConfig, ModelInfo)> {
    let key = env_nonempty("ZAI_API_KEY")?;
    let model = std::env::var("ZAI_MODEL").unwrap_or_else(|_| DEFAULT_ZAI_MODEL.to_string());
    let base = std::env::var("ZAI_API_BASE")
        .unwrap_or_else(|_| entanglement_provider::ZAI_CODING_PLAN_BASE.to_string());
    eprintln!("skutter: provider=zai model={model} base={base}");
    Some((
        EngineConfig {
            llm_factory: entanglement_provider::openai_factory(
                base,
                Some(key),
                model.clone(),
                http_client.clone(),
            ),
            ..EngineConfig::default()
        },
        ModelInfo {
            id: model.clone(),
            display_name: model.clone(),
            context_window: models_for(PROVIDER_ZAI)
                .into_iter()
                .find(|m| m.id == model)
                .and_then(|m| m.context_window),
        },
    ))
}

fn openai_config(http_client: &HttpClient) -> Option<(EngineConfig, ModelInfo)> {
    let key = env_nonempty("OPENAI_API_KEY")?;
    let model = std::env::var("OPENAI_MODEL").unwrap_or_else(|_| DEFAULT_OPENAI_MODEL.to_string());
    let base = std::env::var("OPENAI_API_BASE")
        .unwrap_or_else(|_| entanglement_provider::OPENAI_BASE.to_string());
    eprintln!("skutter: provider=openai model={model} base={base}");
    Some((
        EngineConfig {
            llm_factory: entanglement_provider::openai_factory(
                base,
                Some(key),
                model.clone(),
                http_client.clone(),
            ),
            ..EngineConfig::default()
        },
        ModelInfo {
            id: model.clone(),
            display_name: model.clone(),
            context_window: models_for(PROVIDER_OPENAI)
                .into_iter()
                .find(|m| m.id == model)
                .and_then(|m| m.context_window),
        },
    ))
}

fn ollama_config(http_client: &HttpClient) -> (EngineConfig, ModelInfo) {
    let model = std::env::var("OLLAMA_MODEL").unwrap_or_else(|_| DEFAULT_OLLAMA_MODEL.to_string());
    let base = std::env::var("OLLAMA_BASE")
        .unwrap_or_else(|_| entanglement_provider::OLLAMA_BASE.to_string());
    eprintln!("skutter: provider=ollama model={model} base={base}");
    (
        EngineConfig {
            llm_factory: entanglement_provider::openai_factory(
                base,
                None,
                model.clone(),
                http_client.clone(),
            ),
            ..EngineConfig::default()
        },
        ModelInfo {
            id: model.clone(),
            display_name: model.clone(),
            context_window: models_for(PROVIDER_OLLAMA)
                .into_iter()
                .find(|m| m.id == model)
                .and_then(|m| m.context_window),
        },
    )
}

fn anthropic_config(http_client: &HttpClient) -> Option<(EngineConfig, ModelInfo)> {
    let key = env_nonempty("ANTHROPIC_API_KEY")?;
    let model =
        std::env::var("ANTHROPIC_MODEL").unwrap_or_else(|_| DEFAULT_ANTHROPIC_MODEL.to_string());
    eprintln!("skutter: provider=anthropic model={model}");
    Some((
        EngineConfig {
            llm_factory: entanglement_provider::anthropic_factory(
                key,
                model.clone(),
                http_client.clone(),
            ),
            ..EngineConfig::default()
        },
        ModelInfo {
            id: model.clone(),
            display_name: model.clone(),
            context_window: models_for(PROVIDER_ANTHROPIC)
                .into_iter()
                .find(|m| m.id == model)
                .and_then(|m| m.context_window),
        },
    ))
}

fn echo_config() -> (EngineConfig, ModelInfo) {
    eprintln!("skutter: provider=echo (history-debugging stub)");
    (
        EngineConfig::default(),
        ModelInfo {
            id: "echo".to_string(),
            display_name: "Echo (debug)".to_string(),
            context_window: None,
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
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let filter = if cli.verbose { "debug" } else { "warn" };
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let http_client = HttpClient::new();
    let (config, model_info, tools) = build_config(&http_client);
    // The runtime keeps its own copy of the profile registry to resolve
    // permissions (#59); the engine gets the same shape via `config`.
    let profiles = config.profiles.clone();
    let holly = Holly::spawn(config);
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));

    // Runtime owns tool execution (#58) and permission dispatch + approval (#59):
    // answer the engine's ToolExec round-trip, gating each call on `profiles`.
    let _tool_executor = tool_runner::spawn_tool_executor(&holly, tools, profiles);

    // Spawn the persistence subscriber to log all events
    let _persistence_handle = persistence::spawn_persistence_subscriber(&holly, cwd.clone());

    match cli.cmd {
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

                let mut paired_records: Vec<(Option<InMsg>, OutEvent)> = Vec::new();
                let mut last_in: Option<InMsg> = None;

                for record in &records {
                    match &record.payload {
                        LogPayload::In(in_msg) => {
                            last_in = Some(in_msg.clone());
                        }
                        LogPayload::Out(out_event) => {
                            paired_records.push((last_in.clone(), out_event.clone()));
                            last_in = None;
                        }
                    }
                }

                holly.resume(session_id.clone(), paired_records).await?;
            }

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
            run_one(&holly, &SessionId::new_uuid(), None, &prompt, "text").await
        }
    }
}
