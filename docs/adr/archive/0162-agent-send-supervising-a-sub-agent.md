# 0162. `agent_send` — supervising a sub-agent instead of replacing it

- Status: Accepted
- Date: 2026-08-03
- Amends: [ADR-0138](0138-sponsored-build-child-and-propose-plan-cycle.md) (the sponsored build
  child's handle becomes visible, and the review loop can reuse the child)

## Context

A sub-agent can be talked to exactly once. `agent`/`agent_spawn` send a prompt at launch; after
that the only verb is *wait*. There is no way to send anything into a child session — running or
finished — so a follow-up means respawning and losing everything the child accumulated.

Three consequences, in increasing order of cost:

1. **The `propose_plan` review loop re-reads the world every round.** Each approval spawns a *fresh*
   `build` child ([ADR-0138](0138-sponsored-build-child-and-propose-plan-cycle.md)), even when the
   plan agent only needs one detail fixed from the previous round.
2. **A stuck child has nowhere to go.** Its options are guess, finish early, or error.
3. **[ADR-0155](0155-errored-subagent-turn-parks-for-steering.md) tells the parent to do something
   it cannot do.** When a child's turn ends in error with no answer, `collect_child_answer` keeps
   the parent parked and emits an `Error` on the *parent* saying the child "is still alive — steer
   it (e.g. prompt it to continue)." The parent has no verb for that. The steering ADR-0155
   describes is a human action only.

The obvious-looking fix is an escalation protocol: a way for a child to raise "I'm blocked, advise"
as a distinct signal, with a matching answer channel. That would be a new tool, a new event, and a
routing rule between agent-answerer and human-answerer.

**It is not needed, and this was checked against the code rather than assumed:**

- `collect_child_answer` (`entanglement-runtime/src/subagent.rs`) ends its wait on **any `Done`
  carrying text**. A child that concludes *"I can't proceed — the plan says X but the code does Y,
  advise"* **already** unparks its parent and delivers that text as the tool result. Today.
- `InMsg::Prompt { session, content }` is session-addressed with no parent/child restriction, and
  `Holly::send` — the privileged in-process entry — is already held by the tool executor.
- A child session task **stays alive after its turn ends**; it exits only when its `SessionCmd`
  channel drops. Prompting it starts a fresh turn on its existing context. ADR-0155 states this
  outright: an errored child "is not dead. It's an ordinary idle session, exactly as steerable as
  one that finished cleanly."
- If the parent is *still* parked, a prompt to the child that produces text on its next `Done`
  already resolves that park. The delivery path exists in both directions.

So the child→parent channel is already complete. The only missing piece is the **reply**.

## Decision

### 1. One new tool, no protocol change

`agent_send { agent_id, prompt, background? }` — structurally "a launcher against an existing
session," which is why it carries the same `background` flag as the other launchers
([ADR-0161](0161-unified-async-work-background-flag-and-one-poll.md)) and reuses the same machinery
end to end.

- Sends `InMsg::Prompt { session: child, … }` over the `Holly::send` handle the executor already
  holds. No new `InMsg`, no new `OutEvent`, no new wire type.
- **Blocking (default)** parks on `collect_child_answer` and returns the child's next answer — the
  exact path the blocking `agent` tool already takes.
- **`background: true`** returns immediately; collect with `poll` on the same handle.
- Intercepted before permission resolution like the rest of the `agent_*` family, and gated by the
  same descendant check ADR-0161 introduces for `poll`.

One verb covers all three needs: steer a running child, follow up a finished one, and re-engage a
build child for details the plan agent found missing.

### 2. Escalation is a message, not a mechanism

A child that ends its turn saying it is blocked **is** the escalation. The parent reads that text as
an ordinary tool result, decides what to do, and replies with `agent_send`. No escalation event, no
`ask_supervisor` tool, no routing rule.

Escalation routes **to the parent agent**, which reaches the human through its own existing
`ask_user` if it cannot decide. This composes one level at a time up the spawn chain and needs no
race rule between an agent answerer and a human answerer — the two never contend for the same
question, because each level asks its own supervisor.

### 3. The parent re-parks explicitly

The child's answer ends the parent's tool call. The parent regains control, may investigate (read
files, re-check the plan), and then calls `agent_send` — which blocks again for the child's next
result. The alternative, keeping the wait alive inside the launcher and surfacing the question
in-place, would force the parent to answer blind. Investigating before replying is the whole point
of making the parent the supervisor.

### 4. Refusals: closed and hibernated children

Two states must never silently produce a wrong-but-plausible result:

- **Closed (tombstoned)** — `CloseSession` cascades over the spawn subtree
  ([ADR-0056](0056-closesession-cascades-over-spawn-subtree.md)); a prompt at a closed id already
  gets a supervisor error. `agent_send` surfaces that as a clear refusal.
- **Hibernated** — this is the dangerous one. A prompt to a hibernated child falls into the lazy
  respawn path and comes back as a **blank** session: the parent link survives, the context does
  not. `agent_send` must detect hibernation and either resume via the
  [ADR-0112](0112-resume-cascades-over-the-spawn-subtree.md) cascade or refuse loudly — never
  silently hand back a context-free agent wearing the right id.

The idle-TTL sweep ([ADR-0090](0090-idle-ttl-auto-hibernation.md)) makes this the *likely* failure,
not a corner case: a plan agent reviewing a build report for a while is exactly the shape that lets
its child hibernate underneath it.

### 5. The sponsored build child's handle becomes visible

`propose_plan` never closes its build child and already registers it in the shared `AgentRegistry`
*before* sending `Spawn`, so the child stays alive and addressable. But its id is never surfaced:
the result is `"plan file: {plan_path}\n\nbuild completed in {:.1}s:\n\n{answer}"` — no `agent_id`,
unlike `agent`/`agent_spawn`, which both hand the child id to the model.

Appending the child's `agent_id` to that result is a one-line change with outsized effect: it is
what lets the plan agent `agent_send` corrections to the **same** build child, with its accumulated
context, instead of spawning a fresh one per phase. This amends ADR-0138's cycle without changing
its authorization model — sponsorship is still granted at plan acceptance, and the child's
clamp-exempt permissions are unchanged.

### 6. Non-termination

A supervision loop can fail to terminate: parent steers child, child comes back stuck, parent steers
again, indefinitely. ADR-0155 already accepted an unbounded park for the *human*-steered case, on
the reasoning that there is no natural bound on how long a person takes to notice and intervene.
Making the **parent** the steerer removes that natural rate limit — an agent will retry immediately
and forever.

v1 relies on the existing per-turn and spawn-depth budgets rather than inventing a round cap:
`agent_send` consumes the parent's turn like any other tool call, so a parent looping on a stuck
child burns its own budget and stops. This is recorded as a known-thin guarantee with an explicit
revisit trigger: an observed loop that the turn budget fails to bound.

## Open question, deliberately not settled here

ADR-0155 keeps the parent parked when a child's turn ends with *empty* text plus an `Error`, and
tells the human to steer. Once the parent has a reply verb, the alternative becomes available:
unpark the parent with the error text and let it decide — retry, redirect, or give up.

That is a change to ADR-0155's trade-off, not a detail of this one. ADR-0155 chose parking because
the old behavior "silently produced a *wrong* answer (success reported on a failed build) rather
than a slow one" — and unparking the parent is only better if the parent reliably does something
sensible with the error, which is exactly what is unproven. Left open rather than folded in
silently; it wants its own ADR once `agent_send` has real usage behind it.

## Consequences

### Positive

- **A dead end becomes a supervision loop**, with no new protocol and no new event type — the
  cheapest possible version of the feature, because the hard half already existed.
- **The plan→build loop stops re-reading the world.** Reusing the sponsored child preserves its
  context across review rounds.
- **ADR-0155's instruction becomes actionable.** The runtime already emits "steer it"; now something
  can.
- **One verb, three uses.** Steering, following up, and re-engaging are the same operation against
  different session states, and treating them as one tool is what keeps the surface from growing
  three times.

### Negative / neutral

- **A write verb on a registry with no scoping.** Today's `AgentRegistry` is engine-wide and not
  keyed by parent — any session can address any handle whose id it knows. That is an information
  leak with `poll`; with `agent_send` it becomes a cross-session **injection** path. The descendant
  check is therefore a hard prerequisite, not a hardening pass.
- **Hibernation makes stale handles genuinely dangerous**, and §4's detection is load-bearing rather
  than defensive.
- **Non-termination is bounded only indirectly** (§6).
- **The parent can now steer a child mid-flight**, but only on the `background: true` path — a
  parent blocked in a blocking `agent` call cannot call anything, its turn being parked by
  construction. That asymmetry is not a limitation so much as the reason the parked-parent race
  never arises: there is no outstanding wait to corrupt when the steer is issued.

## Alternatives considered

- **A dedicated escalation protocol** (`ask_supervisor` tool + a new event + answerer routing).
  Rejected: the child→parent channel already exists and already unparks the parent. This would have
  added a tool, a wire type and a race rule to duplicate a working path.
- **Route a child's `ask_user` to its parent instead of the human.** Tempting — it needs no new tool
  — but it silently changes what `ask_user` means inside a sub-agent, and creates exactly the
  answerer race (parent vs human, both able to answer, needing `RetractQuestion` arbitration) that
  routing escalation one level at a time avoids.
- **Automatic re-park: the launcher surfaces the question and keeps waiting.** Fewer round-trips,
  but the parent must answer without investigating. Rejected per §3.
- **Scope the reuse to `propose_plan`'s sponsored build only**, generalizing later. Rejected: all
  three consumers already share `collect_child_answer`, so the general version is not meaningfully
  more work than the special case, and a `propose_plan`-only verb would be a second thing to retire.
- **A round/depth cap on supervision loops.** Rejected for v1 per §6 — an arbitrary cap reintroduces
  ADR-0155's original bug (unblocking on a still-recoverable child) at a different timescale, and
  the turn budget already bounds the common case.

## References

- Plan review: unify the long-running-work tool surface (2026-08-03)
- [ADR-0155](0155-errored-subagent-turn-parks-for-steering.md): the "steer it" instruction this
  makes actionable; its errored-turn trade-off is left open above
- [ADR-0138](0138-sponsored-build-child-and-propose-plan-cycle.md): the sponsored build child whose
  handle this surfaces
- [ADR-0161](0161-unified-async-work-background-flag-and-one-poll.md): the `background` flag,
  `poll`, and the shared descendant check
- [ADR-0112](0112-resume-cascades-over-the-spawn-subtree.md): the resume cascade a hibernated child
  needs
- [ADR-0056](0056-closesession-cascades-over-spawn-subtree.md): the tombstone that makes a closed
  child refuse
- [ADR-0090](0090-idle-ttl-auto-hibernation.md): why hibernation is the likely failure, not a corner
  case
- [ADR-0024](0024-subagent-permission-gating.md),
  [ADR-0023](0023-subagent-spawn-limits.md): gating and budgets, charged at launch and unchanged
