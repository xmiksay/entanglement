# 0185. `rhai` joins `background`/`poll` — cooperative kill, own handle kind

- Status: Accepted
- Date: 2026-08-12
- Amends: [ADR-0161](0161-unified-async-work-background-flag-and-one-poll.md)
  (§5's explicit deferral of `background` on `rhai` — its revisit trigger fired)
- Relates to: [ADR-0164](0164-short-sortable-kind-tagged-ids.md) (new `x-` id
  kind), [ADR-0046](0046-rhai-sandboxed-script-tool.md) (the timeout regime it
  raises for the detached path)

## Context

ADR-0161 unified the four launchers on one rule — block by default, opt out
with `background: true`, join with `poll` — but §5 deliberately left `rhai`
out of `background` in v1, with a named revisit trigger ("a concrete need for
a long-running script", #637). The deferral was structural, not incidental:

- the engine runs under `tokio::task::spawn_blocking`, which **cannot be
  aborted** — the 30 s cap is enforced *inside* the engine by the
  `on_progress` callback;
- the only kill primitive is the cooperative `stop: Arc<AtomicBool>` that
  same callback polls, and it cannot reach a binding call already blocked in
  `exec`/`bash` — such a script stops only when that binding's own
  budget-clamped timeout fires.

Backgrounding `rhai` therefore means raising the 30 s cap *and* accepting a
detached task that is unkillable for the duration of an inner binding. That
trade is now made, explicitly.

## Decision

### 1. `rhai { script, timeout?, background? }` — the fourth launcher joins

`background: true` returns immediately with a handle; the script runs
detached with the same sandbox, the same binding bridge, and the same
permission grading as the blocking path. The **launch stays the graded
decision** (ADR-0161 §3): `rhai`'s own `Allow | Ask | Deny` gate runs inside
the live turn *before* the script detaches.

The blocking path is untouched: default 5 s, cap 30 s (ADR-0046). The
background path gets the `bash`/`call` background regime instead — **default
120 s, cap 600 s** (ADR-0165's bounded-background principle) — enforced by the
same in-engine `on_progress` deadline, since there is still no task to abort.

### 2. A background script is its own operation kind: `x-`

A new `IdKind::Script` mints `x-` handles (`x` for script e*x*ecution — `s`,
`r`, `j`, `o` are taken), dispatched by `poll` alongside `j-`/`o-`/agent
handles, and a new `OperationKind::Script` appears in
`ListOperations`/`OperationList` and `poll`'s no-handle listing.

It gets a dedicated `ScriptRegistry` rather than a task-backed `JobRegistry`
variant: a script is an in-process task, not an OS process — no process group
to SIGKILL, no exit code, no stdout/stderr pipes. What *is* shared is the
contract, deliberately mirrored from jobs (#605/#617/#621): owner-scoped
visibility (a wrong owner is indistinguishable from an unknown handle), a
`Notify`-woken wait ending on new output or finish, a destructive-delta drain
(`mem::take`), a capped buffer dropping the oldest bytes with the drop count
reported, and a lazy TTL/count eviction of finished entries.

`print` output **streams** into the registry as it happens; the terminal poll
carries the final `=> value` / `rhai error: …` line the blocking path would
have returned — the uniform result vocabulary of ADR-0161 §7, delivered
incrementally.

### 3. `kill` is cooperative, and says so

`poll { kill: true }` on an `x-` handle is **accepted** (unlike an agent
handle) but only trips the stop flag: the script terminates at its next engine
operation, so an in-flight `exec`/`bash` binding call runs to its own
budget-clamped timeout first — §5's documented limit, carried forward as the
accepted cost rather than engineered around. The kill poll returns immediately
with the buffered output and an explicit "cooperative stop requested — poll
again for the terminal state" notice; the *next* poll reports
`stopped (killed)`. A killed script still records a terminal state — nothing
else ever resolves the handle, so a poll must never see `running` forever.

### 4. Detached lifecycle: survives `Stop`, keeps mid-run `Ask`

- **A session `Stop` does not reach a background script** — the executor skips
  the canceller registration for a `background: true` launch, exactly as a
  background `bash`/`call` job survives `Stop`. The deadline and `poll`'s
  cooperative kill are the only ways it ends early.
- **Binding `Ask`s stay enabled detached.** The `ToolRequest` →
  `Approve`/`Reject` round-trip works without a live turn; the script parks on
  the reply while its deadline keeps counting (an unanswered `Ask` burns the
  budget, same as the blocking path). The one adjustment: a detached binding's
  approval round-trip **suppresses the session-state transitions**
  (`WaitingApproval`/`Thinking`) — there is no live turn for them to describe,
  and flipping an idle session's status would strand it stale; the
  `ToolRequest` event alone carries the prompt to the head.

## Consequences

### Positive

- ADR-0161's "one rule for all launchers" claim stops being aspirational —
  the §5 asymmetry (its own admitted wart) is gone, and the `background`/
  `poll` vocabulary is now literally uniform across `bash`/`call`/`agent`/
  `rhai`.
- Long multi-step scripts (the deferral's revisit trigger) no longer force a
  choice between the 30 s blocking cap and shelling out to `bash`.
- The blocking path's tight bound survives: nothing changes for the common
  case, and the raised regime is opt-in per call.

### Negative / neutral

- **A detached script is unkillable while inside a binding call.** Accepted
  and documented at every surface (`rhai` spec, `poll` spec, kill notice)
  rather than hidden. The binding's own budget-clamped timeout is the real
  bound.
- **An unanswered detached `Ask` holds the script (and its `spawn_blocking`
  thread) until the deadline.** The engine cannot observe wall-clock time
  while parked on the bridge, so the timeout fires at the next operation after
  the reply. A script that asks while nobody is watching effectively idles a
  blocking-pool thread for up to its budget.
- **A new id kind and a new `OperationKind` variant** — additive wire change
  to `OperationList` (a `kind: "script"` entry), plus a third registry the
  executor threads through (`spawn_tool_executor_with_policy` grows a
  parameter).
- The `x-` prefix spends another letter of the one-character namespace.

## Alternatives considered

- **A task-backed `JobRegistry` variant instead of a new registry** (`pgid:
  None`, stop flag in place of SIGKILL, prints into the stdout buffer). Less
  new machinery and `poll` dispatch untouched. Rejected: it grafts a second
  lifecycle onto a type whose every field (process group, exit code, two
  drained pipes, SIGKILL deadline task) describes an OS process, and every
  consumer would need to branch on which kind of "job" it holds. The two
  contracts are the same *shape*; the implementations are not the same thing.
- **Unbounded background scripts** (`timeout` ignored, like the original
  `run_in_background`). Rejected for the same reason ADR-0165 bounded
  background jobs — an immortal blocking-pool task is strictly worse than an
  immortal child process, since it can never be killed from outside.
- **Refuse `kill` on script handles** (like agents). Rejected: the cooperative
  stop exists and works for the common case (a runaway pure-Rhai loop);
  refusing it would leave the deadline as the only recourse for a script the
  model already knows it wants dead.
- **Deny mid-run `Ask`s in background mode** (auto-refuse an `Ask`-graded
  binding when detached). More predictable for unattended runs, but it forks
  the binding semantics in two modes and silently weakens a script that would
  have been approved interactively. The head still renders the prompt; kept.
