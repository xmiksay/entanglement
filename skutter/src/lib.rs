//! `skutter` — the coding-agent CLI over the entanglement engine.
//!
//! The binary (`src/main.rs`) holds the heads (`run`/`pipe`/`tui`) and the
//! startup wiring. This library carries the pieces its integration tests
//! drive directly: the host tools, the WebSocket `serve` head, user slash
//! commands, the engine-less `inspect` views, the layered config and managed
//! files, the definitions watcher and the log setup. Everything below is built on
//! `entanglement-runtime`'s public API only.

// The moved modules name runtime modules as `crate::<name>`, exactly as they
// did inside `entanglement-runtime`.
use entanglement_runtime::{
    attendance, capability, discover, env_date, exec, exec_scope, file_change, jobs, mode, output,
    path_grading, permission, policy, rhai_trust, script_ops, secret_env, tool_names, tool_runner,
    tools,
};

// File-based agent and skill definitions (#932): the layered dirs, the
// built-in agents, the frontmatter parser and the system-prompt assembly
// that bakes each agent's prompt at load.
pub mod agents;
mod frontmatter;
pub mod layers;
pub mod skills;
pub mod system_prompt;

// Per-purpose aux models and the session-title / action-narrator tasks
// that ride them (#932).
pub mod aux_llm;
pub mod narrate;
pub mod session_title;
// The on-disk session store, its file-backed persistence sink, the
// `ReplayFrom` answerer over it and the per-write backups beside it (#932).
pub mod file_backup;
pub mod history;
pub mod persistence;
pub mod session_store;
// The layered `config.yml`, the managed state files' paths and the key
// writer (#967): read by `skutter`, never by the library.
pub mod config;
// Lifecycle hooks (#199, #964): run by `skutter`, never by the library.
pub mod hooks;
// The host tools (#968): file, search, shell and fetch tools, their root
// containment and the bubblewrap sandbox.
pub mod host;
pub mod inspect;
// The global MCP roster, live add/remove and the OAuth flows over the
// library's MCP layer (#932).
pub mod logging;
pub mod mcp;
pub mod serve;
// User-defined slash commands — layered prompt templates (#733): pure
// discovery + text expansion, consumed by the TUI and the `run` head.
pub mod slash_commands;
// The file-backed stores behind the library's in-memory seams (#966).
pub mod state;
// The compile-time tool vocabulary for engine-free views and config checks.
pub mod tool_vocab;
mod user_dirs;
// The definitions watcher: live reload of what `skutter` loads from disk.
pub mod watch;

/// A pool client with cross-process endpoint state disabled (#743): a unit
/// test must never read or write the developer's real endpoint-state dir.
#[cfg(test)]
pub(crate) fn test_http_client() -> entanglement_core::HttpClient {
    entanglement_core::HttpClient::with_shared_state(
        entanglement_core::SharedStateLocation::Disabled,
        entanglement_core::RetryConfig::default(),
    )
    .expect("reqwest client builds in tests")
}
