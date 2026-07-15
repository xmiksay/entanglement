# 0070. Authoritative `ToolExec` profile, fail-closed permission/mask fallback, lag-proof decision delivery

- Status: Accepted
- Date: 2026-07-15
- Fixes the default-open permission/mask fallback (#156) in the runtime tool executor of [0006](0006-core-dependency-hygiene-gate.md)/[0059](0059-tool-trait-and-registry-live-in-the-runtime.md); hardens the sub-agent gating of [0024](0024-subagent-privilege-ceiling.md)/[0038](0038-physical-tool-restriction.md) and the shared-counter seam of [0068](0068-shared-per-session-seq-counter.md). Part of #153.

## Context

The runtime tool executor (`tool_runner`) resolves every tool call's
`Allow | Ask | Deny` and its physical tool mask (#116) against a per-session
`AgentProfile` map, `active`. That map was folded **only** from the lossy
`broadcast<OutEvent>` lifecycle stream — `SessionStarted` / `AgentChanged`. Three
defects compounded into a security inversion under overload:

- **Default-open fallback.** A session absent from `active` resolved to
  `Permission::Allow` (`permission_for`) and to *unmasked* (`tool_masked` returned
  `false`). The broadcast has a bounded capacity; under burst a lagging executor
  drops the oldest frames. Drop a read-only `explore` session's `SessionStarted`
  and that session ran **allow-all / unmasked** — the security posture *inverted*
  exactly when the system was under load.
- **Lagged decision silently dropped.** Each parked approval (`tool_runner`,
  `ask_user`, `propose_plan`, each `rhai` binding) held its *own* `broadcast`
  subscription of the inbound fan-out and filtered it to `(session, request_id)`.
  On `RecvError::Lagged(_)` it silently continued (`=> {}`), so a lagged
  `Approve`/`Reject`/`AnswerQuestion` was gone: the request parked **forever**
  while the user believed they had answered.

The root cause is the same in both: **safety-critical state fed by a lossy
channel, with a fail-*open* fallback.**

## Decision

**Make the executor's per-call gating authoritative, fail closed for the residual
unknown, and deliver decisions over a lag-proof registry.**

- **Carry the profile on `ToolExec`.** `OutEvent::ToolExec` gains an `agent:
  String` — the emitting session's active profile name, set by core from
  `s.profile.name` at emit (and re-offer on resume). `#[serde(default,
  skip_serializing_if = "String::is_empty")]` keeps pre-#156 logs deserializable
  and the common wire additive. On `ToolExec` the executor **self-heals**
  `active[session]` from that name (resolved against the startup
  `ProfileRegistry`) *before* any mask/permission decision. Because a `ToolExec`
  is precisely the event that triggers a decision — and a dropped `ToolExec` needs
  no decision — the leaf's gate is authoritative regardless of a dropped
  `SessionStarted`/`AgentChanged`. Ancestors self-heal the same way: each ran its
  own `agent`/`agent_spawn` `ToolExec` to create the chain.
- **Flip the fallback to fail-closed.** A still-unseen session — an unresolved
  `agent` name, or an ancestor whose spawn `ToolExec` was itself dropped — now
  resolves to `Permission::Deny` (`permission_for`) and to **masked**
  (`tool_masked` treats an unseen link as masking everything). Degraded but safe:
  the turn takes denial `ToolResult`s instead of running allow-all.
- **Lag-proof decision delivery.** A parked approval registers a `oneshot` in a
  shared `PendingDecisions` map keyed by `(session, request_id)` **before**
  emitting its request event, and awaits it. A single light **inbound router**
  (the executor's existing `Stop`/`user_prompt_submit` watcher, now the *sole*
  inbound consumer for decisions) fans each `Approve`/`Reject`/`AnswerQuestion` to
  its waiter and unwinds a session's waiters on `Stop`. A map-lookup-per-frame
  loop drains far faster than a park loop that also competes with tool execution,
  so the realistic lag window that dropped decisions is closed. A dropped sender
  (`Stop`, superseding re-register, executor shutdown) resolves the waiter to
  `Decision::Stop` — preserving the ADR-0017 "inbox closed ⇒ unwind silently".

## Consequences

- **Positive.** The security posture no longer inverts under overload: a dropped
  lifecycle frame can only make a session *more* restricted (fail-closed), never
  allow-all. The leaf's gate is authoritative from the `ToolExec` itself. A lagged
  approval reaches its waiter instead of parking forever. Decision-delivery logic
  is centralized in one router + one registry instead of four per-task
  subscriptions.
- **Neutral.** `ToolExec` carries one extra string (empty-skipped on the wire).
  Orchestrators shed their per-task inbound subscriptions for a `PendingDecisions`
  clone.
- **Negative.** If the *single* router lags (a burst exceeding the inbound
  capacity through one light loop — far less likely than a park loop lagging), a
  decision is still stranded; it is logged loudly (`decision router lagged`). A
  genuinely-unknown session (unresolved `agent`, un-healed ancestor) is denied
  *all* tools until its profile is known — the intended fail-closed trade.

## Alternatives considered

- **Resolve from a shared, core-authoritative session→profile registry** (mirror
  the ADR-0068 `SeqRegistry`). The issue's other sanctioned direction. Rejected as
  the primary mechanism: it heals ancestors too, but requires a new core↔runtime
  seam and still leaves the `SpawnGuard` parent *links* broadcast-folded — whereas
  carrying the name on `ToolExec` rides the event that *already* triggers the
  decision, needs no new seam, and self-heals ancestors via their own spawn
  `ToolExec`s. The fail-closed fallback covers the residual case either way.
- **Keep the fail-open default, only fix the lag.** Rejected: the default-open
  fallback *is* the security inversion (#156's headline); leaving it means any
  dropped `SessionStarted` still un-masks a restricted session.
- **Keep the per-task broadcast park, recover on `Lagged` from a shared mailbox.**
  Rejected: a mailbox populated off the same lossy broadcast needs a `Notify` to
  wake a parked task with no further inbound traffic — reinventing the `oneshot`
  registry with more moving parts. A single router + `oneshot` is the minimal
  lag-proof shape.
