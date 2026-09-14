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

pub mod agent_registry;
pub mod agent_send;
pub mod agents;
// Pre-dispatch argument validation against a tool's advertised `ToolSpec`
// schema (#560, ADR-0196 §6): the three-way error taxonomy (schema
// violation / parameter error / command failure) plus the delivered-schema
// and loop-breaker guards.
pub mod arg_validate;
pub mod ask_user;
pub mod aux_llm;
pub mod cancel;
pub mod config;
mod date;
// Attributed autodecline wording for a call the dispatch gate refuses — the
// one table both the executor ladder and the mask walk render from.
pub mod decline;
// `explore`/`describe` — the ADR-0196 §4 discovery pair (#560): always-on,
// non-maskable internal tools that let a `ToolSearch`-mode session reach the
// rest of the registry. Ungated — pure state/logic over core + the lean
// `mcp` module, needed by the lean build's executor too.
pub mod discover;
pub mod endpoint;
pub mod env_date;
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
// Live action narrator (#635): asks the aux `narrate` LLM what the agent is
// doing on every tool call and sets it as `Session.action`. Mirrors
// `session_title` below, including the `provider` gate (drains a provider
// `LlmStream`).
#[cfg(feature = "provider")]
pub mod narrate;
// Pending-operations listing (#607, ADR-0161 §6): shared by `poll`'s
// no-handle model surface and `InMsg::ListOperations`'s head surface.
pub mod operations;
pub mod pending;
pub mod permission;
pub mod permission_path;
pub mod persistence;
pub mod plan_files;
pub mod plan_tasks;
pub mod plan_watch;
pub mod policy;
// The `poll` join tool (#605, ADR-0161 §1-4) — replaces `bash_output`/
// `agent_poll` outright.
pub mod poll;
pub mod propose_plan;
pub mod questions;
pub mod retained_output;
// Sandboxed `rhai` script tool (#122, ADR-0046). Behind the `rhai` feature
// (default-on, #502/ADR-0135) so a lean embedder can drop the dep via
// `--no-default-features`.
#[cfg(feature = "rhai")]
pub mod script;
// Background-script registry (#637, ADR-0185): shared by `rhai`'s
// `background: true` launcher and `poll`'s `x-` handle path. Ungated — it is
// plain state with no rhai dep, so `poll`/`operations` reference it
// unconditionally (empty forever in a lean build without the `rhai` feature).
pub mod script_ops;
pub mod seam;
// WebSocket `serve` head (#153, ADR-0048). Behind the `serve` feature so axum
// stays out of the lean library and `--no-default-features` builds (ADR-0025).
#[cfg(feature = "serve")]
pub mod serve;
pub mod session_store;
#[cfg(feature = "provider")]
pub mod session_title;
pub mod skills;
pub mod subagent;
pub mod system_prompt;
// Composes the ADR-0196 §5 `ToolSearch`-mode prompt slimming with the
// existing env-date freshness patch into the one `SystemPromptResolver`
// slot `EngineConfig` exposes. Ungated — pure string transforms over core
// types, needed by the lean build's `Config`-driven mode too.
pub mod system_prompt_mode;
// Wire-visible LLM-endpoint throttle transitions (#517, ADR-0141). Behind
// `provider` since it polls `entanglement_provider::HttpClient` directly.
#[cfg(feature = "provider")]
pub mod throttle;
pub mod tool_names;
// Tool-advertising resolution + the per-session mode map (ADR-0196): the
// `full`/`tool_search` knob, its env > config > catalog > default precedence
// chain, and the session→mode pinning the executor folds. Ungated — pure
// state over core types, needed by the lean build's `Config` too.
pub mod tool_advertising;
pub mod tool_runner;
// The three-state (`allowed`/`asks`/`declines`) per-tool posture the profile
// UIs render, now that advertisement no longer varies with the mask.
pub mod tool_state;
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
