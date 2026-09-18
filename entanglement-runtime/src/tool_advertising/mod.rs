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
//! **Per-session, pinned at first resolution.** The resolver and executor are
//! engine-global and session-multiplexed, so the resolved mode is held in a
//! session→mode map ([`SessionToolAdvertising`]), pinned by the tool-spec
//! resolver the first time it runs for a session, from the model core hands
//! it ([`AdvertisingState::ensure_pinned`]) — never from a broadcast event,
//! which core's first round does not wait for. A live
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

use entanglement_core::{
    Catalog, Discovery, ModelEntry, SessionId, SessionModel, ToolAdvertising, ToolSpec,
};

use crate::config::Config;

mod discovered;
pub use discovered::{client_side_surface, DiscoveredSet};

mod discovery;

mod invoke;
pub use invoke::{example_via_invoke, unknown_tool_reply};

mod encoding;
pub use encoding::Encoding;
use encoding::{resolve_encoding, resolve_encoding_by_id};

mod precedence;

pub mod surface;

mod repin;
pub use precedence::{
    configured_advertising, configured_advertising_source, resolve_advertising,
    resolve_advertising_by_id, AdvertisingSource,
};

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
    /// A live re-pin requested before the session's first resolution,
    /// applied by the pin itself ([`repin`]).
    pending: HashMap<SessionId, repin::Override>,
}

/// One session's pinned facts: the advertising mode plus the encoding its
/// wire resolved to — both fixed together at session start, both untouched by
/// a later `SetModel` (§2/§3), so there's exactly one pin call per session.
#[derive(Debug, Clone, Copy)]
struct Pinned {
    mode: ToolAdvertising,
    encoding: Encoding,
    /// ADR-0204; `Append` from `pin`, set via `set_discovery`.
    discovery: Discovery,
}

impl SessionToolAdvertising {
    pub fn new() -> Self {
        Self::default()
    }

    /// Pin a session's mode + encoding. Called once per session, at its first
    /// resolution.
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
            discovery: Discovery::Append,
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
        self.pending.remove(session);
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
    /// The startup backend's `(provider, model)`: what a session with no bound
    /// model actually talks to, which core cannot name.
    default_model: Option<(String, String)>,
}

impl AdvertisingInputs {
    pub fn new(config: Arc<Config>, catalog: Option<Arc<Catalog>>) -> Self {
        Self {
            config,
            catalog,
            default_model: None,
        }
    }

    /// The catalog this instance was built with (#560 P12, ADR-0207 §12):
    /// threaded into `LadderCtx` so `explore`/`describe`'s `models` kind and
    /// `agent`'s `model` parameter validation read the exact same catalog
    /// every advertising resolution already does, rather than a second copy.
    pub fn catalog(&self) -> Option<&Arc<Catalog>> {
        self.catalog.as_ref()
    }

    /// Resolve a session with no bound model as `provider`/`model`.
    pub fn with_default_model(
        mut self,
        provider: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        self.default_model = Some((provider.into(), model.into()));
        self
    }

    /// Pin the session's mode + encoding + discovery from its bound
    /// `(provider, model)` — a bare model id still resolves through the
    /// catalog, and no model at all means the startup default. A re-pin
    /// requested before this point is applied on top.
    pub fn pin_session_start(
        &self,
        modes: &mut SessionToolAdvertising,
        session: &SessionId,
        provider: Option<&str>,
        model: Option<&str>,
    ) {
        let (provider, model) = match (provider, model, &self.default_model) {
            (None, None, Some((p, m))) => (Some(p.as_str()), Some(m.as_str())),
            _ => (provider, model),
        };
        let configured = configured_advertising(&self.config);
        let catalog = self.catalog.as_deref();
        let mode = match (provider, model) {
            // The pin-rebind pair names both halves — resolve precisely.
            (Some(p), Some(m)) => resolve_advertising(configured, catalog, p, m),
            // A bare model id (profile `model:` without `provider:`) still
            // carries a catalog preference if it's listed anywhere.
            (None, Some(m)) => resolve_advertising_by_id(configured, catalog, m),
            // No model fact at all: the config/env tiers alone decide (which
            // is `tool_search` when unset — the new default).
            _ => configured.unwrap_or_default(),
        };
        // The encoding is wire-derived only (ADR-0196 §3) — no config/env
        // override, unlike the mode — so it needs just the provider half
        // when one is known; a bare model id still resolves it by finding
        // which provider lists that id (mirrors `resolve_advertising_by_id`).
        let encoding = match (provider, model) {
            (Some(p), Some(_)) => resolve_encoding(catalog, p),
            (None, Some(m)) => resolve_encoding_by_id(catalog, m),
            _ => Encoding::default(),
        };
        modes.pin(session.clone(), mode, encoding);
        // ADR-0204: same two-tier lookup as `encoding`, a model entry's
        // `discovery` over its provider's; kept across a later `SetModel`.
        modes.set_discovery(
            session,
            discovery::resolve_discovery(&self.config, catalog, provider, model),
        );
        modes.apply_pending(session);
    }

    /// Fold a `ModelChanged` into `modes`: **keep** the pinned mode, but log
    /// when the new model's catalog preference differs (ADR-0196 §2 — the
    /// session keeps its mode until restart). No-op for a session not pinned
    /// yet: its first resolution will pin from the new model anyway.
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
/// the pinned mode map, the discovered-tool set, and the env-date pin (0202).
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
    pub env_dates: Mutex<crate::env_date::EnvDatePins>,
    /// `Full`-mode surface per session, snapshotted at first resolution and
    /// grown only by a delivered schema (ADR-0204 §5): a tool registered or
    /// removed later never inserts into or shrinks the cached array.
    pub full_surfaces: Mutex<HashMap<SessionId, Vec<ToolSpec>>>,
}

impl AdvertisingState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Pin `session` from the model its tool-spec resolution sees; a no-op
    /// once pinned. The resolver calls this before reading anything, so the
    /// facts round 1 advertises are the facts every later round advertises.
    pub fn ensure_pinned(
        &self,
        inputs: &AdvertisingInputs,
        session: &SessionId,
        model: SessionModel<'_>,
    ) {
        let mut modes = self
            .modes
            .lock()
            .expect("tool-advertising mode mutex poisoned");
        if modes.get(session).is_none() {
            inputs.pin_session_start(&mut modes, session, model.provider, model.model);
        }
    }

    /// Release every per-session record on end/hibernate. A resumed session
    /// re-pins at its first round and rediscovers what it needs.
    pub fn forget(&self, session: &SessionId) {
        self.modes
            .lock()
            .expect("tool-advertising mode mutex poisoned")
            .forget(session);
        self.discovered
            .lock()
            .expect("discovered-tool mutex poisoned")
            .forget(session);
        self.env_dates
            .lock()
            .expect("env-date pin mutex poisoned")
            .forget(session);
        self.full_surfaces
            .lock()
            .expect("full-surface mutex poisoned")
            .remove(session);
    }

    /// This session's pinned mode, defaulting to [`ToolAdvertising::default`]
    /// (`tool_search`) before its first resolution pins it — only readers
    /// outside a round see that (the TUI `/tools` view, test wrappers).
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
