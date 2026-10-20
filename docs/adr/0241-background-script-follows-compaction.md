# 0241. A background script follows its session's compaction

- Status: Accepted
- Date: 2026-10-20
- Issue: #886
- Amends: [ADR-0231](0231-compaction-successor-inherits-background-work.md)
  (what the successor inherits: the script's grading session too) and
  [ADR-0238](0238-compaction-moves-a-background-scripts-card.md) (its
  rejected alternative "re-key the script's dispatcher session to S", and
  its known limit naming #886)

## Context

After a compaction, ADR-0231 moves a background `rhai` script from the
predecessor P to the successor S, and ADR-0238 moves its parked cards. The
script's nested calls were still graded as P, because its `Dispatcher` is
built once at launch and its `session` is fixed. The `Session` grants and
`http` host unlocks the script earned after the compaction were recorded
under P. P's grants were dropped on its `SessionEnded`, so these never
applied on S: the model's own identical call on S was prompted again.

A card parked as P before the compaction and answered after it had the same
problem. Its grant was recorded under P too.

## Decision

1. **The script follows adoption.** Before each nested call, a detached
   run's binding loop (`script::service`) calls
   `Dispatcher::follow_session`. `Holly::successor_of` already walks the
   dispatcher's session to the last link of its compaction chain; if that
   session is live, the dispatcher switches to it. From
   then on the script's calls are graded, granted and parked as S: S's
   grants apply, `Session` grants and host unlocks are recorded under S, and
   new cards park on S as S's own (key `(S, id)`, home S).
2. **What switches.** The session id is re-keyed: `Dispatcher.session`,
   its slot in the ancestor `chain`, and its own `overlays` entry. A
   compaction successor starts with its predecessor's overlay, workdir and
   parent (`Lineage::Successor`, `Inherited`), so the launch snapshot already
   holds S's values.
3. **The mode is read per call.** Before every nested call of a detached
   run, switched or not, the grant-scope mode and its limits are re-read
   from the executor's folded `perm_modes` for the dispatcher's session. A
   grant matches only the mode it was earned in (ADR-0207 §8), so a
   `SetMode` on S (or on P before the compaction) reaches the script's next
   call; before this, the launch's mode stayed the grant scope for the whole
   run. The overlay is mode-scoped too: the executor drops a session's
   overlay on a mode change, so the dispatcher remembers the mode its
   overlay snapshot was taken under (`overlay_mode`) and drops its own
   session's entry once the current mode differs. Ancestors' entries keep
   the launch snapshot (#899). The mode grade itself goes through the resolver per
   chain session and always read the live mode.
4. **When it switches.** Lazily, between nested calls, never mid-call. The
   supervisor writes the successor record before S's `SessionStarted`, so a
   lagged lifecycle fold cannot hide it. The binding loop is serial, so
   there is no race with the call in flight. A call already graded or
   parked as P finishes as P, and ADR-0238 moves its card as before. A
   successor that is not live yet is not followed: it has no seq counter,
   so a card parked there would be announced with seq 0. Its
   `SessionStarted` fold moves anything parked on P in the meantime. A
   hibernated successor is not followed either, and the script stays on P
   until it is live again. Once switched, the script stays on S even if S
   hibernates or ends: its later cards park on S with S's retired
   high-water seq (ADR-0239), and it moves on only to a live successor of
   S. The switch also waits until the executor has
   folded S's mode: the resolver fails closed on an unseen session, and the
   serial fold has handled S's `SessionStarted` (which moves P's cards)
   before its `ModeChanged`. If that `ModeChanged` is lost to broadcast lag,
   the script stays on P, and its grants stay on P too (§5), which is the
   behaviour before this ADR.
5. **The grant follows the card.** For a detached script's call, an
   approval's `Session`/`Always` grant and its `http` host unlock are
   recorded where the script's next call will be graded
   (`Dispatcher::earner`), not the session the call was parked as: the
   session the script would switch to under §4's exact condition (one
   helper, `Dispatcher::followable`), else its current session. So a
   card parked as P before the compaction and approved after it earns S's
   grant. The per-run approval cache is per run, not per session, and is
   unchanged.
6. **Only detached runs follow.** A blocking script's turn is parked on its
   own `ToolExec`, so its session does not compact under it, and its cards
   are turn cards that `stop_attached` unwinds with the turn.

What else keys on the dispatcher session, and what happens to it after the
switch:

| Keyed on the session | After the switch |
| --- | --- |
| Session grants (`GrantStore`) | follow S (record and lookup) |
| `http` host unlocks (`HttpGate`) | follow S |
| Approval park (`PendingDecisions` key and home, `ToolRequest`, `ApprovalResolved`) | follow S |
| `pre_tool_use`/`post_tool_use` hooks' `ENTANGLEMENT_SESSION_ID` | follow S |
| Repeat-denial tracker, MCP per-user scopes and lazy connect, `FileChange` | follow S |
| `NestedCall`/`NestedCallSummary` live reports | follow S (`NestedCalls::rehome`); a call's report goes out on the session it ran as |
| File backups (undo, ADR-0212) | recorded under S. The launch's `ToolExec` seq is masked (`file_backup::without_exec_seq`): it is a seq in P's log and would name an unrelated record in S's. The change is then placed like any change without a seq: by the latest `ToolExec` in S's log with the launch's outer request id at or before the write, else by its wall clock. The outer id is normally absent from S's log, so the wall clock decides; a provider that repeats call ids (Gemini's `name#ordinal`) can put the same id in S's log and misplace the change (#900) |
| `ScriptRegistry` owner (`poll`, `ListOperations`) | already moved by ADR-0231's `reassign_owner`, unchanged |
| Mode scoping grants, and its limits (approval timeout, unattended collapse) | re-read for the current session before every call |
| Workdir rewrite of relative paths | launch snapshot (S inherits P's workdir) |
| MCP namespaces registered into the engine | launch snapshot (S inherits P's user) |
| Overlay entries | launch snapshot, re-keyed to S; the session's own entry is dropped once its mode differs from the snapshot's |

ADR-0238's machinery stays. The lifecycle-fold move, the heals and the
park-time forward walk still handle a card parked as P: one in flight
during the compaction, or one parked while S is still starting. A switched
script's parks land on S and need none of it.

## Consequences

- After a compaction, a background script's `Session` approval clears the
  successor's next identical call, whether the script or the model makes it.
- A card a switched script parks answers under S's id only. The
  predecessor's id answers only cards parked as the predecessor.
- `Session` grants the script earned under P *before* the compaction are
  dropped with P, as the model's own are. The script is prompted once more
  after the switch. A compaction successor does not inherit session grants
  in general, and this ADR does not change that.
- A mode change reaches a running background script at its next nested
  call, on P before the compaction as on S after it. A blocking script
  still keeps its launch mode as its grant scope for the run (#898).
- The overlay drop compares modes between calls. A mode change and its
  reversal that both land between two calls leave the script's snapshot
  entry in place, although the executor dropped the session's overlay
  (#899, which also covers the ancestors' entries of §3).

## Alternatives considered

- **A shared cell the lifecycle fold updates on `SessionStarted`.** The
  fold is lossy, so the lazy `successor_of` read would be needed anyway, and
  S's mode is not folded yet at that point. Rejected: the lazy read alone is
  enough.
- **Rebuild the whole dispatcher for S.** Its inputs (the spawn guard, the
  overlay map) are owned by the executor loop, and S's state equals P's at
  the switch anyway.
- **Record the grant under the card's `Parked.home`.** The home follows the
  registry's own lineage record, which only the fold and heals write. The
  supervisor's record is the authoritative one, and the switch already
  reads it.
- **Follow a successor that is not live yet.** A card parked there would be
  announced with seq 0 and nothing would move it.
