//! Per-wire [`LlmFactory`] builders shared by startup and the live
//! `SetModel` resolver (#218), so both bind wire/base/key identically.

use std::sync::Arc;

use entanglement_provider::{
    Catalog, HttpClient, LlmFactory, ModelResolver, ProviderEntry, ResolvedModel, WebSearchConfig,
    Wire,
};

use crate::providers::{env_nonempty, resolve_concurrency, resolve_rpm, web_search_tool_version};

/// The OAuth bearer source for a catalog entry declaring `oauth:` (#684 edge
/// d): tokens come from the managed LLM token file
/// (`llm-tokens.yml` / `ENTANGLEMENT_LLM_TOKENS_FILE`), keyed by the provider
/// name, minted by `skutter config connect <provider>`. `Err` with that hint
/// when nothing is stored yet — an OAuth endpoint without a token could only
/// ever 401, so the miss surfaces here exactly like an unset `key_env`.
fn llm_oauth_source(
    entry: &ProviderEntry,
) -> Result<Option<Arc<dyn entanglement_core::AccessTokenSource>>, String> {
    if entry.oauth.is_none() {
        return Ok(None);
    }
    let store = entanglement_runtime::config::McpTokenStore::load_llm();
    if !store.has(&entry.name) {
        return Err(format!(
            "provider `{}` is OAuth-protected and not yet authorized — run \
             `skutter config connect {}`",
            entry.name, entry.name
        ));
    }
    Ok(Some(Arc::new(entanglement_core::StoredTokenSource::new(
        entry.name.clone(),
        Arc::new(store) as Arc<dyn entanglement_core::TokenStore>,
    ))))
}

/// Build a Gemini-wire [`LlmFactory`] for an explicit `(entry, model)`. Shared by
/// startup and the live-switch resolver (#218). Always keyed; `Err(message)` when
/// the key env is absent/unset. Base from `{NAME}_API_BASE`/`{NAME}_BASE` env else
/// `entry.base_url` else the client's [`GEMINI_BASE`] default.
///
/// `explicit_key`, when `Some`, is used verbatim instead of reading
/// `entry.key_env` from the process environment — the multi-user seam (#522):
/// a per-user resolver supplies the user's own key from its in-memory store,
/// never placing it in `std::env`. `None` (every single-user call site) keeps
/// the original env-lookup behavior byte-identical.
pub(crate) fn gemini_factory_for(
    entry: &ProviderEntry,
    model: &str,
    http_client: &HttpClient,
    catalog: &Catalog,
    explicit_key: Option<&str>,
) -> Result<LlmFactory, String> {
    let auth = llm_oauth_source(entry)?;
    let key = match (&auth, explicit_key) {
        // The bearer replaces the static key on the wire (#684 edge d).
        (Some(_), _) => String::new(),
        (None, Some(k)) => k.to_string(),
        (None, None) => {
            let key_env = entry
                .key_env
                .as_deref()
                .ok_or_else(|| format!("provider `{}` has no API key env", entry.name))?;
            env_nonempty(key_env).ok_or_else(|| format!("{key_env} is not set"))?
        }
    };
    let name = entry.name.to_uppercase();
    let base = env_nonempty(&format!("{name}_API_BASE"))
        .or_else(|| env_nonempty(&format!("{name}_BASE")))
        .or_else(|| entry.base_url.clone())
        .unwrap_or_else(|| entanglement_provider::GEMINI_BASE.to_string());
    Ok(entanglement_provider::gemini_factory(
        base,
        key,
        auth,
        model.to_string(),
        resolve_rpm(entry),
        resolve_concurrency(entry),
        catalog.model_concurrency_resolver(&entry.name),
        http_client.clone(),
    ))
}

/// Build an OpenAI-compat [`LlmFactory`] for an explicit `(entry, model)`.
/// Shared by startup ([`openai_wire_config`]) and the live-switch resolver
/// ([`build_model_resolver`], #218). `Err(message)` when a keyed provider's API
/// key is unset — startup maps it to a skip, the switch surfaces it to the head.
///
/// `explicit_key`, when `Some`, is used verbatim instead of reading
/// `entry.key_env` from the process environment (#522 multi-user seam,
/// mirroring [`gemini_factory_for`]).
pub(crate) fn openai_factory_for(
    entry: &ProviderEntry,
    model: &str,
    http_client: &HttpClient,
    catalog: &Catalog,
    web_search: Option<WebSearchConfig>,
    explicit_key: Option<&str>,
) -> Result<LlmFactory, String> {
    let auth = llm_oauth_source(entry)?;
    let key = match (&auth, explicit_key) {
        // The bearer replaces the static key on the wire (#684 edge d).
        (Some(_), _) => None,
        (None, Some(k)) => Some(k.to_string()),
        (None, None) => match &entry.key_env {
            Some(k) => Some(
                env_nonempty(k)
                    .ok_or_else(|| format!("{k} is not set for provider `{}`", entry.name))?,
            ),
            None => None, // keyless (Ollama)
        },
    };
    let name = entry.name.to_uppercase();
    let base = env_nonempty(&format!("{name}_API_BASE"))
        .or_else(|| env_nonempty(&format!("{name}_BASE")))
        .or_else(|| entry.base_url.clone())
        .unwrap_or_else(|| entanglement_provider::OPENAI_BASE.to_string());
    Ok(entanglement_provider::openai_factory(
        base,
        key,
        auth,
        model.to_string(),
        resolve_rpm(entry),
        resolve_concurrency(entry),
        catalog.model_concurrency_resolver(&entry.name),
        web_search,
        entry.prompt_cache_key,
        catalog.thinking_spec_resolver(&entry.name),
        http_client.clone(),
    ))
}

/// Build an OpenAI Responses-wire [`LlmFactory`] for an explicit `(entry,
/// model)` (P7). Shared by startup ([`openai_responses_wire_config`]) and
/// the live-switch resolver ([`build_model_resolver`]). Key/base resolution
/// mirrors [`openai_factory_for`] exactly; `explicit_key` mirrors its
/// multi-user seam too.
pub(crate) fn openai_responses_factory_for(
    entry: &ProviderEntry,
    model: &str,
    http_client: &HttpClient,
    catalog: &Catalog,
    explicit_key: Option<&str>,
) -> Result<LlmFactory, String> {
    let auth = llm_oauth_source(entry)?;
    let key = match (&auth, explicit_key) {
        (Some(_), _) => None,
        (None, Some(k)) => Some(k.to_string()),
        (None, None) => match &entry.key_env {
            Some(k) => Some(
                env_nonempty(k)
                    .ok_or_else(|| format!("{k} is not set for provider `{}`", entry.name))?,
            ),
            None => None,
        },
    };
    let name = entry.name.to_uppercase();
    let base = env_nonempty(&format!("{name}_API_BASE"))
        .or_else(|| env_nonempty(&format!("{name}_BASE")))
        .or_else(|| entry.base_url.clone())
        .unwrap_or_else(|| entanglement_provider::OPENAI_RESPONSES_BASE.to_string());
    Ok(entanglement_provider::openai_responses_factory(
        base,
        key,
        auth,
        model.to_string(),
        resolve_rpm(entry),
        resolve_concurrency(entry),
        catalog.model_concurrency_resolver(&entry.name),
        catalog.thinking_spec_resolver(&entry.name),
        http_client.clone(),
    ))
}

/// Build an Anthropic-wire [`LlmFactory`] for an explicit `(entry, model)`.
/// Shared by startup and the live-switch resolver (#218). Always keyed;
/// `Err(message)` when the key env is absent/unset. `web_search_tool_version`
/// selects the server-tool type (#481) when web search is enabled. Base from
/// `{NAME}_API_BASE`/`{NAME}_BASE` env else `entry.base_url` else the client's
/// own [`entanglement_provider::ANTHROPIC_BASE`] default — mirroring
/// [`openai_factory_for`]/[`gemini_factory_for`] (#551; before this a
/// catalog `base_url` on an `anthropic`-wire entry was silently ignored).
///
/// `explicit_key`, when `Some`, is used verbatim instead of reading
/// `entry.key_env` from the process environment (#522 multi-user seam,
/// mirroring [`gemini_factory_for`]).
#[allow(clippy::too_many_arguments)]
pub(crate) fn anthropic_factory_for(
    entry: &ProviderEntry,
    model: &str,
    http_client: &HttpClient,
    catalog: &Catalog,
    web_search: Option<WebSearchConfig>,
    web_search_tool_version: Option<String>,
    explicit_key: Option<&str>,
) -> Result<LlmFactory, String> {
    let auth = llm_oauth_source(entry)?;
    let key = match (&auth, explicit_key) {
        // The bearer replaces the static key on the wire (#684 edge d).
        (Some(_), _) => String::new(),
        (None, Some(k)) => k.to_string(),
        (None, None) => {
            let key_env = entry
                .key_env
                .as_deref()
                .ok_or_else(|| format!("provider `{}` has no API key env", entry.name))?;
            env_nonempty(key_env).ok_or_else(|| format!("{key_env} is not set"))?
        }
    };
    let name = entry.name.to_uppercase();
    let base = env_nonempty(&format!("{name}_API_BASE"))
        .or_else(|| env_nonempty(&format!("{name}_BASE")))
        .or_else(|| entry.base_url.clone())
        .unwrap_or_else(|| entanglement_provider::ANTHROPIC_BASE.to_string());
    Ok(entanglement_provider::anthropic_factory(
        base,
        key,
        auth,
        model.to_string(),
        resolve_rpm(entry),
        resolve_concurrency(entry),
        catalog.model_concurrency_resolver(&entry.name),
        web_search,
        web_search_tool_version,
        // Thinking shape, replay, effort tiers, temperature support — resolved
        // inside the factory builder (not threaded like
        // `web_search_tool_version`) because they apply to every request. An
        // unlisted model gets `AnthropicModelSpec::default()`: budget shape,
        // replay on (the API requires the block back on a tool round-trip).
        catalog.anthropic_model_spec(&entry.name, model),
        http_client.clone(),
    ))
}

/// Build the live model/provider resolver the engine calls on `SetModel` (#218).
/// Captures the catalog + the per-endpoint HTTP client (clients stay warm across
/// switches, #217) and re-runs the same wire/base/key resolution as startup for
/// an explicit `(provider, model)` — so a mid-session switch binds exactly like a
/// fresh launch would, minus the model-default fallback (the model is chosen by
/// the head). An unknown provider / missing key comes back as `Err` for the head
/// to surface. The opt-in [`WebSearchConfig`] (#305) is captured too, so a live
/// switch binds provider-side web search exactly as startup did.
pub(crate) fn build_model_resolver(
    catalog: Catalog,
    http_client: HttpClient,
    web_search: Option<WebSearchConfig>,
) -> ModelResolver {
    // Single-user mode: every session shares this one process-global catalog +
    // client, so the resolving session's user (#522) is simply ignored — a
    // multi-user embedder wires `entanglement_provider::multi_user::
    // build_user_model_resolver` onto this seam instead (ADR-0184).
    Arc::new(move |_user, provider: &str, model: &str| {
        resolve_catalog_entry(
            &catalog,
            &http_client,
            web_search.clone(),
            provider,
            model,
            None,
        )
    })
}

/// Resolve `(provider, model)` against `catalog` into a [`ResolvedModel`] — the
/// wire-dispatch body shared by the single-user [`build_model_resolver`] and the
/// multi-user [`multi_user::build_user_model_resolver`]. `explicit_key`, when
/// `Some`, is threaded to the `*_factory_for` builders instead of letting them
/// fall back to reading `entry.key_env` from the process environment (#522).
fn resolve_catalog_entry(
    catalog: &Catalog,
    http_client: &HttpClient,
    web_search: Option<WebSearchConfig>,
    provider: &str,
    model: &str,
    explicit_key: Option<&str>,
) -> Result<ResolvedModel, String> {
    let entry = catalog
        .provider(provider)
        .ok_or_else(|| format!("unknown provider `{provider}`"))?;
    let llm_factory = match entry.wire {
        Wire::Openai => {
            openai_factory_for(entry, model, http_client, catalog, web_search, explicit_key)?
        }
        Wire::Anthropic => anthropic_factory_for(
            entry,
            model,
            http_client,
            catalog,
            web_search,
            web_search_tool_version(entry, model, catalog),
            explicit_key,
        )?,
        Wire::Gemini => gemini_factory_for(entry, model, http_client, catalog, explicit_key)?,
        Wire::OpenaiResponses => {
            openai_responses_factory_for(entry, model, http_client, catalog, explicit_key)?
        }
    };
    Ok(ResolvedModel {
        provider: entry.name.clone(),
        model: model.to_string(),
        llm_factory,
        generation: catalog
            .model(provider, model)
            .map(|m| m.generation_params()),
        context_window: catalog
            .model(provider, model)
            .and_then(|m| m.context_window)
            .map(|w| w as usize),
    })
}
