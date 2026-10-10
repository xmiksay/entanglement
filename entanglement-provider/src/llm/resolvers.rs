//! The runtime-supplied resolution seams: per-request model facts
//! (concurrency cap, thinking handling), the per-session [`LlmFactory`],
//! and the model/generation/aux resolvers a live switch goes through.

use super::{GenerationParams, Llm};

/// Resolves a model id to its per-model in-flight concurrency cap on a given
/// provider's endpoint (#521, ADR-0140), re-run **on every request** rather
/// than baked in once at client construction (#550): a backend's
/// `default_model` and a given [`LlmRequest::model`](super::LlmRequest::model) can diverge — a
/// profile's `model:` set without `provider:` is the documented
/// request-level fallback (`Agent::model_pin` returns `None`, so
/// `SetAgent` doesn't rebind the client) — and resolving the cap once from
/// the client's construction-time model would then pair the *wrong* model's
/// cap with the *actual* request's model. `None` ⇒ no per-model cap; the
/// request admits solely through the endpoint-wide gate.
pub type ModelConcurrencyResolver = std::sync::Arc<dyn Fn(&str) -> Option<usize> + Send + Sync>;

/// A [`ModelConcurrencyResolver`] that ignores the model and always returns
/// `cap` — for callers with no catalog to resolve against (tests, or a
/// backend that only ever serves one fixed model).
pub fn fixed_model_concurrency(cap: Option<usize>) -> ModelConcurrencyResolver {
    std::sync::Arc::new(move |_: &str| cap)
}

/// Resolves a model id to the OpenAI-wire thinking handling ([`crate::
/// ThinkingSpec`], ADR-0191), re-run **on every request** — the same #550
/// property as [`ModelConcurrencyResolver`]: a `model:`-only profile pin can
/// send a request under a different model than the client's construction-time
/// `default_model`, and that actual model's `thinking_format` must win.
pub type ThinkingSpecResolver = std::sync::Arc<dyn Fn(&str) -> crate::ThinkingSpec + Send + Sync>;

/// A [`ThinkingSpecResolver`] that ignores the model and always returns `spec`
/// — for callers with no catalog to resolve against (tests, embedders).
pub fn fixed_thinking_spec(spec: crate::ThinkingSpec) -> ThinkingSpecResolver {
    std::sync::Arc::new(move |_: &str| spec)
}

/// Factory that produces a fresh per-session LLM instance. Sessions run
/// concurrently, so each gets its own (cheaply-clonable) backend.
///
/// A session simply owns its `Box<dyn Llm>`; there is deliberately **no
/// per-session wrapper**. The provider layer's resilience state (connection pool,
/// retry/backoff, RPM budget + `Retry-After` window) is **keyed per endpoint**,
/// not per session, since #217 / [ADR-0050]: sessions talking to the same endpoint
/// share one budget so a throttled endpoint doesn't starve siblings. There is thus
/// no honest session-scoped state for a handle to hold — the earlier `LlmSession`
/// newtype was an empty placeholder and was collapsed away (#195 /
/// [ADR-0062]). Re-introduce the newtype only when genuinely per-session state
/// (e.g. a session-pinned model override or conversation-scoped budget) arrives.
///
/// [ADR-0050]: ../../docs/adr/0050-per-endpoint-connection-pool-retry-rate-limit.md
/// [ADR-0062]: ../../docs/adr/0062-collapse-llmsession-placeholder-newtype.md
pub type LlmFactory = std::sync::Arc<dyn Fn() -> Box<dyn Llm> + Send + Sync>;

/// Everything a live session needs to re-bind itself to a different
/// model/provider without an engine restart (#218): the factory that builds the
/// new `Box<dyn Llm>`, the effective model id + provider it resolved to, and the
/// per-model generation knobs + context window that must follow the switch. The
/// runtime resolves it from the catalog + user config (reusing the same wire /
/// base / key resolution as startup); core applies it to a running session on
/// a `SetModel` message — rebuilding `Session::llm`, retargeting the request
/// model, re-budgeting the context window.
#[derive(Clone)]
pub struct ResolvedModel {
    /// Catalog provider name the switch landed on (canonical, from the entry).
    pub provider: String,
    /// The effective model id — sent on every subsequent request and used to
    /// price the turn.
    pub model: String,
    /// Builds the new per-session backend bound to `provider`/`model`.
    pub llm_factory: LlmFactory,
    /// Generation knobs for `model` (temperature / max-output / thinking), gated
    /// by its catalog capabilities. `None` for a model absent from the catalog.
    pub generation: Option<GenerationParams>,
    /// `model`'s context window in tokens, so the session re-budgets its history
    /// against the real window. `None` (unknown model) falls back to the flat cap.
    pub context_window: Option<usize>,
}

/// Identity of the human/tenant a session belongs to, in a multi-user
/// deployment (#522, ADR-0147). Established once at session creation (an
/// embedder-authored `InMsg::Spawn` carries it, in `entanglement-core`) and
/// never mutated afterward — a session's user is a spawn-time fact exactly like
/// its `parent`; a spawned child inherits its parent's user rather than setting
/// its own. `None` (the default, single-user mode) means the session runs under the
/// process-global provider catalog/config, byte-identical to pre-#522 behavior.
/// Opaque to the engine: an embedder picks the string (a DB primary key, an
/// email, whatever it already uses to key its own per-tenant state) — core
/// never validates, parses, or looks it up; it only threads the value to the
/// seams that need it ([`ModelResolver`] here, the runtime's
/// `PermissionResolver`/`GrantStore` via `Session`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct UserId(pub String);

impl UserId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

impl std::fmt::Display for UserId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

/// Re-resolves a `(provider, model)` pair to a [`ResolvedModel`] for a
/// mid-session switch (#218), or `Err(message)` when the provider is unknown or
/// its API key is unset. Held by the engine config so a session can swap its LLM
/// with no restart; the runtime builds it capturing the provider catalog + the
/// per-endpoint HTTP client (already warm, #217).
///
/// Takes the resolving session's [`UserId`] (`None` in single-user mode, #522):
/// a multi-user runtime's resolver closure looks up that user's own catalog
/// overlay + API key instead of the one process-global catalog/env-var key a
/// single-user embedder's closure captures. Every call site (session start's
/// agent-pin rebind, `SetModel`, the supervisor on an `InMsg::Spawn` carrying a
/// `model` (#725), and replay on resume) already has the session's user in
/// scope, so this costs single-user callers nothing — they simply ignore the
/// parameter.
///
/// Runs on the engine's supervisor loop for `Spawn`/`Resume`, so it must be
/// cheap and must not block — no network I/O. A slow resolver stalls every
/// session's routing, not just the one being resolved. For a multi-user
/// engine that includes the store behind
/// [`build_user_model_resolver`][crate::multi_user::build_user_model_resolver]:
/// it calls `UserProviderStore::context` on every resolve, so that lookup
/// must answer from memory too.
pub type ModelResolver = std::sync::Arc<
    dyn Fn(Option<&UserId>, &str, &str) -> Result<ResolvedModel, String> + Send + Sync,
>;

/// Resolves a named agent profile's **persisted** generation override (#374,
/// the generation-parameter analogue of the model pin ADR-0081 bakes directly
/// into `Agent.provider`/`model`). [`GenerationParams`] carries a
/// non-`Eq` `f32` (`temperature`), so it can't join `Agent`'s
/// `PartialEq + Eq` derive the way the pin fields do — this resolver is a
/// separate seam instead, mirroring [`ModelResolver`]'s shape but purely local
/// (a managed-file lookup, no network/key validation), hence `Option` rather
/// than `Result`. Held by the engine config; the runtime supplies it wrapping
/// its `AgentGenerationStore`. `None` (the default, or a lookup miss) means the
/// profile carries no persisted override — the session keeps its current
/// generation binding.
pub type GenerationResolver =
    std::sync::Arc<dyn Fn(&str) -> Option<GenerationParams> + Send + Sync>;

/// Resolves a **side-transformation purpose** (`"summarize"`, …) to the
/// provider/model pinned for it, so an out-of-band LLM call can run on a
/// cheaper or faster backend than the session's own (Issue 5).
///
/// Shaped like [`GenerationResolver`] — purely local, a managed-file lookup, so
/// `Option` rather than `Result` — but reusing [`ResolvedModel`] as its payload
/// so the caller builds the one-shot client from the same `llm_factory` an
/// `InMsg::SetModel` switch would, inheriting the warm per-endpoint pool
/// (ADR-0050) for free.
///
/// `None` — no pin for that purpose, or a pin the catalog no longer knows — is
/// the ordinary case and means "use the session's own backend". That fallback
/// deliberately lands on the *session's* current binding rather than a fixed
/// primary, so a live `/model` switch keeps applying to side transformations
/// exactly as it does to the turn loop.
pub type AuxLlmResolver = std::sync::Arc<dyn Fn(&str) -> Option<ResolvedModel> + Send + Sync>;
