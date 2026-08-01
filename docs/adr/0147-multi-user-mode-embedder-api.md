# 0147. Multi-user mode: session-scoped identity + embedder-API per-user seams

- Status: Accepted
- Date: 2026-08-01
- Amends: none. Builds on [ADR-0047](0047-local-trust-boundary.md) (local
  trust model), supersedes [ADR-0048](0048-serve-head-local-trust-model.md)'s
  scope *only* for a future authenticated `serve` — `serve` itself is
  unchanged by this ADR.
- Related: [ADR-0050](0050-per-endpoint-connection-pool-retry-rate-limit.md)
  (pool keyed by base URL + API-key hash), [ADR-0067](0067-mcp-client-as-runtime-tool-provider.md)
  (`HttpClient` public for per-user registries), [ADR-0140](0140-per-model-concurrency-cap-layered-on-endpoint-cap.md)
  (the admission-layer precedent this composes with)

## Context

[#522](https://github.com/xmiksay/entanglement/issues/522): everything
identity-adjacent in entanglement is process-global today, by design
(ADR-0047/ADR-0048's local single-user trust model):

- **No user identity anywhere.** A session carries only a `SessionId`; no
  `InMsg`/`OutEvent` frame names a user or tenant.
- **One provider catalog + one key set per process.** The catalog resolves
  env > user YAML > embedded once at startup; provider API keys load from the
  managed `.env` file (#220) **into the process environment** — inherently
  global, and structurally incompatible with "user A's key must never be
  readable by user B's request." `EngineConfig::model_resolver` was a single
  process-global closure captured over one `Catalog` + one `HttpClient`.
- **One permission ceiling + one grants file per process** (#172/#174):
  `ProfileResolver`'s config ceiling and `DefaultGrantStore`'s managed
  `grants.yml` apply identically to every session.

What already pointed the right way, and is why this ADR is additive rather
than a rearchitecture:

- **ADR-0050's pool already keys by API-key hash.** Two callers with distinct
  keys against the same provider already get independent `EndpointState`
  (RPM ledger, concurrency semaphore, 429 cool-down) — per-user rate-limit
  isolation for user-owned keys falls out for free once a per-user key
  reaches the request path. No pool change was needed.
- **#311's `PermissionResolver`/`GrantStore` are already pluggable traits**,
  session-scoped by design, with the module doc explicitly anticipating "a
  multi-tenant embedder that stores rules per user in its own DB." The gap
  was only that neither trait had a ready-made per-user implementation.
- **`ToolSpecResolver`/`SystemPromptResolver`** (#308/#310) already
  demonstrated the pattern a per-session/per-user seam should take: a
  `Fn(&SessionId, ...) -> ...` an embedder backs with its own snapshot cache,
  consulted fresh at the point of use rather than baked into `EngineConfig`
  at startup.

## Decision

**v1 ships as the embedder library API only.** `serve` stays exactly as
ADR-0048 scoped it — local, single-user, no authentication — because wiring a
bearer-token-to-`UserId` authenticated wire head is a distinct, orthogonal
design problem (session ownership over an untrusted transport) that deserves
its own ADR once a concrete deployment needs it. Everything below is reachable
today only by an in-process embedder that links `entanglement-core`/
`entanglement-runtime` as libraries and drives `Holly` directly — exactly how
`entanglement-runtime`'s own `main.rs` does, just with a different
`EngineConfig`/policy wiring.

### 1. Identity model: a session-scoped, spawn-time-fixed `UserId`

A new `UserId` newtype (`entanglement-provider::UserId` — the leaf crate,
since `ModelResolver`'s signature needs it and provider owns that type;
re-exported through `entanglement-core`) mirrors `SessionId` exactly: an
opaque `String` wrapper core never parses or validates.

`Session` gains a `user: Option<UserId>` field with the identical lifecycle
`parent`/`predecessor` already have: set once at spawn, carried on
`InMsg::Spawn`/`OutEvent::SessionStarted`, reconstructed on replay, never
mutated afterward — **not a per-frame trust boundary**, the session itself
*is* the identity boundary, matching how `parent` already establishes spawn
lineage without a re-authorization on every subsequent frame.

**Inheritance, not re-specification.** `InMsg::Spawn.user` is only
*consulted* for a genuine fresh root (`parent: None`, `predecessor: None`) —
the multi-user embedder's actual session-creation entry point. A child spawn
(`parent: Some(_)`) or a compaction successor (`predecessor: Some(_)`, #397/
ADR-0110) always inherits the live parent's/predecessor's user from the
supervisor's own `session_meta` map, ignoring whatever the `Spawn` message's
`user` field says. This means every existing spawn call site in the
tree — `agent_spawn`/`agent` (`subagent.rs`), the sponsored `build` child
(`propose_plan.rs`), the `/compact` fork (`tui/app/compact.rs`) — needed no
logic change beyond adding the new mandatory field (`None`, since none of
them know or need to know about users): core resolves the inheritance for
them. The lazy-`Prompt`-creates-a-root-session path (an unknown session id
auto-creating a blank single-user-mode session) has no `Spawn.user` to
consult at all — it inherits from a known parent the same way, and a genuine
fresh root there carries no user, which is correct: that path is the
single-user CLI convenience, never the multi-user entry point.

### 2. Per-user provider context (`entanglement-runtime::multi_user::provider`)

`ModelResolver`'s signature widens from `Fn(&str, &str) -> Result<ResolvedModel, String>`
to `Fn(Option<&UserId>, &str, &str) -> Result<ResolvedModel, String>` — the
three call sites in `entanglement-core/src/session.rs` (session-start pin,
`SetAgent` pin rebind, `SetModel`) already have `&Session` in scope, so this
costs every single-user caller nothing (`main.rs`'s `build_model_resolver`
takes the new parameter and ignores it, byte-identical resolution).

A multi-user embedder instead builds a resolver via
`multi_user::provider::build_user_model_resolver`, backed by a
`UserProviderStore` trait it implements over its own storage (a `DB` row, a
config file) — an `InMemoryUserProviderStore` reference implementation ships
for tests/small deployments. Each `UserProviderContext` bundles a per-user
`Catalog` (the same shape as the process-global `providers.yml`, #118 —
providers, models, per-provider `rpm`/`concurrency`, so **per-user RPM
budgets** are just per-user catalog data, no new plumbing) and a
`HashMap<key_env, String>` of API keys — **never placed in `std::env`**,
satisfying the "keys never touch the process env" acceptance criterion
directly, unlike the single-user `.env` file (#220), which stays exactly as
it is for single-user convenience.

The `openai_factory_for`/`anthropic_factory_for`/`gemini_factory_for` helpers
in `main.rs` gained an `explicit_key: Option<&str>` parameter: `Some` is used
verbatim instead of `env_nonempty(entry.key_env)`. `multi_user::provider`
reimplements the same wire-dispatch shape against the public
`entanglement_provider::{openai,anthropic,gemini}_factory` constructors
directly (it's a lean-library module behind the `provider` feature, so it
cannot depend on `main.rs`'s bin-local, unexported helpers). The **shared**
`HttpClient` is still passed through unchanged — ADR-0050's pool key
(`base_url` + `sha256(api_key)`) means two users' distinct keys already land
in separate `EndpointState`s with no further change; two users sharing one
literal key currently share that key's budget too (an accepted v1 gap, see
Consequences).

### 3. Per-user permission ceiling + grants (`entanglement-runtime::multi_user::permission`)

Built entirely on the existing #311 seams, no core change:

- `PerUserPermissionResolver<R: PermissionResolver>` wraps an inner resolver
  (typically `ProfileResolver`, so the process-global #172 ceiling still
  applies first) and clamps its result a second time by the resolving
  session's own user's ceiling, via the same `clamp_to_base` least-privilege
  composition #172 itself uses — two ceilings compose (whichever is
  stricter wins), neither can widen the other.
- `PerUserGrantStore` keys `Always`-scope grants by `UserId` instead of one
  flat process-wide set (`FileGrantStore`'s shape) — the storage key itself
  is what makes "one user's grant never leaks to another" true, not a filter
  applied after the fact. `Session`-scope grants stay keyed by `SessionId`
  exactly like the default store (a session belongs to exactly one user
  already, so no extra isolation is needed there).
- Both need to map a live session back to its user; neither trait's methods
  carry more than `&SessionId`. `SessionUserRegistry` is the small shared
  directory an embedder populates itself (it already knows the mapping — it
  chose `user` when it sent the session's `InMsg::Spawn`) and both
  `PerUserPermissionResolver`/`PerUserGrantStore` read.

## Consequences

- **(+)** Two users in one process run sessions under distinct provider
  catalogs/keys with genuinely isolated rate-limit state (falls out of
  ADR-0050 for free) and genuinely isolated permission ceilings/grants — all
  through the existing pluggable seams, with **zero** changes to
  `tool_runner`'s executor or the turn loop.
- **(+)** Single-user behavior is byte-identical: every widened signature
  (`ModelResolver`) gained a parameter existing callers ignore, every new
  struct field (`Session.user`, `InMsg::Spawn.user`, `SessionInfo.user`) is
  `Option`, defaulting to `None`.
- **(+)** No wire/trust surface grew: `InMsg::Spawn` was already
  trusted-only, non-wire-allowed (#155) before this change.
- **(−)** Per-user RPM/concurrency budgets on a **shared** literal API key are
  out of scope for v1 — ADR-0050's pool has no concept of "the same key, two
  quotas." Follow-up if a deployment needs it (e.g. a secondary key-derived
  discriminator in `pool_key`).
- **(−)** `PerUserGrantStore`/`PerUserPermissionResolver` are in-memory
  reference implementations, not durable across a restart — a production
  multi-tenant embedder with its own database is expected to implement
  `GrantStore`/`PermissionResolver` directly against it (the trait docs
  already describe this shape: `is_granted` can simply return `false`, with
  the durable check folded into `resolve`), not adopt these structs verbatim.
- **(−)** `serve` remains single-user; a multi-user deployment must embed the
  library today. Wiring bearer-token authentication onto `serve` and
  resolving a `UserId` per WS connection is deliberately deferred to its own
  ADR — it is a distinct trust-boundary decision (untrusted wire → identity),
  not a mechanical extension of this one.
- **(−)** `Session.user`, being spawn-time-fixed, means a live session can
  never change users — reassigning ownership mid-session is unsupported
  (matches how `parent` is equally immutable post-spawn; an embedder that
  needs this closes the session and starts a fresh one under the new user,
  the same pattern `/compact`'s copy-on-write fork already uses for a
  different kind of "continue under a new identity").

## Alternatives considered

- **Thread `UserId` per-frame** (every `InMsg`/`OutEvent` carries it, checked
  on each call) instead of session-scoped. Rejected: every existing
  session-keyed seam (`PermissionResolver`, `GrantStore`, `ToolSpecResolver`,
  `SystemPromptResolver`, the seq/activity registries) already treats a
  session as belonging to one continuous identity for its whole lifetime —
  re-litigating that per frame would be a much larger, redundant change for
  no isolation benefit a spawn-time-fixed field doesn't already give.
- **A user-keyed `EngineConfig` per tenant** (one `Holly` per user) instead of
  one shared engine with per-session resolution. Rejected: defeats the
  "one running engine serving multiple users" framing in the issue title —
  the whole point is sharing the supervisor, seq/activity registries, and
  (where safe) the `HttpClient` pool across tenants, not spinning up a
  parallel engine per user.
- **Bake per-user provider/permission state directly into `EngineConfig`**
  (a `HashMap<UserId, ...>` field) instead of the resolver-closure/trait-object
  seams. Rejected: `EngineConfig` is cloned per session spawn and is meant to
  be immutable engine-lifetime config (per its own doc); a live-editable
  per-user directory belongs behind a resolver an embedder can back with its
  own cache/DB, exactly the precedent `ToolSpecResolver`/`SystemPromptResolver`
  already set.
- **Ship the authenticated `serve` head in the same change.** Rejected per
  the issue's own recommended scoping — v1 = embedder API, v2 = authenticated
  `serve` — since the wire-trust design (bearer token validation, per-connection
  `UserId` binding, interaction with the existing per-connection approval
  ownership of ADR-0107) is substantial enough to deserve independent review
  rather than being folded into an already-large identity-model change.

## References

- Issue #522: multi-user mode — per-user providers, API tokens, RPM budgets,
  config in one running engine
- [ADR-0047](0047-local-trust-boundary.md): local trust boundary (repo
  trusted, config precedence)
- [ADR-0048](0048-serve-head-local-trust-model.md): `serve` scoped local/
  single-user — unchanged by this ADR, superseded only for a *future*
  authenticated multi-user wire head
- [ADR-0050](0050-per-endpoint-connection-pool-retry-rate-limit.md): the pool
  keyed by base URL + API-key hash that gives per-user rate-limit isolation
  for free
- [ADR-0067](0067-mcp-client-as-runtime-tool-provider.md): `HttpClient` made
  public specifically so an embedder could assemble per-user registries —
  the note this ADR follows through on
- [ADR-0081](0081-per-profile-model-pinning-and-rebind-on-set-agent.md)/
  [ADR-0094](0094-reasoning-effort-and-per-profile-generation-persistence.md):
  the `ModelResolver`/`GenerationResolver` resolver-closure precedent this
  ADR's `ModelResolver` widening follows
- [ADR-0076](0076-per-session-tool-spec-override.md)/[ADR-0078](0078-per-turn-system-prompt-override.md):
  `ToolSpecResolver`/`SystemPromptResolver`, the `Fn(&SessionId, ...)`
  snapshot-cache pattern this ADR's seams mirror
- [ADR-0110](0110-compaction-successor-closes-predecessor.md): the
  `predecessor` lineage field this ADR's `user` inheritance rule reuses
  verbatim for a compaction successor
