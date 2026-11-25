# 0004. Structured Plan & TaskList events (profiles + events, both)

- Status: Accepted (the `TaskList` half is superseded by
  [ADR-0039](0039-markdown-task-list.md); the `Plan` event and the
  two-write-paths design below are unchanged)
- Date: 2026-07-04

## Context

ADR-0003 establishes agent profiles for the Build/Plan/Explore **mode** axis. A
separate question: should the *plan* and the *task outline* the agent produces be
**structured output events** (so any UI can render a native plan/task panel), and
if so, does that duplicate or conflict with the profile-based "plan"?

OpenCode has **no** structured plan/task output — `plan` is an agent mode
(ADR-0003), and even the todo list is a plain tool (`todowrite`), emitting text.

## Decision

**Both, as orthogonal axes:**

- **Agent profiles** (ADR-0003) control *what the agent is instructed and
  permitted to do*.
- **Structured events** control *how artifacts are rendered*.

Two new content events, each a **full snapshot re-emitted on every change**
(the `agent`/`design` "snapshot on change" pattern — idempotent, trivial to
render/dedupe):

- `OutEvent::Plan { session, seq, content }` — markdown strategy prose.
- `OutEvent::TaskList { session, seq, tasks }` — statusful outline of
  `TaskItem { id, content, status }`, `status = pending | in_progress | completed | cancelled`.

  > **Superseded (#142, [ADR-0039](0039-markdown-task-list.md)).** The
  > *structured* `TaskList` described here — `Vec<TaskItem>` with `id`/`status`,
  > written by `update_tasks` (JSON array) / `InMsg::SetTasks` — was replaced by
  > a plain markdown snapshot (`content: String`), mirroring `Plan`. The `Plan`
  > event and the two-write-paths design are unchanged.

Each is written **two ways** (by design):

1. A **built-in engine tool** the model calls — `update_plan` (input = markdown)
   and `update_tasks` (input = JSON array). These bypass the permission profile
   (they only mutate session state) and never require approval.
2. A **harness message** — `InMsg::SetPlan` / `InMsg::SetTasks`, so a UI lets the
   user edit the plan/outline directly (à la `agent`'s `redefine`).

A `plan`-profile system prompt instructs the model to populate these, so a Plan
turn produces both text *and* structured artifacts.

## Consequences

- **(+)** Every head (TUI, web, stdio) renders plan/task panels natively without
  parsing prose.
- **(+)** User can override the plan (`SetPlan`) — useful for steering.
- **(−)** Two write paths to the same state (tool + InMsg). Documented and
  intentional; the alternative (one path) would exclude either the model or the
  user.

## Alternatives considered

- **Profiles only, no structured events (pure opencode).** Rejected: UIs would
  have to parse plan text or render it inline; loses the native panel affordance
  that motivated this work.
- **Structured events only, no profiles.** Rejected: no way to control the
  agent's permissions/mode (see ADR-0003).
- **Delta events** (`TaskAdded`, `TaskStatusChanged`) instead of full snapshots.
  Rejected: both reference projects re-send the full list on change; snapshots
  are simpler, idempotent, and dedupe trivially by `seq`.
