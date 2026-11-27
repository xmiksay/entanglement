//! A `skutter` child process isolated from the developer's real dirs (#747).
//!
//! A spawned binary runs the startup session prune, loads `config.yml`, the
//! provider `.env`, `grants.yml` and every other managed file, and sweeps the
//! shared endpoint-state dir — all off the real data/config dirs unless told
//! otherwise. The isolation is set on the **child's** env only: mutating this
//! test process's env would race every parallel test in the harness.

use std::process::Command;

use tempfile::TempDir;

/// A `skutter` [`Command`] whose `HOME` and XDG base dirs point into a fresh
/// temp dir (so `dirs::data_dir()`/`config_dir()` resolve there on every
/// platform), with every inherited `ENTANGLEMENT_*` override scrubbed so a
/// developer's shell can't steer the child back at a real file, and the
/// provider pinned to the explicit `echo` stub. Keep the
/// returned [`TempDir`] alive until the child has exited.
pub fn isolated_skutter() -> (Command, TempDir) {
    let sandbox = tempfile::tempdir().expect("temp dir for an isolated skutter");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_skutter"));
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("ENTANGLEMENT_") {
            cmd.env_remove(key);
        }
    }
    let root = sandbox.path();
    cmd.env("HOME", root.join("home"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("XDG_STATE_HOME", root.join("state"))
        // Endpoint state would land under the temp data dir anyway; disabling
        // it keeps the child's pool purely in-process (#743).
        .env("ENTANGLEMENT_DEBUG", "no-shared-state")
        // No silent echo fallback any more (#732, ADR-0222): a test that wants
        // the stub asks for it, and a developer's real key never leaks a live
        // call into the suite. Tests of selection itself override or remove it.
        .env("ENTANGLEMENT_PROVIDER", "echo");
    (cmd, sandbox)
}
