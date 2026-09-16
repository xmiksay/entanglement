//! ADR-0204's client-side `discovery` strategy: config/catalog resolution, the
//! per-session pin, and the one effective read every consumer (resolver,
//! `describe`, prompt note, declines) shares so they can never disagree.
//!
//! Precedence, highest first — the advertising-mode chain minus its env tier:
//!
//! 1. `config.yml` `discovery:`, a map keyed by **provider name**
//!    ([`Config::discovery`]);
//! 2. the model's catalog `discovery:` ([`ModelEntry`][entanglement_core::ModelEntry]);
//! 3. its provider's catalog `discovery:`;
//! 4. `Discovery::Append`.
//!
//! **There is no environment variable** for this knob, deliberately: unlike
//! `ENTANGLEMENT_TOOL_ADVERTISING` it is per provider, so a single process-wide
//! scalar could only ever be wrong for every provider but one. The config map
//! is the per-provider surface, and the TUI `/set` dialog writes into it
//! ([`crate::config::write_key`]).

use entanglement_core::{Catalog, Discovery, SessionId, ToolAdvertising};

use super::{AdvertisingState, Encoding, SessionToolAdvertising};
use crate::config::Config;

/// Resolve a session's strategy under the full chain (module doc): the user
/// config's per-provider entry, else the model's catalog `discovery`, else its
/// provider's, else `Append`. The provider is taken by name when known, else
/// the one listing the bare model id (the `encoding` lookup's two tiers).
pub(super) fn resolve_discovery(
    config: &Config,
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
    // The config tier is keyed by provider *name*, so a session known only by
    // a bare model id borrows the name from the catalog entry that lists it.
    // Consulted before — and independently of — the catalog tiers: a user may
    // key an entry to a provider only their own `providers.yml` adds, or to
    // one that carries no `discovery:` at all, and it must still win.
    let name = provider.or_else(|| entry.map(|e| e.name.as_str()));
    if let Some(configured) = name.and_then(|n| config.discovery.get(n)) {
        return *configured;
    }
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

    /// A config with just the `discovery:` map set, everything else at the
    /// embedded defaults.
    fn config_with(entries: &[(&str, Discovery)]) -> Config {
        Config {
            discovery: entries
                .iter()
                .map(|(name, d)| (name.to_string(), *d))
                .collect(),
            ..crate::config::bare_config()
        }
    }

    #[test]
    fn provider_decides_and_a_model_entry_overrides_it() {
        let c = catalog();
        let cfg = config_with(&[]);
        let r = |p, m| resolve_discovery(&cfg, Some(&c), p, m);
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
        let cfg = config_with(&[]);
        let r = |p, m| resolve_discovery(&cfg, Some(&c), p, m);
        assert_eq!(r(Some("silent"), Some("y")), Discovery::Append);
        assert_eq!(r(Some("nope"), Some("x")), Discovery::Append);
        assert_eq!(r(None, Some("nope")), Discovery::Append);
        assert_eq!(r(None, None), Discovery::Append);
        assert_eq!(
            resolve_discovery(&cfg, None, Some("local"), Some("x")),
            Discovery::Append
        );
    }

    #[test]
    fn the_config_map_wins_over_both_catalog_tiers() {
        let c = catalog();
        // `local` is `invoke` provider-wide and `native_first` on `flash`;
        // a config entry outranks both.
        let cfg = config_with(&[("local", Discovery::Append)]);
        let r = |p, m| resolve_discovery(&cfg, Some(&c), p, m);
        assert_eq!(r(Some("local"), Some("x")), Discovery::Append);
        assert_eq!(r(Some("local"), Some("flash")), Discovery::Append);
        // And it supplies a strategy where the catalog is silent.
        let cfg = config_with(&[("silent", Discovery::Invoke)]);
        assert_eq!(
            resolve_discovery(&cfg, Some(&c), Some("silent"), Some("y")),
            Discovery::Invoke
        );
    }

    #[test]
    fn a_bare_model_id_picks_up_its_providers_config_entry() {
        let c = catalog();
        let cfg = config_with(&[("local", Discovery::Append)]);
        // No provider named: `flash` is listed by `local`, whose config entry
        // therefore applies — beating the model's own `native_first`.
        assert_eq!(
            resolve_discovery(&cfg, Some(&c), None, Some("flash")),
            Discovery::Append
        );
    }

    #[test]
    fn an_entry_for_another_provider_is_inert() {
        let c = catalog();
        let cfg = config_with(&[("elsewhere", Discovery::Append)]);
        // The session is on `local`, so the `elsewhere` key changes nothing:
        // the catalog still decides.
        assert_eq!(
            resolve_discovery(&cfg, Some(&c), Some("local"), Some("flash")),
            Discovery::NativeFirst
        );
    }

    #[test]
    fn a_provider_the_catalog_never_heard_of_still_reads_its_config_entry() {
        let c = catalog();
        let cfg = config_with(&[("home-grown", Discovery::Invoke)]);
        // A user `providers.yml` entry this test catalog doesn't carry: the
        // config tier is looked up by name, not via the catalog, so it wins.
        assert_eq!(
            resolve_discovery(&cfg, Some(&c), Some("home-grown"), Some("whatever")),
            Discovery::Invoke
        );
        // Same with no catalog at all.
        assert_eq!(
            resolve_discovery(&cfg, None, Some("home-grown"), None),
            Discovery::Invoke
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
