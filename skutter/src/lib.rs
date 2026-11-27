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
    agents, attendance, capability, discover, exec, file_backup, file_change, frontmatter, jobs,
    layers, mcp, mode, output, path_grading, permission, policy, rhai_trust, secret_env,
    session_store, skills, system_prompt, tool_names, tool_runner, tools,
};

// Per-purpose aux models and the session-title / action-narrator tasks
// that ride them (#932).
pub mod aux_llm;
pub mod narrate;
pub mod session_title;
// The layered `config.yml`, the managed state files' paths and the key
// writer (#967): read by `skutter`, never by the library.
pub mod config;
// Lifecycle hooks (#199, #964): run by `skutter`, never by the library.
pub mod hooks;
// The host tools (#968): file, search, shell and fetch tools, their root
// containment and the bubblewrap sandbox.
pub mod host;
pub mod inspect;
pub mod logging;
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
