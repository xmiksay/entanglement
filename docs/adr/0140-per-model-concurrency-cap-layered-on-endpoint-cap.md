# 0140. Per-model concurrency cap, layered on the endpoint cap

- Status: Accepted
- Date: 2026-07-31

## Context

[ADR-0111](0111-adaptive-endpoint-pacing-and-429-retry-until-clear.md) and
[ADR-0122](0122-per-provider-concurrency-and-rpm-as-catalog-data.md) made the
in-flight concurrency cap a property of the **endpoint** — one `Semaphore`
sized once per `(base URL, api-key hash)` pool key, defaulting to 3 and
overridable per provider via catalog `concurrency` / `{NAME}_CONCURRENCY`.
That framing assumed a provider's real ceiling is uniform across every model
served from the same base URL.

z.ai does not work that way. Its documented concurrency limits are **per
model**, not per endpoint: only **one** in-flight request is allowed for
`GLM-4.7-Flash`, while **five** may run concurrently for `GLM-5.2` — same base
URL, same API key. Per-profile model pinning
([ADR-0081](0081-per-profile-model-pinning-and-rebind-on-set-agent.md), #323)
makes a mixed-model workload on one endpoint the normal case, not an edge
case: a cheap Flash profile for sub-agents beside a GLM-5.2 main session, both
sharing the same `EndpointState`.

A single endpoint-wide cap cannot express this correctly:

- Set it to 5 (GLM-5.2's real ceiling): five concurrent Flash calls are
  admitted locally and 429-storm z.ai's actual 1-in-flight limit for that
  model. The resulting cool-down (`EndpointState::retry_after`) is
  endpoint-wide, so it also parks every concurrent GLM-5.2 session — a Flash
  sub-agent's mistake stalls the unrelated main session.
- Set it to 1 (Flash's real ceiling): GLM-5.2 is needlessly serialized to a
  fifth of its actual allowance.

There is no single number that is correct for both models on one endpoint.

## Decision

Add a **second, tighter admission gate** scoped to `(endpoint, model)`,
acquired *underneath* the existing endpoint-wide permit — not a replacement
for the per-endpoint pool, a layer on top of it.

### Catalog: `ModelEntry.concurrency: Option<usize>`

Mirrors `ProviderEntry::concurrency` (ADR-0122) but at model granularity.
`None` (the default for nearly every model) means "no tighter cap than this
model's endpoint" — falls through to the endpoint-wide cap exactly as before
this ADR. The embedded `defaults.yml` ships the two z.ai tiers the issue
documents (`glm-4.7-flash: 1`, `glm-5.2: 5`) and leaves every other model
unset — deliberately, mirroring ADR-0122's own reasoning: an invented guess
at an undocumented model's real ceiling is worse than falling back to the
endpoint default, since (unlike RPM) a wrong concurrency value has no
self-tuning safety net. A user's `providers.yml` can set or override
`concurrency` on any model via the catalog's existing deep-merge.

**No env override for this level (v1).** `ProviderEntry::concurrency` reads
`{NAME}_CONCURRENCY`; a model-level analogue would need
`{NAME}_{MODEL}_CONCURRENCY`, but model ids contain `.`/`-`
(`glm-4.7-flash`) with no established mangling rule in this codebase for
turning an arbitrary id into an env-safe token. Rather than invent one
half-considered, this ADR keeps the model level YAML-only; env-override
support is a well-scoped future addition if a real need shows up.

### Client: a per-model `Semaphore`, scoped to its endpoint, acquired *first*

`EndpointState` (`entanglement-provider/src/client/mod.rs`) gains
`model_concurrency: Mutex<HashMap<String, Arc<ModelSlot>>>` — lazily created
per model id on first use, exactly like `EndpointPool::endpoints` itself
lazily creates each `EndpointState`. `HttpClient::execute_with_retry` gains
`model: &str, model_concurrency: Option<usize>` parameters; when the latter
is `Some`, it resolves (and acquires an owned permit from) that model's own
`Semaphore`. Both permits are held for the whole streamed body via a
`StreamGuard` that now wraps `(OwnedSemaphorePermit,
Option<OwnedSemaphorePermit>)`, and both are released together when the body
ends. `model_concurrency: None` means no model permit is ever acquired — a
model with no catalog cap behaves byte-identically to the pre-#521 client.

**The endpoint-wide cap is the ceiling on the *sum* across every model
sharing it** — every request still acquires the endpoint permit regardless of
model, exactly as before this ADR; a model's own cap only ever tightens its
own admission further, never loosens the endpoint's. Example: endpoint cap
10, `glm-5.1: 4`, `glm-4.7: 6` — 4 and 6 may run together (10 total); a 5th
`glm-5.1` call queues on its own model semaphore even with endpoint room to
spare; an 11th call of any mix queues on the endpoint semaphore.

**Acquisition order is model permit first, then endpoint permit** (revised
from an earlier draft of this ADR, which had it the other way around) —
release in the reverse order. This matters for a subtle starvation case: if
the endpoint permit were acquired first, a caller that goes on to block on
its own (saturated) model semaphore would sit there **holding a scarce
endpoint permit** for as long as it waits — starving *every other model*
sharing the endpoint of admission, even though the endpoint itself has spare
capacity and the blocked model is irrelevant to them. Acquiring the model
permit first means a caller blocked on its own model's slot holds nothing
endpoint-wide; only once it actually has room to proceed on the model side
does it compete for the shared endpoint slot. The fixed order (model, then
endpoint) is applied uniformly on every call path, so it introduces no
deadlock risk.

**Validation, not rejection, when a model's cap exceeds its endpoint's.** A
model `concurrency` wider than its provider's `concurrency` is legal — the
narrower endpoint cap simply binds first, so the model cap can never actually
be reached — but it is almost certainly a misconfiguration, so
`EndpointState::model_slot` logs a `tracing::warn!` (once, the first time
that model's slot is created) rather than erroring or refusing to build the
client.

Each of the three wire clients (`OpenAiLlm`, `AnthropicLlm`, `GeminiLlm`)
gains a `model_concurrency: Option<usize>` field, resolved once when the
runtime builds its `LlmFactory` for an explicit `(entry, model)` pair — the
same point `resolve_concurrency`/`resolve_rpm` already resolve the
provider-level knobs (`openai_factory_for`/`anthropic_factory_for`/
`gemini_factory_for` in `entanglement-runtime/src/main.rs`, shared by startup
and the live `SetModel` resolver, ADR-0063). This is sound because a factory
is rebuilt on every model switch — the bound `model_concurrency` always
matches the model the client actually requests.

### The endpoint-wide `Retry-After`/pacing stays shared (v1)

A 429 still parks the *whole* endpoint and slows its one pacing gate,
regardless of which model triggered it — the model-level admission gate only
prevents *this client* from over-admitting a model locally; it does not
change how a 429 response is handled once one arrives. z.ai's error body may
well distinguish "this model is saturated" from "this endpoint is
saturated," and scoping the cool-down to just the offending model is a
reasonable follow-up, but v1 keeps the existing endpoint-wide behavior rather
than speculatively parsing an error shape this codebase hasn't verified
against a live response.

### `ThrottleStatus` reports whichever cap is currently binding

`ThrottleStatus` (`client/status.rs`, #517) gains `model: Option<String>`.
`EndpointState::status` now also inspects every live per-model slot and
compares its occupancy ratio (`in_flight / cap`) against the endpoint's own;
whichever is more saturated is reported (ties keep the endpoint). An idle or
never-created model slot never shadows a genuine endpoint-wide cool-down —
only a slot whose ratio is *strictly* tighter wins. A model with no cap never
creates a slot at all, so it never appears as binding — the TUI's throttle
indicator is unaffected for every provider/model that doesn't opt in. The
TUI's `throttle_label` renders the model id alongside the host when it is the
binding constraint (`⚠ api.z.ai (glm-4.7-flash) busy · 1/1`), so a saturated
Flash slot doesn't read as an unexplained, seemingly-wrong endpoint cap.

## Consequences

### Positive

- The mixed-model scenario the issue reports is fixed: Flash and GLM-5.2 each
  meter against their own real ceiling on one shared endpoint, with neither
  429-storming nor needlessly serializing the other.
- The model-first acquisition order additionally prevents a queued caller of
  one saturated model from starving *every other* model on the same endpoint
  of admission — a failure mode an endpoint-first order would have
  reintroduced at exactly the moment this ADR exists to fix.
- No wire, protocol, or session-visible change — this is entirely inside the
  provider layer's existing per-endpoint pool. A model with no per-model cap
  is provably unaffected (the `Option::map` that would create its slot never
  fires).
- `ThrottleStatus` stays honest under the new two-level admission instead of
  silently under- or over-reporting saturation.

### Negative / neutral

- A 429 still cools down the whole endpoint even when it names one model —
  deferred, see above; tracked as a natural v2 if z.ai's error body turns out
  to carry a reliably-parseable model scope.
- No env override at the model level yet — a user who wants to tune a
  per-model cap without touching `providers.yml` has no shortcut. Acceptable:
  the provider-level `{NAME}_CONCURRENCY` remains available for the common
  "raise/lower this endpoint's overall ceiling" case, and YAML editing for a
  specific model's tier is a one-line change.
- `execute_with_retry` and every `*_factory`/`*Llm::new` constructor gained
  one more parameter each (`model`/`model_concurrency`) — mechanical, but
  touches all three wire clients and their runtime call sites.

## Alternatives considered

- **One endpoint-wide cap, tuned to the tightest model's ceiling.** Rejected:
  this is exactly the "serializes the looser model to a fifth of its real
  allowance" failure the issue reports — correct for no model on a mixed
  endpoint.
- **A separate `HttpClient`/pool per model.** Rejected: throws away the
  endpoint-wide RPM pacing and `Retry-After` coordination ADR-0050/ADR-0111
  built specifically because per-session/per-model isolation let concurrent
  callers 429-storm each other; the endpoint-wide layer is still the right
  scope for "don't collectively overrun this host."
- **Scope the 429 cool-down to the offending model now.** Deferred (see
  above) — would require parsing a provider-specific error-body shape this
  codebase has not verified live, mirroring the caution ADR-0131 already
  applied to the z.ai web-search array shape.
- **`{NAME}_{MODEL}_CONCURRENCY` env override now.** Deferred: no existing
  precedent in this codebase for mangling an arbitrary model id into an env
  var name; YAML-only is simpler and sufficient for v1 (recommended by the
  issue itself).
- **Endpoint permit acquired before the model permit.** An earlier draft of
  this ADR had this order; rejected once the starvation case above was
  identified — it would let a caller queued on a saturated model hold a
  scarce endpoint permit hostage, starving unrelated sibling models on the
  same endpoint even with room to spare.
- **Reject a model cap wider than its endpoint's at load time.** Rejected:
  the configuration isn't unsound (the narrower cap still binds correctly,
  the wider one is just inert), so refusing to start over it would be overly
  strict for what is at worst a no-op setting; a warning gives the same
  visibility without the availability cost.

## References

- [ADR-0111](0111-adaptive-endpoint-pacing-and-429-retry-until-clear.md): the
  per-endpoint concurrency cap, adaptive pacing, and bounded 429-retry this
  layers on top of.
- [ADR-0122](0122-per-provider-concurrency-and-rpm-as-catalog-data.md): the
  provider-level `concurrency`/`rpm` catalog fields and env-override
  precedence this ADR mirrors at model granularity.
- [ADR-0081](0081-per-profile-model-pinning-and-rebind-on-set-agent.md): the
  per-profile model pinning that makes a mixed-model workload on one endpoint
  the normal case.
- [ADR-0050](0050-per-endpoint-connection-pool-retry-rate-limit.md): the
  per-`(endpoint, api-key)` pool both concurrency layers live inside.
- Issue #521 (per-model concurrency limits), referencing #512/#517.
