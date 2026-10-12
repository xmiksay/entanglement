//! Environment variables no child process may inherit (ADR-0227): the
//! `${VAR}`s `http_auth` credentials reference. The managed `.env` exports
//! them into this process so the `http` tool can resolve them at send time —
//! but a `bash("printenv GITHUB_TOKEN")` must not read them back, and neither
//! may a hook, an MCP stdio server, the status-line command or any other
//! child. Process-wide because every spawn site must apply it and the set is
//! fixed by whoever builds an [`crate::http_gate::HttpGate`]; the provider
//! keys keep their own per-tool `secret_env` lists (#164).

use std::collections::BTreeSet;
use std::sync::Mutex;

static HIDDEN: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

/// Hide `vars` from every child spawned from now on.
pub fn hide(vars: impl IntoIterator<Item = String>) {
    if let Ok(mut set) = HIDDEN.lock() {
        set.extend(vars);
    }
}

/// The hidden variable names.
pub fn hidden() -> Vec<String> {
    HIDDEN
        .lock()
        .map(|set| set.iter().cloned().collect())
        .unwrap_or_default()
}

/// Remove every hidden variable from `cmd`'s environment.
pub fn scrub(cmd: &mut tokio::process::Command) {
    for var in hidden() {
        cmd.env_remove(var);
    }
}

/// [`scrub`] for a `std` command (the browser/editor/clipboard launches).
pub fn scrub_std(cmd: &mut std::process::Command) {
    for var in hidden() {
        cmd.env_remove(var);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_hidden_var_never_reaches_a_child() {
        // A name no other test sets: the registry is process-wide.
        let var = "ENTANGLEMENT_TEST_HIDDEN_SECRET_ENV";
        std::env::set_var(var, "s3cret");
        hide([var.to_string()]);
        let mut cmd = tokio::process::Command::new("sh");
        cmd.args(["-c", &format!("printf '%s' \"${var}\"")]);
        scrub(&mut cmd);
        let out = cmd.output().await.unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "");
        std::env::remove_var(var);
    }
}
