//! The coding layer moved out of the library (#932): the on-disk session
//! store, its resume checks and the persistence suites over it, and the
//! aux-model tasks, over the real engine. One binary (ADR-0180); run one
//! module via `cargo test -p skutter --test coding <mod>`.

/// An empty user definitions layer under `root`, so a test never reads the
/// host's real user agents/skills (#773).
pub fn no_user(root: &std::path::Path) -> skutter::layers::UserLayer {
    skutter::layers::UserLayer::Dir(root.join("no-such-user-dir"))
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

mod agent_definitions;
mod compact_fork;
mod log_compat_v060;
mod narrate;
mod parked_prompt_persistence;
mod record_sink;
mod reparent_persistence;
mod replay_from;
mod resume_config_mode;
mod resume_mode_narrowing;
mod session_fork;
mod session_title;
mod spawn_prompt_persistence;
mod system_prompt_assembly;
