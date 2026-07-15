//! `skutter config set-key <provider>` — persist a provider API key (#304).
//!
//! Resolves the provider's key env var from the catalog and writes the value to
//! the managed env file via the shared [`super::env_key::set_key`] writer. The
//! value comes from (in order): the `--key`/positional argument, a hidden
//! terminal prompt (rpassword — never echoed), or a plain stdin read when stdin
//! is piped (scripting). The key is never printed back.

use anyhow::{bail, Context, Result};
use entanglement_provider::Catalog;

use super::env_key;

/// Run the `config set-key` command for `provider`, sourcing the value from
/// `key_arg` when given, else prompting / reading stdin.
pub fn set_key(catalog: &Catalog, provider: &str, key_arg: Option<String>) -> Result<()> {
    let entry = catalog.provider(provider).ok_or_else(|| {
        anyhow::anyhow!(
            "unknown provider `{provider}` (known: {})",
            provider_names(catalog)
        )
    })?;
    let key_env = entry.key_env.as_deref().ok_or_else(|| {
        anyhow::anyhow!("provider `{provider}` is keyless (e.g. Ollama) — no API key to set")
    })?;

    let raw = match key_arg {
        Some(v) => v,
        None => read_secret(key_env)?,
    };
    // Trim surrounding whitespace (a piped read carries the trailing newline);
    // API keys are whitespace-free tokens, so this only strips accidents.
    let value = raw.trim();
    if value.is_empty() {
        bail!("refusing to set an empty value for {key_env}");
    }

    let path = env_key::set_key(key_env, value)
        .with_context(|| format!("writing {key_env} to the managed env file"))?;
    // Never echo the value — only the var name and destination.
    println!("Saved {key_env} to {}", path.display());

    // The file is only read for a var the real environment left unset (env >
    // file): warn when the current process already carries a *different* value,
    // so the user isn't surprised the file "did nothing".
    if let Ok(current) = std::env::var(key_env) {
        if current != value {
            eprintln!(
                "warning: {key_env} is already set in the environment to a different value; \
                 the environment wins over the file (env > file), so unset it for this key to apply"
            );
        }
    }
    Ok(())
}

/// Read the key without echoing it: a hidden terminal prompt when stdin is a TTY,
/// else a plain line read (piped input — scripting / CI).
fn read_secret(key_env: &str) -> Result<String> {
    use std::io::IsTerminal;
    if std::io::stdin().is_terminal() {
        rpassword::prompt_password(format!("Enter value for {key_env}: "))
            .context("reading key from prompt")
    } else {
        let mut line = String::new();
        std::io::stdin()
            .read_line(&mut line)
            .context("reading key from stdin")?;
        Ok(line)
    }
}

/// Pipe-joined provider names for the unknown-provider diagnostic.
fn provider_names(catalog: &Catalog) -> String {
    catalog
        .providers
        .iter()
        .map(|p| p.name.as_str())
        .collect::<Vec<_>>()
        .join("|")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_key_errors_on_unknown_provider() {
        let catalog = Catalog::builtin();
        let err = set_key(&catalog, "nope", Some("v".into())).unwrap_err();
        assert!(err.to_string().contains("unknown provider"));
    }

    #[test]
    fn set_key_errors_on_keyless_provider() {
        let catalog = Catalog::builtin();
        // Ollama is keyless in the embedded defaults.
        let err = set_key(&catalog, "ollama", Some("v".into())).unwrap_err();
        assert!(err.to_string().contains("keyless"));
    }

    #[test]
    fn set_key_errors_on_empty_value() {
        let catalog = Catalog::builtin();
        let err = set_key(&catalog, "zai", Some("   ".into())).unwrap_err();
        assert!(err.to_string().contains("empty"));
    }
}
