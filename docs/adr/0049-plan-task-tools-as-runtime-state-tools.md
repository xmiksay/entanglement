# 0049. `update_plan`/`update_tasks` as runtime state tools

- Status: Accepted
- Date: 2026-07-12
- Supersedes: [ADR-0041](0041-update-plan-ownership-default-closed.md) (in full);
  the core-side halves of [ADR-0004](0004-structured-plan-and-task-events.md) and
  [ADR-0039](0039-markdown-task-list.md) (plan/task state living on `Session`)

## Context

`update_plan` and `update_tasks` were **core built-ins**: executed inside
`entanglement-core/src/session/tools.rs`, stored on `Session.plan`/`Session.tasks`,
gated by authority flags (`owns_plan`) on `AgentProfile`, folded back in
`session/replay.rs`, and set from the head via `InMsg::SetPlan`/`InMsg::SetTasks`.

But core's reasoning loop **never consumes** either value — the `update_tasks`
schema said so explicitly (*"it is not fed back to you"*), and the same holds for
the plan. Core stored two opaque strings, re-emitted them as `OutEvent::Plan`/
`OutEvent::TaskList`, and the head rendered them. That is **display state, not
engine state**.

The [ADR-0006](0006-core-dependency-hygiene-gate.md) hygiene gate enforces the
*cargo dependency* boundary but cannot catch this **semantic** leak: `Session.plan`,
`Session.tasks`, `owns_plan`, `PLAN_TOOL`, `TASKS_TOOL`, `InMsg::SetPlan`,
`InMsg::SetTasks`, and the `emit_plan`/`emit_tasks` helpers were all runtime
concerns living in core. This contradicts the invariants of
[ADR-0006](0006-core-dependency-hygiene-gate.md)/#58 (*"core holds no executable
tools"*) and #59 (*"permission lives entirely in the runtime"*).

ADR-0041's `owns_plan` gate — enforced *in core* because the built-ins never
round-tripped to the runtime — deepened the leak, and #175 (a read-only agent
mutating **task** state) stayed open because `update_tasks` was exempted from the
[ADR-0038](0038-physical-per-agent-tool-restriction.md) tool mask entirely.

## Decision

Move `update_plan` and `update_tasks` into the runtime as ordinary state tools
that round-trip via `ToolExec`/`ToolResult` like every other host tool
([ADR-0010](0010-single-head-crate-and-bash-opt-in.md)/#58).

### Core loses all plan/task knowledge

`Session` drops `plan`/`tasks`; `AgentProfile` drops `owns_plan`; `InMsg` drops
`SetPlan`/`SetTasks`; `session/tools.rs` drops the built-in branches (every tool
now takes the #58 round-trip); `session/replay.rs` stops folding `Plan`/`TaskList`.
`OutEvent::Plan`/`OutEvent::TaskList` **stay** in the protocol — they are the
display channel — but are emitted by the runtime, not core.

### Runtime owns execution, gated by the ordinary permission path

`entanglement-runtime/src/plan_tasks.rs` holds the two specs and the parse/emit
logic. `update_tasks` rides the shared `EngineConfig.tool_specs`; `update_plan`
rides per-profile `profile_tool_specs`. The tool executor (`tool_runner`) does
**not** special-case them for permission: they fall through to the generic
`dispatch`, which resolves `Allow`/`Ask`/`Deny` via `effective_permission` and the
#116 tool mask like any host tool. The only branch is in `run_and_reply`: a state
tool emits its `Plan`/`TaskList` snapshot (reusing the `ToolExec` seq) and acks the
model, instead of dispatching to the host `ToolRegistry` (they touch no host
resource, so they are not registry tools).

This closes **#175**: a read-only profile (`explore`) has `update_tasks` outside
its allowlist (mask refusal) *and* its permission denies it — mutation is refused
before any snapshot is emitted.

### Plan authorship: default-closed via explicit allowlist membership

`owns_plan` is replaced by the tool mask itself. `update_plan` and `propose_plan`
are advertised (in `profile_tool_specs`) only to a profile that **explicitly**
allowlists them (`tools:` names them; an inherit-all `tools: None` profile does
**not** count). So plan authority stays default-closed — a new inherit-all agent
never gains it by accident — with **zero** dedicated flag. `plan.md` gains
`update_plan` in its allowlist and `update_plan: allow` in its permission (so
authoring is not an approval prompt).

### Persistence / resume

The head folds `OutEvent::Plan`/`OutEvent::TaskList` from the persisted log into
its own view state (e.g. the TUI `SessionView.plan`/`task_list`) — the plan/task
reconstruction that used to live in core's `Session::replay` is now the runtime
head's, matching where the display state belongs.

### `propose_plan`

On approve it now only acks the model (`InMsg::SetPlan` is gone); the working plan
was already surfaced by the agent's `update_plan` snapshots, and the head performs
the fresh-`build`-session handoff from the tool input ([ADR-0042](0042-plan-acceptance-via-propose-plan-approval-roundtrip.md),
unchanged). Its advertisement gate moves from `owns_plan` to the same explicit
allowlist membership.

## Consequences

### Positive

- `entanglement-core` holds no plan/task state, tools, authority flag, or
  messages — the semantic leak ADR-0006's cargo gate couldn't catch is closed.
- `update_plan`/`update_tasks` are gated by the *one* permission path, so #175 is
  fixed structurally (mask + permission), not by a core-only special case.
- Plan authorship stays default-closed with no dedicated flag: authority is a
  property of the tool mask, consistent with #116/#119.

### Negative / neutral

- The runtime emits the `Plan`/`TaskList` snapshot reusing the `ToolExec` seq (it
  has no handle on core's per-session counter). This is monotonic and head-safe on
  the `Allow` path; state tools are therefore expected to resolve to `Allow` where
  advertised (a head dedupes an `Ask`-path snapshot against the preceding
  `ToolRequest` at the same seq). The built-in profiles keep them `Allow`.
- Plan authorship is now coupled to the `tools:` allowlist rather than a distinct
  field — an opt-in gained by naming `update_plan`, which reads as a physical
  mask entry rather than a separate authority concept.

## Alternatives considered

- **Keep `owns_plan` as a runtime-only `AgentDefinition` field.** Rejected:
  `profile_tool_specs` is built by iterating the core `AgentProfile` (which no
  longer carries the flag), and enforcement of a hallucinated call would need a
  side registry of plan-owner names. Explicit allowlist membership needs neither.
- **Intercept the state tools in `tool_runner` like `propose_plan`/`ask_user`.**
  Rejected: those force a bespoke approval path; the issue asks for *no special
  casing* on permission. Falling through to `dispatch` with a one-line
  `run_and_reply` branch keeps the ordinary `Allow`/`Ask`/`Deny` path.
- **Register them as real `ToolRegistry` tools with an emit callback.** Rejected:
  the registry `Tool::execute` returns only a `String` and has no `session`/`seq`/
  event-sender handle; threading the snapshot out would be more plumbing than the
  `run_and_reply` branch.
- **`disallowed_tools: [update_plan]` on build/explore.** Rejected for the same
  reason ADR-0041 rejected it: an authority default must not depend on every
  future inherit-all agent remembering to opt *out*.

## References

- Issue #231: migrate `update_plan`/`update_tasks` from core built-ins to runtime
  host tools
- Issue #175: read-only agent must not mutate task/plan state (closed here)
- [ADR-0041](0041-update-plan-ownership-default-closed.md): superseded — `owns_plan`
  default-closed plan authority (the flag this removes)
- [ADR-0004](0004-structured-plan-and-task-events.md),
  [ADR-0039](0039-markdown-task-list.md): the `Plan`/`TaskList` event shape (kept)
  and the plan/task-on-`Session` storage (superseded)
- [ADR-0010](0010-single-head-crate-and-bash-opt-in.md)/#58: tool execution is a
  runtime round-trip (the seam this reuses)
- [ADR-0038](0038-physical-per-agent-tool-restriction.md): the tool mask that now
  carries plan authorship
- [ADR-0042](0042-plan-acceptance-via-propose-plan-approval-roundtrip.md): the
  `propose_plan` accept flow (handoff unchanged; `SetPlan` recording removed)
