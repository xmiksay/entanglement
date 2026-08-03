# 0164. Short, sortable, kind-tagged ids behind a pluggable generator

- Status: Accepted
- Date: 2026-08-03
- Amends: [ADR-0002](0002-session-multiplexed-protocol.md) (the `SessionId` minting scheme only —
  the newtype, its wire shape and the multiplexing contract are unchanged)

## Context

Two id schemes coexist today and neither is quite right:

- **Sessions and agent handles** are `Uuid::new_v4().to_string()` — 36 characters, unordered, and
  awkward to type or eyeball. Session ids appear in `agent_spawn`/`agent` tool results, in TUI
  listings, in log lines and in every `/resume`-style interaction.
- **Background jobs** are `bg-{n}` from a per-registry `AtomicU64`. Because `bash_live::bash_enable`
  builds a **fresh** `JobRegistry` on every live enablement, that counter restarts at 0 — so job ids
  are not unique even within one session, let alone across a process.

[ADR-0161](0161-unified-async-work-background-flag-and-one-poll.md) makes this load-bearing: a single
`poll` tool must dispatch on a handle, which requires one namespace where the kind is legible and
ids never repeat. `bg-N`'s non-uniqueness stops being a wart and becomes a correctness bug.

Two facts make the change much cheaper than it looks:

- `SessionId` is `pub struct SessionId(pub String)` — a newtype over `String`, already a string on
  the wire, with `new_uuid()` as merely *one* constructor. This is a change to a **minting
  function**, not to a type, a serde shape, or any stored format.
- **Most indexed objects are short-lived.** Session logs are pruned by
  `ENTANGLEMENT_SESSION_RETENTION_DAYS` (default 30). Ids need to be unique and ordered among *live*
  objects, not for all time — which is what lets the timestamp be small.

## Decision

### Format

`<kind>-<epoch-seconds in hex><process salt><monotonic counter>` — **15 characters.**

```
s-6a708af0a1002     session / agent handle   (an agent handle IS a session id)
j-6a708af0a1003     background job           (replaces bg-N)
r-6a708af0a1004     runtime-minted request id
└┬┘└───┬──┘└┬┘└┬┘
 │     │    │  └─ 3 hex: counter, resets when the second advances (4096/s)
 │     │    └─ 2 hex: per-process salt, drawn once at startup
 │     └─ 8 hex chars of epoch-seconds
 └─ kind
```

### The generator never returns the same id twice

The tail is a **monotonic counter**, not randomness. A process-global `(last_second, counter)` pair
advances the counter when two ids are minted within the same second and resets it when the second
rolls over. If the counter would wrap inside a single second (4096 ids/s), the generator waits for
the next second rather than repeating.

"Never twice" is therefore a structural guarantee, not a probability. The counter also gives correct
**ordering within a second**, which a random tail could not.

### Seconds, not milliseconds — reduce resolution, don't truncate range

Epoch-seconds in hex is exactly **8 characters and does not wrap until 2106**. Fixed width is what
makes the ids lexicographically sortable by creation time, so session listings and log directories
order chronologically with no timestamp lookup.

The tempting alternative — keep millisecond resolution and truncate it to 8 hex — **wraps every 49.7
days**. Against a 30-day retention window, a wrap boundary can sit *inside* the live set and
silently break sort order for exactly the objects that still matter. Second resolution avoids the
wrap entirely, and sub-second ordering is the cheaper property to give up (the counter restores it
anyway, per process).

### The process salt, and what it does not promise

Multiple engine instances share a project directory
([ADR-0144](0144-file-backed-shared-endpoint-state-across-instances.md)), and a counter alone would
have every process minting the identical `…a1000, …a1001` sequence. A 2-hex salt drawn once at
startup separates them.

This is **probabilistic where the intra-process counter is absolute**, and the asymmetry is stated
rather than glossed: within a process, repeats are impossible; across concurrent processes, they are
merely unlikely. An embedder needing an absolute cross-process guarantee replaces the generator.

### The generator is a pluggable seam

```rust
pub trait IdGen: Send + Sync {
    fn next(&self, kind: IdKind) -> String;
}
```

behind an `Arc<dyn IdGen>` on `EngineConfig`, defaulting to the scheme above. This follows the
pattern the project already uses for exactly this situation — the pluggable permission resolver and
grant store ([ADR-0079](0079-pluggable-permission-resolver-and-grant-store.md)), `model_resolver`
([ADR-0063](0063-realtime-model-provider-switch.md)), `aux_llm_resolver`
([ADR-0154](0154-per-purpose-auxiliary-models.md)) and `SandboxResolver`
([ADR-0134](0134-per-profile-sandbox-scoping-and-spawn-chain-clamp.md)) are all the same shape.

An embedder that needs fleet-coordinated ids (a database sequence, a Snowflake-style generator, or
plain UUIDs) replaces one object and changes nothing else. Keeping `SessionId` a newtype over
`String` is what makes this free: any generator's output is a valid id.

### Scope, and the one boundary

**Sessions, background jobs, and runtime-minted request ids.**

A tool `request_id` is `ToolCall.id`, which on the Anthropic and OpenAI wires is
**provider-supplied**. Those pass through verbatim — reformatting them would break the tool-call
round-trip, which is precisely the bug #444 fixed for Gemini. Only ids the runtime itself mints
adopt the scheme.

### Coexistence with legacy ids

Old UUID-form ids keep working untouched: the two shapes cannot collide, and every consumer treats a
`SessionId` as an opaque string. Existing session logs replay unchanged.

The one concrete migration point is a test: `session_id_new_uuid_generates_unique_ids`
(`entanglement-core/src/protocol.rs`) asserts `Uuid::parse_str(&id.0).is_ok()`. It is the only place
in the tree that parses a session id as a UUID — production code makes no such assumption.

## Consequences

### Positive

- **`poll`'s handle namespace falls out for free.** Dispatching `s-` vs `j-` is a prefix match, so
  ADR-0161's "one namespace" prerequisite is satisfied by doing this first rather than by extra work
  inside `poll`.
- **`bg-N`'s non-uniqueness is retired rather than fixed.**
- **15 characters instead of 36**, typable and greppable — which matters because these ids are
  handed to a model in tool results and to a human in listings.
- **Chronological ordering for free** in session lists and log directories.
- **Embedders get an escape hatch** for id policy they may well have opinions about, at the cost of
  one trait.

### Negative / neutral

- **The generator can block.** Waiting for the next second on counter exhaustion is what makes
  "never twice" absolute, but it is a sleep on a hot path. At 4096 ids/s it is unreachable in
  practice — it still needs to be a documented, tested branch rather than an unexamined `else`.
- **Cross-process uniqueness is probabilistic.** Stated above; the seam is the answer for anyone who
  cannot accept it.
- **The sizing depends on the retention window.** If `ENTANGLEMENT_SESSION_RETENTION_DAYS` is raised
  substantially, or an id is embedded somewhere that outlives its object (an external tracker, a
  bookmarked link), both the collision budget and the ordering guarantee need re-checking. A future
  retention change should trigger a review of this ADR.
- **Sortable ids leak creation time.** Inherent to the design and harmless for a local single-user
  tool ([ADR-0048](0048-serve-head-local-trust-model.md)), but recorded rather than discovered later.
- **Two id shapes coexist indefinitely.** No expiry, no rewrite pass; a project directory will hold
  both for as long as its oldest log survives.

## Alternatives considered

- **Keep UUIDs; display a git-style short prefix.** No minting change and no migration at all.
  Rejected: it needs a prefix resolver with an ambiguity error path, gives jobs and operations no
  shared scheme, and leaves the stored form 36 characters — so logs, listings and tool results are
  unimproved.
- **A random tail instead of a counter.** Simpler, stateless, and the obvious ULID-shaped choice.
  Rejected: it makes uniqueness probabilistic *even within one process*, which is the case fully
  under our control, and it cannot order two ids minted in the same second.
- **Milliseconds truncated to 8 hex.** Rejected per §"Seconds, not milliseconds" — a 49.7-day wrap
  inside a 30-day window is a silent ordering bug.
- **A custom epoch (project start) to keep millisecond resolution short.** Buys ~2.2 years at 9 hex
  or ~35 years at 10 hex, at the cost of an arbitrary constant every reader must know. Rejected: a
  hex second count is self-describing and needs no epoch lore.
- **A shared cross-process id sequence file**, alongside the ADR-0144 shared endpoint state.
  Rejected as disproportionate: a lock and a file round-trip per id to convert an unlikely collision
  into an impossible one, when the seam already lets anyone who needs that guarantee install it.
- **Making `SessionId` a real typed id (newtype over a `u128` or a parsed struct).** Rejected: it
  would turn a minting change into a protocol and API change, and the opaque-string property is
  exactly what makes the generator pluggable.

## References

- Plan review: unify the long-running-work tool surface (2026-08-03)
- [ADR-0161](0161-unified-async-work-background-flag-and-one-poll.md): the `poll` handle namespace
  this supplies — this ADR is its prerequisite
- [ADR-0163](0163-live-bash-enablement-is-a-tool-overlay-entry.md): the single long-lived
  `JobRegistry` that stops job ids restarting
- [ADR-0079](0079-pluggable-permission-resolver-and-grant-store.md),
  [ADR-0154](0154-per-purpose-auxiliary-models.md),
  [ADR-0134](0134-per-profile-sandbox-scoping-and-spawn-chain-clamp.md): the pluggable-seam pattern
  `IdGen` follows
- [ADR-0144](0144-file-backed-shared-endpoint-state-across-instances.md): the multi-process
  coexistence the salt addresses
