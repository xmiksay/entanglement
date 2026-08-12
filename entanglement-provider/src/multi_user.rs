//! Per-user provider context: catalog + API keys (#522, ADR-0147; moved here
//! from the runtime by ADR-0181/ADR-0184, #687).
//!
//! Single-user mode's provider resolution (`skutter`'s `main.rs`) is
//! deliberately process-global: one [`Catalog`], one managed `.env` key file
//! loaded into `std::env` at startup. A multi-user embedder instead gives each
//! [`UserId`] its own catalog overlay + API keys via a [`UserProviderStore`]
//! it implements over its own storage, and wires
//! [`build_user_model_resolver`]'s output onto its engine's `model_resolver`
//! seam in place of the process-global one. Keys never touch `std::env` —
//! they are read straight out of the store and handed to the provider client
//! constructors. The runtime crate carries none of this (ADR-0181): the
//! embedder maps its sessions to users itself and hands the finished
//! [`ModelResolver`] in.
//!
//! Rate-limit isolation falls out of the existing per-endpoint pool for free
//! (ADR-0050): [`HttpClient`] keys its `EndpointState` by `(base_url,
//! sha256(api_key))`, so two users with distinct keys on the same provider
//! already get separate RPM/concurrency/429 cool-down state with no extra
//! plumbing here. Two users configured to *share* one literal key additionally
//! get a per-user admission gate layered on top of that shared endpoint state
//! (#632, ADR-0175, [`HttpClient::with_user_budget`]) — resolved from *that
//! user's own* catalog `rpm`/`concurrency` rather than whichever user's
//! session happened to size the endpoint first.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crate::catalog::{Catalog, ProviderEntry, Wire};
use crate::client::{HttpClient, UserBudget};
use crate::gemini::{gemini_factory, GEMINI_BASE};
use crate::llm::{ModelResolver, ResolvedModel, UserId};
use crate::openai::{openai_factory, OPENAI_BASE};
use crate::web_search::WebSearchConfig;
use crate::{anthropic_factory, ANTHROPIC_BASE};

/// One user's provider surface: their own [`Catalog`] (same shape as the
/// process-global `providers.yml`, #118 — providers, models, per-provider
/// `rpm`/`concurrency`) and their own API keys, keyed by the catalog entry's
/// `key_env` name purely as a stable per-provider label (the value is never
/// read from or written to an actual environment variable in multi-user
/// mode).
#[derive(Clone)]
pub struct UserProviderContext {
    pub catalog: Catalog,
    keys: HashMap<String, String>,
    /// Per-provider OAuth bearer sources (#684 edge d), keyed by the catalog
    /// entry's `name`. Consulted for a provider whose entry carries an
    /// `oauth:` block — typically `StoredTokenSource::new(provider_name,
    /// user_scoped(store, user))` over the embedder's own `UserTokenStore`.
    token_sources: HashMap<String, Arc<dyn crate::mcp::auth::AccessTokenSource>>,
}

impl UserProviderContext {
    pub fn new(catalog: Catalog) -> Self {
        Self {
            catalog,
            keys: HashMap::new(),
            token_sources: HashMap::new(),
        }
    }

    /// Register this user's API key for the provider whose catalog entry
    /// declares `key_env` (e.g. `"ZAI_API_KEY"`) — the same label the
    /// single-user `.env` file would use for that provider, reused here only
    /// as a lookup key, never as an actual env var name.
    pub fn with_key(mut self, key_env: impl Into<String>, key: impl Into<String>) -> Self {
        self.keys.insert(key_env.into(), key.into());
        self
    }

    /// Register this user's OAuth bearer source for the catalog provider
    /// named `provider` (#684 edge d) — required for an entry carrying an
    /// `oauth:` block, ignored otherwise.
    pub fn with_token_source(
        mut self,
        provider: impl Into<String>,
        source: Arc<dyn crate::mcp::auth::AccessTokenSource>,
    ) -> Self {
        self.token_sources.insert(provider.into(), source);
        self
    }

    fn key_for(&self, entry: &ProviderEntry) -> Option<&str> {
        entry
            .key_env
            .as_deref()
            .and_then(|k| self.keys.get(k))
            .map(String::as_str)
    }

    /// The OAuth bearer source for `entry`, when it declares an `oauth:`
    /// block: missing one is a hard error — an OAuth endpoint without a token
    /// source could only ever 401.
    fn auth_for(
        &self,
        entry: &ProviderEntry,
    ) -> Result<Option<Arc<dyn crate::mcp::auth::AccessTokenSource>>, String> {
        if entry.oauth.is_none() {
            return Ok(None);
        }
        match self.token_sources.get(&entry.name) {
            Some(source) => Ok(Some(source.clone())),
            None => Err(format!(
                "provider `{}` is OAuth-protected — register this user's token source \
                 via UserProviderContext::with_token_source",
                entry.name
            )),
        }
    }
}

/// Looks up a user's [`UserProviderContext`] — the seam a multi-user embedder
/// implements over its own per-tenant storage (a DB row, a config file, a
/// secrets manager). Called on every model resolution (session start, agent
/// pin rebind, a live model switch), so an implementation backed by I/O
/// should cache.
pub trait UserProviderStore: Send + Sync {
    fn context(&self, user: &UserId) -> Option<UserProviderContext>;
}

/// An in-memory [`UserProviderStore`] — good for tests and small
/// deployments. An embedder with its own per-user database should implement
/// [`UserProviderStore`] directly against it instead of mirroring every
/// user's catalog/keys into this map.
#[derive(Clone, Default)]
pub struct InMemoryUserProviderStore {
    users: Arc<RwLock<HashMap<UserId, UserProviderContext>>>,
}

impl InMemoryUserProviderStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&self, user: UserId, ctx: UserProviderContext) {
        self.users
            .write()
            .expect("user provider store lock poisoned")
            .insert(user, ctx);
    }
}

impl UserProviderStore for InMemoryUserProviderStore {
    fn context(&self, user: &UserId) -> Option<UserProviderContext> {
        self.users
            .read()
            .expect("user provider store lock poisoned")
            .get(user)
            .cloned()
    }
}

/// Build the [`ModelResolver`] a multi-user engine wires in place of the
/// single-user process-global one: every call looks the resolving session's
/// [`UserId`] up in `store` and resolves against *that user's own* catalog +
/// key. A session with no user (`None` — a single-user-mode session sharing a
/// multi-user engine) or a user absent from `store` is a hard `Err`,
/// surfaced to the head exactly like an unknown provider.
pub fn build_user_model_resolver(
    store: Arc<dyn UserProviderStore>,
    http_client: HttpClient,
    web_search: Option<WebSearchConfig>,
) -> ModelResolver {
    Arc::new(move |user, provider, model| {
        let user =
            user.ok_or_else(|| "multi-user model resolution requires a session user".to_string())?;
        let ctx = store
            .context(user)
            .ok_or_else(|| format!("no provider context configured for user `{user}`"))?;
        resolve_for_user(
            &ctx,
            &http_client,
            web_search.clone(),
            provider,
            model,
            user,
        )
    })
}

fn resolve_for_user(
    ctx: &UserProviderContext,
    http_client: &HttpClient,
    web_search: Option<WebSearchConfig>,
    provider: &str,
    model: &str,
    user: &UserId,
) -> Result<ResolvedModel, String> {
    let entry = ctx
        .catalog
        .provider(provider)
        .ok_or_else(|| format!("unknown provider `{provider}` for this user"))?;
    let key = ctx.key_for(entry);
    // OAuth bearer source (#684 edge d): required when the entry declares
    // `oauth:`, in which case it replaces the static key on the wire.
    let auth = ctx.auth_for(entry)?;
    let rpm = entry.rpm;
    let concurrency = entry.concurrency;
    // Resolved per request against the user's own catalog (#550), not once
    // here against just this call's `model` — see `Catalog::
    // model_concurrency_resolver`.
    let model_concurrency = ctx.catalog.model_concurrency_resolver(provider);
    // Per-user admission gate on top of the shared endpoint pool (#632,
    // ADR-0175): this user's own rpm/concurrency becomes an additional,
    // narrower budget keyed by `user` — so two users sharing one literal key
    // each stay within their own slice regardless of who sized the shared
    // endpoint first.
    let http_client = http_client.with_user_budget(UserBudget {
        user: user.clone(),
        rpm,
        concurrency,
    });
    let http_client = &http_client;

    let llm_factory = match entry.wire {
        Wire::Openai => {
            let base = entry
                .base_url
                .clone()
                .unwrap_or_else(|| OPENAI_BASE.to_string());
            openai_factory(
                base,
                key.map(str::to_string),
                auth,
                model.to_string(),
                rpm,
                concurrency,
                model_concurrency,
                web_search,
                entry.prompt_cache_key,
                http_client.clone(),
            )
        }
        Wire::Anthropic => {
            // An OAuth entry needs no static key — the bearer replaces it.
            let key = match &auth {
                Some(_) => String::new(),
                None => key
                    .ok_or_else(|| format!("no API key configured for provider `{provider}`"))?
                    .to_string(),
            };
            let base = entry
                .base_url
                .clone()
                .unwrap_or_else(|| ANTHROPIC_BASE.to_string());
            let web_search_tool_version = ctx
                .catalog
                .model(provider, model)
                .and_then(|m| m.web_search_tool_version.clone());
            // Which extended-thinking shape this model takes; the newer Anthropic
            // models reject the fixed-budget form outright.
            let thinking_style = ctx
                .catalog
                .model(provider, model)
                .map(|m| m.resolved_thinking_style())
                .unwrap_or_default();
            // Anthropic requires a captured thinking block back on a tool
            // round-trip, and replay is inert when thinking is off, so the wire
            // default (and the unknown-model default) is on.
            let replay_thinking = ctx
                .catalog
                .model(provider, model)
                .map(|m| m.replays_thinking(true))
                .unwrap_or(true);
            anthropic_factory(
                base,
                key,
                auth,
                model.to_string(),
                rpm,
                concurrency,
                model_concurrency,
                web_search,
                web_search_tool_version,
                thinking_style,
                replay_thinking,
                http_client.clone(),
            )
        }
        Wire::Gemini => {
            // An OAuth entry needs no static key — the bearer replaces it.
            let key = match &auth {
                Some(_) => String::new(),
                None => key
                    .ok_or_else(|| format!("no API key configured for provider `{provider}`"))?
                    .to_string(),
            };
            let base = entry
                .base_url
                .clone()
                .unwrap_or_else(|| GEMINI_BASE.to_string());
            gemini_factory(
                base,
                key,
                auth,
                model.to_string(),
                rpm,
                concurrency,
                model_concurrency,
                http_client.clone(),
            )
        }
    };

    Ok(ResolvedModel {
        provider: entry.name.clone(),
        model: model.to_string(),
        llm_factory,
        generation: ctx
            .catalog
            .model(provider, model)
            .map(|m| m.generation_params()),
        context_window: ctx
            .catalog
            .model(provider, model)
            .and_then(|m| m.context_window)
            .map(|w| w as usize),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::ModelEntry;

    fn zai_catalog(rpm: Option<u32>) -> Catalog {
        Catalog {
            providers: vec![ProviderEntry {
                name: "zai".into(),
                wire: Wire::Openai,
                base_url: None,
                key_env: Some("ZAI_API_KEY".into()),
                oauth: None,
                rpm,
                concurrency: None,
                mcp_servers: Default::default(),
                prompt_cache_key: false,
                default_model: "glm-5.2".into(),
                models: vec![ModelEntry {
                    id: "glm-5.2".into(),
                    display_name: None,
                    context_window: Some(128_000),
                    supports_thinking: false,
                    supports_temperature: true,
                    default_temperature: None,
                    max_output_tokens: None,
                    thinking_budget_tokens: None,
                    thinking_style: None,
                    replay_thinking: None,
                    default_reasoning_effort: None,
                    pricing: None,
                    concurrency: None,
                    web_search_tool_version: None,
                }],
            }],
        }
    }

    #[test]
    fn resolver_errors_without_a_user() {
        let store: Arc<dyn UserProviderStore> = Arc::new(InMemoryUserProviderStore::new());
        let resolver = build_user_model_resolver(store, HttpClient::new().unwrap(), None);
        let err = resolver(None, "zai", "glm-5.2").err().unwrap();
        assert!(err.contains("session user"));
    }

    #[test]
    fn resolver_errors_for_an_unregistered_user() {
        let store: Arc<dyn UserProviderStore> = Arc::new(InMemoryUserProviderStore::new());
        let resolver = build_user_model_resolver(store, HttpClient::new().unwrap(), None);
        let user = UserId::new("alice");
        let err = resolver(Some(&user), "zai", "glm-5.2").err().unwrap();
        assert!(err.contains("alice"));
    }

    #[test]
    fn resolver_resolves_against_the_users_own_catalog_and_key() {
        let store = InMemoryUserProviderStore::new();
        let alice = UserId::new("alice");
        store.set(
            alice.clone(),
            UserProviderContext::new(zai_catalog(Some(7))).with_key("ZAI_API_KEY", "alice-key"),
        );
        let store: Arc<dyn UserProviderStore> = Arc::new(store);
        let resolver = build_user_model_resolver(store, HttpClient::new().unwrap(), None);
        let resolved = resolver(Some(&alice), "zai", "glm-5.2").expect("resolves");
        assert_eq!(resolved.provider, "zai");
        assert_eq!(resolved.model, "glm-5.2");
        assert_eq!(resolved.context_window, Some(128_000));
    }

    #[test]
    fn two_users_resolve_independently_even_with_the_same_provider_name() {
        let store = InMemoryUserProviderStore::new();
        let alice = UserId::new("alice");
        let bob = UserId::new("bob");
        store.set(
            alice.clone(),
            UserProviderContext::new(zai_catalog(Some(5))).with_key("ZAI_API_KEY", "alice-key"),
        );
        // Bob has no `zai` in his catalog at all — a distinct provider surface.
        store.set(
            bob.clone(),
            UserProviderContext::new(Catalog { providers: vec![] }),
        );
        let store: Arc<dyn UserProviderStore> = Arc::new(store);
        let resolver = build_user_model_resolver(store, HttpClient::new().unwrap(), None);

        assert!(resolver(Some(&alice), "zai", "glm-5.2").is_ok());
        let err = resolver(Some(&bob), "zai", "glm-5.2").err().unwrap();
        assert!(err.contains("unknown provider"));
    }

    /// An `oauth:` catalog entry (#684 edge d) requires a per-user token
    /// source: missing one is a hard error naming the seam, present one
    /// resolves with no static key at all.
    #[test]
    fn oauth_provider_requires_and_uses_a_token_source() {
        struct StaticToken;
        #[async_trait::async_trait]
        impl crate::mcp::auth::AccessTokenSource for StaticToken {
            async fn access_token(&self, _force: bool) -> anyhow::Result<String> {
                Ok("tok".into())
            }
        }

        let mut catalog = zai_catalog(None);
        catalog.providers[0].oauth = Some(Default::default());
        catalog.providers[0].key_env = None; // purely OAuth — no static key

        let store = InMemoryUserProviderStore::new();
        let alice = UserId::new("alice");
        store.set(alice.clone(), UserProviderContext::new(catalog.clone()));
        let shared: Arc<dyn UserProviderStore> = Arc::new(store.clone());
        let resolver = build_user_model_resolver(shared, HttpClient::new().unwrap(), None);
        let err = resolver(Some(&alice), "zai", "glm-5.2").err().unwrap();
        assert!(err.contains("OAuth-protected"), "{err}");
        assert!(err.contains("with_token_source"), "{err}");

        store.set(
            alice.clone(),
            UserProviderContext::new(catalog).with_token_source("zai", Arc::new(StaticToken)),
        );
        let shared: Arc<dyn UserProviderStore> = Arc::new(store);
        let resolver = build_user_model_resolver(shared, HttpClient::new().unwrap(), None);
        assert!(resolver(Some(&alice), "zai", "glm-5.2").is_ok());
    }

    #[test]
    fn two_users_sharing_one_literal_key_both_resolve() {
        // The scenario #632/ADR-0175 is about: two users configured with the
        // *same* literal key on the same provider. Both must still resolve
        // (each through their own `with_user_budget`-attached `HttpClient`,
        // not just whichever resolves first) — the actual admission
        // isolation this produces is verified deeply in `client::user_budget`
        // tests, since that's where the per-user slot state lives.
        let store = InMemoryUserProviderStore::new();
        let alice = UserId::new("alice");
        let bob = UserId::new("bob");
        store.set(
            alice.clone(),
            UserProviderContext::new(zai_catalog(Some(5))).with_key("ZAI_API_KEY", "shared-key"),
        );
        store.set(
            bob.clone(),
            UserProviderContext::new(zai_catalog(Some(2))).with_key("ZAI_API_KEY", "shared-key"),
        );
        let store: Arc<dyn UserProviderStore> = Arc::new(store);
        let resolver = build_user_model_resolver(store, HttpClient::new().unwrap(), None);

        assert!(resolver(Some(&alice), "zai", "glm-5.2").is_ok());
        assert!(resolver(Some(&bob), "zai", "glm-5.2").is_ok());
    }
}
