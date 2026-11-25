# 0063. Realtime model/provider switch without engine restart

- Status: Accepted
- Date: 2026-07-14
- Builds on the seam inversion of [0053](0053-invert-core-provider-seam.md) and the
  collapsed backend handle of [0062](0062-collapse-llmsession-placeholder-newtype.md);
  reuses the per-endpoint pool of [0050](0050-per-endpoint-connection-pool-retry-rate-limit.md).
  Issue #218, epic #190.

## Context

The provider/model was resolved **once** at startup: `select_provider` picked a
catalog entry from `ENTANGLEMENT_PROVIDER` / key auto-detect, baked an
`LlmFactory` (+ `generation`, `context_window`, `pricing`) into `EngineConfig`,
and `Holly::spawn` moved that config in. Changing model or provider meant killing
`skutter` and restarting. The TUI already had a `/model` picker, but Enter only
closed it — nothing reached the live engine.

Two facts shape the fix:

- The entry→`Llm` mapping (wire dispatch, base/key resolution) lives in the
  **runtime**, not core or provider — core can't call it (dep direction
  `runtime → core → provider`, [0053](0053-invert-core-provider-seam.md)).
- A `Session` owns a plain `Box<dyn Llm>` with **no** re-resolution handle
  ([0062](0062-collapse-llmsession-placeholder-newtype.md)); `generation`/
  `context_window`/`pricing` were read from the immutable `EngineConfig`, and the
  effective model came from `AgentProfile::model`.

## Decision

Add a **`SetModel { provider, model }`** inbound message and a **`ModelChanged`**
outbound event, and a runtime-supplied **resolver closure** on the config:

- `EngineConfig::model_resolver: Option<ModelResolver>` where
  `ModelResolver = Arc<dyn Fn(&str, &str) -> Result<ResolvedModel, String> + Send + Sync>`.
  `ResolvedModel` (in `entanglement-provider`) bundles `{ provider, model,
  llm_factory, generation, context_window }`. The runtime builds the closure
  capturing the `Catalog` + the warm per-endpoint `HttpClient` (#217), reusing the
  **same** wire/base/key helpers as startup (`openai_factory_for` /
  `anthropic_factory_for`), so a mid-session switch binds exactly like a fresh
  launch would, minus the model-default fallback (the head chooses the model).
- On `SetModel`, the session loop rebuilds `Session::llm = (resolved.llm_factory)()`
  and updates **per-session** effective state: new `Session::model` (overrides the
  profile's pinned model on every request and in pricing) and `Session::generation`
  (seeded from `EngineConfig::generation` at creation), and re-budgets the history
  via `Context::set_window`. It emits `ModelChanged { provider, model,
  context_window }`; an unknown provider / missing key emits `Error`.
- Both fields are **catalog-qualified** (a head's picker yields the provider
  alongside the model), so one message covers a same-provider model change and a
  full provider switch uniformly.
- Deferred during a live turn (stash replay), exactly like `SetAgent`. Replay
  re-applies `ModelChanged` so a resumed session re-binds to the switched model.

`pricing` did not need per-session state: `EngineConfig::pricing` already maps
**every** catalog model id, so pricing the new model is just a new lookup key.

### Why `Session` fields, not a re-introduced `LlmSession`

[0062](0062-collapse-llmsession-placeholder-newtype.md) foresaw re-introducing the
newtype when "a session-pinned model override" arrived — which is exactly this.
We chose the **most direct expression (KISS)**: two `Session` fields
(`model`, `generation`) plus the existing `Box<dyn Llm>`. The switched state is
genuinely per-session engine state that lives next to `Session::profile` and
`Session::ctx`; wrapping the backend in a newtype would not hold any of it (model
and generation aren't backend state, and resilience stays per-endpoint). A newtype
would add indirection without a home for the new fields.

## Consequences

### Positive

- Model/provider change is a live protocol round-trip — no restart, no lost
  conversation. Per-endpoint clients stay warm across switches (#217).
- The core↔runtime seam stays honest: core holds only a closure; all catalog/wire
  knowledge stays in the runtime.
- Startup and the live switch share one wire/base/key resolution path, so they
  can't drift.

### Negative / neutral

- `EngineConfig` grows one optional field; an embedder that doesn't wire a
  resolver makes `SetModel` a clean `Error`, not a panic.
- The effective model now has a precedence: `Session::model` (a manual switch)
  overrides `AgentProfile::model`. A later `SetAgent` to a model-pinning profile
  does **not** clear a manual override — the explicit user choice wins until the
  next `SetModel`. Documented, and reversible by a further switch.

## References

- Issue #218: realtime model/provider switch without engine restart
- [0050](0050-per-endpoint-connection-pool-retry-rate-limit.md): per-endpoint pool
  the switch reuses (clients stay warm, #217)
- [0053](0053-invert-core-provider-seam.md): dep direction that forces the resolver
  closure seam
- [0062](0062-collapse-llmsession-placeholder-newtype.md): foresaw this as the
  trigger to re-introduce `LlmSession`; we chose `Session` fields instead
- Part of epic #190 (provider seam + per-endpoint pool)
