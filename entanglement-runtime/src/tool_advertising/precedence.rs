//! The advertising-mode precedence chain (ADR-0196 §2): `config.yml` >
//! catalog > `tool_search`. Split out of `mod.rs` for the file cap. The
//! former env tier (`ENTANGLEMENT_TOOL_ADVERTISING`) is retired (ADR-0213).

use entanglement_core::{Catalog, ModelEntry, ToolAdvertising};

use crate::config::Config;

/// The config half of the precedence chain, independent of any model: `None`
/// when unset, so the catalog gets its say. Shared by the per-session
/// resolver.
pub fn configured_advertising(config: &Config) -> Option<ToolAdvertising> {
    config.tool_advertising
}

/// Resolve a `(provider, model)` pair's tool-advertising mode under the full
/// precedence chain: config > that model's catalog entry > `tool_search`. The catalog miss (unknown provider/model — a user
/// `providers.yml` entry can name anything) is not an error: it simply
/// contributes no preference, exactly like an entry that omits
/// `tool_advertising:`. `configured` is the config tier,
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
