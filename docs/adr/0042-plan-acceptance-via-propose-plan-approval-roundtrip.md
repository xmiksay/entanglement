# 0042. Plan acceptance via the propose_plan approval round-trip

- Status: Accepted
- Date: 2026-07-11

## Context

The agent hierarchy (epic #111) ends with the `plan` agent's output being
**accepted by the user into a fresh `build` session**. `update_plan`
([ADR-0041](0041-update-plan-ownership-default-closed.md)) records working plan
snapshots, but authoring a snapshot is not *finalizing* — the plan agent needs a
distinct "the plan is done, accept it" step whose outcome:

1. is a user decision (accept / revise), not a model decision;
2. is learned **in-band** by the plan agent, so it can end its turn on accept or
   revise-and-re-propose on reject;
3. hands the accepted plan to a `build` session to implement — that session must be
   able to `edit`/`write`, and its first user message must be the plan.

Everything needed for (1)+(2) already exists: the tool-approval round-trip (#59) —
`OutEvent::ToolRequest` → head `Approve`/`Reject` → the runtime folds the outcome
back as a `ToolOutput`. `InMsg::Reject.reason` and the TUI's
`ApprovalMode::EnteringRejectReason` give a typed revision channel for free.

## Decision

Add a runtime-owned tool `propose_plan { plan }` — the plan agent's *finalize*
step. Acceptance rides the existing tool-approval round-trip; **no new protocol
message.**

### Interception + unconditional force-park

`propose_plan` is intercepted on `OutEvent::ToolExec` in `runtime::tool_runner`,
after the [ADR-0038](0038-physical-per-agent-tool-restriction.md) mask check and in
the same interception family as `ask_user` ([ADR-0027](0027-ask-user-interactive-prompt.md)).
Unlike a host tool it is **force-parked on the `Ask` path unconditionally** — a
permission profile can never `Allow` it, because user approval *is* the tool's
semantics. `runtime::propose_plan::run_propose_plan` emits a standard
`OutEvent::ToolRequest` (the head renders its usual approve/reject prompt) and
parks for the head's decision on the inbound fan-out.

- **Approve** → record the plan with `InMsg::SetPlan` (engine state stays
  consistent for every head), then reply `ToolOutput("plan accepted by the user")`
  so the plan agent knows the outcome and can end its turn.
- **Reject + reason** → the existing fold-back (`tool \`propose_plan\` rejected:
  <reason>`); the model revises and re-proposes in the same turn. Zero new code.
- **Stop while parked** → unwind silently (core cancels the turn on the same
  `Stop`), exactly like the approval and `ask_user` paths.

### Advertisement — gated by `owns_plan`, not the tool mask

`propose_plan` rides the #119 `profile_tool_specs` seam
([ADR-0040](0040-per-profile-spawn-control.md)) and is advertised **only to a
profile that `owns_plan`** ([ADR-0041](0041-update-plan-ownership-default-closed.md)) —
the same default-closed-authority argument: the `tools:` mask is default-open, so a
mask-only gate would leak the tool to every unmasked user profile. `plan.md`'s
`tools:` allowlist also gains `propose_plan` (the mask is the second gate, applied
by core's `run_turn` after the `owns_plan` gate fills the per-profile specs).

### The handoff is head policy, not engine surface

On approving a `propose_plan` request the head **additionally** performs the
handoff (it has the plan text from the request input):

1. mint a fresh `SessionId::new_uuid()`;
2. `SetAgent { session: new, agent: "build" }` — lazy session creation starts a
   **root** session under `build`;
3. `Prompt { session: new, text: wrap(plan) }` — the accepted plan verbatim as the
   first user message;
4. switch the view to the new session.

This is **head policy** — zero new protocol surface — documented as a recipe
(`docs/architecture.md` §…) so pipe/WS heads implement it identically. The plan
session stays alive after accept; a later re-propose mints another fresh build
session. One-shot heads (`run`/`pipe`) can't park an interactive approval, so they
auto-reject `propose_plan` with a "non-interactive head" reason — the plan agent
still learns the outcome in-band.

### Why the build session is a root, not a child of the plan session

A parent link carries *semantics*, not grouping:

1. **Tool set** — the [ADR-0024](0024-subagent-permission-gating.md) permission
   clamp + #116 mask intersection would clamp `build` to `plan`'s read-only tool
   set; `build` could never `edit`/`write`, and an escape hatch would break the
   child-never-more-privileged invariant.
2. **Budget** — [ADR-0023](0023-subagent-spawn-limits.md) depth/fan-out: `build`'s
   own spawns would drain the plan session's root budget.
3. **Authority** — accept is a transfer of authority *from the user*, correctly
   modeled as a root.

Provenance ("came from plan session X") can be shown by the TUI locally; a
`SessionInfo` provenance field is a possible follow-up, orthogonal to authority.

## Consequences

### Positive

- Reuses the #59 round-trip wholesale: `ToolRequest`, `Reject.reason` fold-back,
  and the TUI's reject-reason mode all pre-exist. The iterate loop costs no new
  code.
- Zero new protocol message; the handoff is head policy, so every head implements
  one documented recipe rather than the engine encoding UX + session policy.
- Default-closed authority is consistent with #140/#119 — a user agent gains
  `propose_plan` only by opting in with `owns_plan: true` and listing it in
  `tools:`.

### Negative / neutral

- The handoff is duplicated per head (TUI now; pipe/WS later) rather than
  centralized. Deliberate: centralizing it means a new protocol message that bakes
  head UX + fresh-session policy into the engine.
- `SetPlan` is stashed while the tool call is parked and replayed after the turn,
  so `OutEvent::Plan` follows the turn's `Done`. Harmless — the plan is recorded;
  only event ordering shifts.

## Alternatives considered

- **`InMsg::AcceptPlan` / `OutEvent::PlanAccepted`.** Rejected: the engine would
  encode head UX + fresh-session policy; every head would reimplement it, and the
  in-band reject-reason revision channel would need re-inventing.
- **Approve `update_plan` itself.** Rejected: core built-ins never round-trip
  through the runtime approval path (handled inside `handle_tool_call`), so they
  can't surface a `ToolRequest`.
- **A `/accept` head command decoupled from the model.** User-paced, but the plan
  agent never learns the outcome and revision feedback loses the in-band
  reject-reason channel.
- **Same-session `SetAgent` to `build`.** Rejected: drags plan-mode history along;
  the requirement is a *fresh* session whose first user message is the plan.

## References

- Issue #141: runtime `propose_plan` — plan acceptance via the approval round-trip
- Epic #111: agents/skills/system-prompt
- [ADR-0041](0041-update-plan-ownership-default-closed.md): `update_plan`
  ownership — `owns_plan`, the default-closed authority gate this reuses for
  advertisement
- [ADR-0040](0040-per-profile-spawn-control.md): per-profile spawn control (the
  `profile_tool_specs` seam this rides)
- [ADR-0027](0027-ask-user-interactive-prompt.md): `ask_user` (the runtime-owned
  interception family this joins)
- [ADR-0024](0024-subagent-permission-gating.md) /
  [ADR-0023](0023-subagent-spawn-limits.md): the clamp + budget that make a child
  build session wrong — hence a root
