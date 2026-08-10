# 0181. A `narrate` aux purpose, and per-user aux pins for the multi-user embedder API

- Status: Accepted — Amends [0154]
- Date: 2026-08-10
- Issue: [#635](https://github.com/xmiksay/entanglement/issues/635) (orig.
  tui-ux-batch Issue 5), part of #624

## Context

[ADR-0154]'s "Consequences" explicitly left two things uncovered, tracked as
deferred-work-ledger row 15:

1. A `narrate` purpose — the plan had floated it, but was deferred as
   "rendering 'what the agent is doing' is a stream concern, not an LLM call."
2. Per-user aux pins under the multi-user embedder API ([ADR-0147]) — the
   `AuxModelStore`/`AuxLlmRegistry` pair is process-global, with no per-user
   analogue of [ADR-0147]'s `UserProviderStore`.

`Session.action` ([ADR-0151]) already carries the "what the agent is doing
now" concept on the wire, mid-turn-mutable — but had no in-tree producer.
Nothing renders it. Revisiting deferral 1: turning a raw tool call
(`OutEvent::ToolCall { tool, input, .. }`, already display-only and emitted
for every call before execution) into a short phrase *is* naturally an LLM
call — the same shape as the session-title generator turning a first prompt
into a title. The "stream concern" framing undersold it.

Deferral 2 turned out to have a second, sharper bug once examined:
`AuxLlmRegistry::resolve`/`resolve_pin` call the injected `ModelResolver` with
a hardcoded `None` for the resolving user, always — even before this ADR. A
multi-user `ModelResolver`
([`build_user_model_resolver`][ADR-0147-provider]) treats a missing user as a
hard error ("multi-user model resolution requires a session user"). So a
multi-user embedder plugging its `ModelResolver` into an otherwise-unmodified
`AuxLlmRegistry` wouldn't just get the wrong user's pins — every aux call
would fail outright. The store being process-global was the visible half of
the gap; the hardcoded `None` was the other half.

## Decision

### `narrate`: closed-enum extension, one new runtime-side consumer

`Purpose` (`entanglement-runtime/src/config/aux_models.rs`) gains a third
variant, `Narrate` (`"narrate"` on the wire — `as_str`/`parse`, plus every
enumeration site: `inspect aux-models`, the TUI's `/aux-model` status line and
usage/error text). Same closed-enum shape as `Summarize`/`SessionTitle`: no
new file format, no new persistence code — `AuxModelStore` is generic over
`Purpose`.

The one new consumer, `entanglement-runtime/src/narrate.rs`, mirrors
`session_title.rs` structurally (a background task off `holly.subscribe()`,
tracked per-call tasks in a `JoinSet`, best-effort throughout) but differs at
the trigger: instead of firing once per session on the first `Prompt`, it
fires on every `OutEvent::ToolCall` — the natural, already-existing signal for
"the agent just decided to do something." It asks the aux `narrate` LLM for a
short present-tense phrase (`tool(input)` → e.g. `"Reading src/main.rs"`,
capped input/output like the title generator's prompt/output caps) and sends
it back as `InMsg::SetSessionMeta { action: Some(_), if_unset: false, .. }` —
`action` is unconditionally overwritten on `Some` (`Session`'s fold,
[ADR-0151]), so `if_unset` (a `name`-only guard) doesn't apply here.

Unlike the title generator (fires once, idempotent via a permanent
per-session guard), a burst of tool calls within one turn could otherwise pile
up concurrent aux requests behind a slow model. The narrator instead tracks
**at most one narration call in flight per session**: a later `ToolCall` for a
session already narrating is skipped, not queued, and the guard clears when
the in-flight call's task finishes (success or failure) — so the *next* tool
call after that always gets a fresh narration, it just may not be every call
during a fast burst.

### Per-user aux pins: the `UserProviderStore` pattern, plus the resolver-user fix

`entanglement-runtime/src/multi_user/aux.rs` (new, `provider`-feature-gated
like the sibling `multi_user::provider` module) adds:

- `UserAuxModelStore` — a trait an embedder implements over its own storage,
  the exact counterpart of `UserProviderStore`: `fn pins(&self, user:
  &UserId) -> BTreeMap<Purpose, (String, String)>`. Unlike
  `UserProviderStore::context` (which must succeed to resolve a model at
  all), an unregistered user is `Ok`-shaped here — an empty map, since an aux
  purpose with no pin has a well-defined fallback (the primary model), not a
  hard failure.
- `InMemoryUserAuxModelStore` — the in-memory reference impl, mirroring
  `InMemoryUserProviderStore`.
- `build_user_aux_registry(store, user, resolver, primary, catalog,
  primary_concurrency) -> AuxLlmRegistry` — snapshots `user`'s pins into an
  in-memory `AuxModelStore` (`AuxModelStore::in_memory`, a new public
  constructor alongside the existing `#[cfg(test)]` `for_test`) and binds the
  resulting registry to `user`.

That binding is `AuxLlmRegistry::for_user(user) -> Self`, a new builder method
backed by a new `user: Option<UserId>` field (`None` by default — every
existing single-user call site is unaffected). `resolve`/`resolve_pin` now
call `(self.resolver)(self.user.as_ref(), &provider, &model)` instead of the
hardcoded `(self.resolver)(None, ...)` — fixing the sharper bug from Context
as a side effect of adding the seam, not a separate change.

Like `UserProviderStore::context`, `UserAuxModelStore::pins` is a snapshot
consulted fresh whenever an embedder wants an up-to-date view (session start,
after its own `/aux-model`-equivalent write) — not a live per-call lookup.
`AuxLlmRegistry` already re-reads its wrapped `AuxModelStore` per call
(`resolve`/`resolve_pin` lock and `.get()` each time), so a per-user registry
built once at session start and cached by the embedder — the same lifecycle
`build_user_model_resolver`'s output already has — is what "rides the same
embedder-store pattern" means concretely: reuse the existing consult-per-call
registry, don't invent a second one.

### What stays out of scope

The narrator's trigger cadence (every `ToolCall`, no coalescing/throttling
beyond the one-in-flight-per-session guard) is deliberately the simplest
correct thing, not a final design — a high tool-call-rate session pays for
one aux call per call that lands while the narrator is free. No debouncing
window, no "only narrate every Nth call" heuristic: the in-flight guard alone
already bounds concurrent cost, and premature throttling would be tuning
without a reported problem to tune against (the file-cap discipline in
`CLAUDE.md` cuts the same way — ship the simplest version that's correct,
extend when a concrete cost/UX complaint shows up).

Per-user aux pins are, like [ADR-0147] itself, a **library seam** — this ADR
does not wire `main.rs`'s single-user `serve`/`tui`/stdio heads to it (they
keep the process-global `AuxModelStore`/`AuxLlmRegistry` unchanged). An
embedder wires `multi_user::aux` the same way it already wires
`multi_user::provider`/`permission`.

## Consequences

### Positive

- Closes deferred-work-ledger row 15 exactly along the path the ledger itself
  proposed: a closed-enum extension for `narrate`, an embedder-store pattern
  for per-user pins.
- `Session.action` finally has an in-tree producer — a head that renders it
  (the TUI status line, a future `serve` client) now sees live updates during
  a turn, not just a static "what happened" transcript.
- The `AuxLlmRegistry::resolve`/`resolve_pin` hardcoded-`None` bug (Context)
  is fixed for every future multi-user aux caller, not just `narrate`'s.
- `AuxLlmRegistry::new`'s signature is unchanged (the new `user` field
  defaults via `for_user`, an additive builder method) — no call-site churn
  for `main.rs`, existing tests, or the `session_title` integration test.

### Negative / neutral

- A chatty tool-calling turn now makes one aux LLM call per tool call that
  lands while the narrator is idle — real cost on a per-tool-call cadence,
  unlike the title generator's once-per-session cost. Users who don't want
  this leave `narrate` unpinned *and* never observe cost from it only in the
  sense that an unpinned purpose still falls back to firing against the
  **primary** model (`AuxLlmRegistry::resolve`'s documented no-pin fallback,
  same as session-title) — so the feature is opt-out via a future config
  flag, not opt-in, in this first cut. Acceptable for v1, matching how
  session-title itself shipped opt-out in [ADR-0154]; a per-purpose
  enable/disable toggle is a natural v2 if this proves too aggressive in
  practice.
- `UserAuxModelStore::pins` returning an owned `BTreeMap<Purpose, (String,
  String)>` (not `&AuxModelStore` or similar) means every call allocates a
  fresh snapshot — fine at "once per session start," not designed for a
  hot per-request path.
- `multi_user::aux` has no wiring into any shipped head — like
  `multi_user::provider` before it, it is exercised only by its own unit
  tests until an embedder adopts it.

## Alternatives considered

- **A stream-only `narrate` (no LLM call)**, formatting `tool`/`input`
  directly into a phrase (`"read(src/main.rs)"` → `"Reading src/main.rs"` via
  a fixed per-tool template). Rejected: brittle across the open-ended set of
  tool names/argument shapes (MCP tools, `rhai`, future host tools), and the
  whole point of the `Purpose`/aux-model machinery is to let a user route this
  to a cheap model rather than hardcode a template maintained tool-by-tool.
- **Debounce `narrate` on a fixed timer** (e.g. at most once per 2s per
  session) instead of the in-flight guard. Rejected for v1: adds a
  `tokio::time` dependency on the hot path and a tunable magic number with no
  reported cost complaint to size it against; the in-flight guard is simpler,
  bounds concurrency (the actual risk — pile-up, not raw call count), and is
  easy to layer a timer on top of later without revisiting the trigger.
- **Widen `AuxLlmResolver`/`EngineConfig::aux_llm_resolver` (the core seam
  compaction uses) to also carry `Option<&UserId>`**, so per-user pins reach
  compaction too, not just the runtime-side generators. Rejected for this
  ADR: `Session.user` is already in scope at both `turn.rs`/`ops.rs` call
  sites, so it's plumbable, but it's a second core-facing signature change
  beyond what closing ledger row 15 requires — row 15 only asks for the
  *store* seam, mirroring `UserProviderStore`. A worthwhile follow-up once a
  multi-user embedder actually wants per-user-pinned compaction.
- **A per-user `HashMap<UserId, AuxLlmRegistry>` cache inside the runtime**,
  built once per user and kept fresh some other way, instead of a
  build-fresh-on-demand function. Rejected: matches `UserProviderStore`'s own
  contract (`context` is a lookup, caching is the embedder's job if it wants
  I/O-backed storage) — inventing caching here would duplicate a decision
  [ADR-0147] already made once for the sibling seam.

## References

- [ADR-0154]: the `Purpose` enum, `AuxModelStore`, `AuxLlmRegistry`, and the
  session-title generator this ADR extends; its "Consequences" section is the
  literal source of deferred-work-ledger row 15.
- [ADR-0151]: `Session.action`/`SetSessionMeta`, whose "no in-tree producer
  yet" gap `narrate` fills.
- [ADR-0147]: the multi-user embedder API and `UserProviderStore`
  ([ADR-0147-provider]), the pattern `UserAuxModelStore` mirrors.
- [ADR-0158]: the prior amendment to ADR-0154 (session-title concurrency
  deferral) — same "amend, don't edit" precedent this ADR follows.

[0154]: 0154-per-purpose-auxiliary-models.md
[ADR-0154]: 0154-per-purpose-auxiliary-models.md
[ADR-0151]: 0151-settable-session-metadata.md
[ADR-0147]: 0147-multi-user-mode-embedder-api.md
[ADR-0147-provider]: 0147-multi-user-mode-embedder-api.md
[ADR-0158]: 0158-defer-session-title-aux-call-under-contended-primary-concurrency.md
