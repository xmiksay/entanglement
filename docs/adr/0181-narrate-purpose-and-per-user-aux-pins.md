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

Fixing that second bug still has to respect a constraint `UserProviderStore`
already established: `entanglement-runtime`'s own `skutter` binary is a
strict single-user application (`serve` stays local single-user, ADR-0048),
and `AuxLlmRegistry` is the type its single-user `main.rs` builds one
process-global instance of. `UserId` belongs to the provider/core seams and
the embedder-facing `multi_user` module — not to the single-user defaults
every plain `skutter` run also constructs. So the fix cannot be "give
`AuxLlmRegistry` a `UserId` field"; it has to bind the user *outside* that
type entirely.

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
  constructor alongside the existing `#[cfg(test)]` `for_test`) and returns an
  ordinary, unmodified `AuxLlmRegistry` — the same type, same fields,
  single-user-shaped exactly as before this ADR.

The user is bound by **wrapping the resolver, not the registry**: before
handing `resolver` to `AuxLlmRegistry::new`, `build_user_aux_registry` closes
over `user` in a new closure — `move |_none, provider, model| resolver(Some(&user),
provider, model)` — that substitutes the captured `user` for whatever
`AuxLlmRegistry` passes (always `None`, since it has no `UserId` field to pass
anything else). `AuxLlmRegistry` itself is untouched: it still always calls
`(self.resolver)(None, &provider, &model)`, exactly as it did before this ADR
— the wrapping closure is what turns that `None` into `Some(user)` before the
call ever reaches the real multi-user `ModelResolver`. This mirrors how
`build_user_model_resolver` itself already captures `store`/`http_client` in a
closure rather than mutating `HttpClient` with a `UserId` field — the same
closure-capture idiom, applied one seam later.

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
  is fixed for every future multi-user aux caller, not just `narrate`'s —
  fixed at the resolver, so it also covers a future `AuxLlmRegistry` consumer
  this ADR didn't anticipate.
- `AuxLlmRegistry` gains no new field and no new public method —
  `entanglement-runtime/src/aux_llm.rs` carries no `UserId` import at all.
  `main.rs`'s single-user construction, every existing test, and the
  `session_title` integration test are byte-identical to before this ADR;
  `UserId` stays confined to `entanglement-provider`/`entanglement-core` (where
  it's a generic embedder concept) and `entanglement-runtime::multi_user`
  (the opt-in embedder-facing module) — never the process-global defaults
  every plain `skutter` run also builds.

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
- **A `user: Option<UserId>` field + `AuxLlmRegistry::for_user(user)` builder
  method directly on `AuxLlmRegistry`**, threaded to the resolver call in
  place of the hardcoded `None`. This was the first cut, and it worked, but it
  put a `UserId` field on the exact type `main.rs`'s single-user path also
  constructs one process-global instance of — `entanglement-runtime`'s
  `skutter` binary is a strict single-user application, so every field on its
  core types should make sense there too, and "the user this single-user
  registry resolves on behalf of, always `None`" doesn't. Rejected in favor of
  wrapping the resolver closure instead (this ADR's Decision): the binding
  moves entirely into `multi_user::aux`, `AuxLlmRegistry` carries no `UserId`
  import, and the two designs are behaviorally identical — same fallback,
  same tests, same call sites — so nothing was given up to make the type
  boundary cleaner.

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
