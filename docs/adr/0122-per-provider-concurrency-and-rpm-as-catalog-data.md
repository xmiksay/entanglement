# 0122. Per-provider concurrency and RPM as catalog data

- Status: Accepted
- Date: 2026-07-21

## Context

[ADR-0111](0111-adaptive-endpoint-pacing-and-429-retry-until-clear.md) shipped
the per-endpoint **concurrency cap** (default 3, `ENTANGLEMENT_MAX_CONCURRENCY`),
the adaptive AIMD pacing gate, and the bounded 429-retry-until-clear loop. Its
"Negative / neutral" and "Alternatives considered" sections explicitly deferred
the natural follow-up: making the concurrency cap — and its RPM sibling —
**per-provider catalog data** rather than one global default applied per
endpoint. Quoting ADR-0111:

> Concurrency is one global default (3) applied per endpoint, not yet per-provider
> catalog data — a deferred follow-up (see the deferred-work ledger).

> **Per-provider concurrency in the catalog now.** Deferred: a global default +
> env override covers the reported case; per-provider tuning can layer on later.

That deferral was tracked under [issue
#414](https://github.com/xmiksay/entanglement/issues/414) and has since shipped.
This ADR records the decision that landed and supersedes ADR-0111's
"Deferred" framing — the feature is no longer deferred. (Per the project's
immutability rule, ADR-0111 itself is not edited in place.)

## Decision

`ProviderEntry` (in `entanglement-provider/src/catalog.rs`) carries two optional
catalog fields mirroring the existing client-level `RetryConfig` knobs:

- `rpm: Option<u32>` — requests-per-minute budget for this provider's endpoint
  bucket; `None` falls back to the client default (`RetryConfig::rpm`).
- `concurrency: Option<usize>` — max simultaneously in-flight requests to this
  provider's endpoint; `None` falls back to the client default
  (`RetryConfig::concurrency`, 3).

Both are `#[serde(default)]` and `deny_unknown_fields`-guarded exactly like the
rest of the catalog. Precedence, unified with the catalog's existing rule, is
**env > user YAML (`providers.yml`) > embedded defaults (`defaults.yml`) > client
default**:

- The head's `resolve_rpm` / `resolve_concurrency` helpers
  (`entanglement-runtime/src/main.rs`) read `<NAME>_RPM` / `<NAME>_CONCURRENCY`
  (uppercased provider name) first, then fall back to the catalog entry's
  `rpm` / `concurrency`, then to `None` (the client default).
- `ENTANGLEMENT_MAX_CONCURRENCY` stays as the **last-resort process-wide
  override** of the client default itself — it changes what `None` resolves to,
  not the per-provider value. So a per-provider cap, when set, always wins.

Neither field is set in the embedded `defaults.yml` — a deliberate choice that
keeps the catalog's stated budgets honest (we don't know a real provider's
concurrency ceiling; a wrong default either storms or needlessly serializes), so
every provider falls through to the client default (3) unless a user opts in.
This mirrors how `rpm` already behaved pre-#414.

## Consequences

### Positive

- A user with a known provider ceiling (e.g. a paid z.ai tier allowing 8
  concurrent streams, or a self-hosted vLLM endpoint) can set it once in
  `providers.yml` instead of process-wide via `ENTANGLEMENT_MAX_CONCURRENCY`,
  so a mix of providers in one process each get their real budget.
- The catalog's "env > user YAML > embedded defaults" precedence — already
  proven on model metadata — now covers the resilience knobs too; one rule, not
  two.
- Sub-agent spawn against a high-concurrency provider no longer serializes
  behind the conservative default 3; against a low-ceiling provider, it no
  longer storms.

### Negative / neutral

- The embedded defaults stay unset: a user must opt in per provider. A wrong
  user-set value still either storms (too high) or serializes (too low); the
  AIMD pacing gate (ADR-0111) remains the self-tuning safety net for RPM, and
  the bounded 429-retry surfaces a saturated endpoint as an error rather than
  hanging.
- `ENTANGLEMENT_MAX_CONCURRENCY` is now a fallback of last resort rather than
  the primary knob; existing users who set it are unaffected (it still overrides
  the client default), but the documented primary surface is the per-provider
  field + `<NAME>_CONCURRENCY`.

## Alternatives considered

- **Keep the global-only knob forever.** Rejected: a multi-provider process
  (z.ai primary + a self-hosted vLLM proxy, say) has wildly different real
  ceilings, and one global number can't be right for both.
- **Seed embedded defaults with per-provider guesses.** Rejected: we don't
  have authoritative concurrency limits for any provider, and a wrong embedded
  default is worse than the conservative client default (3) the user can
  override. The AIMD gate covers RPM self-tuning; concurrency has no such
  self-correction, so a wrong static value sticks.
- **Drop `ENTANGLEMENT_MAX_CONCURRENCY` now that the per-provider field
  exists.** Rejected: it remains a useful escape hatch for "raise every
  endpoint's ceiling at once" without touching the catalog, and removing it
  would break existing setups for no behavior win.

## References

- [ADR-0111](0111-adaptive-endpoint-pacing-and-429-retry-until-clear.md): the
  per-endpoint concurrency cap, adaptive pacing, and bounded 429-retry this
  builds on; its "Deferred" framing is superseded by this ADR.
- [ADR-0032](0032-yaml-provider-model-catalog.md): the YAML provider/model
  catalog whose precedence rule (`env > user YAML > embedded defaults`) this
  extends to the resilience knobs.
- [ADR-0050](0050-per-endpoint-connection-pool-retry-rate-limit.md): the
  per-`(endpoint, api-key)` pool that consumes both knobs.
