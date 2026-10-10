# 0212. Undo/rewind of file edits via per-write backups

- Status: Accepted
- Date: 2026-10-11
- Relates to: [ADR-0060](0060-filechange-audit-via-executor-as-path-kind-hash.md) (`FileChange`, the
  audit event this complements), [ADR-0020](0020-event-sourced-session-persistence.md)
  (the session log a revert reads turns from),
  [ADR-0069](0069-trusted-untrusted-wire-frame-split.md) (wire allowlist),
  [ADR-0109](0109-escape-root-access-via-approval.md) (out-of-root writes),
  [ADR-0118](0118-ambiguous-stop-reason-bounded-retry.md) (the persisted
  user-role note pattern reused here)

## Context

opencode (`/undo`) and Claude Code (checkpoints, `/rewind`) can roll back the
agent's file edits; entanglement could not — `ReplayFrom` only fetches
history (#731). The maintainer chose **per-write backups** over a git-based
snapshot: no dependency on the project being a repository, no stash/commit
side effects in the user's tree, and nothing beyond what the agent's own file
tools wrote.

## Decision

1. **Backups are recorded by the file tools.** `write`/`edit`/`apply_patch`
   built `with_backups(FileBackups)` call `FileBackups::record` after a
   session-scoped write (`run_for_session`), handing over the bytes they
   already read for the write itself — no extra read, and the `rhai`
   `write`/`edit` bindings are covered for free because they run the same
   registry tools. A backup failure is logged, never a tool error: the edit
   already happened.
2. **Storage is per root, beside the log.** `SessionStore::backups_dir(root)`
   = `<store>/backups/<root_id>/`: a content-addressed `blobs/<sha256>`
   (write-then-rename, deduped per root) and an append-only
   `manifest.jsonl` of `change` lines `{id, ts, session, request_id,
   exec_seq?, path (absolute), display, before: absent|stored{hash,size}|
   skipped{reason}, after: sha256|null, mode?}` and `reverted{ids,
   attempted}` markers. The id counter and stored-blob set are cached in
   memory per root, so a write never re-reads the manifest. A spawned child's
   changes resolve to its root through `SessionStarted` lineage
   (`FileBackups::track_roots`). Retention is the log's: `delete` removes the
   directory with the `.jsonl`, `prune` removes any directory whose `.jsonl`
   is gone.
3. **Caps record, never drop.** A prior version over 8 MiB, or past the
   root's 256 MiB stored budget, is recorded as `skipped{reason}` so a revert
   reports "can't restore X: no backup (…)". Constants for now
   (`DEFAULT_MAX_FILE_BYTES`/`DEFAULT_MAX_ROOT_BYTES`,
   `FileBackups::with_limits` for embedders); `config.yml` knobs are #787.
4. **Turns are user prompts, placed at record time.** The executor runs each
   dispatched call with its `ToolExec` seq in a task-local
   (`file_backup::with_exec_seq`), and the change line records it:
   `(session, exec_seq)` names exactly one log record, and the change belongs
   to the turn of the target session's last `Prompt` before it (the tap logs
   a prompt ahead of everything it causes). Request ids alone are **not** a
   key — Gemini's `name#ordinal` ids repeat every turn and in children.
   Without a seq (a detached `rhai` script) the change falls back to the
   latest `(session, request_id)` call logged no later than itself (a
   binding's `<id>:rhai:<tool>` resolves to the outer call), then to record
   timestamps.
5. **Restore is conflict-checked per file, atomic, and refused while the
   tree is busy.** A revert is refused (an `Error`) while any session of the
   target's sub-tree is mid-turn (`Status` folded by `track_roots`) or runs
   detached work (a background job or detached script, via the
   `BusyProbe`) — its writes would race the restore. Otherwise, reverting
   after turn `N` takes every unreverted change of the session and its
   sub-tree in turns `> N`, groups them per file, and puts the file back to
   the **first** change's `before` (rewrite; delete when the agent created
   it; recreate when the agent deleted it) only if it still hashes to the
   **last** change's `after`. Rewrites go through a synced sibling temp file
   renamed over the target, carrying the file's mode (or the recorded one).
   A file already back at `before` (an earlier revert whose marker was lost)
   counts as restored. Outcomes:
   - restored → marked `reverted`;
   - no backup (over a cap, blob missing) → reported once and marked
     `reverted` too, since it can never be restored;
   - changed since (a user edit, a `bash` command) → reported, never
     clobbered, and marked `attempted`: it stays revertable, but "undo"
     (`to_turn: None`) targets the turn below the latest turn with
     *unattempted* changes, so a conflict never pins it.
   A failed marker append is logged; the report still goes out. Escape-root
   writes are covered (the manifest holds the resolved absolute path;
   reverting needs no new grant — it only writes back bytes the agent
   replaced, on a trusted frame).
6. **Protocol: one trusted-only round trip through the session.**
   `InMsg::RevertFiles{session, to_turn?}` → `OutEvent::FilesReverted{session,
   seq, to_turn, restored, skipped:[{path,reason}]}`. It is **wire-refused**
   (it rewrites the user's tree). Core routes it to the session as a
   deferrable command (stashed while a turn is live or paused, like
   `SetModel`) and calls a new `EngineConfig::file_reverter` seam — core does
   no file I/O; `None` answers with a recoverable `Error`. The session itself
   emits `FilesReverted` and, when something was restored, appends a
   user-role note to `Context` (what was restored/skipped, that exec changes
   were not reverted, re-read before editing).
7. **Replay never touches the disk.** `Session::replay` folds
   `FilesReverted` into the identical note — ahead of a prompt paired with
   it, since a prompt sent during the revert is logged first but was
   processed after — and never calls the seam.
8. **TUI.** `/undo` sends `to_turn: None`; `/rewind <N>` sends `Some(N)`;
   bare `/rewind` lists the turns with unreverted edits (turn, file count,
   prompt snippet) straight from the store. The `FilesReverted` reply renders
   as a notice that always states that `bash`/`call`/`rhai` exec changes are
   not tracked.

## Consequences

- Not covered, by design: filesystem changes made by `bash`, `call`, a `rhai`
  `exec`/`bash` binding, `propose_plan`'s plan files, and the TUI's `!bash`
  passthrough. Every report and the model's note say so.
- A compaction successor is a new root with its own log; rewinding does not
  cross a compaction boundary.
- `/fork` (a non-compacting copy of a session) is **not** part of this
  change: the existing fork machinery seeds a successor from a *text* prompt
  (summary or pruned transcript), so a verbatim copy needs new core API —
  #786.
- `FileChange` stays the audit event; backups are a separate runtime store
  and never ride the broadcast.

## Rejected

- **Git snapshots / stash per turn** — requires a repository, mutates the
  user's VCS state, and captures unrelated user changes.
- **Recording in the executor via the `file_change` task-local** — the sink
  holds one record per call, a binding loop writes many, and a backup must be
  taken where the prior bytes already are.
- **Runtime-consumed `RevertFiles` (like `AnswerQuestion`)** — the session
  could not learn of the revert to tell the model, and a revert mid-turn
  would race the turn's own writes; routing through the session gives both
  the note and the deferral for free.
- **Ordering changes by timestamp only** — a lagging persistence tap could
  stamp a prompt after the writes it caused; log position is exact.
- **Locating a change by `request_id` alone** — ids repeat across turns and
  sessions (Gemini), which attributed later edits to the first turn and made
  one undo revert every edit since.
- **Silently overwriting a file changed since** — loses user work; refusal
  with a reason is the only safe default.
