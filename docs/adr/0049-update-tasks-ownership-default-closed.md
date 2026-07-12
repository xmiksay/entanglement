# 0049. update_tasks ownership — default-closed task authority

- Status: Accepted
- Date: 2026-07-12
- Revises: the "`update_tasks` stays unconditional" sub-decision of
  [ADR-0041](0041-update-plan-ownership-default-closed.md) (the `update_plan`
  ownership gate and everything else in 0041 are unchanged)

## Context

[ADR-0041](0041-update-plan-ownership-default-closed.md) made `update_plan`
default-closed on `owns_plan` but left `update_tasks` **unconditional**, reasoning
that the task list is per-session progress bookkeeping shown to the user, never fed
back to the model, so it carries no cross-agent authority.

Both built-ins are still session-state tools that run in core's `handle_tool_call`
and never round-trip to the runtime, so they bypass the [ADR-0038](0038-physical-per-agent-tool-restriction.md)
tool mask **and** the runtime's `Allow`/`Ask`/`Deny` dispatch. With `update_plan`
now gated but `update_tasks` not, a read-only `explore` subagent — masked down to
`read`/`glob`/`grep`, denied every host write — could **still** author the session
task list (`turn.rs` appended the `update_tasks` spec unconditionally after the
mask filter, and `handle_tool_call` ran it for any profile). A physically
read-only agent mutating user-visible session state is exactly the boundary #116
and #140 were built to close (#175).

## Decision

Add `AgentProfile.owns_tasks: bool` (serde default **false**) and gate the
`update_tasks` built-in on it — **the same shape as `owns_plan`**, enforced
entirely in core because the built-in never reaches the runtime:

- **Advertisement** — `run_turn` appends the `update_tasks` spec only when
  `s.profile.owns_tasks`. A non-owner's model never sees the schema.
- **Enforcement** — `handle_tool_call` refuses a hallucinated `update_tasks` from a
  non-owner via a refusal `ToolOutput` (no task mutation, no `OutEvent::TaskList`);
  the turn continues. A masked schema is not a guarantee the model won't emit the
  call, so the runtime-independent refusal is the hard boundary.

`InMsg::SetTasks` remains head/user authority, unchanged — the user may always set
the task list.

### Built-in profiles

- `build.md` gains `owns_tasks: true` — it is the execution agent that tracks a
  progress checklist. `build` is the synthesized fallback profile, so the default
  session keeps its task list.
- `plan` and `explore` stay default-false: `plan` authors a plan and delegates
  (its checklist is not the execution one), and `explore` is the read-only leaf
  this closes the gap on.

## Consequences

### Positive

- Task authorship is default-closed: a future user-defined agent gains it only by
  opting in with `owns_tasks: true`, never by forgetting to opt out — mirroring
  plan authority, one consistent rule for both session-state built-ins.
- The read-only `explore` subagent can no longer mutate session task state.
- Zero new protocol message — the field rides `AgentProfile` like #116/#119/#140,
  and enforcement is a one-line gate in each of the two core call sites.

### Negative / neutral

- A user-defined **primary/execution** agent that wants a task list must now add
  `owns_tasks: true`; previously every profile got it for free. Accepted: this is
  the same opt-in cost #140 accepted for plans, and the security win (no silent
  task mutation from a read-only agent) is the point.
- Enforcement stays split from #116/#119's runtime location because the built-ins
  never round-trip. Inherent to their being session-state tools (same as #140).

## Alternatives considered

- **Route `update_tasks` through the #116 tool mask (`advertises_tool`).**
  Rejected for the same reason [ADR-0041](0041-update-plan-ownership-default-closed.md)
  rejected it for `update_plan`: the mask is default-open (`tools: None` = inherit
  all), so every future user-defined agent would silently receive task authority
  unless its author remembers to deny it. An authority default must not depend on
  each file opting out — and gating both built-ins the same way keeps one rule.
- **Reuse `owns_plan` for both.** Rejected: it conflates two independent
  authorities. The `plan` agent owns the plan but not the execution checklist, and
  `build` owns the checklist but not the plan — a single flag cannot express that.
- **Leave `update_tasks` unconditional (status quo of #140).** Rejected: that is
  precisely the hole #175 reports — a read-only agent mutating session state.

## References

- Issue #175: `update_tasks` is unconditional for every profile
- Epic #171: user config & permissions (parent)
- [ADR-0041](0041-update-plan-ownership-default-closed.md): update_plan ownership
  (the default-closed authority pattern this mirrors; its "stays unconditional"
  note for tasks is what this revises)
- [ADR-0038](0038-physical-per-agent-tool-restriction.md): physical per-agent tool
  restriction (the mask the built-ins bypass)
- [ADR-0039](0039-markdown-task-list.md): markdown task list (`update_tasks`, the
  built-in this now gates)
