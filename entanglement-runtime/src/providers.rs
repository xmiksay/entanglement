//! Provider selection at startup: env/config/auto-detect precedence over the
//! catalog, and the per-wire engine-config builders it dispatches to.

use std::collections::HashMap;

use entanglement_core::EngineConfig;
use entanglement_provider::{
    Catalog, GenerationParams, HttpClient, ModelInfo, ModelPricing, ProviderEntry, WebSearchConfig,
    Wire,
};

use crate::config;
use crate::factories::{
    anthropic_factory_for, gemini_factory_for, openai_factory_for, openai_responses_factory_for,
};

/// Resolve the active provider from the catalog:
///
/// - `ENTANGLEMENT_PROVIDER=<name>`, else the user config's `provider`, looks
///   `<name>` up **in the catalog** (so user-defined providers work); `echo`
///   stays a built-in stub. A missing key for the named provider exits cleanly.
/// - neither set → auto-detect by iterating catalog order and picking the first
///   provider whose `key_env` is set and non-empty (keyless Ollama is skipped),
///   else fall back to `EchoLlm`.
///
/// Precedence is env > config > auto-detect, mirroring every other setting.
pub(crate) fn select_provider(
    catalog: &Catalog,
    http_client: &HttpClient,
    user_config: &config::Config,
) -> (EngineConfig, ModelInfo, String) {
    let selected = std::env::var("ENTANGLEMENT_PROVIDER")
        .ok()
        .or_else(|| user_config.provider.clone());
    match selected.as_deref() {
        Some("echo") => {
            let (cfg, info) = echo_config();
            (cfg, info, "echo".to_string())
        }
        Some(name) => {
            let Some(entry) = catalog.provider(name) else {
                eprintln!(
                    "skutter: unknown provider='{name}' \
                     (not in catalog: {}, or echo)",
                    catalog_names(catalog)
                );
                std::process::exit(2);
            };
            let (cfg, info) =
                wire_config(entry, http_client, catalog, user_config).unwrap_or_else(|| {
                    exit_missing_key(
                        &entry.name,
                        entry.key_env.as_deref().unwrap_or("its API key"),
                    )
                });
            (cfg, info, entry.name.clone())
        }
        None => {
            for entry in &catalog.providers {
                // Auto-detect only over keyed providers (keyless Ollama can't be
                // sniffed and stays opt-in), first one with a key present wins.
                if entry.key_env.as_deref().and_then(env_nonempty).is_some() {
                    if let Some((cfg, info)) = wire_config(entry, http_client, catalog, user_config)
                    {
                        return (cfg, info, entry.name.clone());
                    }
                }
            }
            eprintln!(
                "skutter: no provider key set — using EchoLlm \
                 (set ENTANGLEMENT_PROVIDER=ollama for local, or a *_API_KEY, or echo)"
            );
            let (cfg, info) = echo_config();
            (cfg, info, "echo".to_string())
        }
    }
}

/// Comma-joined provider names, for the unknown-provider diagnostic.
fn catalog_names(catalog: &Catalog) -> String {
    catalog
        .providers
        .iter()
        .map(|p| p.name.as_str())
        .collect::<Vec<_>>()
        .join("|")
}

/// Explicit `ENTANGLEMENT_PROVIDER` set but its key env var is absent: exit
/// cleanly (like the unknown-provider branch) instead of panicking on `.expect`.
fn exit_missing_key(provider: &str, key_env: &str) -> ! {
    eprintln!("skutter: ENTANGLEMENT_PROVIDER={provider} requires {key_env} to be set");
    std::process::exit(2);
}

pub(crate) fn env_nonempty(name: &str) -> Option<String> {
    match std::env::var(name) {
        Ok(v) if !v.is_empty() => Some(v),
        _ => None,
    }
}

/// Build the engine config for a catalog provider, dispatching on its wire.
/// Returns `None` when a required key env is absent (keyed provider, no key).
fn wire_config(
    entry: &ProviderEntry,
    http_client: &HttpClient,
    catalog: &Catalog,
    user_config: &config::Config,
) -> Option<(EngineConfig, ModelInfo)> {
    match entry.wire {
        Wire::Openai => openai_wire_config(entry, http_client, catalog, user_config),
        Wire::Anthropic => anthropic_wire_config(entry, http_client, catalog, user_config),
        Wire::Gemini => gemini_wire_config(entry, http_client, catalog, user_config),
        Wire::OpenaiResponses => {
            openai_responses_wire_config(entry, http_client, catalog, user_config)
        }
    }
}

/// Model id from `{NAME}_MODEL` env, else the user config's `model`, else the
/// entry's `default_model` (env > config > catalog default).
fn resolve_model(entry: &ProviderEntry, user_config: &config::Config) -> String {
    let name = entry.name.to_uppercase();
    env_nonempty(&format!("{name}_MODEL"))
        .or_else(|| user_config.model.clone())
        .unwrap_or_else(|| entry.default_model.clone())
}

/// The opt-in provider-side web search settings to bind onto the LLM client
/// (#305), or `None` when the config leaves it disabled — the disabled case makes
/// the client request no web-search tool, exactly as before. Cloned so both
/// startup and the live `/model` switch resolver bind identically.
pub(crate) fn web_search_config(user_config: &config::Config) -> Option<WebSearchConfig> {
    user_config
        .web_search
        .enabled
        .then(|| user_config.web_search.clone())
}

/// Per-minute request budget from `{NAME}_RPM` env, else the catalog entry's
/// `rpm` (env > catalog; `None` → the client's built-in default). Mirrors the
/// rest of the catalog's precedence for this per-endpoint bucket size (#241).
pub(crate) fn resolve_rpm(entry: &ProviderEntry) -> Option<u32> {
    let name = entry.name.to_uppercase();
    env_nonempty(&format!("{name}_RPM"))
        .and_then(|v| v.parse::<u32>().ok())
        .or(entry.rpm)
}

/// In-flight concurrency cap from `{NAME}_CONCURRENCY` env, else the catalog
/// entry's `concurrency` (env > catalog; `None` → the client's built-in default,
/// itself overridable process-wide via `ENTANGLEMENT_MAX_CONCURRENCY`). Mirrors
/// `resolve_rpm`'s precedence for this per-endpoint storm guard (#414).
pub(crate) fn resolve_concurrency(entry: &ProviderEntry) -> Option<usize> {
    let name = entry.name.to_uppercase();
    env_nonempty(&format!("{name}_CONCURRENCY"))
        .and_then(|v| v.parse::<usize>().ok())
        .or(entry.concurrency)
}

/// Summarize the chosen model against the catalog (context window, display name).
fn model_info_for(entry: &ProviderEntry, model: &str, catalog: &Catalog) -> ModelInfo {
    ModelInfo::from_catalog(catalog.model(&entry.name, model), model)
}

/// Resolve the generation knobs for `model` from its catalog capability metadata
/// (#191): temperature only when `supports_temperature`, thinking budget only when
/// `supports_thinking`, plus `max_output_tokens`. `None` when the chosen model is
/// absent from the catalog (an env-typed id) — the client then uses its defaults.
fn generation_for(
    entry: &ProviderEntry,
    model: &str,
    catalog: &Catalog,
) -> Option<GenerationParams> {
    catalog
        .model(&entry.name, model)
        .map(|m| m.generation_params())
}

/// Per-model USD pricing keyed by model id, flattened across every provider in
/// the catalog (#192). The engine looks a turn's effective model up here to price
/// its reported usage; a model with no `pricing` block is simply absent (unknown
/// cost). Later providers win on a duplicate id, matching the catalog's own
/// `model_by_id` precedence.
fn pricing_map(catalog: &Catalog) -> HashMap<String, ModelPricing> {
    let mut map = HashMap::new();
    for provider in &catalog.providers {
        for model in &provider.models {
            if let Some(pricing) = model.pricing {
                map.insert(model.id.clone(), pricing);
            }
        }
    }
    map
}

/// OpenAI-compatible provider (z.ai/OpenAI/Ollama/any proxy). Key from
/// `entry.key_env` (absent → `None` = skip a keyed provider); base from
/// `{NAME}_API_BASE` else `{NAME}_BASE` env else `entry.base_url`.
fn openai_wire_config(
    entry: &ProviderEntry,
    http_client: &HttpClient,
    catalog: &Catalog,
    user_config: &config::Config,
) -> Option<(EngineConfig, ModelInfo)> {
    let model = resolve_model(entry, user_config);
    // Reuse the mid-session builder so startup and a live switch resolve the
    // wire/base/key identically (#218); a keyed provider missing its key → skip.
    let llm_factory = openai_factory_for(
        entry,
        &model,
        http_client,
        catalog,
        web_search_config(user_config),
        None,
    )
    .ok()?;
    eprintln!("skutter: provider={} model={model}", entry.name);
    Some((
        EngineConfig {
            llm_factory,
            default_model: Some(model.clone()),
            generation: generation_for(entry, &model, catalog),
            pricing: pricing_map(catalog),
            ..EngineConfig::default()
        },
        model_info_for(entry, &model, catalog),
    ))
}

/// Anthropic web-search server-tool version for `model` (#481, follow-up to
/// the hardcoded `_20250305`): the catalog's `ModelEntry::web_search_tool_version`
/// capability flag, or `None` to keep the client's own fallback.
pub(crate) fn web_search_tool_version(
    entry: &ProviderEntry,
    model: &str,
    catalog: &Catalog,
) -> Option<String> {
    catalog
        .model(&entry.name, model)
        .and_then(|m| m.web_search_tool_version.clone())
}

/// Anthropic-wire provider. Always keyed; base from env/catalog else the
/// client's own default (#551, see [`anthropic_factory_for`]).
fn anthropic_wire_config(
    entry: &ProviderEntry,
    http_client: &HttpClient,
    catalog: &Catalog,
    user_config: &config::Config,
) -> Option<(EngineConfig, ModelInfo)> {
    let model = resolve_model(entry, user_config);
    let llm_factory = anthropic_factory_for(
        entry,
        &model,
        http_client,
        catalog,
        web_search_config(user_config),
        web_search_tool_version(entry, &model, catalog),
        None,
    )
    .ok()?;
    eprintln!("skutter: provider={} model={model}", entry.name);
    Some((
        EngineConfig {
            llm_factory,
            default_model: Some(model.clone()),
            generation: generation_for(entry, &model, catalog),
            pricing: pricing_map(catalog),
            ..EngineConfig::default()
        },
        model_info_for(entry, &model, catalog),
    ))
}

/// Gemini-wire provider (#309). Always keyed; base is the client's own default
/// (`GEMINI_BASE`) unless overridden. Gemini has no provider-side web-search knob,
/// so — unlike the OpenAI/Anthropic arms — none is threaded here.
fn gemini_wire_config(
    entry: &ProviderEntry,
    http_client: &HttpClient,
    catalog: &Catalog,
    user_config: &config::Config,
) -> Option<(EngineConfig, ModelInfo)> {
    let model = resolve_model(entry, user_config);
    let llm_factory = gemini_factory_for(entry, &model, http_client, catalog, None).ok()?;
    eprintln!("skutter: provider={} model={model}", entry.name);
    Some((
        EngineConfig {
            llm_factory,
            default_model: Some(model.clone()),
            generation: generation_for(entry, &model, catalog),
            pricing: pricing_map(catalog),
            ..EngineConfig::default()
        },
        model_info_for(entry, &model, catalog),
    ))
}

/// OpenAI Responses API wire (P7, ADR-0196 §3). Key/base resolution mirrors
/// [`openai_wire_config`] (same `key_env`/`{NAME}_API_BASE` precedence); no
/// provider-side web search or `prompt_cache_key` hint — see
/// `entanglement_provider::openai_responses`'s module doc for why this
/// client doesn't (yet) carry those OpenAI-compat-only knobs.
fn openai_responses_wire_config(
    entry: &ProviderEntry,
    http_client: &HttpClient,
    catalog: &Catalog,
    user_config: &config::Config,
) -> Option<(EngineConfig, ModelInfo)> {
    let model = resolve_model(entry, user_config);
    let llm_factory =
        openai_responses_factory_for(entry, &model, http_client, catalog, None).ok()?;
    eprintln!("skutter: provider={} model={model}", entry.name);
    Some((
        EngineConfig {
            llm_factory,
            default_model: Some(model.clone()),
            generation: generation_for(entry, &model, catalog),
            pricing: pricing_map(catalog),
            ..EngineConfig::default()
        },
        model_info_for(entry, &model, catalog),
    ))
}

fn echo_model_info() -> ModelInfo {
    ModelInfo {
        id: "echo".to_string(),
        display_name: "Echo (debug)".to_string(),
        context_window: None,
    }
}

fn echo_config() -> (EngineConfig, ModelInfo) {
    eprintln!("skutter: provider=echo (history-debugging stub)");
    (EngineConfig::default(), echo_model_info())
}

#[cfg(test)]
mod tests {
    fn test_entry(name: &str) -> entanglement_provider::ProviderEntry {
        entanglement_provider::ProviderEntry {
            name: name.to_string(),
            wire: Default::default(),
            base_url: None,
            key_env: None,
            oauth: None,
            rpm: None,
            concurrency: None,
            default_model: "test-model".to_string(),
            models: Vec::new(),
            mcp_servers: Default::default(),
            prompt_cache_key: false,
            thinking_control: None,
            discovery: None,
        }
    }

    #[test]
    fn resolve_concurrency_falls_back_to_catalog_then_none() {
        let mut entry = test_entry("resolveconcurrencyfallback");
        assert_eq!(super::resolve_concurrency(&entry), None);
        entry.concurrency = Some(7);
        assert_eq!(super::resolve_concurrency(&entry), Some(7));
    }

    #[test]
    fn resolve_concurrency_env_overrides_catalog() {
        let entry_name = "resolveconcurrencyenvoverride";
        let mut entry = test_entry(entry_name);
        entry.concurrency = Some(7);
        std::env::set_var(format!("{}_CONCURRENCY", entry_name.to_uppercase()), "12");
        assert_eq!(super::resolve_concurrency(&entry), Some(12));
        std::env::remove_var(format!("{}_CONCURRENCY", entry_name.to_uppercase()));
    }
}
