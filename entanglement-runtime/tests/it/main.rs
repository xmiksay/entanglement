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

// A single-mode `ModeTable` (ADR-0207 stage 4) most test modules in this
// harness wire in for `ModeResolver`: they exercise something *other*
// than permission grading (spawn plumbing, MCP lazy re-enable, invoke-
// envelope unwrapping, skill posture, ...) and just want every call to run
// unprompted, mirroring the pre-ADR-0207 `build` agent's `default: allow`.
// Tests that exercise mode grading itself (`tool_mask`, `permission_dispatch`,
// `policy_seam`) build their own table instead.
mod mode_support;

mod advertising_pin;
mod agent_definitions;
mod agent_generation;
mod agent_invariant_tools;
mod agent_model;
mod agent_models;
mod agent_send;
mod alias_grading;
mod apply_patch;
mod arg_validate;
mod ask_user;
mod aux_models;
mod compact_fork;
#[cfg(feature = "serve")]
mod endpoint;
mod hooks;
mod host_tools;
mod invoke_envelope;
mod list_operations;
mod load_skill;
#[cfg(all(feature = "mcp-http", feature = "serve"))]
mod mcp_http;
#[cfg(all(feature = "mcp-http", feature = "serve"))]
mod mcp_lazy_reenable;
#[cfg(feature = "mcp-http")]
mod mcp_oauth_device;
#[cfg(feature = "mcp-http")]
mod mcp_oauth_refresh;
#[cfg(all(feature = "mcp-http", feature = "serve"))]
mod mcp_scoped;
#[cfg(feature = "provider")]
mod narrate;
mod parked_prompt_persistence;
mod permission_dispatch;
mod plan_tasks;
mod plan_watch;
mod policy_seam;
mod propose_plan;
mod provider_selection;
mod record_sink;
mod reoffer_dedupe;
mod replay_from;
mod request_mode;
#[cfg(feature = "serve")]
mod serve;
#[cfg(feature = "provider")]
mod session_title;
mod skill_posture;
mod spawn_prompt_persistence;
mod stop_abort;
mod stop_abort_inflight_cleanup;
mod subagent_spawn;
mod system_prompt_assembly;
mod tool_mask;
mod unattended_mode;
mod user_config;
