//! Tool-advertising resolution + the per-session advertising map (ADR-0196,
//! Phase P1).
//!
//! One knob — `full` (today's full advertised surface) vs `tool_search`
//! (lean kernel + `explore`/`describe` discovery pair, Phase P3, **the
//! default**) — resolved per session. Three inputs, strict precedence (the
//! plan's "Mode selection & plumbing"):
//!
//! 1. env `ENTANGLEMENT_TOOL_ADVERTISING` (`full`/`tool_search`,
//!    case-insensitive; a bad value warns and falls through — the
//!    `ENTANGLEMENT_SESSION_RETENTION_DAYS` house pattern, never fatal);
//! 2. `config.yml` `tool_advertising` (see [`crate::config`]);
//! 3. the model's catalog entry (`ModelEntry.tool_advertising`, settable per
//!    model in `providers.yml`);
//! 4. default `tool_search`.
//!
//! **Per-session, resolved at start.** The resolver and executor are
//! engine-global and session-multiplexed, so the resolved mode is held in a
//! session→mode map ([`SessionToolAdvertising`]) owned by the tool
//! executor's event loop, seeded from the session's initial model. A live
//! `SetModel` keeps the session's mode — switching mid-session would bust
//! the prompt cache the mode exists to protect and strand half-emitted
//! history — but a differing catalog preference on the new model is logged.
//! Subagents are separate sessions with their own `SessionStarted`, so they
//! resolve their own mode at spawn for free. No mid-session switching.
//!
//! Phase P1 is plumbing only: the map is threaded and observable, nothing
//! consumes it yet beyond logging and `skutter inspect config`. With the env
//! var unset, no `tool_advertising` in any config layer, and no catalog
//! entry setting `tool_advertising:`, every resolution is `tool_search` —
//! this is P1's one behavior change (the default flipped from `full`), but
//! since nothing dispatches on the mode yet it changes no observable runtime
//! behavior.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use entanglement_core::{Catalog, ModelEntry, SessionId, ToolAdvertising};

use crate::config::{Config, TOOL_ADVERTISING_ENV};

mod discovered;
pub use discovered::{append_discovered_tail, DiscoveredSet};

mod encoding;
pub use encoding::Encoding;
use encoding::{resolve_encoding, resolve_encoding_by_id};

mod overlay;
pub use overlay::advertise_new_overlay_enables;

/// Which precedence tier won an advertising resolution — reported by
/// `skutter inspect config` so "why is this session tool_search?" has an
/// answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdvertisingSource {
    Env,
    Config,
    Catalog,
    Default,
}

impl AdvertisingSource {
    pub fn label(self) -> &'static str {
        match self {
            AdvertisingSource::Env => "env",
            AdvertisingSource::Config => "config",
            AdvertisingSource::Catalog => "catalog",
            AdvertisingSource::Default => "default",
        }
    }
}

/// The config/env half of the precedence chain, independent of any model:
/// `None` when neither tier is set, so the catalog gets its say. Shared by
/// the per-session resolver and `skutter inspect config` (whose global view
/// has no model to consult).
pub fn configured_advertising(config: &Config) -> Option<ToolAdvertising> {
    if let Some(raw) = std::env::var(TOOL_ADVERTISING_ENV)
        .ok()
        .filter(|s| !s.is_empty())
    {
        // Warn-and-fall-through, not error: a typo'd env var must not kill
        // startup (the retention-env house pattern) — but it must be loud,
        // because the user asked for a mode and silently getting another is
        // the worse failure.
        return match ToolAdvertising::parse(&raw) {
            Some(mode) => Some(mode),
            None => {
                tracing::warn!(
                    "ignoring unparseable {TOOL_ADVERTISING_ENV}={raw:?}; \
                     falling back to config/catalog/default"
                );
                config.tool_advertising
            }
        };
    }
    config.tool_advertising
}

/// Which precedence tier the *global* (model-less) view would answer from —
/// `skutter inspect config`'s provenance line. The catalog tier can only be
/// reached per model, so globally it reports as the default.
pub fn configured_advertising_source(config: &Config) -> AdvertisingSource {
    if std::env::var(TOOL_ADVERTISING_ENV)
        .ok()
        .is_some_and(|v| !v.is_empty() && ToolAdvertising::parse(&v).is_some())
    {
        AdvertisingSource::Env
    } else if config.tool_advertising.is_some() {
        AdvertisingSource::Config
    } else {
        AdvertisingSource::Default
    }
}

/// Resolve a `(provider, model)` pair's tool-advertising mode under the full
/// precedence chain: env > config > that model's catalog entry >
/// `tool_search`. The catalog miss (unknown provider/model — a user
/// `providers.yml` entry can name anything) is not an error: it simply
/// contributes no preference, exactly like an entry that omits
/// `tool_advertising:`.
pub fn resolve_advertising(
    config: &Config,
    catalog: Option<&Catalog>,
    provider: &str,
    model: &str,
) -> ToolAdvertising {
    if let Some(mode) = configured_advertising(config) {
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
    config: &Config,
    catalog: Option<&Catalog>,
    model: &str,
) -> ToolAdvertising {
    if let Some(mode) = configured_advertising(config) {
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

/// The session→mode map (ADR-0196 §2): each live session's tool-advertising
/// mode, resolved once at session start from that session's initial model.
/// Lives in the tool executor's single-threaded event loop, so it needs no
/// synchronization; Phase P3's resolver consults it per session.
///
/// An absent entry means "not yet pinned" — the session's initial model
/// event hasn't been observed (or the map's owner runs without a catalog,
/// e.g. the test wrappers). Readers fall back to the global resolution, which
/// is `tool_search` unless config/env says otherwise, so the map is an
/// *overriding* record, never a hole.
#[derive(Debug, Default)]
pub struct SessionToolAdvertising {
    modes: HashMap<SessionId, Pinned>,
}

/// One session's pinned facts: the advertising mode plus the encoding its
/// wire resolved to — both fixed together at session start, both untouched by
/// a later `SetModel` (§2/§3), so there's exactly one pin call per session.
#[derive(Debug, Clone, Copy)]
struct Pinned {
    mode: ToolAdvertising,
    encoding: Encoding,
    /// ADR-0200; defaulted `true` by `pin`, set via `set_advertise_discovered`.
    advertise_discovered: bool,
}

impl SessionToolAdvertising {
    pub fn new() -> Self {
        Self::default()
    }

    /// Pin a session's mode + encoding. Called exactly once per session, at
    /// start.
    pub fn pin(&mut self, session: SessionId, mode: ToolAdvertising, encoding: Encoding) {
        tracing::debug!(
            session = %session.0,
            mode = mode.label(),
            encoding = encoding.label(),
            "tool advertising resolved for session (fixed for its lifetime)"
        );
        let pinned = Pinned {
            mode,
            encoding,
            advertise_discovered: true,
        };
        self.modes.insert(session, pinned);
    }

    /// The session's pinned mode, or `None` when start hasn't been observed.
    pub fn get(&self, session: &SessionId) -> Option<ToolAdvertising> {
        self.modes.get(session).map(|p| p.mode)
    }

    /// The session's pinned encoding, or `None` when start hasn't been
    /// observed — same "not a hole" contract as [`get`][Self::get].
    pub fn get_encoding(&self, session: &SessionId) -> Option<Encoding> {
        self.modes.get(session).map(|p| p.encoding)
    }

    /// Release a ended/hibernated session's entry. Derived state only — a
    /// resume re-pins from its own `SessionStarted`/`ModelChanged` pair.
    pub fn forget(&mut self, session: &SessionId) {
        self.modes.remove(session);
    }
}

/// The two model-independent inputs an advertising resolution needs
/// (ADR-0196): the user config (its `tool_advertising` tier) and the catalog
/// (the per-model `tool_advertising:` tier). `Arc`-wrapped and cloned into
/// the executor's event loop once, so every session-start fold is two map
/// lookups, no rebuilds. The env tier is read live inside
/// [`resolve_advertising`] — same shape as `resolve_session_retention`,
/// since the layered config loader can't see the process env either.
#[derive(Debug, Clone)]
pub struct AdvertisingInputs {
    config: Arc<Config>,
    catalog: Option<Arc<Catalog>>,
}

impl AdvertisingInputs {
    pub fn new(config: Arc<Config>, catalog: Option<Arc<Catalog>>) -> Self {
        Self { config, catalog }
    }

    /// Fold a session start into `modes`: pin the session's mode + encoding,
    /// resolved from the pairing of its initial `SessionStarted.model` (a
    /// profile's model field — a bare model id) with the `ModelChanged` a
    /// pin-driven rebind emits right after, whichever carries a `(provider,
    /// model)`. Called from the executor loop only, so `&mut` needs no lock.
    pub fn pin_session_start(
        &self,
        modes: &mut SessionToolAdvertising,
        session: &SessionId,
        provider: Option<&str>,
        model: Option<&str>,
    ) {
        let mode = match (provider, model) {
            // The pin-rebind pair names both halves — resolve precisely.
            (Some(p), Some(m)) => resolve_advertising(&self.config, self.catalog.as_deref(), p, m),
            // A bare model id (profile `model:` without `provider:`) still
            // carries a catalog preference if it's listed anywhere.
            (None, Some(m)) => resolve_advertising_by_id(&self.config, self.catalog.as_deref(), m),
            // No model fact at all: the config/env tiers alone decide (which
            // is `tool_search` when unset — the new default).
            _ => configured_advertising(&self.config).unwrap_or_default(),
        };
        // The encoding is wire-derived only (ADR-0196 §3) — no config/env
        // override, unlike the mode — so it needs just the provider half
        // when one is known; a bare model id still resolves it by finding
        // which provider lists that id (mirrors `resolve_advertising_by_id`).
        let encoding = match (provider, model) {
            (Some(p), Some(_)) => resolve_encoding(self.catalog.as_deref(), p),
            (None, Some(m)) => resolve_encoding_by_id(self.catalog.as_deref(), m),
            _ => Encoding::default(),
        };
        modes.pin(session.clone(), mode, encoding);
        // ADR-0200: same two-tier lookup as `encoding` above, a plain field.
        modes.set_advertise_discovered(
            session,
            encoding::resolve_advertise_discovered_pair(self.catalog.as_deref(), provider, model),
        );
    }

    /// Fold a `ModelChanged` into `modes`: **keep** the pinned mode, but log
    /// when the new model's catalog preference differs (ADR-0196 §2 — the
    /// session keeps its mode until restart). No-op for an unpinned session
    /// (a start pair the loop missed, e.g. broadcast lag before it
    /// subscribed); the next event self-heals nothing here because mode is
    /// start-only by design — Phase P3's resolver falls back to the global
    /// resolution for such a session.
    pub fn note_model_changed(
        &self,
        modes: &SessionToolAdvertising,
        session: &SessionId,
        provider: &str,
        model: &str,
    ) {
        let Some(held) = modes.get(session) else {
            return;
        };
        let preference = self
            .catalog
            .as_deref()
            .and_then(|c| c.model(provider, model))
            .and_then(
                |ModelEntry {
                     tool_advertising, ..
                 }| *tool_advertising,
            );
        note_model_changed(session, provider, model, held, preference);
    }
}

/// The `ModelChanged` retention rule: a live `SetModel` **keeps** the
/// session's mode (switching mid-session would bust the cache the mode
/// protects and strand half-emitted history), but when the new model's
/// catalog preference differs from the held mode, say so — a user who flipped
/// a model to `tool_advertising: full` and switched onto it deserves to
/// learn the session stays `tool_search` (or vice versa) until restarted.
///
/// `held` is what the session runs; `new_preference` is the new model's
/// *catalog-only* preference (`None` = no opinion), so a config/env override
/// (which applies to every model alike) never trips this notice.
pub fn note_model_changed(
    session: &SessionId,
    provider: &str,
    model: &str,
    held: ToolAdvertising,
    new_preference: Option<ToolAdvertising>,
) {
    if new_preference.is_some_and(|p| p != held) {
        tracing::info!(
            session = %session.0,
            held = held.label(),
            preferred = new_preference
                .map(|p| p.label())
                .unwrap_or_else(|| ToolAdvertising::default().label()),
            "model switch to {provider}/{model}: keeping this session's \
             tool advertising mode (fixed at session start; restart to pick \
             up the model's preference)"
        );
    }
}

/// The shared session state Phase P3's resolvers need (#560, ADR-0196 §2-3):
/// the pinned mode map plus the `client_side`-encoding discovered-tool set.
/// One `Arc` constructed by the caller (`main.rs`) and handed to three
/// places that used to see disjoint state — the executor loop (writer of
/// both), the `tool_spec_resolver` closure (reader of both, ADR-0076), and
/// the `system_prompt_resolver` closure (reader of `modes`, for the §5 prompt
/// slimming) — so all three observe the same pin/discovery facts instead of
/// the executor's former loop-local-only map.
#[derive(Debug, Default)]
pub struct AdvertisingState {
    pub modes: Mutex<SessionToolAdvertising>,
    pub discovered: Mutex<DiscoveredSet>,
}

impl AdvertisingState {
    pub fn new() -> Self {
        Self::default()
    }

    /// This session's pinned mode, defaulting to [`ToolAdvertising::default`]
    /// (`tool_search`) when the map hasn't pinned it — the executor loop
    /// pins synchronously off `SessionStarted`, strictly before any
    /// `ToolExec`/round for that same session can reach this reader (one
    /// sequential loop), so in practice this default only ever fires for the
    /// test-only executor wrappers that wire no [`AdvertisingInputs`] at all
    /// (`get`'s documented "not a hole" contract, `SessionToolAdvertising`).
    pub fn mode(&self, session: &SessionId) -> ToolAdvertising {
        self.modes
            .lock()
            .expect("tool-advertising mode mutex poisoned")
            .get(session)
            .unwrap_or_default()
    }

    /// This session's pinned `ToolSearch`-mode encoding (ADR-0196 §3),
    /// defaulting to [`Encoding::default`] (`client_side`) under the same
    /// "not a hole in practice" contract as [`mode`][Self::mode]. Meaningless
    /// under `Full` mode — nothing reads it there.
    pub fn encoding(&self, session: &SessionId) -> Encoding {
        self.modes
            .lock()
            .expect("tool-advertising mode mutex poisoned")
            .get_encoding(session)
            .unwrap_or_default()
    }
}

/// Shared handle every consumer of [`AdvertisingState`] holds.
pub type SharedAdvertisingState = Arc<AdvertisingState>;

#[cfg(test)]
mod tests;
