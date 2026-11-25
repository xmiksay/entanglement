# 0168. The lazy-`Prompt` path refuses a known hibernated sub-agent child instead of blank-respawning it

- Status: Accepted
- Date: 2026-08-04
- Amends [0077](0077-session-hibernation-evictable-resumable.md): reverses its
  "remove from `sessions`, `session_meta`, **and `parent_links`**" hibernate
  decision and its "a `Prompt` for a hibernated-but-not-closed id lazily
  *would* respawn a blank session" acceptance. Issue #639.

## Context

The supervisor's lazy-`Prompt` path (`holly.rs`, the `!sessions.contains_key`
branch) materializes any unknown-but-not-closed session id as a blank session
under the base `build` profile — the single-user convenience that lets a
first `Prompt` to a never-seen id auto-create a root, with no `Spawn`
round-trip. ADR-0077 explicitly accepted this as the *expected* behavior for
a hibernated id too: "the embedder is expected to `resume` before
re-prompting." ADR-0077 also had the `HibernateSession` handler tear down
`parent_links` alongside `session_meta` on eviction, so by the time a
hibernated id reaches the lazy-`Prompt` path, nothing distinguishes it from a
genuinely fresh one.

That symmetry breaks for a **sub-agent child**. A child is spawned under a
named, often permission-restricted `Subagent` profile (ADR-0040/#119) — the
whole point of `Spawn`'s target-side mode gate is that a leaf never runs with
`build`'s unrestricted permission and tool mask. But the lazy-`Prompt` path
does not check whether the id it's about to blank-respawn ever had a parent —
it decides the profile (`resolve(DEFAULT_PROFILE)`) before consulting
`parent_links`, and by design (ADR-0077) that map was already empty for a
hibernated id anyway. So prompting a hibernated child directly, without an
intervening `Resume`, silently discards its history **and** re-creates it
running under `build` — the exact escalation direction the `Spawn` handler's
own unknown-target case (`holly.rs:623-628`) refuses for a typo'd agent name.
Nothing on the event stream distinguishes this from a legitimate resume:
both emit `SessionStarted`, just with different `profile`/`parent`/`root`.

An embedder that materializes a database row keyed off `SessionStarted` has
no way to tell a resumed child from an escalated blank respawn without
independently tracking parentage itself — the race is purely about whether
the child id happens to still be live in the supervisor's `sessions` map when
it is next prompted.

## Decision

**`parent_links` survives hibernation.** `hibernate_subtree` still tears down
the live `sessions` channel and the `session_meta` entry for every victim in
the evicted sub-tree (so a hibernated id is still gone from `ListSessions`,
per ADR-0077's own acceptance criterion) but no longer removes the
`parent_links` edge. It is the cheapest possible record — one
`SessionId → Option<SessionId>` pair — and it is exactly the fact this fix
needs to survive eviction: "this id was spawned as a child of that id." It is
still torn down on `CloseSession`, so a tombstoned id doesn't linger in the
map forever; only a hibernated-and-not-yet-resumed id keeps its edge.

**The lazy-`Prompt` path consults `parent_links` before deciding a profile,
and refuses a known child instead of respawning it.** The lookup that used to
happen one line *after* `resolve(DEFAULT_PROFILE)` (only to feed `root`/
`user` inheritance) now happens first and gates the decision:

- `parent_links.get(&session_id)` resolving to `Some(parent)` means this id
  is a known sub-agent child that is not currently live and not closed. The
  supervisor emits a supervisor-level `OutEvent::Error` naming `Resume` as
  the correct next step and does **not** create a session — mirroring the
  closed-id refusal already on this same path (issue #105) and the
  unknown-`Spawn`-target refusal (#119).
- Anything else — no `parent_links` entry at all (a genuinely fresh id), or
  an entry recorded with no parent (a hibernated `/compact` successor root,
  ADR-0110) — keeps today's blank-`build`-respawn convenience unchanged.

This is a **refuse**, not a **carry the recorded profile forward**, of the
two directions ADR-0077's issue thread considered: `session_meta` (which
would have carried the profile/tool mask/user) is still evicted on
hibernation, so the only options were reconstructing it from the persisted
log (duplicating what `Resume` already does) or refusing and pointing at
`Resume`. Refusing also surfaces the silent history loss to the caller, not
only the escalation — the same tradeoff ADR-0077 itself weighed for
mid-stream hibernate (§"Safety") and landed on the side of *telling* the
caller rather than quietly degrading.

## Consequences

- A hibernated sub-agent child can no longer be silently re-created under
  `build`'s permission profile and tool mask; the caller gets an explicit
  `Error` telling it to `Resume` first.
- A hibernated **root** (no parent — a fresh id or a `/compact` successor)
  keeps the exact ADR-0077 blank-respawn convenience; this fix narrows the
  hazard to children only, since that convenience was never the problem for
  a root running under `build` in the first place.
- `parent_links` for a hibernated-forever, never-resumed, never-closed child
  now lingers in memory instead of being released at hibernate time — the
  same bounded-but-technically-unbounded growth ADR-0028's `closed` tombstone
  set already accepts, and orders of magnitude smaller than the `Context` +
  task ADR-0077 actually frees.
- `CloseSession`'s cascade (`collect_subtree`) now also reaches a child that
  was hibernated-but-never-resumed when an ancestor is later closed, since
  the edge is no longer torn down at hibernate time — previously such a
  child's `parent_links` entry vanished at hibernation, so closing an
  ancestor afterward silently failed to tombstone it. This is a
  correctness improvement, not a behavior this ADR trades away.

## Rejected alternatives

- **Carry the child's recorded profile from `session_meta` instead of
  refusing.** `session_meta` is evicted on hibernate for the same
  memory-release reason `sessions` is; keeping it alive defeats the point of
  hibernation (the `Context` is the expensive part, but `SessionInfo` would
  have to live exactly as long to make this work, undermining "hibernated
  holds no supervisor map entry"). Refusing needs only the tiny
  `parent_links` edge, not the fuller `SessionInfo`.
- **Leave `parent_links` cleared and instead add a separate
  ever-spawned-child registry.** Functionally equivalent to keeping
  `parent_links` around, but duplicates a map that already exists for
  exactly this shape of data (`SessionId → Option<SessionId>`) instead of
  relaxing what one existing map tears down.
- **Refuse every non-live, non-closed lazy-`Prompt` id, child or root.**
  Rejected: this would break the ADR-0077-documented single-user convenience
  for a genuinely fresh id and for a hibernated root, neither of which carries
  the escalation hazard — only a child running under a
  permission-restricted profile does.

## References

- Issue #639: lazy-`Prompt` path respawns a known sub-agent child blank and
  under `build`, escalating its profile
- [ADR-0077](0077-session-hibernation-evictable-resumable.md): the hibernate
  contract this amends
- [ADR-0040](0040-per-profile-spawn-control.md)/#119: the target-side mode
  gate and unknown-`Spawn`-target refusal this fix extends to the
  lazy-`Prompt` path
- Issue #105: the closed-id refusal on this same lazy-`Prompt` path, the
  precedent for refusing instead of silently resurrecting
- `entanglement-core/src/holly.rs`: `hibernate_subtree`, the lazy-`Prompt`
  branch of `supervisor`
- `entanglement-core/tests/lazy_prompt_known_child.rs`: regression coverage
