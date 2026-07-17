# 0113. Persistence synthesizes a spawned child's initiating prompt from the cached `Spawn`

- Status: Accepted
- Date: 2026-07-17
- Issue [#421](https://github.com/xmiksay/entanglement/issues/421), surfaced as
  a non-goal while fixing [#415](https://github.com/xmiksay/entanglement/issues/415)
  ([ADR-0112](0112-resume-cascades-over-the-spawn-subtree.md)). Tracked in
  [`../deferred-work-ledger.md`](../deferred-work-ledger.md), part of epic #396.

## Context

`InMsg::Spawn { prompt, .. }` handling in the supervisor (`entanglement-core/src/holly.rs`)
delivers the initial prompt directly to the child's session-command channel
(`stx.send(SessionCmd::Prompt(content))`), bypassing `Holly::send`/the inbound
broadcast the persistence tap (`entanglement-runtime/src/persistence.rs`)
observes. So no `InMsg::Prompt` record ever exists for it in the persisted log
— only the assistant's eventual reply gets folded (via the `Done` flush in
`Session::replay`). Replaying (or, since ADR-0112, resuming) a spawned child
reconstructs a `Context` with the assistant's reply but not the user-role task
instruction that produced it.

The tap already skips `InMsg::Spawn` outright (it would create a stray
single-line child file: `roots` can't resolve the child to its parent's root
file until the child's own `SessionStarted` is observed — that's the whole
reason `InMsg::Spawn` is skipped rather than appended like any other inbound
message).

## Decision

**Cache a `Spawn`'s prompt in the persistence tap; synthesize an
`InMsg::Prompt { session: child, .. }` record for it once the child's
`SessionStarted` resolves `roots`.**

- The tap gains `pending_spawn_prompts: HashMap<SessionId, String>`. On
  `InMsg::Spawn`, instead of dropping the message, its `prompt` is cached
  under the child's `SessionId` and the message is still never appended
  verbatim.
- On `OutEvent::SessionStarted`, after `roots` is updated (so the child now
  resolves to its parent's root file), the tap checks
  `pending_spawn_prompts.remove(&session_id)`. A hit synthesizes
  `InMsg::prompt(child, prompt)` and appends it as an ordinary `LogPayload::In`
  record, immediately after the `SessionStarted` record itself.
- Ordering matters for `session_store::pair_records`, which pairs each `Out`
  record with the most recently written `In` record regardless of session
  (#275, a known, separately-tracked general hazard). Appending
  `SessionStarted` first, then the synthesized prompt, means `pair_records`
  produces `(None, SessionStarted)` — matching every other session's first
  record — followed by `(Some(Prompt), <child's next event>)`, so
  `Session::replay` folds the prompt as the child's opening user message
  exactly like a live `Prompt` sent to any other session.
- Idempotency: the cache entry is consumed (removed) the moment it's used.
  Resuming a child never re-sends `InMsg::Spawn` — `Holly::resume`'s
  `spawn_resumed` helper (ADR-0112) reconstructs and re-spawns from the log
  directly — so a resumed child's re-announced `SessionStarted` finds nothing
  in the cache and synthesizes nothing a second time. A `Spawn` whose child
  never reaches `SessionStarted` (e.g. the supervisor refuses it) leaves an
  orphaned cache entry; accepted as a bounded, per-attempted-spawn cost, not a
  correctness issue.

## Consequences

- **Positive.** Replaying or resuming a spawned child reconstructs the same
  `Context` a live child would have accumulated — the task instruction that
  framed its work, not just its eventual answer. Closes the gap ADR-0112 left
  as a non-goal.
- **Positive.** No wire/protocol change: no new `InMsg`/`OutEvent` variant.
  The fix is entirely in what the persistence tap does with data it already
  observes (the `Spawn`'s `prompt` field, the child's `SessionStarted`).
- **Neutral.** `InMsg::Spawn` is still never persisted verbatim — the stray
  bogus-root-file problem this was designed to avoid is unchanged.
- **Negative / accepted.** Inherits the pre-existing #275 hazard: if another
  session's event is broadcast and processed by the tap between the
  synthesized prompt and the child's own next event (true concurrent spawn +
  parent activity), `pair_records`' session-blind pairing could misattribute
  the prompt to the wrong out-event. This is the same hazard every live
  `Prompt` sent to a session already carries when siblings run concurrently;
  fixing it is `pair_records` becoming session-aware, tracked separately under
  #275, not reopened here.

## Alternatives considered

- **Add a new protocol field/event carrying the spawn prompt explicitly (e.g.
  on `SessionStarted`).** Rejected: duplicates data the existing `InMsg::Prompt`
  record type already represents, and every other head/consumer of the log
  already knows how to fold an `InMsg::Prompt` — a new field would need its own
  replay-fold arm for no behavioral gain.
- **Route the child's initial prompt through `Holly::send` (the inbound
  broadcast) instead of directly to `SessionCmd::Prompt`.** Rejected: changes
  the supervisor's spawn hot path from a direct channel send to a full
  broadcast round-trip purely to satisfy the persistence tap's observation
  point, and reopens ordering questions between `SessionStarted` and the
  first `Prompt` the child would need to already exist under `roots` for. The
  cache-and-synthesize approach keeps the spawn path unchanged and confines
  the fix to the tap, which already owns exactly this kind of causal
  reordering (it biases inbound before outbound for the equivalent reason).
- **Persist `InMsg::Spawn` verbatim once `roots` resolves it, instead of
  synthesizing a `Prompt`.** Rejected: `Session::replay` has no fold arm for
  an inbound `Spawn` (it isn't the format any other session's turn-starting
  message takes), so this would need a new replay case duplicating
  `InMsg::Prompt`'s existing one for no benefit — the child's spawn prompt
  *is* semantically its first `Prompt`.
