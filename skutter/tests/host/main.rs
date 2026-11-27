//! `skutter`'s host tools over the real engine and tool executor (#968):
//! the file, search, shell and fetch tools, their workdir and root
//! containment, the per-write backups and the plan files they write. One
//! binary (ADR-0180); run one module via `cargo test -p skutter --test host
//! <mod>`.

/// An empty user definitions layer under `root`, so a test never reads the
/// host's real user agents/skills (#773).
pub fn no_user(root: &std::path::Path) -> entanglement_runtime::layers::UserLayer {
    entanglement_runtime::layers::UserLayer::Dir(root.join("no-such-user-dir"))
}

/// Poll `cond` until it holds or 10s pass (#815): a fixed sleep waiting on
/// another task is a race on a loaded CI runner; this waits for the actual
/// fact instead. Returns whether it held.
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

// The single-mode allow-all `ModeTable` most suites grade against.
mod mode_support;

mod apply_patch;
mod host_tools;
mod load_skill;
mod mcp_schema_violation;
mod plan_watch;
mod propose_plan;
mod revert_files;
mod session_workdir;
mod skill_posture;
mod undo_config;
mod web_fetch;
