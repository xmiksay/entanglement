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
//! Kept entirely runtime-owned: core never sees it. A caller builds its own
//! `Box<dyn Llm>` for a one-shot call (`summarize::oneshot_text`-style) and
//! drops it when done; the protocol is unchanged.
//!
//! # Compaction wiring (the documented next step)
//!
//! Batch A of Issue 5 ships the store + registry + the `/aux-model` command +
//! the session-title generator. Routing the `summarize` purpose through the
//! compact path (`InMsg::Oneshot { op: "compact" }` → `session::summarize`,
//! which the *engine* drives with its own `&mut s.llm`) needs a runtime-side
//! compaction intercept or a new seam; that is deliberately deferred to avoid
//! threading an aux `Llm` through the core protocol in this first batch. The
//! pin is storable today; the resolver is exercised by the session-title path.

use std::sync::{Arc, Mutex};

use entanglement_core::{Llm, LlmFactory, ModelResolver};

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
}

impl AuxLlmRegistry {
    /// Build a registry over the given store + catalog resolver + primary
    /// fallback. The resolver is the same closure built once at startup and
    /// threaded onto `EngineConfig::model_resolver` (capturing the catalog +
    /// the warm per-endpoint HTTP client), so an aux client reuses the warm
    /// pool rather than opening its own.
    pub fn new(
        store: Arc<Mutex<AuxModelStore>>,
        resolver: ModelResolver,
        primary: LlmFactory,
    ) -> Self {
        Self {
            store,
            resolver,
            primary,
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

    /// The primary-model fallback a caller would get from [`resolve`](Self::resolve)
    /// when no pin is set. Exposed so the session-title generator (and any
    /// future caller) can reuse the same build-one-shot-client pattern without
    /// routing through the registry for the no-pin case.
    pub fn primary(&self) -> Box<dyn Llm> {
        (self.primary)()
    }
}

#[cfg(all(test, feature = "provider"))]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use entanglement_core::{
        Llm, LlmEvent, LlmRequest, LlmResponse, LlmStream, ModelResolver, ResolvedModel, UserId,
    };
    use std::sync::Mutex;

    /// `ENTANGLEMENT_AUX_MODELS_FILE` is process-global; the aux_llm tests that
    /// `set()` a pin serialize here so they don't race with each other.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// A stub `Llm` that tags every reply with which factory built it, so a
    /// test can assert which path the registry took.
    #[derive(Clone)]
    struct TagLlm(&'static str);

    #[async_trait]
    impl Llm for TagLlm {
        async fn stream(&mut self, _req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
            Ok(entanglement_core::stream_from_response(LlmResponse {
                text: self.0.to_string(),
                tool_calls: vec![],
            }))
        }
    }

    async fn drain(llm: &mut dyn Llm) -> String {
        use futures::StreamExt;
        let req = LlmRequest {
            system: "",
            model: None,
            messages: &[],
            tools: &[],
            generation: None,
        };
        let mut s = llm.stream(req).await.unwrap();
        let mut out = String::new();
        while let Some(ev) = s.next().await {
            if let Ok(LlmEvent::Text(t)) = ev {
                out.push_str(&t);
            }
        }
        out
    }

    fn primary_factory() -> LlmFactory {
        Arc::new(move || Box::new(TagLlm("primary")) as Box<dyn Llm>)
    }

    /// A resolver that always fails — mirrors an empty catalog / unknown
    /// provider so the fallback path is exercised without standing up a real
    /// provider entry.
    fn failing_resolver() -> ModelResolver {
        Arc::new(|_u: Option<&UserId>, _p: &str, _m: &str| {
            Err::<ResolvedModel, String>("unknown provider".to_string())
        })
    }

    /// A resolver that always succeeds with the `aux` stub Llm — mirrors a pin
    /// that resolves cleanly against the catalog.
    fn succeeding_resolver() -> ModelResolver {
        let factory: LlmFactory = Arc::new(|| Box::new(TagLlm("aux")) as Box<dyn Llm>);
        Arc::new(move |_u, _p, _m| {
            Ok(ResolvedModel {
                provider: "stub".to_string(),
                model: "stub".to_string(),
                llm_factory: factory.clone(),
                generation: None,
                context_window: None,
            })
        })
    }

    /// Build a store backed by a temp file (so `set()` works), under the
    /// `ENTANGLEMENT_AUX_MODELS_FILE` env override the store reads.
    fn store_with_tmp_path(label: &str) -> Arc<Mutex<AuxModelStore>> {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let path = std::env::temp_dir().join(format!(
            "entanglement-aux-llm-test-{label}-{}.yml",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        // SAFETY: single-threaded test guarded by ENV_LOCK.
        unsafe {
            std::env::set_var("ENTANGLEMENT_AUX_MODELS_FILE", &path);
        }
        Arc::new(Mutex::new(AuxModelStore::load()))
    }

    fn cleanup_env() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        unsafe {
            std::env::remove_var("ENTANGLEMENT_AUX_MODELS_FILE");
        }
    }

    #[tokio::test]
    async fn resolve_falls_back_to_primary_when_no_pin() {
        let store = store_with_tmp_path("no-pin");
        let reg = AuxLlmRegistry::new(store, failing_resolver(), primary_factory());
        let mut llm = reg.resolve(Purpose::SessionTitle);
        assert_eq!(drain(&mut *llm).await, "primary");
        cleanup_env();
    }

    #[tokio::test]
    async fn resolve_falls_back_when_pin_does_not_resolve() {
        // A pin points at a provider/model the resolver rejects → the registry
        // must fall back to the primary rather than panic/error.
        let store = store_with_tmp_path("no-resolve");
        store
            .lock()
            .unwrap()
            .set(Purpose::Summarize, "ghost", "no-such-model")
            .unwrap();
        let reg = AuxLlmRegistry::new(store, failing_resolver(), primary_factory());
        let mut llm = reg.resolve(Purpose::Summarize);
        assert_eq!(drain(&mut *llm).await, "primary");
        cleanup_env();
    }

    #[tokio::test]
    async fn resolve_uses_the_aux_factory_when_pin_resolves() {
        let store = store_with_tmp_path("resolves");
        store
            .lock()
            .unwrap()
            .set(Purpose::SessionTitle, "stub", "stub-model")
            .unwrap();
        let reg = AuxLlmRegistry::new(store, succeeding_resolver(), primary_factory());
        let mut llm = reg.resolve(Purpose::SessionTitle);
        assert_eq!(drain(&mut *llm).await, "aux");
        cleanup_env();
    }
}
