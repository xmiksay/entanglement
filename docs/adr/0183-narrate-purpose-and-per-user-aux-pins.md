# 0183. A `narrate` aux purpose; per-user aux pins stay embedder-side

- Status: Accepted — Amends [0154]
- Date: 2026-08-12
- Issue: [#635](https://github.com/xmiksay/entanglement/issues/635) (orig.
  tui-ux-batch Issue 5), part of #624. Conforms to
  [ADR-0181](0181-userid-leaves-the-runtime-crate.md) (`UserId` does not
  appear in the runtime crate).

## Context

[ADR-0154]'s "Consequences" explicitly left two things uncovered, tracked as
deferred-work-ledger row 15:

1. A `narrate` purpose — the plan had floated it, but was deferred as
   "rendering 'what the agent is doing' is a stream concern, not an LLM call."
2. Per-user aux pins under the multi-user embedder API ([ADR-0147]) — the
   `AuxModelStore`/`AuxLlmRegistry` pair is process-global, with no per-user
   resolution path.

`Session.action` ([ADR-0151]) already carries the "what the agent is doing
now" concept on the wire, mid-turn-mutable — but had no in-tree producer.
Nothing renders it. Revisiting deferral 1: turning a raw tool call
(`OutEvent::ToolCall { tool, input, .. }`, already display-only and emitted
for every call before execution) into a short phrase *is* naturally an LLM
call — the same shape as the session-title generator turning a first prompt
into a title. The "stream concern" framing undersold it.

Deferral 2 turned out to have a second, sharper bug once examined:
`AuxLlmRegistry::resolve`/`resolve_pin` call the injected `ModelResolver` with
a hardcoded `None` for the resolving user, always. A multi-user
`ModelResolver` ([`build_user_model_resolver`][ADR-0147-provider]) treats a
missing user as a hard error ("multi-user model resolution requires a session
user"). So a multi-user embedder plugging its `ModelResolver` into an
otherwise-unmodified `AuxLlmRegistry` wouldn't just get the wrong user's pins
— every aux call would fail outright.

Between drafting and landing, [ADR-0181](0181-userid-leaves-the-runtime-crate.md)
fixed the direction for everything per-user: `UserId` must not appear anywhere
in `entanglement-runtime` — no `UserId`-keyed runtime modules; per-user
knowledge stays with the embedder, which reaches the runtime through plain
closures and data. An earlier draft of this change shipped a
`multi_user::aux` module (`UserAuxModelStore` / `InMemoryUserAuxModelStore` /
`build_user_aux_registry`, mirroring `UserProviderStore`); that shape is
exactly what ADR-0181 rejects, and it is not built here.

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

### Per-user aux pins: an embedder closure over public pieces, no runtime module

Per [ADR-0181](0181-userid-leaves-the-runtime-crate.md), the runtime gains
**no per-user module**. Instead the two public pieces an embedder needs are
made available, and the recipe is documented here as the seam's contract:

- **`AuxModelStore::in_memory(pins)`** — a public constructor (production
  sibling of the test-only `for_test`) that wraps a plain
  `BTreeMap<Purpose, (provider, model)>` into the store shape
  `AuxLlmRegistry` already consults per call. The embedder looks a user's
  pins up in its own storage (it owns the session→user mapping and the
  per-user data) and hands them over as data.
- **The resolver is wrapped, not the registry.** `AuxLlmRegistry` keeps no
  user notion and still calls its injected `ModelResolver` with `None` for
  the user; a multi-user embedder closes its resolver over the right user
  *before* construction — `move |_none, provider, model|
  resolver(Some(&user), provider, model)` — the same closure-capture idiom
  `build_user_model_resolver` already uses one seam earlier. This also fixes
  the hardcoded-`None` failure from Context for every aux caller: the
  substitution happens before the call reaches the real multi-user resolver.

An embedder thus builds, per user (typically once at session start, cached
alongside the artifacts of `build_user_model_resolver`):
`AuxLlmRegistry::new(AuxModelStore::in_memory(my_pins_for(user)),
wrapped_resolver, primary, catalog, primary_concurrency)` — an ordinary,
unmodified registry. A null/constant user (single-user embedder) simply skips
the wrap and gets today's process-global behavior; `skutter`'s own heads keep
the one process-global `AuxModelStore`/`AuxLlmRegistry`, byte-identical.

### What stays out of scope

The narrator's trigger cadence (every `ToolCall`, no coalescing/throttling
beyond the one-in-flight-per-session guard) is deliberately the simplest
correct thing, not a final design — a high tool-call-rate session pays for
one aux call per call that lands while the narrator is free. No debouncing
window, no "only narrate every Nth call" heuristic: the in-flight guard alone
already bounds concurrent cost, and premature throttling would be tuning
without a reported problem to tune against.

## Consequences

### Positive

- Closes deferred-work-ledger row 15: a closed-enum extension for `narrate`;
  per-user pins resolved as an embedder recipe over public pieces —
  everything it needs (`AuxModelStore::in_memory`, closure-wrapped resolver,
  plain `AuxLlmRegistry::new`) is public API, nothing left to build in-tree.
- `Session.action` finally has an in-tree producer — a head that renders it
  (the TUI status line, a future `serve` client) now sees live updates during
  a turn, not just a static "what happened" transcript.
- The `AuxLlmRegistry::resolve`/`resolve_pin` hardcoded-`None` bug (Context)
  has a documented, structural fix at the resolver seam — covering every
  future `AuxLlmRegistry` consumer, not just `narrate`'s.
- `AuxLlmRegistry` gains no new field and no new public method;
  `entanglement-runtime` carries no `UserId` in the aux path at all —
  conforming to [ADR-0181](0181-userid-leaves-the-runtime-crate.md) ahead of
  the #687 migration of the older `multi_user` modules.

### Negative / neutral

- A chatty tool-calling turn now makes one aux LLM call per tool call that
  lands while the narrator is idle — real cost on a per-tool-call cadence,
  unlike the title generator's once-per-session cost. An unpinned `narrate`
  purpose falls back to firing against the **primary** model
  (`AuxLlmRegistry::resolve`'s documented no-pin fallback, same as
  session-title) — so the feature is opt-out via a future config flag, not
  opt-in, in this first cut. Acceptable for v1, matching how session-title
  itself shipped opt-out in [ADR-0154]; a per-purpose enable/disable toggle
  is a natural v2 if this proves too aggressive in practice.
- The per-user recipe is documentation + public constructors, not a
  compiled-against trait — an embedder gets no type error if it forgets the
  resolver wrap, just the hard error the multi-user resolver already raises
  on a `None` user. Accepted: that error is loud, immediate, and named, and
  ADR-0181 rates keeping the runtime `UserId`-free above a compile-time rail
  for a seam only embedders touch.

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
- **A `multi_user::aux` runtime module** (`UserAuxModelStore` trait +
  `InMemoryUserAuxModelStore` + `build_user_aux_registry`, mirroring
  `UserProviderStore`). This was the first shipped draft of this change, and
  it worked — but it is another `UserId`-keyed runtime module holding
  knowledge the embedder already owns, exactly the drift
  [ADR-0181](0181-userid-leaves-the-runtime-crate.md) reversed. The
  behavior-preserving core of that draft (wrap the resolver, snapshot pins
  into `AuxModelStore::in_memory`, hand back an ordinary registry) survives
  as the embedder-side recipe in Decision; only the module and its trait die.
- **Widen `AuxLlmResolver`/`EngineConfig::aux_llm_resolver` (the core seam
  compaction uses) to also carry `Option<&UserId>`**, so per-user pins reach
  compaction too, not just the runtime-side generators. Rejected for this
  ADR: a second core-facing signature change beyond what closing ledger row
  15 requires. A worthwhile follow-up once a multi-user embedder actually
  wants per-user-pinned compaction (and consistent with ADR-0181 — the
  `UserId` would live in core, not the runtime).
- **A `user: Option<UserId>` field + `AuxLlmRegistry::for_user(user)` builder
  directly on `AuxLlmRegistry`**. Rejected: puts a `UserId` field on the
  exact type `main.rs`'s single-user path constructs one process-global
  instance of — `entanglement-runtime`'s `skutter` binary is a strict
  single-user application, and after ADR-0181 the type (and crate) must not
  name `UserId` at all. The resolver-closure wrap is behaviorally identical.

## References

- [ADR-0181](0181-userid-leaves-the-runtime-crate.md): `UserId` leaves the
  runtime crate — the constraint this ADR's per-user half conforms to.
- [ADR-0154]: the `Purpose` enum, `AuxModelStore`, `AuxLlmRegistry`, and the
  session-title generator this ADR extends; its "Consequences" section is the
  literal source of deferred-work-ledger row 15.
- [ADR-0151]: `Session.action`/`SetSessionMeta`, whose "no in-tree producer
  yet" gap `narrate` fills.
- [ADR-0147]: the multi-user embedder API; its `UserProviderStore` pattern is
  what the rejected draft mirrored (and what #687 migrates per ADR-0181).
- [ADR-0158]: the prior amendment to ADR-0154 (session-title concurrency
  deferral) — same "amend, don't edit" precedent this ADR follows.

[0154]: 0154-per-purpose-auxiliary-models.md
[ADR-0154]: 0154-per-purpose-auxiliary-models.md
[ADR-0151]: 0151-settable-session-metadata.md
[ADR-0147]: 0147-multi-user-mode-embedder-api.md
[ADR-0147-provider]: 0147-multi-user-mode-embedder-api.md
[ADR-0158]: 0158-defer-session-title-aux-call-under-contended-primary-concurrency.md
