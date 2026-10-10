//! The `skutter` clap surface: global flags and every subcommand.

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "skutter",
    version,
    about = "Terminal head for the entanglement agent engine (TUI is the default)"
)]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub(crate) cmd: Option<Cmd>,
    /// Positional prompt for the implicit one-shot run. Bare `skutter` with no
    /// prompt *and* no subcommand launches the TUI instead; a prompt runs one
    /// turn (implicit `run`). Use an explicit `run`/`tui`/`pipe` subcommand to
    /// disambiguate.
    pub(crate) prompt: Vec<String>,
    /// Log at `debug` (unless `RUST_LOG` is set, which always wins). Global, so
    /// it may follow the subcommand: `skutter run … --verbose`.
    #[arg(long, global = true)]
    pub(crate) verbose: bool,
    /// Retired (#554, ADR-0207 §11): auto-approving every `ToolRequest` was
    /// head-level, all-or-nothing, and invisible to core/the TUI — precisely
    /// what a permission mode is not. Parsed only so `skutter … --yes` fails
    /// loudly with a pointer to its replacement instead of clap's unknown-flag
    /// error; see the startup check in `main`. Hidden from `--help` so a
    /// retired flag is never advertised.
    #[arg(long, global = true, hide = true)]
    pub(crate) yes: bool,
    /// Run the session under this permission mode (`research`/`plan`/`build`/
    /// `auto`, ADR-0207) instead of the engine's default (`build`). Only
    /// affects `run` heads (explicit `run` and the implicit one-shot form) —
    /// `auto` is the unattended posture `--yes` used to approximate, now with
    /// a bounded posture (unlisted calls refuse, then park a 60s approval —
    /// ADR-0209), run/turn/duration budgets, and no silent widening. Global so it may follow the subcommand:
    /// `skutter run … --mode auto`.
    #[arg(long, global = true)]
    pub(crate) mode: Option<String>,
}

#[derive(Subcommand)]
pub(crate) enum Cmd {
    /// One-shot: send a prompt and stream the turn.
    Run {
        /// Prompt text.
        prompt: Vec<String>,
        /// Session id to use (generates UUID if not specified).
        #[arg(long)]
        session: Option<String>,
        /// Agent profile to run under (general | plan | debug | custom).
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
        /// Agent profile to run under (general | plan | debug | custom).
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
pub(crate) enum ConfigCmd {
    /// Persist a provider's API key to the managed env file (#304). The value
    /// comes from `--key`, a hidden prompt, or piped stdin — never echoed.
    SetKey {
        /// Provider name (catalog key: zai | openai | anthropic | …).
        provider: String,
        /// The key value. Omit to be prompted (hidden) or to read piped stdin.
        #[arg(long)]
        key: Option<String>,
    },
    /// Authorize an OAuth-protected LLM provider (#684): run the browser (or
    /// device-code) flow against a catalog entry carrying an `oauth:` block
    /// and persist the credential to the managed `llm-tokens.yml`.
    Connect {
        /// Provider name (a catalog entry with an `oauth:` block).
        provider: String,
        /// RFC 8628 device-code flow: no browser, no loopback listener —
        /// for SSH/headless hosts.
        #[arg(long)]
        device_code: bool,
    },
    /// Drop an OAuth LLM provider's stored credential (revoking it upstream
    /// when the server supports RFC 7009).
    Disconnect {
        /// Provider name.
        provider: String,
    },
}

#[derive(Subcommand)]
pub(crate) enum InspectCmd {
    /// Print an agent's assembled system prompt (preamble + body + brief + env
    /// + skill index + preloaded skills), exactly as it ships to the model.
    Prompt {
        /// Agent profile to resolve (general | plan | debug | custom).
        #[arg(long)]
        agent: String,
        /// Break the prompt into its component parts, each with its source path.
        #[arg(long)]
        parts: bool,
    },
    /// List resolved agents with their winning layer + provenance (no `name`), or
    /// print one agent's full resolved identity (description, model pin,
    /// prompt length) and what lower layers it overrode. Permissions live on
    /// the session's mode — see `inspect modes`.
    Agents {
        /// Agent to detail (general | plan | debug | custom). Omit for the table.
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
    /// Print the persisted per-purpose auxiliary-LLM pins (#558), i.e. the
    /// `/aux-model` state (`summarize`/`session_title`).
    AuxModels,
    /// Print which MCP servers hold a stored OAuth credential (#558), redacted —
    /// never a token value.
    McpTokens,
    /// Print the resolved bundled/available MCP server roster (#558): which
    /// servers auto-connect at session start vs. wait for `/enable`.
    Mcp,
    /// Print one session's resolved live state folded from its persisted log
    /// (#558) — agent, model, and the per-session tool overlay (ADR-0149),
    /// which has no managed file of its own. Takes a **root** session id (see
    /// `skutter sessions`).
    Session { id: String },
    /// List the four permission modes (no `name`), or print one mode's
    /// resolved rules — built-in shape plus any `config.yml` `modes:` tuning
    /// — plus its limits, sandbox, and per-tool outcome for a known roster
    /// (#560, ADR-0207 stage 6c).
    Modes {
        /// Mode to detail (research | plan | build | auto). Omit for the table.
        name: Option<String>,
    },
}

/// Whether this invocation runs the TUI as its head — either the explicit
/// `skutter tui` subcommand or a bare `skutter` (no subcommand) with no prompt,
/// which falls through to the TUI as the default head. Both hold the terminal in
/// raw mode, so both must route logs to the file sink (see [`crate::logging::init`]).
/// Must stay in sync with the `Cmd::Tui`/`None` dispatch arms in [`crate::heads::dispatch`].
pub(crate) fn launches_tui_head(cmd: &Option<Cmd>, prompt: &[String]) -> bool {
    match cmd {
        Some(Cmd::Tui { .. }) => true,
        None => prompt.is_empty(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::{launches_tui_head, Cmd};

    #[test]
    fn tui_head_covers_bare_skutter_and_explicit_subcommand() {
        // Explicit `skutter tui`.
        assert!(launches_tui_head(
            &Some(Cmd::Tui {
                session: None,
                agent: None,
            }),
            &[],
        ));
        // Bare `skutter` with no prompt → the TUI is the default head (the bug:
        // this used to log to stderr and corrupt the interface).
        assert!(launches_tui_head(&None, &[]));
        // Bare `skutter "do a thing"` → a one-shot `run`, not the TUI.
        assert!(!launches_tui_head(&None, &["hi".to_string()]));
        // Any other subcommand is not the TUI.
        assert!(!launches_tui_head(&Some(Cmd::Sessions), &[]));
    }
}
