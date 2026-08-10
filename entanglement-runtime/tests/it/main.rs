//! Single integration-test harness (ADR-0180): every former per-file test
//! binary is a module here, so the crate + deps compile and link once instead
//! of 33 times. Run one module via `cargo test -p entanglement-runtime --test it <mod>`.
//! `tests/rhai.rs` stays its own binary for its `required-features` gate.

use std::sync::{Mutex, MutexGuard, PoisonError};

/// One process-wide env-var lock. Tests from *different* former binaries now
/// share an address space, so the per-file `ENV_LOCK`s merged into this one:
/// every test that mutates process env must hold it until the matching
/// `remove_var`. Poison-tolerant — a panicking test must not cascade.
static ENV_LOCK: Mutex<()> = Mutex::new(());

pub fn env_lock() -> MutexGuard<'static, ()> {
    ENV_LOCK.lock().unwrap_or_else(PoisonError::into_inner)
}

mod agent_definitions;
mod agent_generation;
mod agent_models;
mod agent_send;
mod apply_patch;
mod ask_user;
mod aux_models;
mod compact_fork;
mod hooks;
mod host_tools;
mod list_operations;
mod load_skill;
#[cfg(all(feature = "mcp-http", feature = "serve"))]
mod mcp_http;
mod permission_dispatch;
mod plan_tasks;
mod plan_watch;
mod policy_seam;
mod propose_plan;
mod provider_selection;
mod record_sink;
mod reoffer_dedupe;
mod replay_from;
#[cfg(feature = "serve")]
mod serve;
#[cfg(feature = "serve")]
mod serve_auth;
#[cfg(feature = "provider")]
mod session_title;
mod skill_mask;
mod spawn_prompt_persistence;
mod stop_abort;
mod stop_abort_inflight_cleanup;
mod subagent_spawn;
mod system_prompt_assembly;
mod tool_mask;
mod user_config;
