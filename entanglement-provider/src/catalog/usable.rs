//! Provider usability predicate (#560 P12 follow-up to ADR-0207 §12): whether
//! a catalog entry can possibly authenticate *right now*, so a model-facing
//! listing (the sub-agent spawn `model` refusal, `list_models`)
//! offers only providers that could actually serve a request instead of the
//! whole catalog. Split out of `catalog.rs` to stay clear of its 400-line
//! code-line cap.

use super::ProviderEntry;

impl ProviderEntry {
    /// `oauth: Some(_)` is usable unconditionally — the endpoint authenticates
    /// with a refreshed bearer token (`skutter config connect`), and a
    /// missing/expired token is a runtime failure surfaced at request time,
    /// not a catalog fact this predicate can see. `key_env: None` is keyless
    /// (e.g. local Ollama). `key_env: Some(var)` needs `var` set and
    /// non-empty in the *process* environment, read live so a `/key` save
    /// (which `set_var`s the key) is picked up without a restart — mirroring
    /// `AvailableServer::key_ok` (`mcp/available.rs`) and `select_provider`'s
    /// own auto-detect check in `main.rs`. Reading live is safe here: the
    /// managed `.env` is loaded into the process env once at startup
    /// (`env_file::load`), strictly before the catalog is handed to any
    /// consumer of this predicate — `select_provider`, agent/session
    /// construction, and every live spawn/`list_models` call all run after.
    pub fn is_usable(&self) -> bool {
        self.is_usable_with(|var| std::env::var(var).ok())
    }

    /// [`is_usable`](Self::is_usable) over an injected env lookup, so the
    /// predicate is unit-testable without mutating process env (#773).
    fn is_usable_with(&self, env: impl Fn(&str) -> Option<String>) -> bool {
        if self.oauth.is_some() {
            return true;
        }
        match &self.key_env {
            None => true,
            Some(var) => env(var).is_some_and(|v| !v.is_empty()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(yaml_tail: &str) -> ProviderEntry {
        serde_yaml::from_str(&format!("name: test\ndefault_model: m\n{yaml_tail}"))
            .expect("test entry must parse")
    }

    #[test]
    fn keyless_is_usable() {
        assert!(entry("").is_usable());
    }

    #[test]
    fn oauth_is_usable_regardless_of_key_env() {
        assert!(entry(
            "oauth:\n  authorization_url: https://as.example/authorize\n  token_url: https://as.example/token\n"
        )
        .is_usable());
    }

    #[test]
    fn keyed_provider_needs_a_set_nonempty_env_var() {
        let e = entry("key_env: TEST_KEY\n");
        assert!(!e.is_usable_with(|_| None));
        assert!(!e.is_usable_with(|_| Some(String::new())));
        assert!(e.is_usable_with(|v| (v == "TEST_KEY").then(|| "sk-test".into())));
        assert!(!e.is_usable_with(|v| (v == "OTHER").then(|| "sk-test".into())));
    }
}
