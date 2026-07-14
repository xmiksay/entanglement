//! Runtime crate for the entanglement agent engine.
//!
//! This crate provides the runtime environment and host tool implementations
//! for the headless agent engine defined in `entanglement-core`.
//!
//! # Feature gates (ADR-0025)
//!
//! The binary head (`skutter`) and the reusable library live in the same crate,
//! split by cargo features:
//!
//! - `default = ["tui"]` — the full `skutter` binary (stdio `run`/`pipe` + the
//!   terminal UI), pulling clap, the LLM providers (reqwest), and the render
//!   stack (ratatui, syntect, …).
//! - `cli` — head plumbing: clap arg parsing + log init (tracing-subscriber).
//! - `provider` — the LLM providers (reqwest via `entanglement-provider`), split
//!   from `cli` (#208) so a future `serve`/`ws` head can pull providers without
//!   dragging in clap.
//! - `tui` — the terminal UI head; implies `cli` + `provider`.
//!
//! With `--no-default-features` the crate is a **lean library**: the modules
//! below import only `entanglement-core` + tokio + serde/serde_yaml/anyhow/
//! tracing + `glob`/`regex`/`dirs`, so a consumer can reuse the tool-execution
//! loop, permission dispatch, sub-agent spawn, file-based agent definitions,
//! skill discovery, and event-sourced persistence without compiling any
//! CLI/TUI/transport dependency.
//! `make check-lean` enforces this.

pub mod agent_poll;
pub mod agents;
pub mod ask_user;
pub mod cancel;
pub mod config;
pub mod file_change;
pub mod frontmatter;
pub mod grants;
pub mod hooks;
pub mod host;
pub mod inspect;
pub mod layers;
pub mod permission;
pub mod persistence;
pub mod plan_tasks;
pub mod propose_plan;
pub mod script;
pub mod seam;
pub mod session_store;
pub mod skills;
pub mod subagent;
pub mod system_prompt;
pub mod tool_names;
pub mod tool_runner;
// The host-tool vocabulary (`Tool` trait + `ToolRegistry`) lives here, not in
// core: core holds no executable tools, only advertises schemas and round-trips
// each call back to the runtime (#206, ADR-0006/0010/0053).
pub mod tools;

pub use tools::{Tool, ToolRegistry};

// Tracing-subscriber setup is head plumbing, so it rides the `cli` feature and
// stays out of the lean library (tracing-subscriber is on the `check-lean`
// blocklist). The bin only ever builds with `cli`, so `logging` is always
// available to it.
#[cfg(feature = "cli")]
pub mod logging;
