//! Saving an onboarding [`Setup`] (#732): the key (or an edited base URL) goes
//! to the managed `.env` through the same writer `/key` and `config set-key`
//! drive (ADR-0073); `provider:`/`model:` go to the user `config.yml` through
//! the comment-preserving key writer, so the next start selects the same
//! provider without asking. Then the running process is primed the way `/key`
//! primes it, and the in-memory config is pointed at the choice.

use std::path::PathBuf;

use serde_yaml::Value;

use super::state::Setup;
use crate::config;

const PROVIDER_COMMENT: &str = "Provider chosen in the first-run setup dialog (#732).";
const MODEL_COMMENT: &str = "Model chosen in the first-run setup dialog (#732).";

/// Write `setup` to the managed env file at `env_path` and the user config at
/// `config_path`. Best-effort per file: each failure becomes a warning (never
/// containing the key) and the rest still runs — the session can start on the
/// in-memory choice either way, it just won't be remembered.
pub(crate) fn persist(
    setup: &Setup,
    env_path: Option<PathBuf>,
    config_path: Option<PathBuf>,
) -> Vec<String> {
    let mut warnings = Vec::new();
    for (var, value) in setup.key.iter().chain(setup.base.iter()) {
        let result = match &env_path {
            Some(path) => config::env_key::set_key_at(path.clone(), var, value).map(drop),
            None => Err(anyhow::anyhow!("no managed env file path")),
        };
        if let Err(e) = result {
            warnings.push(format!("could not save {var}: {e:#}"));
        }
    }
    let Some(config_path) = config_path else {
        warnings.push("could not save the provider choice: no user config path".into());
        return warnings;
    };
    for (key, value, comment) in [
        ("provider", &setup.provider, PROVIDER_COMMENT),
        ("model", &setup.model, MODEL_COMMENT),
    ] {
        let saved = config::write_key::save_key_at(config_path.clone(), key, comment, |_| {
            Ok(Value::String(value.clone()))
        });
        if let Err(e) = saved {
            warnings.push(format!("could not save `{key}:` to the user config: {e:#}"));
        }
    }
    warnings
}

/// Make the choice effective for **this** process: export the key / base the
/// way `/key` does (startup already loaded the `.env`, so the file write alone
/// would only apply next time) and pin the in-memory config.
pub(crate) fn apply(setup: &Setup, user_config: &mut config::Config) {
    for (var, value) in setup.key.iter().chain(setup.base.iter()) {
        std::env::set_var(var, value);
    }
    user_config.provider = Some(setup.provider.clone());
    user_config.model = Some(setup.model.clone());
}

#[cfg(test)]
mod tests {
    use entanglement_provider::{Catalog, HttpClient, RetryConfig, SharedStateLocation};

    use super::*;

    fn keyed() -> Setup {
        Setup {
            provider: "anthropic".into(),
            key: Some(("ANTHROPIC_API_KEY".into(), "sk-onboard".into())),
            base: None,
            model: "claude-sonnet-5".into(),
        }
    }

    #[test]
    fn keyed_setup_writes_the_env_file_and_pins_the_config() {
        let dir = tempfile::tempdir().unwrap();
        let env = dir.path().join(".env");
        let cfg = dir.path().join("config.yml");
        let warnings = persist(&keyed(), Some(env.clone()), Some(cfg.clone()));
        assert!(warnings.is_empty(), "{warnings:?}");

        let env_text = std::fs::read_to_string(&env).unwrap();
        assert!(env_text.contains("ANTHROPIC_API_KEY=sk-onboard\n"));
        let doc: Value = serde_yaml::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(doc["provider"], Value::String("anthropic".into()));
        assert_eq!(doc["model"], Value::String("claude-sonnet-5".into()));
    }

    #[test]
    fn an_unwritable_destination_warns_without_leaking_the_key() {
        let warnings = persist(&keyed(), None, None);
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings.iter().all(|w| !w.contains("sk-onboard")));
    }

    #[test]
    fn an_edited_base_lands_in_the_env_file_too() {
        let dir = tempfile::tempdir().unwrap();
        let env = dir.path().join(".env");
        let setup = Setup {
            provider: "ollama".into(),
            key: None,
            base: Some(("OLLAMA_API_BASE".into(), "http://gpu:11434/v1".into())),
            model: "llama3.1".into(),
        };
        let warnings = persist(&setup, Some(env.clone()), Some(dir.path().join("c.yml")));
        assert!(warnings.is_empty(), "{warnings:?}");
        let text = std::fs::read_to_string(&env).unwrap();
        assert!(text.contains("OLLAMA_API_BASE=http://gpu:11434/v1\n"));
    }

    /// The applied choice is what startup's provider selection then binds —
    /// keyless Ollama, so no process env needs mutating.
    #[test]
    fn applied_setup_selects_that_provider_and_model() {
        let setup = Setup {
            provider: "ollama".into(),
            key: None,
            base: None,
            model: "qwen-onboarded".into(),
        };
        let mut cfg = config::Config::embedded_defaults().unwrap();
        apply(&setup, &mut cfg);
        let http =
            HttpClient::with_shared_state(SharedStateLocation::Disabled, RetryConfig::default())
                .unwrap();
        let (engine, info, provider) =
            crate::providers::select_provider(&Catalog::builtin(), &http, &cfg);
        assert_eq!(provider, "ollama");
        assert_eq!(info.id, "qwen-onboarded");
        assert_eq!(engine.default_model.as_deref(), Some("qwen-onboarded"));
    }
}
