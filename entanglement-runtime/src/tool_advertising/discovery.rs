//! ADR-0204's client-side `discovery` strategy: catalog resolution, the
//! per-session pin, and the one effective read every consumer (resolver,
//! `describe`, prompt note, declines) shares so they can never disagree.
//!
//! Precedence, highest first — **catalog data only** (ADR-0213 retired the
//! `config.yml` `discovery:` override map: the strategy is a fact about what a
//! wire accepts, not a user preference):
//!
//! 1. the model's catalog `discovery:` ([`ModelEntry`][entanglement_core::ModelEntry]);
//! 2. its provider's catalog `discovery:`;
//! 3. `Discovery::Append`.
//!
//! The TUI `/set` dialog can still re-pin a running session's strategy live
//! ([`super::repin`]); that never persists.

use entanglement_core::{Catalog, Discovery, SessionId, ToolAdvertising};

use super::{AdvertisingState, Encoding, SessionToolAdvertising};

/// Resolve a session's strategy under the chain (module doc): the model's
/// catalog `discovery`, else its provider's, else `Append`. The provider is
/// taken by name when known, else the one listing the bare model id (the
/// `encoding` lookup's two tiers).
pub(super) fn resolve_discovery(
    catalog: Option<&Catalog>,
    provider: Option<&str>,
    model: Option<&str>,
) -> Discovery {
    let entry = catalog.and_then(|c| match (provider, model) {
        (Some(p), _) => c.provider(p),
        (None, Some(m)) => c
            .providers
            .iter()
            .find(|p| p.models.iter().any(|e| e.id == m)),
        (None, None) => None,
    });
    let Some(entry) = entry else {
        return Discovery::default();
    };
    model
        .and_then(|m| entry.models.iter().find(|e| e.id == m))
        .and_then(|e| e.discovery)
        .or(entry.discovery)
        .unwrap_or_default()
}

impl SessionToolAdvertising {
    /// The session's pinned strategy, or `None` before `pin`.
    pub fn get_discovery(&self, session: &SessionId) -> Option<Discovery> {
        self.modes.get(session).map(|p| p.discovery)
    }

    /// Override `pin`'s `Append` default; a no-op for an unpinned session.
    pub fn set_discovery(&mut self, session: &SessionId, value: Discovery) {
        if let Some(p) = self.modes.get_mut(session) {
            p.discovery = value;
        }
    }
}

impl AdvertisingState {
    /// The strategy that actually applies: the pin under `ToolSearch` +
    /// `client_side`, else `Append` — `Full` advertises everything and the
    /// native encodings defer on the wire, so neither has an `invoke`
    /// envelope or a tail to grow. Unpinned sessions (test wrappers) read
    /// `Append`, today's behavior.
    pub fn discovery(&self, session: &SessionId) -> Discovery {
        let modes = self
            .modes
            .lock()
            .expect("tool-advertising mode mutex poisoned");
        let applies = modes.get(session) == Some(ToolAdvertising::ToolSearch)
            && modes.get_encoding(session) == Some(Encoding::ClientSide);
        match modes.get_discovery(session) {
            Some(d) if applies => d,
            _ => Discovery::Append,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> Catalog {
        serde_yaml::from_str(
            "providers:\n\
             \x20 - name: local\n\
             \x20   default_model: x\n\
             \x20   discovery: invoke\n\
             \x20   models:\n\
             \x20     - id: x\n\
             \x20     - id: flash\n\
             \x20       discovery: native_first\n\
             \x20 - name: silent\n\
             \x20   default_model: y\n\
             \x20   models:\n\
             \x20     - id: y\n",
        )
        .expect("valid catalog yaml")
    }

    #[test]
    fn provider_decides_and_a_model_entry_overrides_it() {
        let c = catalog();
        let r = |p, m| resolve_discovery(Some(&c), p, m);
        assert_eq!(r(Some("local"), Some("x")), Discovery::Invoke);
        assert_eq!(r(Some("local"), None), Discovery::Invoke);
        assert_eq!(r(Some("local"), Some("flash")), Discovery::NativeFirst);
        // A bare model id finds its provider.
        assert_eq!(r(None, Some("flash")), Discovery::NativeFirst);
        assert_eq!(r(None, Some("x")), Discovery::Invoke);
    }

    #[test]
    fn silence_or_a_miss_resolves_append() {
        let c = catalog();
        let r = |p, m| resolve_discovery(Some(&c), p, m);
        assert_eq!(r(Some("silent"), Some("y")), Discovery::Append);
        assert_eq!(r(Some("nope"), Some("x")), Discovery::Append);
        assert_eq!(r(None, Some("nope")), Discovery::Append);
        assert_eq!(r(None, None), Discovery::Append);
        assert_eq!(
            resolve_discovery(None, Some("local"), Some("x")),
            Discovery::Append
        );
    }

    fn pinned(
        mode: ToolAdvertising,
        encoding: Encoding,
        d: Discovery,
    ) -> (AdvertisingState, SessionId) {
        let state = AdvertisingState::new();
        let s = SessionId::new("s");
        let mut modes = state.modes.lock().unwrap();
        modes.pin(s.clone(), mode, encoding);
        modes.set_discovery(&s, d);
        drop(modes);
        (state, s)
    }

    #[test]
    fn the_effective_read_applies_only_to_client_side_tool_search() {
        let (state, s) = pinned(
            ToolAdvertising::ToolSearch,
            Encoding::ClientSide,
            Discovery::Invoke,
        );
        assert_eq!(state.discovery(&s), Discovery::Invoke);
        let (state, s) = pinned(
            ToolAdvertising::Full,
            Encoding::ClientSide,
            Discovery::Invoke,
        );
        assert_eq!(state.discovery(&s), Discovery::Append);
        let (state, s) = pinned(
            ToolAdvertising::ToolSearch,
            Encoding::AnthropicNative,
            Discovery::NativeFirst,
        );
        assert_eq!(state.discovery(&s), Discovery::Append);
        assert_eq!(
            AdvertisingState::new().discovery(&SessionId::new("unpinned")),
            Discovery::Append
        );
    }
}
