//! `skutter` — the coding-agent CLI over the entanglement engine.
//!
//! The binary (`src/main.rs`) holds the heads (`run`/`pipe`/`tui`) and the
//! startup wiring. This library carries the pieces its integration tests
//! drive directly: the WebSocket `serve` head, user slash commands, the
//! engine-less `inspect` views and the log setup. Everything below is built on
//! `entanglement-runtime`'s public API only.

// The moved modules name runtime modules as `crate::<name>`, exactly as they
// did inside `entanglement-runtime`.
use entanglement_runtime::{
    agents, attendance, capability, config, discover, frontmatter, hooks, layers, mcp, mode,
    session_store, skills, system_prompt, tool_names,
};

pub mod inspect;
pub mod logging;
pub mod serve;
// User-defined slash commands — layered prompt templates (#733): pure
// discovery + text expansion, consumed by the TUI and the `run` head.
pub mod slash_commands;
