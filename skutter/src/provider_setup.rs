//! "Is any LLM provider configured?" and the actionable hint printed when none
//! is (#732, ADR-0222). With nothing configured the TUI opens its first-run
//! onboarding dialog; every other head exits with [`no_provider_hint`] — there
//! is no silent `EchoLlm` fallback any more, echo is only ever explicit.

use entanglement_provider::Catalog;

use crate::config;

/// Whether startup has a provider to select without asking: an explicit
/// `ENTANGLEMENT_PROVIDER` (any value — even a bad one is `select_provider`'s
/// loud error to report, not a reason to onboard), a config `provider:`, or a
/// catalog `key_env` set to a non-empty value (what auto-detect keys on).
pub(crate) fn provider_configured(catalog: &Catalog, user_config: &config::Config) -> bool {
    configured_with(catalog, user_config, |name| std::env::var(name).ok())
}

/// [`provider_configured`] over an injected env lookup (unit-testable without
/// mutating the process env).
fn configured_with(
    catalog: &Catalog,
    user_config: &config::Config,
    env: impl Fn(&str) -> Option<String>,
) -> bool {
    env("ENTANGLEMENT_PROVIDER").is_some()
        || user_config.provider.is_some()
        || catalog.providers.iter().any(|entry| {
            entry
                .key_env
                .as_deref()
                .and_then(&env)
                .is_some_and(|v| !v.is_empty())
        })
}

/// The multi-line stderr hint for a head that found no provider: the catalog's
/// key vars, the `config set-key` command with every keyed provider name, and
/// the two explicit keyless choices (local Ollama, the echo stub).
pub(crate) fn no_provider_hint(catalog: &Catalog) -> String {
    let keyed: Vec<&str> = catalog
        .providers
        .iter()
        .filter(|p| p.key_env.is_some())
        .map(|p| p.name.as_str())
        .collect();
    format!(
        "skutter: no LLM provider configured.\n\
         \x20 set one of: {keys}\n\
         \x20 or run: skutter config set-key <{names}>\n\
         \x20 local keyless model: ENTANGLEMENT_PROVIDER=ollama\n\
         \x20 debugging stub (no LLM): ENTANGLEMENT_PROVIDER=echo\n\
         \x20 (bare `skutter` opens a first-run setup dialog)",
        keys = catalog.key_envs().join(" / "),
        names = keyed.join("|"),
    )
}

/// Print [`no_provider_hint`] and exit 2 — the same clean code every other
/// provider-selection error uses.
pub(crate) fn exit_no_provider(catalog: &Catalog) -> ! {
    eprintln!("{}", no_provider_hint(catalog));
    std::process::exit(2);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |name| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v.to_string())
        }
    }

    fn bare() -> config::Config {
        config::Config::embedded_defaults().unwrap()
    }

    #[test]
    fn nothing_set_is_unconfigured() {
        assert!(!configured_with(&Catalog::builtin(), &bare(), env_of(&[])));
    }

    #[test]
    fn an_empty_key_does_not_count() {
        let env = env_of(&[("ZAI_API_KEY", "")]);
        assert!(!configured_with(&Catalog::builtin(), &bare(), env));
    }

    #[test]
    fn a_catalog_key_counts() {
        let env = env_of(&[("ANTHROPIC_API_KEY", "sk")]);
        assert!(configured_with(&Catalog::builtin(), &bare(), env));
    }

    #[test]
    fn explicit_provider_env_or_config_counts() {
        let env = env_of(&[("ENTANGLEMENT_PROVIDER", "echo")]);
        assert!(configured_with(&Catalog::builtin(), &bare(), env));
        let mut cfg = bare();
        cfg.provider = Some("ollama".into());
        assert!(configured_with(&Catalog::builtin(), &cfg, env_of(&[])));
    }

    #[test]
    fn hint_names_keys_set_key_and_explicit_echo() {
        let hint = no_provider_hint(&Catalog::builtin());
        for needle in [
            "ZAI_API_KEY",
            "ANTHROPIC_API_KEY",
            "skutter config set-key <zai|",
            "ENTANGLEMENT_PROVIDER=echo",
            "ENTANGLEMENT_PROVIDER=ollama",
        ] {
            assert!(hint.contains(needle), "missing {needle}: {hint}");
        }
        // Keyless Ollama has no key to set.
        assert!(!hint.contains("|ollama"), "{hint}");
    }
}
