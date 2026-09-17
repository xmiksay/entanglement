//! Provider usability predicate (#560 P12 follow-up to ADR-0207 §12): whether
//! a catalog entry can possibly authenticate *right now*, so a model-facing
//! listing (the sub-agent spawn `model` refusal, `explore(kind: "models")`)
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
    /// construction, and every live spawn/`explore` call all run after.
    pub fn is_usable(&self) -> bool {
        if self.oauth.is_some() {
            return true;
        }
        match &self.key_env {
            None => true,
            Some(var) => std::env::var(var).map(|v| !v.is_empty()).unwrap_or(false),
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
        let e = entry("key_env: USABLE_TEST_KEY_UNSET_560\n");
        std::env::remove_var("USABLE_TEST_KEY_UNSET_560");
        assert!(!e.is_usable());

        let e = entry("key_env: USABLE_TEST_KEY_EMPTY_560\n");
        std::env::set_var("USABLE_TEST_KEY_EMPTY_560", "");
        assert!(!e.is_usable());
        std::env::remove_var("USABLE_TEST_KEY_EMPTY_560");

        let e = entry("key_env: USABLE_TEST_KEY_SET_560\n");
        std::env::set_var("USABLE_TEST_KEY_SET_560", "sk-test");
        assert!(e.is_usable());
        std::env::remove_var("USABLE_TEST_KEY_SET_560");
    }
}
