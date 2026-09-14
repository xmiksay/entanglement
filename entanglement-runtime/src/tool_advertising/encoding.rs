//! Per-wire encoding of `ToolSearch` mode (ADR-0196 §3) — split out of
//! `tool_advertising/mod.rs` along the 400-line file cap, mirroring
//! `discovered.rs`'s split.

use entanglement_core::{Catalog, SessionId, Wire};

/// How a discovered tool's schema actually *reaches* the model, and how much
/// of the advertised array a request re-sends. Derived from the session's
/// resolved wire — a capability, not a user-visible knob — and resolved at
/// the same pin as the advertising mode itself, for the same reason
/// (switching mid-session would bust the cache stability either exists to
/// protect).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    /// OpenAI-compat (incl. z.ai, the priority target), Ollama, Gemini:
    /// `describe()` appends the described tool's full spec into the
    /// session's advertised array, append-only.
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

/// Resolve ADR-0200's `advertise_discovered` catalog knob for a
/// `(provider, model)` pair — by provider name when known, else by the
/// provider that lists a bare model id (mirrors [`resolve_encoding`]/
/// [`resolve_encoding_by_id`]'s two-tier lookup, collapsed into one function
/// since both branches just read one `Option<bool>` field). `true` (today's
/// behavior) when neither is known or the catalog is silent.
pub(super) fn resolve_advertise_discovered_pair(
    catalog: Option<&Catalog>,
    provider: Option<&str>,
    model: Option<&str>,
) -> bool {
    let entry = match (provider, model) {
        (Some(p), _) => catalog.and_then(|c| c.provider(p)),
        (None, Some(m)) => catalog.and_then(|c| {
            c.providers
                .iter()
                .find(|p| p.models.iter().any(|mm| mm.id == m))
        }),
        (None, None) => None,
    };
    entry.and_then(|p| p.advertise_discovered).unwrap_or(true)
}

impl super::SessionToolAdvertising {
    /// The session's pinned `advertise_discovered` bit (ADR-0200), or `None`
    /// before `pin` has run — same "not a hole in practice" contract as
    /// [`get`][super::SessionToolAdvertising::get].
    pub fn get_advertise_discovered(&self, session: &SessionId) -> Option<bool> {
        self.modes.get(session).map(|p| p.advertise_discovered)
    }

    /// Override the `true`-by-`pin` default (ADR-0200), called right after
    /// `pin` by [`AdvertisingInputs::pin_session_start`][super::AdvertisingInputs::pin_session_start].
    /// A no-op for a session `pin` hasn't reached yet.
    pub fn set_advertise_discovered(&mut self, session: &SessionId, value: bool) {
        if let Some(p) = self.modes.get_mut(session) {
            p.advertise_discovered = value;
        }
    }
}

impl super::AdvertisingState {
    /// This session's pinned `advertise_discovered` bit (ADR-0200): whether a
    /// `client_side` `ToolSearch` session may grow the advertised array via
    /// `describe()`/overlay discovery. Same default-`true` "not a hole in
    /// practice" contract as [`mode`][super::AdvertisingState::mode] —
    /// meaningless (never consulted) outside `ToolSearch`/`ClientSide`.
    pub fn advertise_discovered(&self, session: &SessionId) -> bool {
        self.modes
            .lock()
            .expect("tool-advertising mode mutex poisoned")
            .get_advertise_discovered(session)
            .unwrap_or(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog_with(advertise_discovered: bool) -> Catalog {
        serde_yaml::from_str(&format!(
            "providers:\n\
             \x20 - name: local\n\
             \x20   default_model: x\n\
             \x20   advertise_discovered: {advertise_discovered}\n\
             \x20   models:\n\
             \x20     - id: x\n"
        ))
        .expect("valid catalog yaml")
    }

    #[test]
    fn resolves_by_provider_name_then_falls_back_to_true() {
        let catalog = catalog_with(false);
        assert!(!resolve_advertise_discovered_pair(
            Some(&catalog),
            Some("local"),
            Some("x")
        ));
        assert!(resolve_advertise_discovered_pair(
            Some(&catalog),
            Some("unknown"),
            Some("x")
        ));
        assert!(resolve_advertise_discovered_pair(None, None, None));
    }

    #[test]
    fn resolves_by_bare_model_id_when_provider_is_unknown() {
        let catalog = catalog_with(false);
        assert!(!resolve_advertise_discovered_pair(
            Some(&catalog),
            None,
            Some("x")
        ));
        assert!(resolve_advertise_discovered_pair(
            Some(&catalog),
            None,
            Some("nope")
        ));
    }
}
