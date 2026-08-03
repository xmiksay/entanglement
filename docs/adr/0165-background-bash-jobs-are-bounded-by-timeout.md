# 0165. Background `bash` jobs are bounded by `timeout`, not exempt from it

- Status: Accepted
- Date: 2026-08-03
- Related: [ADR-0161](0161-unified-async-work-background-flag-and-one-poll.md)
  (documents the pre-existing gap as part of a larger future unification —
  this ADR closes the gap now, on the current `run_in_background` surface,
  without waiting on that larger rework)

## Context

`bash`'s schema documented `timeout` as "Ignored when run_in_background=true"
— a deliberate, stated gap, not a silent bug. The consequence: a backgrounded
job had no deadline at all. A runaway `npm run dev` or a wedged build ran
until the engine process exited or a human remembered to poll `bash_output`
with `kill: true`. Nothing in the tool surface could bound it up front (#617).

`background` is becoming a first-class flag across every launcher
(ADR-0161), and the natural reading of `bash { command, timeout, background:
true }` is that the timeout applies to the call, however it returns. "Backgrounded
⇒ unbounded" stops being defensible once backgrounding is the normal way to
start any long-running tool call rather than `bash`'s own special case.

## Decision

**`JobRegistry::spawn` takes the same `timeout: Duration` a foreground `bash`
call already computes** (`min(timeout.unwrap_or(120), 600)`,
`entanglement-runtime/src/host/bash.rs`) and starts a deadline task alongside
the existing output-drain and reaper tasks. If the job is still running when
the deadline elapses, the task SIGKILLs its process group — the same
`kill_process_group` path an explicit `bash_output { kill: true }` poll
already uses — and marks the job `timed_out`. The reaper then observes the
child exit as normal and flips `finished`, so the terminal status is
`Exited(None)`, identical to a user-initiated kill.

**`timed_out` is tracked separately from `Exited(None)`** so a poll can tell
the two apart in its message (`[killed: timed out after Ns]`) instead of
leaving the model to guess whether the process died on its own, was killed by
timeout, or was killed by an explicit `kill: true`. The distinction matters:
"the process exited" and "the engine killed it because it ran too long" call
for different next actions from the model.

**No default changed** — `timeout` keeps its 120 s default and 600 s cap for
background jobs exactly as for foreground calls. A job that must outlive the
default now needs an explicit larger `timeout`, capped at 600 s like every
other `bash` call. There is no unbounded background option; a job that must
run longer than 600 s needs to be started, polled, and — if still useful —
restarted, or (once ADR-0161 lands) reached through whatever longer-lived
primitive that unification settles on.

## Consequences

- **(+)** A background job can no longer outlive the engine unbounded — the
  gap #617 named is closed on the tool surface as it exists today, without
  waiting on ADR-0161's larger `background`-flag-family rework.
- **(+)** The kill path is the one `JobRegistry` already had
  (`kill_process_group` on the job's `pgid`) — no new termination mechanism,
  just a second caller of the existing one.
- **(+)** `bash_output`'s poll message names the cause (`timed out after Ns`)
  instead of leaving a `killed`-without-explanation status for the model to
  puzzle over.
- **(−)** A long-lived background job (a dev server meant to run for the rest
  of the session) now needs its `timeout` set explicitly up to the 600 s cap,
  and anything meant to outlive that still has no supported path other than
  restarting it — background did not, and still does not, mean "runs
  forever." This is accepted as the correct reading of the issue: unbounded
  was the bug, not a feature to preserve a workaround for.

## Alternatives considered

- **Wait for ADR-0161's `background` rename and build the timeout into that
  rework.** Rejected for the immediate fix: ADR-0161 is a substantially larger
  change (a new `poll` tool, handle namespace, session-scoped ownership) that
  the maintainers have not committed to a timeline for, and the schema's own
  documented gap ("timeout ignored") is a live bug independent of that
  larger unification. Fixing it now on `run_in_background` does not
  foreclose ADR-0161; the same deadline task carries over unchanged when the
  field is later renamed to `background`.
- **A separate, longer default timeout for background jobs than foreground
  calls.** Rejected: it reintroduces a second bound to reason about (which
  default applies when?) for no clear benefit — a caller that wants longer
  already has the `timeout` argument, capped at the same 600 s every other
  `bash` call respects.
- **Leave the terminal status as bare `Exited(None)` and let the model infer
  timeout from elapsed time.** Rejected: the registry already knows why it
  killed the process; making the model reconstruct that from timing is
  strictly worse than reporting it directly, for the cost of one boolean.

## References

- Issue #617: background bash jobs cannot be time-bounded (part of #604)
- [ADR-0161](0161-unified-async-work-background-flag-and-one-poll.md): the
  larger `background`-flag-family unification this issue is filed under; its
  context table names this exact gap
- `entanglement-runtime/src/host/jobs.rs`: `JobRegistry::spawn`/`poll`, the
  existing `kill_process_group` path this reuses
