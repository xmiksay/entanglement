//! Per-user auxiliary-model pins (#635, deferred from ADR-0154 "Consequences"
//! — ledger row 15): the single-user [`AuxModelStore`] is process-global
//! (one `aux-models.yml`, one `/aux-model` command). A multi-user embedder
//! instead gives each [`UserId`] its own pins via a [`UserAuxModelStore`] it
//! implements over its own storage — mirroring [`UserProviderStore`] in the
//! sibling [`provider`][super::provider] module exactly: a trait an embedder
//! implements, an in-memory reference impl for tests/small deployments, and a
//! builder that wires the result onto the same seam `main.rs` uses in
//! single-user mode.
//!
//! [`build_user_aux_registry`] snapshots a user's pins into an in-memory
//! [`AuxModelStore`] ([`AuxModelStore::in_memory`]) and hands it to
//! [`AuxLlmRegistry::new`] exactly like `main.rs` wraps the file-backed store
//! — so [`AuxLlmRegistry::resolve`]/[`resolve_pin`][AuxLlmRegistry::resolve_pin]
//! need no separate per-user code path, and [`AuxLlmRegistry`] itself stays
//! entirely single-user-shaped (no `UserId` field — `entanglement-runtime`'s
//! own `skutter` binary is a strict single-user application; `UserId` belongs
//! to the embedder-facing multi-user seams, not the process-global defaults
//! every plain `skutter` run also constructs). Instead, `user` is bound by
//! *wrapping* the supplied [`ModelResolver`] in a closure that substitutes the
//! captured `user` for whatever the registry passes (always `None`, since it
//! has no `UserId` concept) — the same closure-capture shape
//! [`build_user_model_resolver`][super::provider::build_user_model_resolver]
//! itself uses to capture its `store`/`http_client`. Without this, a
//! multi-user `ModelResolver` (which requires `Some(user)`) would reject every
//! aux call outright.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, RwLock};

use entanglement_core::{Catalog, LlmFactory, ModelResolver, UserId};

use crate::aux_llm::AuxLlmRegistry;
use crate::config::aux_models::{AuxModelStore, Purpose};

/// Looks up a user's auxiliary-model pins — the seam a multi-user embedder
/// implements over its own per-tenant storage (a DB table, a config file), the
/// exact counterpart of [`UserProviderStore`][super::provider::UserProviderStore].
/// An unregistered user yields an empty map, not an error: unlike provider
/// resolution (which must succeed to get an `Llm` at all), an aux purpose with
/// no pin has a well-defined fallback — the primary model — so a user who
/// never ran `/aux-model` is simply unpinned everywhere, not a failure.
pub trait UserAuxModelStore: Send + Sync {
    fn pins(&self, user: &UserId) -> AuxPins;
}

/// One user's purpose → `(provider, model)` pins — the return type of
/// [`UserAuxModelStore::pins`], named to keep the trait signature and
/// [`InMemoryUserAuxModelStore`]'s field readable.
pub type AuxPins = BTreeMap<Purpose, (String, String)>;

/// An in-memory [`UserAuxModelStore`] — good for tests and small deployments.
/// An embedder with its own per-user database should implement
/// [`UserAuxModelStore`] directly against it instead of mirroring every
/// user's pins into this map.
#[derive(Clone, Default)]
pub struct InMemoryUserAuxModelStore {
    users: Arc<RwLock<HashMap<UserId, AuxPins>>>,
}

impl InMemoryUserAuxModelStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record `user`'s pin for `purpose`, alongside whatever pins they
    /// already have. Mirrors [`InMemoryUserProviderStore::set`][super::provider::InMemoryUserProviderStore::set]'s
    /// direct-write shape (no managed file, no lock discipline — an in-memory
    /// store has neither).
    pub fn set(
        &self,
        user: UserId,
        purpose: Purpose,
        provider: impl Into<String>,
        model: impl Into<String>,
    ) {
        self.users
            .write()
            .expect("user aux-model store lock poisoned")
            .entry(user)
            .or_default()
            .insert(purpose, (provider.into(), model.into()));
    }
}

impl UserAuxModelStore for InMemoryUserAuxModelStore {
    fn pins(&self, user: &UserId) -> AuxPins {
        self.users
            .read()
            .expect("user aux-model store lock poisoned")
            .get(user)
            .cloned()
            .unwrap_or_default()
    }
}

/// Build the [`AuxLlmRegistry`] a multi-user embedder wires in place of the
/// single-user process-global one built from `AuxModelStore::load()`: `user`'s
/// pins are snapshotted out of `store` into an in-memory `AuxModelStore`, and
/// `resolver` is wrapped so every call the registry makes into it resolves
/// against `user`'s own catalog/key — the same way
/// [`build_user_model_resolver`][super::provider::build_user_model_resolver]'s
/// output resolves the turn loop's own model. `resolver`/`primary`/`catalog`/
/// `primary_concurrency` are the same per-user values an embedder already
/// builds for [`EngineConfig::model_resolver`][entanglement_core::EngineConfig]
/// (see [`provider::resolve_for_user`][super::provider]) — this reuses them
/// rather than opening a second resolution path.
///
/// Called fresh whenever an embedder wants an up-to-date view of `user`'s
/// pins (session start, after a `/aux-model`-equivalent write) — like
/// [`UserProviderStore::context`][super::provider::UserProviderStore::context],
/// this is a snapshot, not a live handle: a `store` backed by I/O should
/// cache its own reads.
pub fn build_user_aux_registry(
    store: &dyn UserAuxModelStore,
    user: &UserId,
    resolver: ModelResolver,
    primary: LlmFactory,
    catalog: Catalog,
    primary_concurrency: Option<usize>,
) -> AuxLlmRegistry {
    let aux_store = AuxModelStore::in_memory(store.pins(user));
    let user = user.clone();
    // `AuxLlmRegistry` has no `UserId` concept of its own (it stays the same
    // single-user-shaped type `main.rs` builds one process-global instance of)
    // and always calls a resolver with `None` — substitute the captured
    // `user` here instead of threading a user field through the registry.
    let bound_resolver: ModelResolver =
        Arc::new(move |_none, provider, model| resolver(Some(&user), provider, model));
    AuxLlmRegistry::new(
        Arc::new(Mutex::new(aux_store)),
        bound_resolver,
        primary,
        catalog,
        primary_concurrency,
    )
}

#[cfg(all(test, feature = "provider"))]
mod tests {
    use super::*;
    use entanglement_core::{Llm, LlmEvent, LlmRequest, LlmResponse, LlmStream, ResolvedModel};

    #[derive(Clone)]
    struct TagLlm(&'static str);

    #[async_trait::async_trait]
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

    fn empty_catalog() -> Catalog {
        Catalog { providers: vec![] }
    }

    /// A resolver stubbing `build_user_model_resolver`'s contract: `Err`
    /// without a user, `Ok` for a known (user, provider, model) triple.
    fn user_scoped_resolver() -> ModelResolver {
        Arc::new(|user, provider, model| {
            let user = user.ok_or_else(|| "requires a session user".to_string())?;
            if user.0 == "alice" && provider == "zai" && model == "glm-4.5-flash" {
                Ok(ResolvedModel {
                    provider: provider.to_string(),
                    model: model.to_string(),
                    llm_factory: Arc::new(|| Box::new(TagLlm("alice-pin")) as Box<dyn Llm>),
                    generation: None,
                    context_window: None,
                })
            } else {
                Err(format!("no such model for {user}"))
            }
        })
    }

    #[test]
    fn unregistered_user_has_no_pins() {
        let store = InMemoryUserAuxModelStore::new();
        let bob = UserId::new("bob");
        assert!(store.pins(&bob).is_empty());
    }

    #[test]
    fn two_users_have_independent_pins() {
        let store = InMemoryUserAuxModelStore::new();
        let alice = UserId::new("alice");
        let bob = UserId::new("bob");
        store.set(alice.clone(), Purpose::Summarize, "zai", "glm-4.5-flash");
        store.set(bob.clone(), Purpose::Narrate, "ollama", "llama3.1");

        let alice_pins = store.pins(&alice);
        assert_eq!(
            alice_pins.get(&Purpose::Summarize),
            Some(&("zai".to_string(), "glm-4.5-flash".to_string()))
        );
        assert!(!alice_pins.contains_key(&Purpose::Narrate));

        let bob_pins = store.pins(&bob);
        assert_eq!(
            bob_pins.get(&Purpose::Narrate),
            Some(&("ollama".to_string(), "llama3.1".to_string()))
        );
        assert!(!bob_pins.contains_key(&Purpose::Summarize));
    }

    #[tokio::test]
    async fn registry_resolves_a_pin_against_its_bound_user() {
        let store = InMemoryUserAuxModelStore::new();
        let alice = UserId::new("alice");
        store.set(alice.clone(), Purpose::Summarize, "zai", "glm-4.5-flash");

        let registry = build_user_aux_registry(
            &store,
            &alice,
            user_scoped_resolver(),
            primary_factory(),
            empty_catalog(),
            None,
        );
        let mut llm = registry.resolve(Purpose::Summarize);
        assert_eq!(drain(&mut *llm).await, "alice-pin");
    }

    #[tokio::test]
    async fn registry_falls_back_to_primary_without_the_resolver_wrapped() {
        // `AuxLlmRegistry` itself has no `UserId` concept and always calls
        // its resolver with `None` — plugging the raw (unwrapped)
        // `user_scoped_resolver` straight into `AuxLlmRegistry::new` (instead
        // of going through `build_user_aux_registry`) hits that `None` and
        // gets an `Err`, so it falls back to the primary model exactly like
        // an unresolvable pin would.
        let store = InMemoryUserAuxModelStore::new();
        let alice = UserId::new("alice");
        store.set(alice.clone(), Purpose::Summarize, "zai", "glm-4.5-flash");
        let aux_store = AuxModelStore::in_memory(store.pins(&alice));

        let registry = AuxLlmRegistry::new(
            Arc::new(Mutex::new(aux_store)),
            user_scoped_resolver(),
            primary_factory(),
            empty_catalog(),
            None,
        );
        let mut llm = registry.resolve(Purpose::Summarize);
        assert_eq!(drain(&mut *llm).await, "primary");
    }

    #[tokio::test]
    async fn registry_falls_back_to_primary_for_an_unpinned_purpose() {
        let store = InMemoryUserAuxModelStore::new();
        let alice = UserId::new("alice");

        let registry = build_user_aux_registry(
            &store,
            &alice,
            user_scoped_resolver(),
            primary_factory(),
            empty_catalog(),
            None,
        );
        let mut llm = registry.resolve(Purpose::Narrate);
        assert_eq!(drain(&mut *llm).await, "primary");
    }
}
