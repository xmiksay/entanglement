//! The advertising-mode precedence chain (ADR-0196 §2): env > `config.yml` >
//! catalog > `tool_search`. Split out of `mod.rs` for the file cap.
//!
//! The process env is read only by the two thin public wrappers
//! ([`configured_advertising`], [`configured_advertising_source`]); every
//! other function takes the env tier as a value. Tests exercise the `_from`
//! forms and never mutate `ENTANGLEMENT_TOOL_ADVERTISING` — a process-global
//! that parallel test threads otherwise race on.

use entanglement_core::{Catalog, ModelEntry, ToolAdvertising};

use crate::config::{Config, TOOL_ADVERTISING_ENV};

fn env_value() -> Option<String> {
    std::env::var(TOOL_ADVERTISING_ENV).ok()
}

/// Which precedence tier won an advertising resolution — reported by
/// `skutter inspect config` so "why is this session tool_search?" has an
/// answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdvertisingSource {
    Env,
    Config,
    Catalog,
    Default,
}

impl AdvertisingSource {
    pub fn label(self) -> &'static str {
        match self {
            AdvertisingSource::Env => "env",
            AdvertisingSource::Config => "config",
            AdvertisingSource::Catalog => "catalog",
            AdvertisingSource::Default => "default",
        }
    }
}

/// The config/env half of the precedence chain, independent of any model:
/// `None` when neither tier is set, so the catalog gets its say. Shared by
/// the per-session resolver and `skutter inspect config` (whose global view
/// has no model to consult).
pub fn configured_advertising(config: &Config) -> Option<ToolAdvertising> {
    configured_advertising_from(env_value().as_deref(), config)
}

/// [`configured_advertising`] with the env tier passed in.
pub(super) fn configured_advertising_from(
    env: Option<&str>,
    config: &Config,
) -> Option<ToolAdvertising> {
    if let Some(raw) = env.filter(|s| !s.is_empty()) {
        // Warn-and-fall-through, not error: a typo'd env var must not kill
        // startup (the retention-env house pattern) — but it must be loud,
        // because the user asked for a mode and silently getting another is
        // the worse failure.
        return match ToolAdvertising::parse(raw) {
            Some(mode) => Some(mode),
            None => {
                tracing::warn!(
                    "ignoring unparseable {TOOL_ADVERTISING_ENV}={raw:?}; \
                     falling back to config/catalog/default"
                );
                config.tool_advertising
            }
        };
    }
    config.tool_advertising
}

/// Which precedence tier the *global* (model-less) view would answer from —
/// `skutter inspect config`'s provenance line. The catalog tier can only be
/// reached per model, so globally it reports as the default.
pub fn configured_advertising_source(config: &Config) -> AdvertisingSource {
    configured_advertising_source_from(env_value().as_deref(), config)
}

/// [`configured_advertising_source`] with the env tier passed in.
pub(super) fn configured_advertising_source_from(
    env: Option<&str>,
    config: &Config,
) -> AdvertisingSource {
    if env.is_some_and(|v| !v.is_empty() && ToolAdvertising::parse(v).is_some()) {
        AdvertisingSource::Env
    } else if config.tool_advertising.is_some() {
        AdvertisingSource::Config
    } else {
        AdvertisingSource::Default
    }
}

/// Resolve a `(provider, model)` pair's tool-advertising mode under the full
/// precedence chain: env > config > that model's catalog entry >
/// `tool_search`. The catalog miss (unknown provider/model — a user
/// `providers.yml` entry can name anything) is not an error: it simply
/// contributes no preference, exactly like an entry that omits
/// `tool_advertising:`. `configured` is the env+config tier,
/// [`configured_advertising`]'s answer.
pub fn resolve_advertising(
    configured: Option<ToolAdvertising>,
    catalog: Option<&Catalog>,
    provider: &str,
    model: &str,
) -> ToolAdvertising {
    if let Some(mode) = configured {
        return mode;
    }
    catalog
        .and_then(|c| c.model(provider, model))
        .and_then(
            |ModelEntry {
                 tool_advertising, ..
             }| *tool_advertising,
        )
        .unwrap_or_default()
}

/// Resolve by model id alone, when only `SessionStarted.model` (a profile's
/// bare model field, no provider) is known. Model ids are unique across the
/// embedded catalog in practice; on a cross-provider collision the first
/// match wins, which is harmless — both candidates lacking a
/// `tool_advertising:` preference (the shipped state) is the only case
/// where it could matter.
pub fn resolve_advertising_by_id(
    configured: Option<ToolAdvertising>,
    catalog: Option<&Catalog>,
    model: &str,
) -> ToolAdvertising {
    if let Some(mode) = configured {
        return mode;
    }
    catalog
        .and_then(|c| c.model_by_id(model))
        .and_then(
            |ModelEntry {
                 tool_advertising, ..
             }| *tool_advertising,
        )
        .unwrap_or_default()
}
