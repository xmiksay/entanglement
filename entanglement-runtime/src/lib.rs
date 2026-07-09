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
//! - `cli` — head plumbing (clap arg parsing, log init, LLM providers) without
//!   the render stack; room for a future lean stdio-only build.
//! - `tui` — the terminal UI head; implies `cli`.
//!
//! With `--no-default-features` the crate is a **lean library**: the modules
//! below import only `entanglement-core` + tokio + serde/anyhow/tracing +
//! `glob`/`regex`/`dirs`, so a consumer can reuse the tool-execution loop,
//! permission dispatch, sub-agent spawn, and event-sourced persistence without
//! compiling any CLI/TUI/transport dependency. `make check-lean` enforces this.

pub mod ask_user;
pub mod host;
pub mod permission;
pub mod persistence;
pub mod session_store;
pub mod subagent;
pub mod tool_runner;
