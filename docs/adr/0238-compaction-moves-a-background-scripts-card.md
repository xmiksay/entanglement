# 0238. A compaction moves a background script's parked card to the successor

- Status: Accepted
- Date: 2026-10-19
- Issue: #871
- Amends: [ADR-0231](0231-compaction-successor-inherits-background-work.md)
  (what the successor inherits: the scripts' parked cards too) and
  [ADR-0237](0237-detached-flag-on-tool-request.md) (its Consequences: the
  card no longer stays on the closed predecessor's view)

## Context

After a compaction, ADR-0231 moves a background `rhai` script from the
predecessor P to the successor S. A nested `Ask` the script parked stayed in
`PendingDecisions` under `(P, request_id)`. So S's root-filtered
`ListPending` and S's handle-less `poll` (#829) missed the card, a `Stop` on
S (`stop_session(S)`) did not unwind it, and the TUI kept it on P's closed
view. An unfiltered `ListPending` and an `Approve` on P still worked.

The script's grading session (`Dispatcher::session`) is fixed at launch, so
a nested call the script makes after the compaction parked on P too.

Nested request ids are `<outer call id>:rhai:<tool>`, and some providers
reuse call ids every round (Gemini's `name#ordinal`). So S can park a card of
its own with the same id as a card moved from P.

## Decision

1. **A card has a key and a home.** The map key `(session, request_id)` is
   where the card was parked, and it never changes. `Parked.home` is the
   session the card is shown, listed and stopped on. It starts as the key's
   session. `PendingDecisions::reassign_detached(P, S)` moves the home of
   every detached card homed on P to S, rewrites its
   `PendingApproval.session` to S, and records the lineage P → S. Turn cards
   stay on P: their turn ends with P, and `stop_attached(P)` unwinds them.
   `ListPending`, the poll subtree section and `stop_session` all filter by
   home.
2. **When it runs.** It runs at the ADR-0231 trigger points:
   - eagerly in the executor's lifecycle fold on
     `SessionStarted { predecessor: Some(P) }`, next to `reassign_owner`;
   - as a self-heal from the supervisor's directory (`Holly::session_info`):
     the decision router heals S before a `Stop` or a decision addressed to
     it (`pending::heal_before`), and a `ListPending { root }` and a `poll`
     heal every session of the walked spawn sub-tree
     (`pending::heal_subtree`, after `AgentRegistry::heal_from`), so a
     compacted sub-agent's card is covered as well;
   - before every approval park (`pending::park_approval_as`): the parking
     session heals from its predecessor, and a detached park first moves
     its own session's cards forward along `Holly::successor_of`, which the
     supervisor writes before the successor's `SessionStarted`. The walk
     stops at a successor that is not live yet (`Holly::is_live`): it has
     no seq counter, so an announce there would carry seq 0 and be lost.
     Its `SessionStarted` fold moves the card with a real seq. So the
     successor's own park always meets a moved card already on it (and is
     refused, §5), and a late predecessor park lands on the live end.

   A move targets the live end of the recorded lineage, not a successor
   that compacted in turn. A heal addressed to a session that is not live
   moves nothing.

   The move is idempotent.
3. **Re-announce on S.** Each moved card that still waits is emitted again
   as an `OutEvent::ToolRequest` on S with a fresh seq. It carries the same
   `request_id`, `tool`, `input`, `reason` and `scopes`, and
   `detached: true`. There are two exceptions. The `Stop` heal moves cards
   silently. The heal before a decision announces every moved card except
   the one being answered on its own session (a card that moved past that
   session is still announced). Announcing a card that is about to be unwound or
   answered would leave a dead one on S.
4. **Which card a decision answers.** `resolve` for `(X, request_id)`
   considers the exact key and the card *moved onto* X with that id, and
   takes the live one (§5 leaves at most one); a waiter that gave up is
   taken only when nothing live matches. `forget`, a timeout's cleanup,
   removes only its park's exact key, and only once that waiter is gone.
   So:
   - `Approve{P, id}` from a head that showed the card on P, or from an
     embedder that cached P, still resolves the moved card.
   - `Approve{S, id}` resolves it too.
   - A predecessor's id only ever matches its own key, so a stale
     `Approve{P, id}` never reaches a card S parked itself, and a late
     `forget` never takes a live card moved onto its id.
   - A waiter that gave up (a timeout's entry before its `forget`, an
     aborted task's) is ignored by listings, the park guard and the move;
     live-first `resolve` keeps a dead entry with the same id from
     shadowing a moved card.
5. **A home never holds two live cards with one id, and a live detached
   card is never displaced.** Every compaction ends P, so a card displaced
   from S back to P would be stranded on a dead session: unlisted on S,
   untouched by S's `Stop`, and announced with seq 0 (#884). Two live cards
   with one id on S would let `Approve{S, id}` answer a card the user never
   saw. So:
   - Only a turn card supersedes a live turn card under the same key, which
     resolves to `Stop` (a resume's re-offer after a lagged-away
     `SessionHibernated`, whose old turn waiter is still live).
   - Any other park sharing a live card's key, or its id on its home, is
     refused. It gets an immediate `Reject` with a reason the model sees
     ("an approval for request … is already parked") and emits no card.
     That covers the successor's own park of a moved card's id (a
     foreground repeat of a background id too), a detached park under a
     live turn card's key, and a fresh detached park whose lineage ends at
     a session already showing its id.
   - A move whose card's id the target already shows leaves that card on
     its current home, unannounced, still answerable under its own key.
     With the park-time heal (§2) this needs a park that skipped the heal,
     which no approval park does.

   `resolve`'s fallback in §4 therefore has at most one live candidate; it
   wins over any waiter that gave up.
6. **Events are atomic with the map.** A park registers *and* emits its
   `ToolRequest` under the registry lock (`register_parked`'s `emit`), and a
   move announces each moved card under the same lock
   (`reassign_detached`'s `emit`). So no move runs between a park and its
   event, no decision resolves a card between its move and its
   announcement, and every event names the card's current home.
7. **The TUI moves the card.** `SessionView` records `predecessor` from
   `SessionStarted`. When a detached `ToolRequest` folds into S's view (it
   passes the seq dedupe), `SessionRegistry` drops the same `request_id`
   from every view along S's predecessor chain. A newer card with an id the
   view already queues replaces that entry in place, and when it is the
   prompted card with new content the pager closes and a half-typed reject
   reason is taken, as for a moved prompt. One answerable copy is left, on the
   view `request_stop(S)` clears. A predecessor's prompt that moves off the
   card resets the pager (ADR-0237 §6).

Unwinding is otherwise unchanged. `stop_session(S)` unwinds the moved card.
The script's own timeout removes it (`await_timed` forgets the park's own
key once its waiter is gone). `stop_attached` still spares detached cards.

## Consequences

- `ListPending` and the poll subtree section report the card under S. An
  unfiltered listing shows it under P until a move runs.
- A head sees two `ToolRequest`s for one park, on P and then on S. A head
  that tracks cards per session should drop P's copy when S announces the
  same `request_id` (the TUI does). Within one session, a repeated id is
  the same card.
- A park that reuses a live background card's id on its session is now
  refused, where before it silently superseded the script's waiter and
  stopped the script. The refused call fails with the reason; a turn
  card's re-park still supersedes.
- Known limits:
  - §5's move fallback is reachable only through a park that bypasses
    `park_approval_as` (`PendingDecisions::register`, used by `ask_user`,
    whose call ids never take a script's `<id>:rhai:<tool>` shape) or a
    multi-hop lineage whose middle move never ran (#888). The card then
    stays on its old home, listed and answerable there; if a successor's
    detached announce of the same id already dropped the TUI's copy, only
    `ListPending` shows it.
  - The directory heal is one hop, as in ADR-0231 (#888). The recorded
    lineage covers longer chains only for moves that ran.
  - The lineage map gains one small entry per compaction and is never
    pruned (#887).
  - The script still grades its nested calls as P, so the session-scoped
    grants it earns are recorded under the ended P (#886).

## Alternatives considered

- **Re-key the map entry to `(S, id)`.** A card that S parks itself under
  the same id would overwrite it, and answering under the predecessor's id
  would need a lineage fallback that can reach S's own card. Keeping the
  key and moving only the home avoids both.
- **Send the moved card back to P when S parks the same id.** Every
  compaction ends P, so the card would be stranded where nothing lists,
  shows or stops it.
- **Refuse every second park of a live key.** A resume's re-offer after a
  lagged-away `SessionHibernated` would then be refused while the stale
  turn card stays approvable.
- **Resolve-time aliasing only, with no home.** That leaves S's
  root-filtered `ListPending`, its `poll` and its `Stop` blind, and those
  are the whole issue.
- **Re-key the script's dispatcher session to S.** The dispatcher is built
  once per launch, and its session is also the grading and grant key.
  Moving it would change what the script is allowed to do mid-run.
- **Drop any other view's card with the same `request_id` in the TUI.**
  Ids are not unique across unrelated sessions, so only the compaction
  lineage is a safe scope.
- **A "card moved" wire event.** The fresh `ToolRequest` already carries
  everything a head needs.
