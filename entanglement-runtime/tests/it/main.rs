//! Single integration-test harness (ADR-0180): every former per-file test
//! binary is a module here, so the crate + deps compile and link once instead
//! of 33 times. Run one module via `cargo test -p entanglement-runtime --test it <mod>`.
//! Suites that drive `skutter`'s host tools live in `skutter/tests/` (#968).

/// The three agents most suites spawn by name (`skutter`'s built-in roster,
/// #932): `general`, `plan` (starting in the `plan` mode) and `debug`.
pub fn agents() -> entanglement_core::AgentCatalog {
    let mut catalog = entanglement_core::AgentCatalog::default();
    for (name, default_mode) in [("general", None), ("plan", Some("plan")), ("debug", None)] {
        catalog.insert(entanglement_core::Agent {
            name: name.into(),
            description: format!("the {name} agent"),
            system_prompt: format!("You are the {name} agent."),
            model: None,
            provider: None,
            default_mode: default_mode.map(String::from),
        });
    }
    catalog
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
mod agent_invariant_tools;
mod agent_model;
mod agent_send;
mod approval_resolved;
mod arg_validate;
mod ask_user;
mod attended_mode;
mod attended_rejoin;
mod discovery_tools;
mod executor_builder;
mod list_operations;
mod list_pending;
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
mod permission_dispatch;
mod persistence_signal;
mod plan_tasks;
mod policy_seam;
mod poll_subtree;
mod record_sink;
mod reoffer_dedupe;
mod reparent_subagents;
mod request_mode;
mod resumable_prefix;
mod script_bindings;
mod spawn_grading;
mod spawn_grading_park;
mod spawn_grading_scoped;
mod stop_abort;
mod stop_abort_inflight_cleanup;
mod subagent_spawn;
mod subagent_tenant_prefix;
mod tool_mask;
mod tool_surface_guard;
mod unattended_mode;
mod unattended_reason;
