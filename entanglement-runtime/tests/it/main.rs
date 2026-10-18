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

/// An empty user definitions layer under `root`: loaders take it instead of
/// the process-global `ENTANGLEMENT_*_DIR` env, so a test never reads the
/// host's real user agents/skills (#773).
pub fn no_user(root: &std::path::Path) -> entanglement_runtime::layers::UserLayer {
    entanglement_runtime::layers::UserLayer::Dir(root.join("no-such-user-dir"))
}

/// Poll `cond` until it holds or 10s pass (#815): a fixed sleep waiting on
/// another task (the persistence subscriber, the tool executor) is a race on
/// a loaded CI runner; this waits for the actual fact instead. Returns
/// whether it held.
pub async fn eventually(mut cond: impl FnMut() -> bool) -> bool {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while !cond() {
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    true
}

// A single-mode `ModeTable` (ADR-0207 stage 4) most test modules in this
// harness wire in for `ModeResolver`: they exercise something *other*
// than permission grading (spawn plumbing, MCP lazy re-enable, invoke-
// envelope unwrapping, skill posture, ...) and just want every call to run
// unprompted, mirroring the pre-ADR-0207 `build` agent's `default: allow`.
// Tests that exercise mode grading itself (`tool_mask`, `permission_dispatch`,
// `policy_seam`) build their own table instead.
mod mode_support;

// A `skutter` child isolated from the developer's real dirs (#747).
mod skutter_cmd;

mod advertising_pin;
mod agent_definitions;
mod agent_generation;
mod agent_invariant_tools;
mod agent_model;
mod agent_models;
mod agent_send;
mod apply_patch;
mod arg_validate;
mod ask_user;
mod aux_models;
mod compact_fork;
mod discovery_tools;
#[cfg(feature = "serve")]
mod executor_builder;
mod hook_events;
mod hooks;
mod host_tools;
mod launch_mode_typo;
mod list_operations;
mod list_pending;
mod load_skill;
mod log_compat_v060;
#[cfg(all(feature = "mcp-http", feature = "serve"))]
mod mcp_compat;
mod mcp_envelope_runtime;
#[cfg(all(feature = "mcp-http", feature = "serve"))]
mod mcp_http;
#[cfg(all(feature = "mcp-http", feature = "serve"))]
mod mcp_lazy_connect;
#[cfg(feature = "mcp-http")]
mod mcp_oauth_device;
#[cfg(feature = "mcp-http")]
mod mcp_oauth_refresh;
mod mcp_schema_violation;
#[cfg(all(feature = "mcp-http", feature = "serve"))]
mod mcp_scoped;
#[cfg(feature = "provider")]
mod narrate;
mod overlay_resume;
mod parked_prompt_persistence;
mod permission_dispatch;
mod persistence_signal;
mod plan_tasks;
mod plan_watch;
mod policy_seam;
mod poll_subtree;
mod propose_plan;
mod provider_selection;
mod record_sink;
mod reoffer_dedupe;
mod reparent_persistence;
mod reparent_subagents;
mod replay_from;
mod request_mode;
mod resumable_prefix;
mod resume_config_mode;
mod resume_mode_narrowing;
mod retired_knobs;
mod revert_files;
#[cfg(feature = "serve")]
mod serve;
mod session_fork;
#[cfg(feature = "provider")]
mod session_title;
mod session_workdir;
mod skill_posture;
mod slash_commands;
mod spawn_grading;
mod spawn_grading_park;
mod spawn_grading_scoped;
mod spawn_prompt_persistence;
mod state_dir;
mod stop_abort;
mod stop_abort_inflight_cleanup;
mod subagent_spawn;
mod subagent_tenant_prefix;
mod system_prompt_assembly;
mod tool_mask;
mod unattended_mode;
mod undo_config;
mod user_config;
#[cfg(feature = "web-fetch")]
mod web_fetch;
