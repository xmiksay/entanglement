# 0039. Markdown task list (structured `Vec<TaskItem>` → plain snapshot)

- Status: Accepted
- Date: 2026-07-11
- Supersedes: the `TaskList` half of [ADR-0004](0004-structured-plan-and-task-events.md)
  (the `Plan` event and the two-write-paths design are unchanged)

## Context

ADR-0004 made the task outline a **structured** event: `OutEvent::TaskList`
carried `Vec<TaskItem>` (`{id, content, status}`, `status = pending |
in_progress | completed | cancelled`), written by the `update_tasks` built-in
(input = a JSON array of items) or `InMsg::SetTasks`. The rationale was native
panel rendering in any head.

Practice showed the structure costs more than it renders:

- **The model pays for it.** Emitting `id`/`status` JSON per item on every
  update is pure token/latency overhead, and the schema invites malformed-JSON
  retries. The task list exists **for the user** as progress info — the engine
  never consumes the structure (no scheduling, no gating, nothing reads
  `status`), and the list is not fed back to the model.
- **The rendering it bought is markdown-native anyway.** The status icons the
  TUI derived from `TaskStatus` are exactly what a `- [ ]`/`- [x]` checkbox
  list expresses, and the TUI's markdown renderer already draws checkbox
  markers (pulldown-cmark `TaskListMarker`). The export path literally
  converted `TaskItem`s *back into* checkbox markdown.
- **The sibling snapshot already works this way.** `Plan` has been a plain
  markdown `content` string from day one, written by `update_plan { content }`;
  the two-snapshot machinery is otherwise identical.

## Decision

The task outline becomes a **plain markdown snapshot**, mirroring `Plan`
exactly (#142):

- `OutEvent::TaskList { session, seq, content: String }` — still a full
  snapshot re-emitted on every change (ADR-0004's snapshot pattern is kept).
- `update_tasks { content: string }` — same shape as `update_plan { content }`;
  its description tells the model the list is user-facing progress info, not
  fed back, and to keep it a short checklist.
- `InMsg::SetTasks { session, content: String }` — mirrors `SetPlan`.
- `TaskItem`/`TaskStatus` are deleted from the protocol. Heads render the
  markdown through the same renderer as the plan (the TUI checkbox marker does
  the icons); the transcript export embeds the string verbatim.

Both write paths (built-in tool + harness message) survive — only the payload
shape changed.

## Consequences

- **(+)** Cheaper, more reliable tool calls: the model writes the checklist it
  would have written anyway, no per-item JSON envelope, no enum to violate.
- **(+)** One rendering path for plan and tasks in every head.
- **(−)** Heads can no longer count tasks or filter by status without parsing
  markdown. Accepted: nothing did — the list is a display artifact, and a head
  that wants structure can parse checkbox lines, which are more constrained in
  practice than the old free-form `content` strings were.
- **(−)** Wire break for `SetTasks`/`TaskList`. Accepted: pre-1.0, no external
  heads exist, and the NDJSON `kind` tags are unchanged. A persisted session
  log holding an old structured `task_list` record fails resume with the
  existing loud "log has a hole" error (never a silent mis-fold) — delete the
  stale `.jsonl` to move on.

## Alternatives considered

- **Keep the structured list, add a markdown mirror.** Rejected: two shapes for
  one artifact, and the model still pays the JSON envelope.
- **Structured items but optional `id`/`status`.** Rejected: keeps the
  malformed-JSON failure mode and the per-item envelope; the user-facing goal
  needs neither.
- **Drop the task list entirely (fold into `Plan`).** Rejected: plan (strategy
  prose) and tasks (live progress) update on different cadences; separate
  snapshots let heads pin them to different panes.
