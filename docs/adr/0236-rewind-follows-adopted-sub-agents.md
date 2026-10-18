# 0236. Rewind follows adopted sub-agents' backups

- Status: Accepted
- Date: 2026-10-18
- Issue: #870 (found while implementing #862)
- Amends: [ADR-0212](0212-undo-rewind-via-per-write-file-backups.md) (a
  change records the tree it was made in, and a revert reads more than its
  root's own backups) and
  [ADR-0234](0234-live-sub-agents-survive-compaction.md) (its consequence
  "`/rewind` stays per root file" no longer holds; §8's "file backups stay
  per file" now applies to where backups are *written* only)

## Context

ADR-0212 keeps one backup directory per root file, and a revert read only
the target's root. ADR-0234 moves a parent's live sub-agents under its
compaction successor, but the tap keeps logging each one into the root file
it started in, and its file backups land beside that file too. So a
`/rewind` or `RevertFiles` on the successor did not reach the writes those
children made.

Reconstructing, from the logs, which tree a write belonged to proved
unreliable:
- A background `rhai` script runs under its call's exec seq but logs its
  "started" result at once, then keeps writing.
- The tap can log a result before an earlier-broadcast `ParentChanged`.
- A record's timestamp is when it was logged, not when its event happened.
The engine, though, knows the tree at the moment of every write.

## Decision

1. **Record the tree at write time.** Each backup `change` line gains
   `ancestry`: the writing session and its live ancestors, nearest first.
   The field is `#[serde(default)]` and omitted when empty. The source is
   Holly's session directory (`file_backup::holly_ancestry`, installed by
   `track_roots`). Backups are still *written* where they always were: no
   routing change.
2. **Stamped before the write, so a race can only miss a change.** The
   host tools (`write`, `edit`, `apply_patch`, and so the `rhai` bindings
   that run them) call `FileBackups::stamp` *before* they touch the file.
   The stamp takes the change's `ts` and its ancestry; `record_stamped`
   stores them after the write. The directory is asked even with undo
   disabled, so a config reload enabling it in between leaves no change
   without its tree.
   - The supervisor's `adopt_children` re-parents the child in the directory
     (`SessionDirectory::set_parent`, under its lock) *before* it broadcasts
     `ParentChanged`. A spawn or a cold resume inserts the parent before the
     session's task starts.
   - So the directory lock orders every stamp against the move: a stamp
     after the move reads the new parent, a stamp before it reads the old
     one, and no broadcast consumer, so no lag, sits in between.
   - A move between the stamp and the write makes a post-move write look
     pre-move. A revert of the successor then misses it. It never undoes a
     write that was another tree's.
   - A chain cut short is still a true prefix of the session's path. That
     happens when an ancestor is not, or no longer, in the directory, for
     example a successor not yet registered. Every session it names did
     hold the write.
3. **Selection.** A revert, an undo or the `/rewind` picker for session `T`
   takes the changes whose `ancestry` contains `T`. This is ADR-0212's
   "the session and its sub-tree", now taken as of the write. A sub-agent
   target takes its own sub-tree's changes. A moved child's pre-move writes
   stay with the old parent's tree, including for a cold-resumed
   predecessor. Its post-move writes go to the new one. A move inside one
   tree (a sub-agent's own compaction) keeps every write.
4. **Which files.** The changes are read from every file that
   `SessionStore::read_tree` reaches from the target's root: the `Adopted`
   pointers, recursively, and the one-hop predecessor fallback.
   `SessionStore::tree_files` returns those files raw, the root's own first.
   An unreadable adopted manifest is warned about and skipped. The root's
   own unreadable manifest is still an error.
5. **Turns.** A change is placed against the target's prompts as before, by
   its call's record position when both are in one file. A prompt in
   another file is compared by the change's wall-clock `ts`, because two
   files share no positions. A change attributed to `T` is in `T`'s turn 1
   at least. The tap logs a successor's seed prompt only once the
   successor's own `SessionStarted` arrives, after the move, so a post-move
   write can precede it by the clock. Recording the target's turn as well
   was rejected: no ancestor's prompt count is known at the write without
   asking each session task.
6. **Ordering and markers.** Each manifest's changes keep their record
   order. Manifests are merged by wall clock only between each other: the
   smallest head `ts` goes next, and the earlier manifest wins a tie. A
   file goes back to the first change's `before`, from the blob beside that
   change's manifest, if it still hashes to the last one's `after`. Each
   `reverted` marker goes back to the manifest holding its ids. A revert
   parses the logs before it takes the manifest lock every tracked write
   needs.
7. **Busy check.** The sub-tree to wait on is read from the logs: each
   session's latest parent edge (`SessionStarted.parent`, then
   `ParentChanged`), merged across files. So a revert waits on moved
   children, and not on ones that moved away (see also §9).
8. **Changes without ancestry use the pre-#870 rule.** Such a change
   comes from one of three places:
   - a manifest written before this ADR;
   - an embedder that never calls `track_roots`, so no probe is installed;
   - a session the directory no longer knew at the write, for example a
     closed or hibernated sub-agent whose background script writes on.
   It counts for `T` if its session is in `T`'s tree *now*, by each
   session's latest parent edge in the logs (`lineage::current_tree`, the
   same set the busy check uses). There is no turn clamp. For a gone
   session that is the right answer: its latest edge names the tree it left
   in.
   - **Dev-build caveat:** #862 is unreleased, so only dev builds wrote
     manifests with moved sub-agents and no ancestry. In those, a moved
     child's pre-move writes count as the successor's. The conflict check
     still never clobbers a file changed since.
9. **Busy check covers every writer.** Besides the logs' current tree, a
   revert refuses while the session of any selected change is busy. A tap
   lagging on `ParentChanged` leaves a moved child out of the logs' tree,
   but not out of the selection.

## Consequences

- Rewinding a compaction successor covers the tracked writes every
  sub-agent in its live tree made while in it, whatever order the logs hold
  them in and whatever the clock did.
- Rewinding a cold-resumed predecessor no longer reverts what a moved-away
  child wrote after the move. It still reverts the writes from before.
- Each change line grows by its ancestry (a few ids). Recording reads the
  directory once per ancestor.
- A revert reads one log file and one manifest per file in the tree, so the
  predecessor's are read even when nothing moved. The cost is the same as a
  resume's.
- A pointed file that is gone (deleted, or aged out) has taken its backups
  with it. Its changes cannot be reverted.
- The wall clock (`SystemTime`, not monotonic) still orders changes across
  manifests and places cross-file changes in turns. A clock step can
  misorder or misplace them. If it misorders one file's changes, the
  conflict check reports the file as skipped, never clobbered. Tree
  membership never depends on the clock.

## Alternatives considered

- **Route backups by the live root** after a move. Rejected, following the
  user's decision: the backups stay where they were written, and a child's
  backups would be split anyway.
- **Infer the tree from the logs for every change** (this ADR's first
  draft). Background scripts, the tap's result-before-move ordering and
  logging-time timestamps all misattributed writes. Dropped entirely, also
  for legacy changes, which use the plain latest-edge rule (§8).
- **Stamp after the write** (inside `try_record`). A move between the write
  and the stamp would hand a pre-move write to the successor, which is the
  unsafe direction. Rejected for stamping first (§2).
- **Fold `ParentChanged` in `track_roots` and read that map at write
  time.** It is fed by a broadcast consumer, so a write right after the
  move could still read the old parent. Rejected for the directory, which
  is updated before the broadcast.
- **Record only the root.** A rewind target can be a sub-agent (ADR-0212
  reverts a session's sub-tree), and the root alone cannot answer whether a
  change is in that sub-tree. Rejected for the ancestry, which is a few ids.
- **Offer pre-move child writes on the successor.** Rejected (confirmed by
  the user): it reaches into the predecessor's era for the children only.
