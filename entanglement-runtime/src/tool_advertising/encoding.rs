//! Per-wire encoding of `ToolSearch` mode (ADR-0196 §3) — split out of
//! `tool_advertising/mod.rs` along the 400-line file cap, mirroring
//! `discovered.rs`'s split.

use entanglement_core::{Catalog, Wire};

/// How a discovered tool's schema actually *reaches* the model, and how much
/// of the advertised array a request re-sends. Derived from the session's
/// resolved wire — a capability, not a user-visible knob — and resolved at
/// the same pin as the advertising mode itself, for the same reason
/// (switching mid-session would bust the cache stability either exists to
/// protect).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    /// OpenAI-compat (incl. z.ai, the priority target), Ollama, Gemini:
    /// the session's `discovery` strategy (ADR-0204) decides — `append`
    /// grows the advertised array per `describe()`, `native_first`/`invoke`
    /// keep it fixed and advertise the `invoke` envelope.
    ClientSide,
    /// Anthropic `/v1/messages`: non-kernel, non-discovered tools carry
    /// `defer_loading: true`; `describe()`'s reply carries `tool_reference`
    /// content blocks the API auto-expands.
    AnthropicNative,
    /// OpenAI Responses API (P7): non-kernel, non-discovered tools carry
    /// `defer_loading: true` exactly like `AnthropicNative` — same
    /// `mark_defer_loading` flagging, shared rather than duplicated (ADR-0196
    /// §3) — but discovery itself rides the wire's native client-executed
    /// `tool_search` primitive instead of a describe-carried content block:
    /// a streamed `tool_search_call` is answered by the runtime's
    /// `discover::tool_search` dispatch (reusing the same explore/describe
    /// lookup), never `describe` directly.
    ResponsesNative,
}

impl Default for Encoding {
    /// The safe fallback for an unresolvable wire (an unknown provider in a
    /// user `providers.yml`, or a test harness with no catalog wired): the
    /// encoding requiring no wire-specific support at all.
    fn default() -> Self {
        Encoding::ClientSide
    }
}

impl Encoding {
    pub fn label(self) -> &'static str {
        match self {
            Encoding::ClientSide => "client_side",
            Encoding::AnthropicNative => "anthropic_native",
            Encoding::ResponsesNative => "responses_native",
        }
    }

    fn from_wire(wire: Wire) -> Self {
        match wire {
            Wire::Anthropic => Encoding::AnthropicNative,
            Wire::OpenaiResponses => Encoding::ResponsesNative,
            Wire::Openai | Wire::Gemini => Encoding::ClientSide,
        }
    }
}

/// Resolve a provider name's `ToolSearch`-mode encoding from the catalog's
/// `wire:` tag. A catalog miss (unknown provider) contributes no preference,
/// same as `resolve_advertising`'s treatment of an unlisted model.
pub(super) fn resolve_encoding(catalog: Option<&Catalog>, provider: &str) -> Encoding {
    catalog
        .and_then(|c| c.provider(provider))
        .map(|p| Encoding::from_wire(p.wire))
        .unwrap_or_default()
}

/// [`resolve_encoding`] by model id alone (mirrors
/// `resolve_advertising_by_id`) — finds the provider that lists this model
/// id, since a bare model id carries no provider name of its own. On a
/// cross-provider id collision the first match wins, same caveat as the
/// advertising-mode counterpart.
pub(super) fn resolve_encoding_by_id(catalog: Option<&Catalog>, model: &str) -> Encoding {
    catalog
        .and_then(|c| {
            c.providers
                .iter()
                .find(|p| p.models.iter().any(|m| m.id == model))
        })
        .map(|p| Encoding::from_wire(p.wire))
        .unwrap_or_default()
}
