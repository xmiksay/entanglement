# 0144. `PauseSession`/`ResumeSession` — a hold between cancel and hibernate

- Status: Accepted
- Date: 2026-07-31
- Amends: [ADR-0017](0017-stop-cancels-turn-not-session.md) (adds a state
  `Stop` doesn't reach), [ADR-0077](0077-session-hibernation-evictable-resumable.md)
  (adds a lighter-weight hold beside eviction), [ADR-0139](0139-visible-session-states-working-and-waitingagent.md)
  (the `AgentState` enum widens again)

## Context

[#516](https://github.com/xmiksay/entanglement/issues/516): the engine has no
true pause primitive — only cancel (`InMsg::Stop`) or evict
(`InMsg::HibernateSession`), and neither covers "hold this session's work
without losing it and without evicting memory":

- **`Stop`** (ADR-0017) cancels the *turn*: a parked batch's `TurnState` is
  cleared outright, and a mid-stream round's uncommitted text is discarded.
  The session survives, but the in-flight round does not — resuming means a
  fresh `Prompt`, paying for a new round.
- **`HibernateSession`** (ADR-0077) evicts the session from memory entirely.
  It's resumable via the embedder's event log, but that's a heavier operation
  than "step away for a minute" calls for, and it tears the task down.

Concretely motivating use cases (from the issue): stepping away from an
approval prompt briefly, a `serve` head wanting to yield a session's LLM
round-trips to a higher-priority one without losing its parked state, and a
rate-limit stall you'd rather park deliberately than let burn through the
provider's own retry budget.

## Decision

**Add `InMsg::PauseSession`/`InMsg::ResumeSession` and `AgentState::Paused`,
scoped to whole-session granularity, with mid-stream pause deferred to the
next round boundary rather than interrupting a live stream.** The issue's own
scope note permits shipping "pause-while-parked only" for v1 if a true
mid-stream freeze is too invasive; this ADR takes a slightly broader version
of that: pause also holds an **idle** session's next `Prompt`, and a **parked**
session's next round-trip (not just the wait itself) — but never touches a
round already in flight.

### Granularity: whole session, not per-turn

A turn is exactly the granularity `Session.turn: Option<TurnState>` already
tracks (ADR-0061); there is no coarser or finer unit meaningfully "pausable"
within one session. Per-turn pausing (freezing this turn but letting a next
`Prompt` start a new one) was considered and rejected — see Alternatives.

### Mid-stream semantics: deferred-until-safe, not interrupted

The issue frames mid-stream as "the hard case" with two options: (a) finish
the current buffer flush then hold the next provider read, or (b) treat it
like `Stop` but checkpoint the partial round for resumption. Both need core to
either interrupt a live `select!` mid-flight or preserve state the engine
currently discards on cancel.

This ADR takes neither. `stream.rs`'s mid-stream `select!` already has a
generic mechanism for exactly this shape: any `SessionCmd` that isn't `Stop`
or inbox-close is `stash.push_back(other)`'d and replayed once the round
reaches a safe point (turn end or tool-call park) — the same path
`SetAgent`/`SetModel` already ride when they arrive mid-stream. `Pause`/
`Unpause` are ordinary `SessionCmd` variants, so they get this for free with
**zero changes to `stream.rs`**: a `PauseSession` sent while actively
streaming has no effect until the round finishes or parks, at which point the
outer loop pops it from the stash and applies it normally.

Consequence: `AgentState::Paused` is never observed while a session is
genuinely streaming (`Thinking`). This is deliberate — the same "wait for a
safe point" discipline the mid-turn stash already enforces for profile/model
switches — not an oversight. A caller that needs an *immediate* interrupt
(the `serve`-yield-to-higher-priority case, or the rate-limit-stall case if
the stall is a live request already in flight) still reaches for `Stop`. This
ADR does not claim to solve those two cases fully; it solves the "hold a
session's *next* piece of work" case, which is what "pause-while-parked" plus
"pause-while-idle" together cover, and leaves genuine mid-stream preemption to
a follow-up if it proves necessary (tracked informally via this ADR — no
separate ledger row, since nothing here is an explicit half-finished
deferral, just a scope boundary).

### What pausing actually holds back

Two session states, two different holds — both driven by one `Session.paused:
bool` (deliberately **not** persisted/replayed, like `Stop`'s cancel: a
hibernate-then-resume cycle always comes back unpaused, exactly as it always
comes back with no in-flight mid-stream tail):

- **Idle** (`turn: None`): the next `Prompt` (and `SetAgent`/`SetModel`/
  `SetGeneration`/`Oneshot`) is deferred onto the existing turn-stash queue
  instead of starting immediately, gated by `s.turn.is_some() || s.paused`
  everywhere those five commands already gate on `s.turn.is_some()` alone.
  The stash-pop condition at the top of the loop gains the matching `&&
  !s.paused` guard so a deferred command isn't immediately popped back off
  the queue and re-stashed — the same busy-loop hazard the pre-existing
  "pop the stash only when idle" comment already calls out for the live-turn
  case.
- **Parked** (`turn: Some(t)`, `t.pending` non-empty): an arriving
  `ToolResult` still resolves and folds into `Context` immediately — it is
  **not** deferred. Stashing it would deadlock: the stash only drains once
  `s.turn` goes back to `None`, but `s.turn` can only go back to `None` once
  every pending result (including the one sitting in the stash) has resolved.
  What *is* held back is the batch's **continuation**: once the last result
  drains the batch (`TurnState::is_drained`), the ordinary path immediately
  calls `drive_turn` to issue the next model round-trip — paused, this call
  is skipped, leaving `s.turn` as `Some(TurnState { pending: vec![], .. })`
  ("drained but undriven") until `ResumeSession` arrives and drives it. No
  new prompt is needed to get there: the same round picks back up.

The re-offer timer (ADR-0071) is also suspended while paused-and-parked — a
held session shouldn't keep nagging the runtime executor for a result it
can't act on differently anyway.

### `Stop`/`HibernateSession` always win; neither lifts the pause

Both are unconditional regardless of `s.paused`, and neither clears it:

- `Stop` still clears `s.turn` and reports a resting state — but the resting
  state it reports is `Paused` (not `Done`) if the session is still paused.
  Cancel and pause are orthogonal holds: cancelling the in-flight round
  doesn't mean the user's separate "hold this session" intent expired. A
  session that is paused-and-idle after a `Stop` still defers its next
  `Prompt` until an explicit `ResumeSession`.
- `Hibernate` tears the session down exactly as it always has, irrespective
  of `paused` — memory eviction is a strictly bigger hammer than a pause, and
  requiring `Unpause`-before-`Hibernate` would just be friction for no safety
  gain (the evicted `Session`, `paused` bit included, is gone either way).
  Because `paused` is never persisted, a resumed-after-hibernate session
  always comes back **unpaused** — a deliberate simplification: pause is
  ephemeral engine-loop state, and re-deriving "should this stay held" from a
  log the embedder controls is the embedder's call to make (re-`PauseSession`
  after resuming, if it still wants the hold), not core's to infer.

### `AgentState::Paused`

A new variant, distinct from every existing wait state (ADR-0139's
`WaitingApproval`/`WaitingAnswer`/`WaitingAgent` all self-resolve once their
awaited event arrives; `Paused` never self-resolves — only an explicit
`ResumeSession` lifts it). Not attention-worthy (no bell/notification, mirroring
the `Attention` module's existing non-signalling states): the user paused it
deliberately, so there is nothing to alert them about.

### Wire trust

Both variants are **wire-allowed**, at the same trust tier as `Stop`: neither
evicts memory, spawns a process, or grants a permission — a wire head merely
holding/releasing its own session's next round-trip carries no more risk than
cancelling it outright.

## Consequences

- **(+)** A parked approval/question wait, and an idle session, can now be
  held without losing the round or paying for eviction+resume.
- **(+)** `ResumeSession` on a parked-and-drained turn continues the *same*
  round — no re-prompt, no re-billed round-trip for context the model already
  has.
- **(+)** Zero changes needed in `session/stream.rs` — mid-stream pause rides
  the existing generic stash mechanism for free, which is also why it's safe:
  that mechanism is already exercised by `SetAgent`/`SetModel`.
- **(+)** `Stop`/`Hibernate` remain simple and unconditional; no new
  precondition ("must unpause first") is added to either.
- **(−)** No true mid-stream interrupt-and-preserve: a session actively
  streaming when paused isn't held until its current round reaches a safe
  point. `Stop` remains the tool for an immediate interrupt, at the cost of
  the in-flight round.
- **(−)** `paused` doesn't survive a hibernate/resume cycle — a caller that
  wants both eviction and a hold must re-pause after resuming.
- **(−)** One more `AgentState` arm in every exhaustive match (compiler-driven
  — no catch-all `_ =>` in the ones that matter, per ADR-0139's precedent).

## Alternatives considered

- **Per-turn pause** (a flag on `TurnState` instead of `Session`). Rejected:
  there is nothing coarser than "the whole session's next round" to scope a
  hold to that isn't already `TurnState.pending`'s per-call granularity — and
  per-call pausing would mean partially executing a batch, which no use case
  in the issue asks for and which would need runtime-side changes to the tool
  executor (out of scope: core doesn't own tool execution, ADR-0006/0010).
- **True mid-stream freeze** (buffer-flush-then-hold, or cancel-and-checkpoint
  per the issue's options (a)/(b)). Rejected for v1 per the issue's own scope
  note: (a) needs a way to pause an in-flight `reqwest` stream mid-read with
  no clean primitive for it; (b) needs core to persist a streamed-so-far
  partial the same way `ADR-0017` explicitly chose *not* to persist (orphaned
  messages are an accepted, documented cost of cancel, not a precedent to
  build checkpointing on top of). The deferred-until-safe-point behavior this
  ADR ships instead is strictly less invasive and, unlike a true freeze,
  needed no new mid-stream code path at all.
- **Reuse `Stop` with a "but don't discard" flag.** Rejected: conflates two
  orthogonal intents — mirroring ADR-0017's own rejection of a distinct
  `Cancel` variant, whose reasoning cuts the other way here ("no current
  caller wants" a third meaning bolted onto one message applies just as much
  to bolting a hold flag onto `Stop`). `Stop` has settled, well-tested cancel
  semantics; overloading it risks the same class of bug ADR-0017 fixed (a
  flag nobody threads through correctly). Two clearly-named variants keep
  each meaning simple to reason about.
- **Persist `paused` and replay it.** Rejected: `paused` is loop control, not
  conversation content — persisting it would be the first non-content lifecycle
  bit `Session::replay` reconstructs, and ADR-0077 already established the
  precedent that ephemeral engine-loop state (a mid-stream tail) is allowed to
  not survive hibernate/resume. Consistency with that precedent was preferred
  over the marginal convenience of a pause surviving eviction.
- **Block `HibernateSession`/`Stop` while paused, requiring `ResumeSession`
  first.** Rejected for the same reason ADR-0077 rejected "refuse hibernate
  during an active stream": it adds a precondition with no safety benefit
  (the session's fate is unconditional either way) and would need `Pause`
  itself to gain refusal semantics no other lifecycle command in the protocol
  has.

## References

- Issue #516: no true pause primitive, only cancel/evict
- [ADR-0017](0017-stop-cancels-turn-not-session.md): `Stop` cancel semantics
- [ADR-0058](0058-mid-turn-prompt-folds-into-live-turn.md)/[ADR-0061](0061-parked-turn-state-batch-tool-resolution.md):
  the mid-turn stash and parked-`TurnState` mechanisms `Pause` reuses
- [ADR-0071](0071-parked-turn-reoffer-timer.md): the reoffer timer suspended
  while paused-and-parked
- [ADR-0077](0077-session-hibernation-evictable-resumable.md): hibernation,
  the heavier eviction this ADR sits beside
- [ADR-0139](0139-visible-session-states-working-and-waitingagent.md): the
  `AgentState` precedent for a dedicated rendering state over inferring one
- Refs #512
