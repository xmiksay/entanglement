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
//! - `default = ["tui", "serve"]` — the full `skutter` binary (stdio `run`/`pipe`
//!   plus the terminal UI and the local WebSocket server), pulling clap, the LLM
//!   providers (reqwest), the render stack (ratatui, syntect, …), and axum.
//! - `cli` — head plumbing: clap arg parsing + log init (tracing-subscriber).
//! - `provider` — the LLM providers (reqwest via `entanglement-provider`), split
//!   from `cli` (#208) so the `serve`/`ws` head pulls providers without dragging
//!   in clap.
//! - `tui` — the terminal UI head; implies `cli` + `provider`.
//! - `serve` — the local WebSocket `serve` head (axum, #153); implies
//!   `cli` + `provider`. Keeps axum out of the lean library (ADR-0025/ADR-0048).
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
// Live bash enablement (#498, ADR-0133): register `bash`/`bash_output` in a
// running process, graded by a `BashGrade` — mirrors the MCP `SharedRegistry`
// live-management seam (#372/#375). Lean-library-safe: only entanglement-core
// + std + the already-unconditional `host` module.
pub mod bash_live;
pub mod cancel;
pub mod config;
pub mod extra_roots;
pub mod file_change;
pub mod frontmatter;
pub mod grants;
pub mod history;
pub mod hooks;
pub mod host;
pub mod inspect;
pub mod layers;
// MCP client — attach external tool servers as a runtime-side tool provider
// (#198, #312). The stdio transport lives in the lean library (tokio process +
// serde_json only), so an embedder gets external tools without any
// CLI/TUI/transport dep; the streamable-HTTP transport rides the `mcp-http`
// feature (reqwest), keeping the lean build transport-free (ADR-0025).
pub mod mcp;
pub mod pending;
pub mod permission;
pub mod permission_path;
pub mod persistence;
pub mod plan_tasks;
pub mod policy;
pub mod propose_plan;
pub mod script;
pub mod seam;
// WebSocket `serve` head (#153, ADR-0048). Behind the `serve` feature so axum
// stays out of the lean library and `--no-default-features` builds (ADR-0025).
#[cfg(feature = "serve")]
pub mod serve;
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
// inotify-backed watcher for definition dirs + managed files (#329):
// live-reloads the runtime's own profile/skill registry mirrors — never
// core's `EngineConfig`, which stays pinned for the process lifetime.
pub mod watch;

pub use tools::{SharedRegistry, Tool, ToolRegistry};

// Tracing-subscriber setup is head plumbing, so it rides the `cli` feature and
// stays out of the lean library (tracing-subscriber is on the `check-lean`
// blocklist). The bin only ever builds with `cli`, so `logging` is always
// available to it.
#[cfg(feature = "cli")]
pub mod logging;
