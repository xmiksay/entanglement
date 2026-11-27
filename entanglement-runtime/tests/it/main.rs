//! Single integration-test harness (ADR-0180): every former per-file test
//! binary is a module here, so the crate + deps compile and link once instead
//! of 33 times. Run one module via `cargo test -p entanglement-runtime --test it <mod>`.
//! Suites that drive `skutter`'s host tools live in `skutter/tests/` (#968).

/// An empty user definitions layer under `root`, so a test never reads the
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

mod advertising_pin;
mod agent_definitions;
mod agent_invariant_tools;
mod agent_model;
mod agent_send;
mod approval_resolved;
mod arg_validate;
mod ask_user;
mod attended_mode;
mod attended_rejoin;
mod attended_resume;
mod compact_fork;
mod discovery_tools;
mod executor_builder;
mod list_operations;
mod list_pending;
mod log_compat_v060;
#[cfg(feature = "mcp-http")]
mod mcp_compat;
mod mcp_envelope_runtime;
#[cfg(feature = "mcp-http")]
mod mcp_http;
#[cfg(feature = "mcp-http")]
mod mcp_lazy_connect;
#[cfg(feature = "mcp-http")]
mod mcp_oauth_device;
#[cfg(feature = "mcp-http")]
mod mcp_scoped;
mod misaimed_decisions;
#[cfg(feature = "provider")]
mod narrate;
mod parked_prompt_persistence;
mod permission_dispatch;
mod persistence_signal;
mod plan_tasks;
mod policy_seam;
mod poll_subtree;
mod record_sink;
mod reoffer_dedupe;
mod reparent_persistence;
mod reparent_subagents;
mod replay_from;
mod request_mode;
mod resumable_prefix;
mod resume_config_mode;
mod resume_mode_narrowing;
mod session_fork;
#[cfg(feature = "provider")]
mod session_title;
mod spawn_grading;
mod spawn_grading_park;
mod spawn_grading_scoped;
mod spawn_prompt_persistence;
mod stop_abort;
mod stop_abort_inflight_cleanup;
mod subagent_spawn;
mod subagent_tenant_prefix;
mod system_prompt_assembly;
mod tool_mask;
mod unattended_mode;
