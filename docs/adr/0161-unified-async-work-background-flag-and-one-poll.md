# 0161. Unified async work: a `background` flag on every launcher, one `poll` to join

- Status: Accepted
- Date: 2026-08-03
- Supersedes: the launch/join *tool split* of [ADR-0026](0026-async-subagent-spawn-and-poll.md)
  and the separate-tools rationale of [ADR-0033](0033-agent-tool-family-and-blocking-agent.md)
- Amends: [ADR-0123](0123-agent-poll-zero-timeout-waits-for-notification.md) (the `timeout_secs: 0`
  sentinel), [ADR-0045](0045-call-host-tool-argv-exec-tailed-output.md) and
  [ADR-0008](0008-host-tools-workdir-and-bounded-output.md) (the per-tool output bounds converge)

## Context

Four host tools start work that can outlive a single tool call, and each invented its own waiting
rule independently:

| launcher | how you wait | blocks? | bound |
| --- | --- | --- | --- |
| `call` | nothing — always blocks | yes | 120 s default, 600 s cap |
| `rhai` | nothing — always blocks | yes | 5 s default, **30 s cap** |
| `bash` | nothing — always blocks | yes | 120 s default, 600 s cap |
| `bash` `run_in_background=true` | `bash_output { job_id, kill }` | **no** | unbounded (`timeout` documented as ignored) |
| `agent` | blocks internally, handle not usable to wait | yes | unbounded |
| `agent_spawn` | `agent_poll { agent_id, timeout_secs }` | yes | 60 s default, 600 s cap, `0` = unbounded |

Five waiting rules across six tools, and the two *join* tools are the same concept implemented
twice, disagreeing on everything that matters:

- **`bash_output` cannot wait at all.** `JobRegistry::poll`
  (`entanglement-runtime/src/host/jobs.rs`) is a *synchronous* drain: lock, optionally SIGKILL,
  `mem::take` the buffers, return. No timeout parameter, no notification, no blocking path. A poll
  issued 1 ms after spawn returns `(no new output)`, so the model busy-waits across turns and
  **every spin costs a full LLM round-trip**. `agent_poll` solved the identical problem the opposite
  way — a `watch`-channel wait with a `timeout_secs` knob (ADR-0026/ADR-0123).
- **Argument names differ** (`job_id` vs `agent_id`), so nothing signals they are one family.
- **Permission status differs.** `agent_poll` is runtime-owned and intercepted *before* permission
  resolution; `bash_output` is an ordinary registry `Tool` and *is* graded — and having no command
  argument to match, it degrades to `Ask` under a narrowed `bash(pattern): allow` grade
  ([ADR-0133](0133-live-bash-enablement-graded-by-permission.md)).
- **Error conventions differ.** `bash_output` returns an unknown id as ordinary *text*;
  `agent_poll` returns an *error*.

The mismatch has already produced a live bug: the embedded `explore` profile's mask is
`tools: [read, glob, grep, call, bash, rhai]` — it can **start** a background job and has no
`bash_output` to ever read it.

Underneath, all four launchers return the same thing: `anyhow::Result<String>` → `text_parts` →
`Vec<ContentPart>` (`entanglement-runtime/src/tools.rs`). What actually differs between them is
*when* the text is ready, not its shape. The surface encodes that accidental difference as six
tools; it should encode it as one flag.

## Decision

### 1. One rule for starting work

Every launcher blocks by default and takes an optional `background: bool`. `true` returns a handle
immediately instead of the result.

| today | after |
| --- | --- |
| `bash { command, timeout?, workdir?, run_in_background? }` | `bash { command, timeout?, workdir?, tail?, background? }` |
| `call { command, args?, tail?, timeout?, input_file?, output_file?, workdir? }` | `call { …, background? }` |
| `rhai { script, timeout? }` | `rhai { script, timeout? }` — **no `background` in v1**, see §5 |
| `agent { agent, prompt }` + `agent_spawn { agent, prompt }` | `agent { agent, prompt, background? }` |

`agent_spawn` is **removed**; `agent { background: true }` replaces it. `bash`'s
`run_in_background` is renamed to `background` for symmetry.

### 2. One tool for joining

`poll { handle?, timeout_secs?, kill?, offset?, tail? }` replaces both `bash_output` and
`agent_poll` outright (the handle is optional — see §6; `offset`/`tail` page a retained result —
see §7). No
aliases — ADR-0033 already established that tool names are opaque strings in the wire protocol and
a clean rename costs nothing for session logs (but see §7 for what it *does* cost).

- **Waits** up to `timeout_secs` (default 60, cap 600, `0` = wait until terminal). Agent handles
  keep today's `watch`-channel wait. Job handles require adding a `tokio::sync::Notify` to
  `JobRegistry`, woken by the drain tasks and the reaper, so the wait ends on *new output or exit* —
  whichever comes first. This is the substantive new machinery in this ADR.
- **Returns** a status (`running` / `complete`) plus text. The delta-vs-final distinction rides the
  **status**, not the tool name: a job poll returns the incremental delta since the last poll (still
  destructive, still `mem::take`); an agent poll returns the final answer and is idempotent.
- **`kill: true`** SIGKILLs a job's process group, exactly as `bash_output` does today. On an
  **agent** handle it is refused: cancelling a child is a distinct authorization gate that ADR-0033
  explicitly deferred, and this ADR does not open it.
- **Unknown handle** is an *error*, adopting `agent_poll`'s convention over `bash_output`'s
  return-it-as-text. A poll for a handle that does not exist is a model mistake, not a state report.

### 3. `poll` bypasses permission; the launch stays the graded decision

`poll` is runtime-owned and intercepted before permission resolution, matching `agent_poll` today
rather than `bash_output`. ADR-0026's justification generalizes: `poll` starts nothing and touches
no host resource — it reads state that a *previously graded* launch produced. Grading the read as
well as the write buys no isolation and produces the two warts above (the `explore` bug and the
ADR-0133 `Ask` degradation), both of which this dissolves.

This is stated explicitly rather than inherited silently, because it does mean background-job output
is readable without a second permission decision.

### 4. Handles are one namespace, supplied by ADR-0164

`poll` dispatches on the handle's kind prefix (`s-` session/agent, `j-` job) from
[ADR-0164](0164-short-sortable-kind-tagged-ids.md). That ADR is a **prerequisite**, not a
companion: today's `bg-N` job ids are minted from a per-registry `AtomicU64` that restarts at 0
whenever a fresh `JobRegistry` is built, so they are not even unique within a session. A shared
`poll` cannot safely dispatch on ids that repeat.

`poll` and `agent_send` ([ADR-0162](0162-agent-send-supervising-a-sub-agent.md)) both verify the
handle belongs to the caller's own subtree via `parent_links`/`SpawnGuard`. Today's `AgentRegistry`
is engine-wide and unscoped — its "it was never launched from this session" message is simply false,
since any session can poll any handle whose id it knows. The descendant check replaces that honour
system.

### 5. `rhai` gets the uniform *output* treatment but **not** `background` in v1

`rhai` is the fourth launcher and belongs in the family, but backgrounding it is materially harder
than backgrounding a subprocess, and the reason is structural: the engine runs under
`tokio::task::spawn_blocking`, which **cannot be aborted**, so its 30 s cap is enforced *inside* the
engine by an `on_progress` callback rather than by a task abort. A cooperative kill primitive does
exist (`stop: Arc<AtomicBool>` checked from `on_progress`), but the code documents its limit: the
interrupt cannot reach into a binding call already blocked in `exec`/`bash`, so such a script only
stops when that binding's own budget-clamped timeout fires.

Backgrounding `rhai` therefore means raising the 30 s cap *and* accepting a detached task that is
unkillable for the duration of an inner binding. That trade may well be worth making later, but it
is a different decision from the one this ADR is making, and bundling it would let the weakest case
set the terms for the other three. `rhai` adopts the uniform result shape (§6) now; `background` is
deferred with an explicit revisit trigger: a concrete need for a long-running script.

### 6. Pending operations are listable per session

A handle is only useful while the model still has it. ADR-0026 named the failure and left it open —
"a launched-but-never-polled child runs to completion unobserved" — and backgrounding three more
tool kinds makes it worse: a handle lost to a compaction, a new turn, or a resumed session is work
that is still running, still consuming budget, and no longer reachable by anything.

So the outstanding set becomes queryable, on both seams:

- **For the model:** `poll`'s `handle` is **optional**. Called without one, it returns this
  session's pending operations — kind, handle, what launched it, elapsed, status — instead of
  joining a single one. With a handle it answers "how is *this* going"; without, "what do I still
  have running." Same question at two scopes, which is why it is the same tool rather than a sixth.
- **For heads:** `InMsg::ListOperations { correlation_id }` → `OutEvent::OperationList`, following
  the existing correlation-id query pattern of `ListSessions`/`ListQuestions`
  ([ADR-0072](0072-protocol-warts-settled-before-serve.md),
  [ADR-0146](0146-ask-user-list-retract-replace.md)). Wire-allowed: it is a read of the caller's own
  outstanding work, mutating nothing.

This requires attributing every operation to the session that launched it. Jobs carry no session
today, and `AgentRegistry` is engine-wide and unscoped. But that attribution is the *same*
bookkeeping the descendant check in §4 needs, and the seam already exists —
[ADR-0088](0088-session-aware-tool-execution.md) threads `SessionId` through
`Tool::run_for_session`, so a backgrounding `bash` already knows who called it. Ownership tracking
is therefore one mechanism serving both requirements, not a second one.

**Lifetime is deliberately not uniform across kinds**, and the list must say so rather than imply
otherwise: agent handles survive hibernation and resume with the session
([ADR-0112](0112-resume-cascades-over-the-spawn-subtree.md)), whereas background jobs are OS
processes owned by this engine process and do not outlive it. A resumed session's list can therefore
legitimately show agents and no jobs; that is correct, not a loss, and the rendering should make the
distinction visible instead of silently dropping entries.

### 7. One result shape, and `poll` serves the remainder — no temp files

All four launchers produce text, so the bounding becomes uniform rather than per-tool: a status
line, then a tailed/capped body. `bash` gains the `tail` knob `call` already has.

**A truncated result must stay fully reachable**, or the cap trades one failure (context blow-up)
for a worse one (silently discarded work). The existing answer to that is `call`'s: persist the full
output to a scratch file outside the project, name the absolute path in the result, and let the
model `read` it back. That works, but it is a lot of apparatus — a scratch directory, per-call
artifact naming, a `.stderr` sibling, a degraded-write notice, an absolute path spent in context,
and files nothing ever cleans up.

**`poll` replaces all of it.** The operation registry already holds the operation; it can hold its
output too, and `poll { handle, offset?, tail? }` pages the retained text. The remainder is fetched
with the same verb already used to join the operation — no filesystem round-trip, no path in
context, no artifact to garbage-collect.

`tail` **defaults to 30 lines**, the same default `call` already uses and for the same reason:
command output concentrates its value at the end, and an unbounded page would reintroduce the
context blow-up the cap exists to prevent. `tail: 0` returns the full page, still bounded by the
byte cap. Paging is therefore explicit at every step — the model asks for exactly as much as it
wants, one page at a time, instead of being handed everything or a file path.

**When an operation *does* have an output file, `poll` names it.** An explicit `call` `output_file`
is a real artifact the user asked for, and the operation knows its path; reporting it in the poll
result means the model never has to reconstruct or remember it. This is the one place a path still
appears — because there genuinely is a file, not because truncation manufactured one.

This requires one thing that falls out naturally: **an operation whose output was cut keeps its
handle even if it never went to background.** A blocking `call` that overflows the cap returns its
tail *and* a handle, and the handle is how the rest is reached. So the rule generalizes from
"backgrounded work has a handle" to "**work you might still have questions about has a handle**",
which covers both cases with one concept.

Retention is the cost: the full text lives in the registry rather than on disk, so it needs a
per-operation size cap and eviction (§6's operation lifetime already implies both). Beyond that cap
the output is genuinely gone — which is why the cap must be generous enough that the tail plus the
pageable remainder covers real build logs, and why exceeding it must say so explicitly rather than
truncate silently.

`call`'s **explicit** `output_file` is unaffected and stays: that is a user asking for a file at a
chosen path, not a truncation workaround. Only the implicit scratch artifact goes away.

This also closes the largest hole in the bounded-output story: sub-agent answers (`subagent.rs`,
`agent_poll.rs`) are folded into the parent's context today with **no truncation at all**, so a
verbose child can inject an unbounded string into its parent. Under this rule a long answer is
tailed, keeps its handle, and is paged on demand.

## Consequences

### Positive

- **One waiting concept instead of five.** Launchers block; `background` opts out; `poll` waits.
- **The busy-wait is gone.** A `poll` with a real timeout collapses N model round-trips into one
  wait. A five-minute background build currently costs a full turn per check.
- **Three live bugs die as side effects**: `explore` can read its own background jobs; the ADR-0133
  `bash_output`→`Ask` degradation disappears (no `bash_output` to degrade); unbounded sub-agent
  answers get capped *and* stay recoverable.
- **ADR-0123's `0` sentinel stops being a workaround.** It reclaimed a dead value because widening
  the schema was expensive; a fresh `poll` says "wait until done" honestly and keeps `0` meaning the
  same thing, now by design rather than by salvage.
- **Backgrounded work stops being losable.** §6 closes the gap ADR-0026 opened and left open: a
  handle dropped from context no longer strands a running job or child, because the session can ask
  what it still has outstanding.
- **The scratch-artifact apparatus disappears.** §7 removes a scratch directory, per-call artifact
  naming, `.stderr` siblings, degraded-write notices, an absolute path spent in every truncated
  result, and a class of files nothing ever cleaned up — replaced by paging the registry the tool
  already needs. This ADR is one of the few that *removes* moving parts while adding capability.
- **Smaller advertised surface** (six tools → five) while *adding* capability, and the shared
  `background` rule means each description gets shorter, because the rule is stated once and learned
  once rather than re-explained per tool.

### Negative / neutral

- **This reverses ADR-0033's central argument.** That ADR rejected a `blocking:` flag on
  `agent_spawn` precisely because "a boolean that flips the return type forces a vaguer, do-both
  tool description." That reasoning was correct *for one tool in isolation*. It stops holding when
  the same flag governs four tools: the cost of a flag is paid once and amortized across the family,
  whereas the cost of separate launch/join tool pairs is paid per tool and multiplies. The return
  type still varies with the flag — that much of ADR-0033's objection stands and is simply accepted.
- **One tool, three return shapes.** A job poll is a destructive delta; an agent poll is an
  idempotent final answer; a handle-less poll is a list. Riding the first two on a status field
  keeps them sharp, and the third is distinguished by an absent argument rather than a mode flag —
  but this is exactly where a merged tool can go vague, and the schema text needs care. It is the
  strongest remaining form of ADR-0033's objection, and it is accepted rather than answered.
- **`ListOperations`/`OperationList` are new wire types**, so this ADR is not wire-neutral. Taken
  together with [ADR-0163](0163-live-bash-enablement-is-a-tool-overlay-entry.md)'s removals
  (`BashEnable`, `BashDisable`, `BashChanged`, `BashGrade`) the protocol still shrinks on net — one
  fewer `InMsg` variant, no change in `OutEvent` count, one fewer enum — but the two ADRs have to be
  read together for that to be true.
- **Per-session ownership tracking is new bookkeeping.** Jobs carry no session today. It is shared
  with the descendant check rather than additional to it, but it is not free.
- **Retained output moves from disk to memory.** `call`'s artifact was disk-backed and effectively
  unbounded; a registry-held remainder needs a size cap and eviction, and past that cap the text is
  genuinely unrecoverable where a file would still have held it. This is a real regression in the
  extreme case, accepted because the extreme case (a single tool call emitting more than the cap)
  is one the model cannot usefully consume anyway — provided the overflow is reported, not silent.
- **Truncation now mints handles.** A blocking `call` that overflows returns a handle it previously
  had no reason to have, so the operation registry accumulates entries from ordinary foreground
  calls, not just backgrounded ones. Eviction pressure goes up accordingly.
- **`JobRegistry` gains a notification path** it never had. This is real work, easy to
  under-estimate from the outside: today there is no wakeup mechanism of any kind to extend.
- **`rhai` stays asymmetric** until its own backgrounding decision is made — the one place the "one
  rule for all launchers" claim is currently aspirational rather than true.
- **Config churn.** Removing `bash_output`/`agent_poll`/`agent_spawn` invalidates any tool mask,
  permission rule or persisted grant naming them — including the embedded `plan.md` mask
  (`tools: [read, glob, grep, agent, agent_spawn, agent_poll, …]`), user-layer overrides
  ([ADR-0083](0083-in-app-tool-allowlist-editing-as-user-layer-materialization.md)) and grant files.
  ADR-0033's "tool names are opaque, renames are free" holds for *session logs*, which replay fine,
  but **not** for these. A migration note plus a startup warning on unknown mask entries is part of
  the change, not a follow-up.

## Alternatives considered

- **Keep both poll tools, just give `bash_output` a timeout.** The smallest change, and it fixes the
  busy-wait. Rejected because it leaves the two tools diverging on argument name, permission status
  and error convention — the thing that produced the `explore` bug — and does nothing about the five
  waiting rules the model must hold in its head.
- **Always return a handle; never block.** Perfectly uniform and the simplest rule to state.
  Rejected: a 200 ms command would cost two round-trips, and the overwhelming majority of calls are
  short. Blocking-by-default matches actual usage; backgrounding is the exception and should be the
  one that costs a keystroke.
- **Keep `agent_spawn`/`agent` split and add `background` only to the exec tools.** Preserves
  ADR-0033 intact. Rejected: it re-introduces the per-tool waiting rule this ADR exists to remove,
  and leaves the family with both a flag and a tool split expressing the same thing.
- **Give `rhai` `background` now.** Rejected for v1 per §5 — the unkillable-inner-binding case would
  set the terms for the whole family.
- **A separate `operations` tool for the pending list.** Keeps `poll`'s description to one job.
  Rejected: it is the same question ("what is the state of my outstanding work?") at a different
  scope, and a sixth tool to answer it would undo the surface reduction this ADR exists to make.
  An absent argument is a cheaper distinction than a new name.
- **Emit the pending list automatically on every `poll` result** instead of a query mode. Rejected:
  it does not help the case that matters — a model with *no* handle at all, which is exactly the
  post-compaction state — and it taxes every ordinary poll with output nobody asked for.
- **Keep `call`'s scratch artifact and let the model `read` the remainder.** The status quo, and it
  needs no retention policy because the filesystem is the retention policy. Rejected: it spends an
  absolute path in context on every truncated result, routes a tool's own output back through the
  permission-graded file tools, writes files outside the project that nothing ever removes, and
  makes each launcher re-implement the same artifact plumbing. Paging the registry is the same
  capability with none of that.
- **A general parallel tool execution model in core** (dispatch independent tool calls in a turn
  concurrently). This is the more powerful answer and benefits every tool, and ADR-0026 already
  deferred it once. Still deferred: it is a much larger change to the core turn loop, protocol and
  stash discipline ([ADR-0018](0018-turn-loop-stash-discipline.md)), and it does not by itself
  unify the *waiting* vocabulary, which is what the model actually trips over.

## References

- [ADR-0026](0026-async-subagent-spawn-and-poll.md): non-blocking spawn + `agent_poll` (the
  launch/join tool split this supersedes; its handle-table and no-gating reasoning is kept)
- [ADR-0033](0033-agent-tool-family-and-blocking-agent.md): `agent_*` family + blocking `agent`
  (the separate-tools rationale this supersedes)
- [ADR-0123](0123-agent-poll-zero-timeout-waits-for-notification.md): the `0` sentinel, carried
  forward by design rather than by salvage
- [ADR-0133](0133-live-bash-enablement-graded-by-permission.md): the `bash_output`→`Ask`
  degradation this dissolves; itself superseded by
  [ADR-0163](0163-live-bash-enablement-is-a-tool-overlay-entry.md)
- [ADR-0164](0164-short-sortable-kind-tagged-ids.md): the handle namespace `poll` dispatches on —
  a prerequisite
- [ADR-0162](0162-agent-send-supervising-a-sub-agent.md): shares the descendant check and the
  `background` flag
- [ADR-0008](0008-host-tools-workdir-and-bounded-output.md),
  [ADR-0045](0045-call-host-tool-argv-exec-tailed-output.md): the two output-bound regimes this
  converges
- [ADR-0016](0016-host-tool-empty-result-contract.md): the empty/truncated-result contract the
  artifact rule extends
- [ADR-0072](0072-protocol-warts-settled-before-serve.md),
  [ADR-0146](0146-ask-user-list-retract-replace.md): the correlation-id query pattern
  `ListOperations` follows
- [ADR-0088](0088-session-aware-tool-execution.md): the `SessionId`-in-`run_for_session` seam that
  makes per-session operation ownership trackable
