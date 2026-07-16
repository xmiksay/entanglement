# 0094. `reasoning_effort` model + per-profile generation persistence

- Status: Accepted
- Date: 2026-07-16
- Builds on the generation-parameter channel of
  [0050](0050-per-endpoint-connection-pool-retry-rate-limit.md)'s sibling #191
  work (`GenerationParams` seeded from the catalog into every `LlmRequest`),
  the realtime model/provider switch of
  [0063](0063-realtime-model-provider-switch.md), and directly mirrors the
  per-profile model pin of
  [0081](0081-per-profile-model-pinning-and-rebind-on-set-agent.md) (issue
  #323) — the precedent this ADR follows and deliberately deviates from where
  `GenerationParams`'s shape forces it to. Issue #374 (part of the
  model-parameters umbrella #378). Phase 1 — the TUI `/set`/`/show` surface and
  persist-on-confirmation write are #376.

## Context

`GenerationParams { temperature, max_output_tokens, thinking_budget_tokens }`
is seeded from the provider catalog at session creation and replaced wholesale
only on a `/model` switch (`Session::rebind`). There is no way to change a
single knob mid-session (`/set temperature 0.7` has nowhere to land), no
`effort`/`reasoning_effort` field exists at all, and nothing persists a
generation choice the way ADR-0081 persists a model pin.

Two problems, one shape:

1. **A coarse reasoning-effort knob.** OpenAI's `/chat/completions` wire has a
   native `reasoning_effort: "low"|"medium"|"high"` field with no entanglement
   equivalent. Anthropic and Gemini have no such field, but both have a
   thinking-budget channel already wired (`thinking_budget_tokens`) that an
   effort tier can reasonably map onto.
2. **Live, persisted generation changes.** A user picking "high effort" or
   tweaking temperature while a profile is active should (a) take effect
   immediately, and (b) stick to that profile the next time it's activated —
   this session, and once confirmed via the TUI (#376), future sessions too.

## Decision

### A — `reasoning_effort` on the wire

`GenerationParams` gains `reasoning_effort: Option<ReasoningEffort>`
(`enum ReasoningEffort { Low, Medium, High }`, `#[serde(rename_all =
"lowercase")]` — matches OpenAI's wire values verbatim). The whole struct
(`GenerationParams` + `ReasoningEffort`) gains `Serialize`/`Deserialize` — it
now rides three surfaces that need it: the wire (`InMsg::SetGeneration`,
`OutEvent::GenerationChanged`), the managed generation-override file, and
(already) request logging. `GenerationParams::apply_overrides(&mut self,
overrides: GenerationParams)` is the merge primitive: each `Some` field in
`overrides` replaces the corresponding field in `self`, `None` leaves it
untouched — the seam that makes a partial `/set temperature 0.7` possible.

Per-provider mapping (each client's `build_body` maps unconditionally — an
explicit `thinking_budget_tokens` always takes precedence over a derived
tier):

| Provider | Mapping |
| --- | --- |
| OpenAI-compat | `reasoning_effort` passed through verbatim as the native wire field. |
| Anthropic | No effort concept. `High` → `thinking.budget_tokens` = explicit budget or a 32,000-token tier default; `Medium` → 8,000; `Low`/unset → thinking stays off (temperature passes through, matching the pre-existing no-budget path). |
| Gemini | No effort concept either. Same shape onto `generationConfig.thinkingConfig.thinkingBudget` (`includeThoughts: true`): `High` → 16,384 (a conservative default — Gemini's actual per-model ceiling varies, e.g. 24,576 for 2.5 Flash; an explicit budget always overrides it), `Medium` → 4,096, `Low`/unset → no `thinkingConfig` sent. |
| Echo/Dummy | Ignored — no wire to map onto. |

`ModelEntry` gains `default_reasoning_effort: Option<ReasoningEffort>` so the
catalog can opt a model into a default tier the same way
`default_temperature` does; unset in the embedded defaults (the knob starts
off, exactly like `thinking_budget_tokens` did).

### B — Live change: `InMsg::SetGeneration` / `OutEvent::GenerationChanged`

`SetGeneration { session, overrides: GenerationParams }` — wire-allowed,
session-scoped, deferred (stashed) while a turn is live, mirroring
`SetAgent`/`SetModel`. Unlike `SetModel` there is **no resolver to fail
against** (no network call, no catalog lookup — it's a pure local merge), so
it always succeeds and **always** emits `GenerationChanged` with the full
merged params, even when every override happens to match the current value —
a head can rely on the reply alone to confirm the write landed, with no
separate "did anything change" query.

`GenerationChanged { session, generation: GenerationParams }` carries the
**full** effective params (not a diff) — a point event (`seq() == None`, like
`ModelChanged`), so replay restores it by direct assignment: `session.generation
= Some(generation)`. It also records `session.profile_generation.insert(active_profile,
generation)` — the generation-parameter analogue of `profile_models`, ADR-0081's
per-session memory of `/model` choices.

### C — Per-profile persistence: where this ADR diverges from ADR-0081

ADR-0081's model pin lives *inside* `AgentProfile` (`provider`/`model` fields),
overlaid by `AgentModelStore::apply(&mut ProfileRegistry)` before the engine
builds — so the persisted value and a hand-authored frontmatter pin are, from
core's perspective, the same field. **`GenerationParams` cannot join that
shape**: it carries `temperature: Option<f32>`, and `f32` has no total `Eq` —
`AgentProfile` derives `PartialEq + Eq` (compared on `SetAgent`'s unchanged
guard's sibling checks and in tests), and an `Eq` derive fails to compile over
a non-`Eq` field. Three ways out were considered (see Alternatives); this ADR
picks a **separate resolver seam**, `EngineConfig.generation_resolver: Option<GenerationResolver>`
where `GenerationResolver = Arc<dyn Fn(&str) -> Option<GenerationParams> + Send + Sync>`,
called with a profile *name* rather than baked into the `AgentProfile` value.

Storage: `entanglement-runtime::config::agent_generation::AgentGenerationStore`,
same shape as `AgentModelStore` — a managed (not layered)
`${config_dir}/entanglement/agent-generation.yml` (override
`ENTANGLEMENT_AGENT_GENERATION_FILE`), a `BTreeMap<String, GenerationParams>`
under `load`/`get`/`set`/`reload`, advisory-locked like every other managed
file (`config::lock::with_locked_file`, #329). Its `resolver(store: Arc<Mutex<Self>>)
-> GenerationResolver` wraps a shared handle in the closure `EngineConfig`
consumes — resolved fresh on every call (a `set`/`reload` is visible with no
closure rebuild), the one place this store differs procedurally from
`AgentModelStore::apply` (a one-shot registry overlay at startup).

Precedence — identical in spirit to ADR-0081's *session memory > static pin >
current binding*, applied at the same two loci (`SetAgent`, session start):

1. **Session memory** — `Session.profile_generation.get(profile_name)`, a
   live `SetGeneration` made under that profile this session. Stores the
   **full** merged value (mirrors `profile_models`' full `(provider, model)`
   tuple, not a partial override).
2. **Persisted store** — `cfg.generation_resolver(profile_name)`, also a full
   value (whatever the store last had written for that profile — the future
   TUI persist-on-confirmation write, #376, records the live effective
   params, not a diff).
3. **Current binding, unchanged** — a profile with neither emits no
   `GenerationChanged` (the no-op guard ADR-0081 uses for a pin-less profile).

Both tiers are direct-assignment "full snapshot" values — unlike the
`apply_overrides` partial merge `SetGeneration` itself uses — so `SetAgent`'s
overlay logic is exactly `s.generation = Some(resolved)` with an
inequality-guarded emit, structurally identical to the pin's `s.rebind(...)`
call. Session start applies the tier-2 lookup when `Session.profile_generation`
carries no entry yet for the starting profile (the generation analogue of the
pin's `s.model.is_none()` guard) — a resumed session's replay-reconstructed
memory (from logged `GenerationChanged` records) skips it.

`Session` gains `profile_generation: HashMap<String, GenerationParams>` next
to `profile_models`. Replay reconstructs it the same way replay reconstructs
`profile_models`: fold each `GenerationChanged` record, keyed by the active
profile the preceding `AgentChanged` fold set; a later record in the log still
wins (last-write, consistent with the live engine and with `ModelChanged`).

## Consequences

- One locus (`SetAgent` + session start) covers every entry point exactly as
  ADR-0081 established, no per-head duplication.
- The resolver seam is one extra level of indirection versus ADR-0081's direct
  field overlay, but it's the minimum needed to keep `GenerationParams` a
  plain `Copy + PartialEq` value type without smuggling a non-`Eq` field onto
  `AgentProfile` or dropping that struct's own `Eq` derive (which other code —
  profile-equality checks, deny_unknown_fields-adjacent tests — currently
  relies on).
- `AgentGenerationStore` intentionally has **no** `apply(&mut ProfileRegistry)`
  method, unlike `AgentModelStore` — there is nothing on `AgentProfile` for it
  to overlay onto. A reader expecting API-shape parity with `agent_models.rs`
  should read this ADR's rationale rather than assume an omission.
- The provider-side `reasoning_effort` tier-default budgets (Anthropic
  32,000/8,000, Gemini 16,384/4,096) are conservative constants, not
  per-model-ceiling-aware — a future catalog field could parameterize them,
  but the *explicit* `thinking_budget_tokens` escape hatch already covers
  anyone who needs a specific number today.
- Live reload (ADR-0084/#329) is **not** wired for the generation store in
  this phase — `main.rs` loads it once at startup, matching the model-pin
  store's own startup-only overlay. A `watch.rs` hook is a natural follow-up,
  not required for #374's scope.

## Alternatives considered

- **Bake `generation: Option<GenerationParams>` directly onto `AgentProfile`,
  overlaid the same way `AgentModelStore::apply` overlays `provider`/`model`.**
  Rejected: `AgentProfile` derives `PartialEq + Eq`; `GenerationParams`'s
  `temperature: Option<f32>` has no total `Eq`, so the derive would no longer
  compile without either dropping `Eq` from `AgentProfile` (touching every
  exhaustive/derived-trait use site across the runtime, including ~15 test
  files that construct `AgentProfile` by struct literal) or implementing `Eq`
  by hand with a documented "temperature comparison is bitwise, NaN is not
  itself" caveat — both a larger and a subtler blast radius than a resolver
  seam.
- **A hand-rolled partial `Eq`/hash on `GenerationParams` (treat `f32` fields
  as bit patterns) so it *could* join `AgentProfile`'s derive.** Rejected as
  solving a problem `f32` deliberately doesn't have — bit-pattern equality is
  a footgun (`0.1 + 0.2 != 0.3` bitwise) for a type whose only real comparisons
  are "did the user's intent change," which the resolver seam's `PartialEq`
  (not `Eq`) already answers correctly.
- **Route the persisted override through `EngineConfig::model_resolver` /
  `ResolvedModel`** (piggyback the existing model-switch seam rather than add
  a new one). Rejected: `ModelResolver` is keyed by `(provider, model)` and
  returns a `Result` (it can fail — unknown provider, missing key); a
  generation override is keyed by *profile name* alone and is a pure local
  lookup that cannot fail the way a network-backed model resolve can.
  Conflating the two would force every generation lookup through a
  `(provider, model)` pair it doesn't have at hand, or force the resolver's
  error path to handle a case (a config-file miss) that isn't actually
  fallible.
