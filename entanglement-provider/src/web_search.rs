//! Provider-side web search configuration (#305, ADR-0075).
//!
//! Both z.ai (OpenAI-compat) and Anthropic can execute a web search **mid-turn,
//! server-side** — the provider runs the search and cites the results, with no
//! client tool round-trip. This is opt-in, **client-construction-time** config:
//! it is bound onto the LLM client when the client is built, never seen by
//! `entanglement-core` (`LlmRequest`/`ToolSpec`/`LlmEvent`/`OutEvent` untouched).
//! Results surface on the existing [`crate::LlmEvent::Reasoning`] channel.
//!
//! Enabling this **is the consent**: a server tool runs provider-side, *outside*
//! the runtime permission ladder (ADR-0047 — the config file is trusted). See
//! ADR-0075 for the MVP limitations (search blocks are not persisted into
//! `Message` history, `pause_turn` ends the turn).

use serde::Deserialize;

/// Opt-in provider-side web search settings, bound onto a client at build time.
///
/// `deny_unknown_fields` makes a typo'd key a loud error, matching the rest of
/// the layered config. All fields default, so the whole section is optional.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebSearchConfig {
    /// Master switch. `false` (the default) ⇒ no web-search tool is requested,
    /// exactly as before. The runtime only hands the client a `Some(..)` when
    /// this is `true`.
    #[serde(default)]
    pub enabled: bool,
    /// Cap the number of server-side searches per turn. `None` ⇒ the provider's
    /// own default.
    #[serde(default)]
    pub max_uses: Option<u32>,
    /// Restrict searches to these domains. Empty ⇒ no restriction.
    #[serde(default)]
    pub allowed_domains: Vec<String>,
}
