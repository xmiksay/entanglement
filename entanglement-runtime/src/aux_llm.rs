//! Per-purpose auxiliary LLM resolver (Issue 5, the tui-ux-batch plan).
//!
//! The main turn loop uses a session's primary model — but a user may want a
//! separate, cheaper/faster model for side transformations (compaction summary,
//! auto session title). This registry resolves a [`Purpose`] to a fresh
//! `Box<dyn Llm>` by reusing the runtime's catalog resolver (the same
//! [`ModelResolver`] the engine calls on `SetModel`), falling back to the
//! primary model when:
//! - the purpose has no persisted pin ([`AuxModelStore::get`] misses), or
//! - the pin's provider/model fails to resolve (unknown to the catalog, or a
//!   missing API key for that provider — both surfaced as an `Err` from the
//!   resolver).
//!
//! The pin store stays runtime-owned; the *protocol* is unchanged. Two consumers
//! reach it by different routes, and the difference is deliberate:
//!
//! - **The session-title generator** has no session backend to fall back to, so
//!   it calls [`AuxLlmRegistry::resolve`] and gets the primary model when no pin
//!   is set.
//! - **Session compaction** (`/compact` *and* the auto-summarize overflow path)
//!   runs inside core, which reaches the pin through the
//!   [`AuxLlmResolver`] seam on `EngineConfig` — built here by
//!   [`AuxLlmRegistry::resolver`]. There `None` means "use the session's own
//!   backend", which is strictly better than a fixed primary: a live `/model`
//!   switch keeps applying to compaction. Core knows only the purpose *string*
//!   (`session::summarize::AUX_PURPOSE_SUMMARIZE`), never this registry.

use std::sync::{Arc, Mutex};

use entanglement_core::{AuxLlmResolver, Catalog, Llm, LlmFactory, ModelResolver, ResolvedModel};

use crate::config::aux_models::{AuxModelStore, Purpose};

/// The runtime's per-purpose auxiliary LLM resolver (Issue 5). Wraps a shared
/// handle to the [`AuxModelStore`] (so a live `/aux-model` write is visible
/// without rebuilding the resolver) plus the catalog [`ModelResolver`] the
/// runtime already builds at startup (the same one the engine calls on
/// `SetModel`), so an aux client binds exactly like a fresh launch would.
///
/// The fallback — returned when a purpose is unset or its pin won't resolve —
/// is the primary model's [`LlmFactory`] the runtime built at startup, so an
/// unset pin is byte-identical to "use the main model" (the pre-Issue-5
/// behavior). Kept as an [`Arc`] clone of the factory (the type is itself an
/// `Arc<dyn Fn>`), cheap to hand out.
#[derive(Clone)]
pub struct AuxLlmRegistry {
    store: Arc<Mutex<AuxModelStore>>,
    resolver: ModelResolver,
    primary: LlmFactory,
    /// The catalog, kept around solely so [`concurrency_cap`](Self::concurrency_cap)
    /// can look a resolved pin's effective in-flight cap up without a second
    /// resolver round-trip (#589).
    catalog: Catalog,
    /// The primary model's effective concurrency cap ([`Catalog::effective_concurrency`]
    /// against the provider/model the runtime built [`Self::primary`] from),
    /// snapshotted once at startup — the fallback [`concurrency_cap`](Self::concurrency_cap)
    /// reports when a purpose has no pin, mirroring [`resolve`](Self::resolve)'s
    /// own no-pin fallback to `primary`.
    primary_concurrency: Option<usize>,
}

impl AuxLlmRegistry {
    /// Build a registry over the given store + catalog resolver + primary
    /// fallback. The resolver is the same closure built once at startup and
    /// threaded onto `EngineConfig::model_resolver` (capturing the catalog +
    /// the warm per-endpoint HTTP client), so an aux client reuses the warm
    /// pool rather than opening its own. `catalog` + `primary_concurrency` back
    /// [`concurrency_cap`](Self::concurrency_cap) (#589): a caller that wants to
    /// fire an aux call *alongside* a live primary-model call (the session-title
    /// generator) can check whether it would contend for the same per-model
    /// permit before doing so.
    pub fn new(
        store: Arc<Mutex<AuxModelStore>>,
        resolver: ModelResolver,
        primary: LlmFactory,
        catalog: Catalog,
        primary_concurrency: Option<usize>,
    ) -> Self {
        Self {
            store,
            resolver,
            primary,
            catalog,
            primary_concurrency,
        }
    }

    /// Resolve `purpose` to a fresh `Box<dyn Llm>`. Returns the primary model
    /// ([`Self::primary`]) when the purpose has no pin, or when the pin's
    /// provider/model can't be resolved against the catalog (logged at debug —
    /// a stale pin after a catalog edit is the expected trigger, and the safe
    /// fallback keeps a side transformation working rather than wedging it).
    pub fn resolve(&self, purpose: Purpose) -> Box<dyn Llm> {
        let pin = self
            .store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(purpose)
            .map(|(p, m)| (p.to_string(), m.to_string()));

        let Some((provider, model)) = pin else {
            // No pin → primary model. The common case until a user runs
            // `/aux-model`, and the documented fallback.
            return self.primary();
        };

        match (self.resolver)(None, &provider, &model) {
            Ok(resolved) => (resolved.llm_factory)(),
            Err(reason) => {
                // A pin whose provider/model the catalog no longer knows (a
                // typo, a since-removed catalog entry, a missing key) is
                // inertly fallen back rather than fatal — a dropped pin only
                // reverts the purpose to the primary model.
                tracing::debug!(
                    purpose = purpose.as_str(),
                    %provider,
                    %model,
                    reason,
                    "aux-models: pin did not resolve against the catalog; falling back to the primary model"
                );
                self.primary()
            }
        }
    }

    /// Resolve `purpose` to its catalog-resolved pin, or `None` when the
    /// purpose is unset or its pin no longer resolves.
    ///
    /// The `Option`-returning counterpart to [`resolve`](Self::resolve), and the
    /// shape core's [`AuxLlmResolver`] seam wants: there, `None` means "use the
    /// session's own backend", which is a *better* fallback than this type's
    /// fixed primary — it keeps a live `/model` switch applying to side
    /// transformations. So the two differ deliberately, and only callers that
    /// have no session in hand (the session-title generator) want `resolve`.
    pub fn resolve_pin(&self, purpose: Purpose) -> Option<ResolvedModel> {
        let (provider, model) = self
            .store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(purpose)
            .map(|(p, m)| (p.to_string(), m.to_string()))?;

        match (self.resolver)(None, &provider, &model) {
            Ok(resolved) => Some(resolved),
            Err(reason) => {
                tracing::debug!(
                    purpose = purpose.as_str(),
                    %provider,
                    %model,
                    reason,
                    "aux-models: pin did not resolve against the catalog; \
                     falling back to the session's own model"
                );
                None
            }
        }
    }

    /// The effective per-model in-flight concurrency cap that a live
    /// [`resolve`](Self::resolve) call for `purpose` would land on right now
    /// (#589): the pin's cap when one is set and resolves, else the primary
    /// model's — mirroring `resolve`'s own fallback exactly, so this never
    /// disagrees with which client `resolve` would actually hand back.
    /// `None` means uncapped at this layer (falls through to the endpoint-wide
    /// default only), so contention with a concurrent primary-turn call is
    /// unlikely. Lets a caller that wants to fire an aux call *alongside* a
    /// live primary-model call (the session-title generator) judge contention
    /// risk without holding an `Llm` handle, which is opaque.
    pub fn concurrency_cap(&self, purpose: Purpose) -> Option<usize> {
        match self.resolve_pin(purpose) {
            Some(resolved) => self
                .catalog
                .effective_concurrency(&resolved.provider, &resolved.model),
            None => self.primary_concurrency,
        }
    }

    /// The [`AuxLlmResolver`] core consults for a side transformation (Issue 5),
    /// mapping core's purpose *string* onto this registry's typed [`Purpose`].
    /// An unrecognized key resolves to `None` (the session's own backend), so a
    /// future core purpose this build doesn't know is inert rather than fatal.
    pub fn resolver(self) -> AuxLlmResolver {
        Arc::new(move |purpose: &str| Purpose::parse(purpose).and_then(|p| self.resolve_pin(p)))
    }

    /// The primary-model fallback a caller would get from [`resolve`](Self::resolve)
    /// when no pin is set. Exposed so the session-title generator (and any
    /// future caller) can reuse the same build-one-shot-client pattern without
    /// routing through the registry for the no-pin case.
    pub fn primary(&self) -> Box<dyn Llm> {
        (self.primary)()
    }
}

#[cfg(all(test, feature = "provider"))]
mod tests;
