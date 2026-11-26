//! Construction for [`AnthropicLlm`]: the constructor and
//! [`anthropic_factory`] — split out of `mod.rs` along the 400-line file cap.

use crate::catalog::AnthropicModelSpec;
use crate::client::HttpClient;
use crate::{Llm, ModelConcurrencyResolver, WebSearchConfig};

use super::{AnthropicLlm, DEFAULT_MAX_TOKENS};

impl AnthropicLlm {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        default_model: impl Into<String>,
        rpm: Option<u32>,
        concurrency: Option<usize>,
        model_concurrency: ModelConcurrencyResolver,
        web_search: Option<WebSearchConfig>,
        web_search_tool_version: Option<String>,
        model_spec: AnthropicModelSpec,
        http: HttpClient,
    ) -> Self {
        Self {
            api_key: api_key.into(),
            base_url: base_url.into(),
            default_model: default_model.into(),
            default_max_tokens: DEFAULT_MAX_TOKENS,
            rpm,
            concurrency,
            model_concurrency,
            web_search,
            web_search_tool_version,
            model_spec,
            http,
        }
    }
}

/// Build an [`LlmFactory`] wired to Anthropic. Each session gets its own cloned
/// [`AnthropicLlm`]. `base_url` overrides [`ANTHROPIC_BASE`] — a proxy/gateway
/// catalog entry (#551); `rpm`/`concurrency = None` use the client's (or
/// endpoint's) defaults; `model_concurrency` resolves a tighter per-model cap
/// per request (`|_| None` disables it, #521, resolved per request rather than
/// once at construction, #550); `web_search = Some(..)` requests provider-side
/// web search (#305); `web_search_tool_version` selects the server-tool type
/// when set (#481); `model_spec` carries the bound model's thinking shape,
/// replay flag, effort tiers and temperature support
/// (`Catalog::anthropic_model_spec`, resolved once for `default_model`).
#[allow(clippy::too_many_arguments)]
pub fn anthropic_factory(
    base_url: impl Into<String>,
    api_key: impl Into<String>,
    default_model: impl Into<String>,
    rpm: Option<u32>,
    concurrency: Option<usize>,
    model_concurrency: ModelConcurrencyResolver,
    web_search: Option<WebSearchConfig>,
    web_search_tool_version: Option<String>,
    model_spec: AnthropicModelSpec,
    http: HttpClient,
) -> crate::LlmFactory {
    let llm = AnthropicLlm::new(
        base_url,
        api_key,
        default_model,
        rpm,
        concurrency,
        model_concurrency,
        web_search,
        web_search_tool_version,
        model_spec,
        http,
    );
    std::sync::Arc::new(move || Box::new(llm.clone()) as Box<dyn Llm>)
}
