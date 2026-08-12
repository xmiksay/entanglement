//! Tests for [`super`] (`aux_llm`). Sibling file (default `mod tests;`
//! resolution, so private fields stay reachable) to keep both sides of the
//! 400-line file cap.

use super::*;
use async_trait::async_trait;
use entanglement_core::{
    Llm, LlmEvent, LlmRequest, LlmResponse, LlmStream, ModelResolver, ResolvedModel,
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
        cache_key: None,
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

/// A provider-less catalog — every test below exercises `resolve`/`resolve_pin`
/// through a stubbed [`ModelResolver`], never the real catalog lookup, so an
/// empty catalog is a faithful stand-in wherever [`AuxLlmRegistry::new`]
/// needs one.
fn empty_catalog() -> Catalog {
    Catalog { providers: vec![] }
}

/// A resolver that always fails — mirrors an empty catalog / unknown
/// provider so the fallback path is exercised without standing up a real
/// provider entry.
fn failing_resolver() -> ModelResolver {
    Arc::new(|_u, _p: &str, _m: &str| {
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

/// `resolve_pin` is the core-seam counterpart: `None` means "use the
/// session's own backend", which is why it must NOT fall back to primary
/// the way `resolve` does.
#[tokio::test]
async fn resolve_pin_is_none_when_unset_or_unresolvable() {
    let store = store_with_tmp_path("pin-none");
    let reg = AuxLlmRegistry::new(
        store.clone(),
        failing_resolver(),
        primary_factory(),
        empty_catalog(),
        None,
    );
    assert!(reg.resolve_pin(Purpose::Summarize).is_none());
    // A pin that exists but no longer resolves is also `None`, not primary.
    store
        .lock()
        .unwrap()
        .set(Purpose::Summarize, "ghost", "gone")
        .unwrap();
    assert!(reg.resolve_pin(Purpose::Summarize).is_none());
    cleanup_env();
}

#[tokio::test]
async fn resolve_pin_returns_the_catalog_resolution() {
    let store = store_with_tmp_path("pin-some");
    store
        .lock()
        .unwrap()
        .set(Purpose::Summarize, "stub", "stub-model")
        .unwrap();
    let reg = AuxLlmRegistry::new(
        store,
        succeeding_resolver(),
        primary_factory(),
        empty_catalog(),
        None,
    );
    let resolved = reg.resolve_pin(Purpose::Summarize).expect("pin resolves");
    assert_eq!(resolved.provider, "stub");
    let mut llm = (resolved.llm_factory)();
    assert_eq!(drain(&mut *llm).await, "aux");
    cleanup_env();
}

/// The core seam maps a purpose *string*; an unknown key is inert (falls
/// back to the session's backend) rather than panicking.
#[tokio::test]
async fn resolver_maps_known_keys_and_ignores_unknown_ones() {
    let store = store_with_tmp_path("resolver-seam");
    store
        .lock()
        .unwrap()
        .set(Purpose::Summarize, "stub", "stub-model")
        .unwrap();
    let seam = AuxLlmRegistry::new(
        store,
        succeeding_resolver(),
        primary_factory(),
        empty_catalog(),
        None,
    )
    .resolver();
    assert!(seam("summarize").is_some());
    assert!(seam("no-such-purpose").is_none());
    cleanup_env();
}

#[tokio::test]
async fn resolve_falls_back_to_primary_when_no_pin() {
    let store = store_with_tmp_path("no-pin");
    let reg = AuxLlmRegistry::new(
        store,
        failing_resolver(),
        primary_factory(),
        empty_catalog(),
        None,
    );
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
    let reg = AuxLlmRegistry::new(
        store,
        failing_resolver(),
        primary_factory(),
        empty_catalog(),
        None,
    );
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
    let reg = AuxLlmRegistry::new(
        store,
        succeeding_resolver(),
        primary_factory(),
        empty_catalog(),
        None,
    );
    let mut llm = reg.resolve(Purpose::SessionTitle);
    assert_eq!(drain(&mut *llm).await, "aux");
    cleanup_env();
}

/// #589: `concurrency_cap` must report the *pin's* effective cap when one
/// resolves — not the primary's — mirroring `resolve`'s own precedence.
#[tokio::test]
async fn concurrency_cap_reports_the_pins_cap_when_it_resolves() {
    let store = store_with_tmp_path("cap-pin");
    store
        .lock()
        .unwrap()
        .set(Purpose::SessionTitle, "capped", "capped-model")
        .unwrap();
    let catalog: Catalog = serde_yaml::from_str(
        "providers:\n\
         \x20 - name: capped\n\
         \x20   default_model: capped-model\n\
         \x20   models:\n\
         \x20     - id: capped-model\n\
         \x20       concurrency: 1\n",
    )
    .unwrap();
    let resolver: ModelResolver = Arc::new(|_u, provider, model| {
        Ok(ResolvedModel {
            provider: provider.to_string(),
            model: model.to_string(),
            llm_factory: Arc::new(|| Box::new(TagLlm("aux")) as Box<dyn Llm>),
            generation: None,
            context_window: None,
        })
    });
    // A primary cap of 5 would be misreported as the answer if
    // `concurrency_cap` didn't prefer a resolving pin.
    let reg = AuxLlmRegistry::new(store, resolver, primary_factory(), catalog, Some(5));
    assert_eq!(reg.concurrency_cap(Purpose::SessionTitle), Some(1));
    cleanup_env();
}

/// #589: with no pin (the common case), `concurrency_cap` must report the
/// primary's cap — the same one a concurrent main-turn call would be
/// admitted through, since that's exactly what `resolve` falls back to.
#[tokio::test]
async fn concurrency_cap_falls_back_to_primary_when_no_pin() {
    let store = store_with_tmp_path("cap-no-pin");
    let reg = AuxLlmRegistry::new(
        store,
        failing_resolver(),
        primary_factory(),
        empty_catalog(),
        Some(1),
    );
    assert_eq!(reg.concurrency_cap(Purpose::SessionTitle), Some(1));
    cleanup_env();
}
