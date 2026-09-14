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

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use entanglement_core::{AuxLlmResolver, Catalog, Llm, LlmFactory, ModelResolver, ResolvedModel};

use crate::config::aux_models::{AuxModelStore, Purpose};

/// How long a `(purpose, provider, model)` combination stays cooled down
/// after a failed [`try_resolve`](AuxLlmRegistry::try_resolve) probe (#560
/// aux fail-fast follow-up) — mirrors ADR-0201's `FAILURE_COOLDOWN`
/// (`mcp/available_enable.rs`). A dead endpoint then costs one probe per
/// window, not a storm per tool call.
const AUX_FAILURE_COOLDOWN: Duration = Duration::from_secs(60);

/// Fail-fast cooldown state for [`try_resolve`](AuxLlmRegistry::try_resolve)
/// (#560 follow-up), keyed by `(purpose, provider, model)` so a live
/// `/aux-model` re-pin doesn't inherit a stale cooldown from the model it
/// replaced. Mirrors [`crate::mcp::available_tier`]'s `recent_enable_failures`
/// shape exactly — a `Mutex<HashMap<_, Instant>>`, checked/recorded/cleared
/// by elapsed time rather than a background sweep.
#[derive(Default)]
struct AuxCooldown {
    recent_failures: Mutex<HashMap<(String, String, String), Instant>>,
}

impl AuxCooldown {
    fn key(purpose: Purpose, provider: &str, model: &str) -> (String, String, String) {
        (
            purpose.as_str().to_string(),
            provider.to_string(),
            model.to_string(),
        )
    }

    /// `cooldown` is a parameter (not the [`AUX_FAILURE_COOLDOWN`] constant
    /// read internally) so a test can shrink it to `Duration::ZERO` and
    /// assert expiry without sleeping — mirrors
    /// `mcp::available_tier::AvailableMcp::recently_failed_enable`.
    fn in_cooldown(
        &self,
        purpose: Purpose,
        provider: &str,
        model: &str,
        cooldown: Duration,
    ) -> bool {
        self.recent_failures
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&Self::key(purpose, provider, model))
            .is_some_and(|at| at.elapsed() < cooldown)
    }

    /// Record a failure, returning `true` when this starts a *new* cooldown
    /// window (the caller should warn) rather than refreshing one already in
    /// force (silent — the warn already fired for this window).
    fn record_failure(
        &self,
        purpose: Purpose,
        provider: &str,
        model: &str,
        cooldown: Duration,
    ) -> bool {
        let key = Self::key(purpose, provider, model);
        let mut guard = self
            .recent_failures
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let is_new_window = guard.get(&key).is_none_or(|at| at.elapsed() >= cooldown);
        guard.insert(key, Instant::now());
        is_new_window
    }

    fn clear_failure(&self, purpose: Purpose, provider: &str, model: &str) {
        self.recent_failures
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&Self::key(purpose, provider, model));
    }
}

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
    /// `(provider, model)` identity of the primary-model fallback — the
    /// cooldown key [`try_resolve`](Self::try_resolve) uses when a purpose
    /// has no pin (#560 follow-up). Distinct from `primary` (the factory
    /// itself): this is just the label a cooldown/warn needs.
    primary_identity: (String, String),
    /// Shared (across every `Clone` of this registry — narrate/session-title
    /// each hand a clone to a detached per-call task) fail-fast cooldown
    /// state (#560 follow-up).
    cooldown: Arc<AuxCooldown>,
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
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        store: Arc<Mutex<AuxModelStore>>,
        resolver: ModelResolver,
        primary: LlmFactory,
        catalog: Catalog,
        primary_concurrency: Option<usize>,
        primary_identity: (String, String),
    ) -> Self {
        Self {
            store,
            resolver,
            primary,
            catalog,
            primary_concurrency,
            primary_identity,
            cooldown: Arc::new(AuxCooldown::default()),
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

    /// Resolve `purpose` for a **skippable, display-only** aux call
    /// (narrate / session-title, #560 follow-up) — unlike
    /// [`resolve`](Self::resolve), a `(purpose, provider, model)` combination
    /// that recently failed short-circuits to `None` *before* building a
    /// client, let alone calling it, so a dead endpoint (pinned or the
    /// primary fallback) costs one probe per [`AUX_FAILURE_COOLDOWN`] window
    /// rather than a storm per call. On `Some`, the caller must report the
    /// outcome back through [`note_success`](Self::note_success) /
    /// [`note_failure`](Self::note_failure) so the cooldown state stays
    /// accurate.
    pub fn try_resolve(&self, purpose: Purpose) -> Option<(Box<dyn Llm>, String, String)> {
        let (factory, provider, model) = match self.resolve_pin(purpose) {
            Some(resolved) => (resolved.llm_factory, resolved.provider, resolved.model),
            None => (
                self.primary.clone(),
                self.primary_identity.0.clone(),
                self.primary_identity.1.clone(),
            ),
        };
        if self
            .cooldown
            .in_cooldown(purpose, &provider, &model, AUX_FAILURE_COOLDOWN)
        {
            return None;
        }
        Some((factory(), provider, model))
    }

    /// A [`try_resolve`](Self::try_resolve) call succeeded end-to-end (#560
    /// follow-up): clear any cooldown recorded for this `(purpose, provider,
    /// model)` so a transient earlier failure doesn't outlive its own window
    /// after the endpoint has clearly recovered.
    pub fn note_success(&self, purpose: Purpose, provider: &str, model: &str) {
        self.cooldown.clear_failure(purpose, provider, model);
    }

    /// A [`try_resolve`](Self::try_resolve) call failed (connect/transport,
    /// #560 follow-up): start/refresh the cooldown window, warning exactly
    /// once per window so the user learns their pin (or primary endpoint) is
    /// unreachable without a log line per call.
    pub fn note_failure(&self, purpose: Purpose, provider: &str, model: &str) {
        if self
            .cooldown
            .record_failure(purpose, provider, model, AUX_FAILURE_COOLDOWN)
        {
            tracing::warn!(
                purpose = purpose.as_str(),
                provider,
                model,
                cooldown_secs = AUX_FAILURE_COOLDOWN.as_secs(),
                "aux: probe failed for this purpose/endpoint — cooling down; \
                 further calls for this purpose short-circuit locally until \
                 the window clears (check the pinned aux-model config if this \
                 persists)"
            );
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
