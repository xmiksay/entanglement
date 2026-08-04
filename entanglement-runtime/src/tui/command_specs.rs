//! The clap-derived arg structs for every slash command that takes arguments
//! (Issue 2). Split out of [`super::command_args`] when that file crossed the
//! 400-line cap; the parsing entry points, tokenization, and `help_text`
//! rendering stay there and re-export these, so every existing
//! `command_args::…Args` import keeps working.
//!
//! Each struct is a pure **parser + help generator** — never wired into the
//! binary's own clap CLI in `main.rs`. **Core hygiene:** `clap` stays in
//! `entanglement-runtime` (`tui/`), never `entanglement-core` (`make tree`).

use clap::Parser;

/// `/compact [--keep N] [instructions…]` (#397, ADR-0102). `--keep` preserves the
/// trailing `N` messages verbatim before summarizing the rest; `instructions`
/// is free text passed through to the summarizer. A bare `/compact` summarizes
/// the whole history (`kept = 0`, no instructions).
#[derive(Parser, Debug)]
#[command(name = "/compact", disable_help_flag = false)]
pub struct CompactArgs {
    /// Number of trailing messages to preserve verbatim (default 0).
    #[arg(long)]
    pub keep: Option<u64>,
    /// Free-text instructions for the summarizer.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub instructions: Vec<String>,
}

/// The recognised `/set` keys (Issue 2): each maps onto one field of
/// [`entanglement_provider::GenerationParams`]. Clap's `ValueEnum` kebab-cases
/// variant names by default; explicit `name`s + aliases accept every spelling
/// the pre-Issue-2 parser did: `thinking_budget` == `thinking_budget_tokens`,
/// `max_tokens` == `max_output_tokens` (both the underscore form, which clap's
/// kebab-casing would otherwise reject).
#[derive(clap::ValueEnum, Clone, Debug, PartialEq)]
pub enum SetKey {
    Temperature,
    Effort,
    #[value(name = "thinking_budget", alias = "thinking_budget_tokens")]
    ThinkingBudget,
    #[value(name = "max_tokens", alias = "max_output_tokens")]
    MaxTokens,
}

/// `/set <key> <value>` (#376): one partial generation-override. The `value` is
/// kept as a raw string here — the conversion to the field's typed value (f32 /
/// u32 / `ReasoningEffort`) happens in [`super::commands::parse_set_args`], which
/// preserves the original friendly per-key error messages.
#[derive(Parser, Debug)]
#[command(name = "/set")]
pub struct SetArgs {
    pub key: SetKey,
    pub value: String,
}

/// `/mcp add`'s streamable-HTTP header (`--header KEY:VALUE`, repeatable).
#[derive(Parser, Debug)]
#[command(name = "header")]
pub struct HeaderArgs {
    #[arg(long)]
    pub header: Vec<String>,
}

/// `/mcp add <name> -- <command> [args…]` (stdio) **or**
/// `/mcp add <name> --url <url> [--header KEY:VALUE]…` (streamable HTTP).
/// One of `--`/`--url` must be present; clap's `required_unless_present` keeps
/// the two forms mutually exclusive without a custom validator.
#[derive(Parser, Debug, PartialEq)]
#[command(name = "/mcp add")]
pub struct McpAddArgs {
    /// Server name (the `mcp__<name>__*` tool-namespace prefix).
    pub name: String,
    /// `--url <url>` selects the streamable-HTTP transport.
    #[arg(long)]
    pub url: Option<String>,
    /// Remaining tokens after `--` are the stdio command + its args.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub command: Vec<String>,
}

/// `/mcp` subcommands (#373). A bare `/mcp` defaults to `list`, handled by the
/// caller before reaching clap (clap requires a subcommand).
#[derive(clap::Subcommand, Debug, PartialEq)]
pub enum McpSub {
    /// List configured MCP servers (the default).
    List,
    /// Add a server (stdio via `--` or HTTP via `--url`).
    Add(McpAddArgs),
    /// Remove a server by name.
    Remove { name: String },
    /// Authorize a server over OAuth in the browser (ADR-0153).
    Connect { name: String },
    /// Report (and refresh if needed) a server's stored credential.
    Check { name: String },
    /// Revoke and drop a server's stored credential.
    Disconnect { name: String },
}

#[derive(Parser, Debug)]
#[command(name = "/mcp")]
pub struct McpArgs {
    #[command(subcommand)]
    pub cmd: Option<McpSub>,
}

/// `/allow <path>` (#486, ADR-0126): a single path, normalized root-relative by
/// the caller.
#[derive(Parser, Debug)]
#[command(name = "/allow")]
pub struct AllowArgs {
    pub path: String,
}

/// `/enable`/`/disable` subcommand target (#539, ADR-0149): an MCP server
/// (`mcp <server>` ⇒ `mcp__<server>__*`) or a bare tool name / glob
/// (`tool <name>`).
#[derive(clap::Subcommand, Clone, Debug, PartialEq)]
pub enum EnableTarget {
    /// Enable/disable every tool from an MCP server.
    Mcp { server: String },
    /// Enable/disable a single tool name or glob pattern.
    Tool { name: String },
}

/// `/enable mcp <server> | tool <name> [--allow [<pattern>]]` (bare `/enable`
/// opens the session-tools dialog; `--allow` is enable-only). `--allow` is
/// `global` so it parses after the `mcp`/`tool` subcommand
/// (`/enable tool bash --allow`). The optional trailing `<pattern>` (ADR-0163,
/// #611 — folds live bash enablement's `--allow [<pattern>]` into this one
/// command surface) is a free-form, possibly multi-word command-argument glob
/// (`git *`) that doesn't map onto clap's single-token option-value model, so
/// [`super::command_args::parse_enable_via_clap`] splits it off and rejoins it
/// verbatim *before* this struct ever sees it — `allow` here only ever
/// resolves to whether the bare flag was present.
#[derive(Parser, Debug)]
#[command(name = "/enable")]
pub struct EnableArgs {
    #[command(subcommand)]
    pub target: Option<EnableTarget>,
    /// Only meaningful for `/enable` (ignored by `/disable`): grade the entry as
    /// `allow` instead of the default `ask`.
    #[arg(long, global = true)]
    pub allow: bool,
}

/// `/disable mcp <server> | tool <name>` (bare `/disable` clears the overlay).
#[derive(Parser, Debug)]
#[command(name = "/disable")]
pub struct DisableArgs {
    #[command(subcommand)]
    pub target: Option<EnableTarget>,
}

/// `/stop`, `/pause`, `/continue` shared `--all`/`-a` flag (#6). `Continue` is
/// the resume-a-paused-session command (named `Continue`, not `Resume`).
#[derive(Parser, Debug)]
#[command(name = "/lifecycle")]
pub struct LifecycleArgs {
    /// Fan out to every live session (default: active session only).
    #[arg(long, short)]
    pub all: bool,
}
