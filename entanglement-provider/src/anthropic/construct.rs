//! Construction for [`AnthropicLlm`]: the constructor, the OAuth-bearer
//! opt-in, and [`anthropic_factory`] — split out of `mod.rs` along the
//! 400-line file cap (#684 grew the parent with the bearer request loop).

use crate::catalog::ThinkingStyle;
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
        thinking_style: ThinkingStyle,
        replay_thinking: bool,
        http: HttpClient,
    ) -> Self {
        Self {
            api_key: api_key.into(),
            auth: None,
            base_url: base_url.into(),
            default_model: default_model.into(),
            default_max_tokens: DEFAULT_MAX_TOKENS,
            rpm,
            concurrency,
            model_concurrency,
            web_search,
            web_search_tool_version,
            thinking_style,
            replay_thinking,
            http,
        }
    }

    /// Authenticate with an OAuth bearer from `auth` instead of the static
    /// `x-api-key` (#684 edge d) — see the field docs for the pool-identity
    /// rule. The `api_key` passed at construction is ignored on the wire then
    /// (pass an empty string for a purely OAuth endpoint).
    pub fn with_auth(
        mut self,
        auth: std::sync::Arc<dyn crate::mcp::auth::AccessTokenSource>,
    ) -> Self {
        self.auth = Some(auth);
        self
    }
}

/// Build an [`LlmFactory`] wired to Anthropic. Each session gets its own cloned
/// [`AnthropicLlm`]. `base_url` overrides [`ANTHROPIC_BASE`] — a proxy/gateway
/// catalog entry (#551); `rpm`/`concurrency = None` use the client's (or
/// endpoint's) defaults; `model_concurrency` resolves a tighter per-model cap
/// per request (`|_| None` disables it, #521, resolved per request rather than
/// once at construction, #550); `web_search = Some(..)` requests provider-side
/// web search (#305); `web_search_tool_version` selects the server-tool type
/// when set (#481); `thinking_style` picks the extended-thinking request shape
/// the bound model accepts.
/// `auth = Some(..)` switches the endpoint to an OAuth bearer (#684 edge d),
/// replacing `x-api-key` on the wire (pass an empty `api_key` then).
#[allow(clippy::too_many_arguments)]
pub fn anthropic_factory(
    base_url: impl Into<String>,
    api_key: impl Into<String>,
    auth: Option<std::sync::Arc<dyn crate::mcp::auth::AccessTokenSource>>,
    default_model: impl Into<String>,
    rpm: Option<u32>,
    concurrency: Option<usize>,
    model_concurrency: ModelConcurrencyResolver,
    web_search: Option<WebSearchConfig>,
    web_search_tool_version: Option<String>,
    thinking_style: ThinkingStyle,
    replay_thinking: bool,
    http: HttpClient,
) -> crate::LlmFactory {
    let mut llm = AnthropicLlm::new(
        base_url,
        api_key,
        default_model,
        rpm,
        concurrency,
        model_concurrency,
        web_search,
        web_search_tool_version,
        thinking_style,
        replay_thinking,
        http,
    );
    if let Some(auth) = auth {
        llm = llm.with_auth(auth);
    }
    std::sync::Arc::new(move || Box::new(llm.clone()) as Box<dyn Llm>)
}
