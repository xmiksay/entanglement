# 0242. Compaction heals walk the whole chain

- Status: Accepted
- Date: 2026-10-20
- Issue: #888, #887
- Amends: [ADR-0231](0231-compaction-successor-inherits-background-work.md)
  (its heal is no longer one hop),
  [ADR-0238](0238-compaction-moves-a-background-scripts-card.md) (§2's move
  target and heal scope, §3's decision-heal announce rule, §4's matching
  rule, and its known limits naming #887 and #888), and
  [ADR-0240](0240-approval-resolved-event.md) §5/§8 (where the TUI gets the
  predecessor chain it drops an id along)

## Context

Two moves hand a compacted session's work to its successor. ADR-0231 moves
its jobs, background scripts and retained outputs. ADR-0238 moves a
background script's parked cards. Each runs eagerly in the executor's
lifecycle fold on `SessionStarted { predecessor }` and again as a self-heal
from the supervisor's directory. Both read one link only: the event's
`predecessor`, or `Holly::session_info(S).predecessor`.

If two compactions (P0 → P → S) happen before a fold or heal runs, the move
for S reaches P but not P0. P0's work stays on P0, which has ended. The
fold can be lost to broadcast lag, and a hibernated S is not followed
(ADR-0241 §4), so a script's card can park on P0 after both compactions.
When S resumes, its fold moves only P's cards.

ADR-0238 also kept its own lineage map (predecessor → successor) in
`PendingDecisions`, written by every move and never pruned (#887). The map
decided where a fresh detached park was homed.

A decision under a middle link's id did not reach a card that had moved
through it. The card parks on P, moves to S, then to S2. A head that still
shows S's copy answers `Approve{S, id}`, and nothing matches: the key is P
and the home is S2.

The TUI drops a moved or ended card's id along a view's predecessor chain.
It learns that chain only from `SessionStarted`. A TUI that lost that event
keeps a predecessor's copy forever.

#888 also noted that the park's forward walk stops at a middle link that is
not live. It does not: `Holly::successor_of` already returns the last link
of the chain (ADR-0241), so the forward walk never visits a middle link.

## Decision

1. **Core gives the chain backwards.** `Holly::predecessors_of(s)` returns
   every session `s` succeeded, nearest first. It reads the supervisor's
   successor map, which is never pruned and is written before the
   successor's `SessionStarted` (ADR-0234). The walk is bounded by the map's
   size and is cycle-safe (a visited set). `lineage_head` is now its last
   element.
2. **Every move takes the whole chain.**
   `PendingDecisions::reassign_detached(ancestors, target)` moves the
   detached cards homed on any of `ancestors` to `target`. The same rules
   apply as for one hop: emit under the lock, re-announce on `target`,
   leave turn cards, and leave a card whose id `target` already shows.
   Ancestors are taken nearest first, so if two ancestors hold live cards
   with one id, only the nearest moves. `ApprovalResolved` is unchanged.
   The callers:
   - The lifecycle fold for S moves every ancestor's jobs, scripts,
     retained outputs and cards to S (`LadderCtx::adopt_lineage`).
   - A `poll` adopts every ancestor's jobs, scripts and retained outputs
     (`PollRegistries::adopt_predecessor_work`).
   - The card heals move the chain onto its live end.
3. **One notion of "live end".** `pending::live_end(s)` is
   `successor_of(s)` if that session is live, else `s`. It decides:
   - where a fresh detached park is homed;
   - the target of every card heal: `heal_lineage` for a live session
     (before a `Stop` or a decision, a rooted `ListPending`, a `poll`), and
     `heal_for_park` for any session, even a retired one;
   - the session a background script switches to (`Dispatcher::followable`
     adds the folded-mode condition, as ADR-0241 §4 requires).

   A park now heals the same way whether it is detached or not. The
   separate forward walk is gone.
4. **The registry keeps no lineage (#887).** `PendingDecisions` drops its
   lineage map. A park reads its home from core's map under the registry
   lock, so no move runs between its home and its insert. A move computes
   its target and ancestors before it takes the lock. That race is benign:
   a session going live in between is picked up by its own fold, and an
   older target still lies on the card's chain, so the next heal carries
   the card on. Nothing in the runtime grows per compaction any more; a
   card's own record of its earlier homes (§5) ends with the card.
5. **A decision reaches a card on any home it was shown on.** Each moved
   card records the home it left (`Parked.via`). A decision for `(X, id)`
   matches the exact key, the card's current home, or any session in its
   `via`. That covers the stale copy on a middle link's view. A card parked
   straight onto the live end has an empty `via`, so a stale decision for
   another card with the same id on a middle link never reaches it. A card
   is never shown upstream of where it was parked, so the predecessor's id
   still never reaches a card the successor parked itself. At most one live
   candidate is foreign: a card leaves a home only when that session
   retires, and a park or a move onto a live card's id is refused or
   skipped. The exact key is preferred, and the key string is the
   deterministic last tie-break. A card a move skips records nothing in
   `via`. The decision heal announces every moved card except the ones
   this decision answers, by the same rule.
6. **The TUI learns the chain from the engine.** Before a detached
   `ToolRequest` or an `ApprovalResolved` folds, `App::learn_lineage` reads
   `Holly::predecessors_of` for the event's session. Only the engine can
   tell a lost link from the true head of a chain, so it reads on every
   such event. Each view in that chain is linked to its nearest ancestor
   that has a view, but only where its own link is missing or names a
   session with no view. So a chain that lost only a middle
   `SessionStarted`, or never had a middle view, is completed too. A link
   to an existing view is never overwritten. The TUI runs in-process with
   the engine, so this is a synchronous directory read, and no wire event
   is needed. A user watching the ancestor a learned link names is moved to
   the successor, as the lost `SessionStarted` would have moved them
   (`follow_successor`, carrying the compaction seed). A user who went back
   to an older ancestor on purpose, and any view the user is not watching,
   stays where it is.

## Consequences

- Any number of compactions before a fold or heal still moves the oldest
  ancestor's work. A card a script parks while its successor is hibernated
  moves when the successor resumes.
- Answering a stale copy on any session a card was shown on resolves the
  card.
- `PendingDecisions::register_approval` of a detached card no longer
  follows a lineage inside the registry: its home is its key. Only
  `park_approval_as` homes a card along core's chain.
- The executor's memory no longer grows with the number of compactions.
  Core's successor map still does, by design (ADR-0234).
- Each heal scans core's successor map once per link. The map has one entry
  per compaction in the process.
- A remote head (`serve`, `pipe`) has no in-process `Holly`. One that
  missed `SessionStarted` can rebuild the chain from `ListSessions`'
  `predecessor` field for live sessions only. This ADR adds nothing to the
  wire.
- If two cards with one id exist in one lineage, only one moves. The other
  stays answerable under its own key and listed under its old home, as in
  ADR-0238 §5.

## Alternatives considered

- **Walk the predecessor chain in the runtime through `session_info`.**
  A retired ancestor has no `session_info` entry, so the walk stops at the
  first ended link. Core's successor map keeps every link.
- **Prune the registry's lineage map (#887 as filed).** Pruning needs to
  know which background scripts still refer to a predecessor, which ties
  `PendingDecisions` to `ScriptRegistry`. Core's map already holds every
  link, so the copy is removed instead.
- **Match a decision through core's lineage (key upstream, home
  downstream).** A card parked straight onto the live end was never shown on
  the middle link, yet a stale decision there for another card with the
  same id would answer it.
- **Record every hop (P0 → P, P → S) as a separate move.** The result is
  the same, but each intermediate announce lands on a dead session.
- **A wire event carrying a session's lineage for the TUI.** The TUI is
  in-process. A remote head's gap is not this issue.
