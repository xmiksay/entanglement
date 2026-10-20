# 0243. A running script re-reads its mode and drops overlay entries by mode epoch

- Status: Accepted
- Date: 2026-10-20
- Issues: #898, #899, #900
- Amends: [ADR-0241](0241-background-script-follows-compaction.md) (§3
  "the mode is read per call" now covers blocking runs; its `overlay_mode`
  comparison is replaced; its backup row's request-id fallback is closed;
  the Consequences naming #898/#899) and
  [ADR-0227](0227-one-binding-path-namespaces-http-skill-libraries.md) §2
  ("a mode switch mid-script reaches the next script, not the running one";
  the per-run approval cache now lapses on a mode change), and
  [ADR-0205](0205-every-compaction-forks-a-successor-session.md) /
  [ADR-0110](0110-compaction-successor-closes-predecessor.md) (a compacted-away
  source "stays resumable": it no longer does, §8)

## Context

ADR-0241 left three gaps, filed as follow-ups:

- **#898.** Only a detached run re-read its session's mode before each
  nested call. A blocking script kept its launch mode as its grant scope
  for the whole run, so after a stricter `SetMode` mid-turn, a `Session`
  grant earned under the old mode still cleared the script's next call. A
  grant matches only the mode it was earned in (ADR-0207 §8).
- **#899.** The script dropped its own session's overlay entry when the
  session's current mode differed from the snapshot's (`overlay_mode`).
  The executor drops a session's overlay on every mode switch. (a) A switch
  and its reversal between two calls (A→B→A) left the entry in place,
  although the executor had dropped it. (b) Ancestors' entries were never
  checked at all.
- **The same class as #898, found in review.** The per-run approval cache
  keyed on tool and input only, so an approval answered under the old mode
  cleared the same call after a stricter switch with no card.
- **#900.** After the switch, a backup's launch `ToolExec` seq is masked,
  and `Index::position` then looked up the launch's request id in the
  successor's log before falling back to the wall clock. Gemini repeats ids
  (`name#ordinal`), so an unrelated call in the successor's log could carry
  the same id and take the change into its turn.

## Decision

1. **Every run re-reads its mode per call.** `Dispatcher::follow_session`
   runs before every nested call of every script run. It re-reads the
   grant-scope mode and its limits from the executor's `perm_modes`, and
   drops stale overlay entries (§2), for blocking runs too. Switching to a
   compaction successor stays detached-only (ADR-0241 §6): a blocking
   script's turn is parked on its own `ToolExec`, so its session cannot
   compact under it. Where grants are recorded (`Dispatcher::earner`) is
   unchanged.
2. **Overlay entries are dropped by lineage epoch.** The executor keeps
   one overlay epoch per compaction lineage, keyed by its head
   (`Holly::lineage_head`; `tool_runner::mode_epochs::ModeEpochs`, shared
   like `perm_modes`). Its `ModeChanged` fold moves the lineage's epoch on
   every event after which the executor holds no overlay for a member:
   - an actual mode switch of any member (the executor drops that
     member's overlay);
   - a successor's first mode that differs from the mode the lineage was
     last in (core hands a successor its predecessor's overlay only when
     it starts in that mode, `Inherited`).

   A lineage's first folded mode is epoch 1. A snapshot of a lineage the
   fold has not seen yet records 0, so if that first `ModeChanged` was lost
   to broadcast lag, the next fold still makes the entry stale. A duplicate
   `ModeChanged` (a resume or a no-op `SetMode`) and a hibernate move
   nothing. The fold can place a successor's first mode because the
   supervisor records the successor before its `SessionStarted`.

   A lineage is linear and, once this process knows it, only grows at its
   end, so no known session's lineage head moves under a snapshot. Core
   enforces it:
   - `Spawn` refuses a second successor of one predecessor with a
     supervisor `Error`, on the new id and on the predecessor, before
     recording anything.
   - A resume refuses a compacted-away session (§8).
   - Re-learning a log's lineage (`SessionDirectory::record_successor`)
     skips an edge whose source already has a successor, or whose
     successor already has one or is live: such an edge would branch the
     lineage or prepend to a chain whose head is already known.

   The dispatcher stores, next to each overlay entry it snapshots, the
   entry's lineage head and that lineage's epoch (`overlay_epochs`). Before
   each nested call it drops every entry whose lineage epoch moved, for
   every session of the chain. A switch and its reversal still moves the
   epoch twice. `overlay_mode` is gone.
3. **One guard for the mode and the epochs.** The fold moves the epoch and
   stores the mode under one `perm_modes` guard. The script drops stale
   entries and reads its session's mode under one such guard too
   (`ModeEpochs::read`; lock order `perm_modes`, then the epochs; lineage
   heads are looked up before either). So it never pairs one moment's
   mode, the grant scope, with another moment's overlay: neither the new
   mode with an entry the switch dropped, nor the old mode without an
   entry it still had.
4. **Compactions along the chain only rename.** Every chain link with a
   live, mode-folded compaction successor moves to the end of its lineage
   (`Dispatcher::live_successor`, ADR-0241 §4's condition), as the
   executor's own chain follows it (`ParentChanged`). The script's own
   session moves only on a detached run (ADR-0241 §6); an ancestor moves on
   every run. A move renames the chain link, the overlay entry and its
   snapshot. It decides nothing: the snapshot names the lineage, whose
   epoch already counts every switch and every mode-changing start of
   every member, through any number of compactions between two calls.
5. **Only a switch or a mode-changing start invalidates an entry.** A
   replaced overlay (`ToolOverlayChanged`) does not move the epoch: the
   running script keeps its launch snapshot, as ADR-0227 §2 says. A session
   that ends or hibernates does not move it either: its last entry stays in
   the snapshot, as ADR-0241 keeps a switched script's overlay after S ends.
6. **The per-run approval cache lapses with the mode.** A card a script's
   nested call raised and the user approved is remembered for the run,
   keyed on tool and full input (ADR-0227 §2). On a mode change
   `follow_session` forgets those approvals (`ScriptCall::forget_approvals`):
   they answered a card of the old mode, like a grant earned in it.
7. **An adopted backup is placed by its wall clock.** A change recorded by
   a switched script's call is marked on the manifest `Change`
   (`adopted: true`, serde-defaulted and omitted when false, so older
   manifests read unchanged). The call task's scope says so
   (`file_backup::without_exec_seq` now sets `ExecScope::Adopted`).
   `Index::position` skips the request-id lookup for such a change, so the
   wall clock places it.

8. **A compacted-away session is not resumable.** Its lineage goes on in
   its successor; resuming it would start a second line. `Holly::resume`
   refuses a session this process knows was compacted away, with an
   `Error` naming the end of its chain ("session `p` compacted into `s`;
   resume `s` (fork `s` to branch)"). A fresh process does not know the edge, so the runtime's
   `SessionStore::load_for_resume` refuses the same from the logs: a later
   root log's own `SessionStarted` names it as predecessor
   (`SessionStore::compacted_into`). Every in-tree resume (`skutter run
   --resume`, the TUI modal) goes through it. The TUI picker hides a
   compacted-away root, and shows the refusal as a status line if one is
   picked anyway. To branch, fork the successor (`ForkSession` needs a live
   session). An embedder that resumes from its own store, or through an
   `UnknownSession::Resume` `SessionLoader`, must refuse such an id the
   same way: core refuses only the edges this process has seen.

## Consequences

- A `SetMode` reaches a running script at its next nested call, whether it
  is blocking or background. A grant earned under the old mode no longer
  clears it.
- After any mode switch of a session in the script's chain, that session's
  overlay entry no longer applies to the script. This holds even if the
  switch was reversed before the next call, and even if a switch and its
  reversal were followed by a compaction of that session (an ancestor, or
  the script's own session on a background run), or happened on a
  successor in the middle of several compactions.
- An overlay set after the launch still reaches only later calls of the
  model and later scripts. The running script does not pick it up.
- After a mode change, a script's call that an earlier approval of the run
  cleared is asked again. A **Run all** posture chosen on the launch card
  under the launch mode still force-allows the run's `Ask`s after a
  stricter switch: that predates this ADR and is a deliberate posture for
  the whole run (ADR-0228), not an approval of one call.
- A switched script's undo change lands in the successor's turn its write
  time falls in, even if a reused call id appears earlier in that log.
- Resuming a compacted-away session is refused; resume its successor
  (fork the successor to branch). The pre-compaction state can no longer be
  continued from, which reverses ADR-0205's and ADR-0110's "stays
  resumable". It stays inspectable (`skutter inspect session <id>`), and
  `ForkSession` needs a live session, so a branch starts from the
  successor, not the predecessor. Before, a resumed predecessor ran as a
  second line, and its own later compaction was the refused spawn.
- The executor keeps one counter and one mode per lineage it has folded.
  Like `perm_modes`, they are not dropped when a session ends: a script
  can outlive its session, and a resume must not reset the epoch to one
  the script has already snapshotted.

## Alternatives considered

- **Read the live overlays per call, as `perm_modes` is read** (the
  preferred KISS option in #899). This was rejected because it opens
  fail-open windows around a compaction. P's entry is forgotten at P's
  `SessionEnded`, which can arrive before S is followable, so the script on
  P would briefly grade with no overlay and P's deny entries would be lost.
  S's inherited overlay is also folded one event after S's mode, which is
  the event that unlocks the switch. And an ended or hibernated session's
  entry would disappear from a script that keeps running. It would also
  reverse ADR-0227 §2 for overlays, so a mid-run enable would reach the
  running script.
- **A generation bumped on every drop and replace, dropping the entry on a
  difference.** A replace that only adds a deny would then drop the
  launch's deny entries too, which is fail-open. Taking the live list on a
  difference instead is the live read above.
- **Compare modes, as before.** This misses A→B→A, which is #899.
- **A per-session epoch, re-keyed hop by hop.** A move would have to judge
  the predecessor's own epoch before re-keying, then the successor's from
  its start, and `successor_of` jumps to the end of the lineage, skipping
  any middle link's switches. Each gap was a fail-open hole found in
  review; covering them needed a one-hop core accessor and a per-hop
  walk. Keying the epoch by lineage removes the class: no move judges
  anything.
- **Fix #900 by the record's own session or ordinal.** The id itself is
  what repeats. Only the recorder knows that the change came from an
  adopted call.
