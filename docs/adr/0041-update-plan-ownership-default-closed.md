# 0041. update_plan ownership — default-closed plan authority

- Status: Accepted
- Date: 2026-07-11

## Context

The session plan (`OutEvent::Plan`, a session-owned markdown snapshot) is written
by the built-in `update_plan` tool. Core exempted `update_plan`/`update_tasks`
from the [ADR-0038](0038-physical-per-agent-tool-restriction.md) tool mask —
`run_turn` appended both specs unconditionally after the mask filter, and
`handle_tool_call` ran them for any profile. So **every** agent — `build`, or even
a spawned read-only `explore` child — could author the session plan.

In the target agent hierarchy (epic #111), the plan is authored by the `plan`
agent and consumed by `build` (via the plan-accept flow, [#141](https://github.com/xmiksay/entanglement/issues/141)).
Plan authorship is a **cross-agent authority**, the sibling of #119's spawn
control ([ADR-0040](0040-per-profile-spawn-control.md)): together they enforce the
hierarchy — only a plan-owning profile may author the session plan. That authority
must be **default-closed**.

## Decision

Add `AgentProfile.owns_plan: bool` (serde default **false**) and gate the plan
built-in on it. `update_tasks` stays unconditional — the task list is per-session
progress bookkeeping shown to the user, never fed back to the model, so it carries
no cross-agent authority.

### Advertisement + enforcement both in core

Unlike the #116 tool mask and #119 spawn control (semantics in core, enforcement
in the runtime), plan authority is enforced **entirely in core** — the built-ins
are session-state tools that never round-trip to the runtime, so
`runtime::permission::tool_masked` cannot see them.

- **Advertisement** — `run_turn` appends the `PLAN_TOOL` spec only when
  `s.profile.owns_plan`. A non-owner's model never sees the `update_plan` schema.
- **Enforcement** — `handle_tool_call` refuses a hallucinated `update_plan` from a
  non-owner via a refusal `ToolOutput` (no plan mutation, no `OutEvent::Plan`); the
  turn continues. A masked schema is not a guarantee the model won't emit the call,
  so the runtime-independent refusal is the hard boundary.

`InMsg::SetPlan` remains head/user authority, unchanged — the user (and the
plan-accept flow) may always set the plan.

### Built-in `plan` gets `owns_plan: true` **plus a physical read-only mask**

`plan.md` gains `owns_plan: true` and a tool allowlist
`tools: [read, glob, grep, agent, agent_spawn, agent_poll, ask_user, load_skill]`.
The mask makes `plan` physically read-only (no `edit`/`write`/`bash`), and — via
`tool_masked`'s ancestor intersection ([ADR-0024](0024-subagent-permission-gating.md)'s
clamp applied to the #116 mask) — every child spawned under `plan` is clamped to
that read-only set too. This closes the gap where `plan` (previously unmasked)
could spawn a write-capable `all`-mode user agent. The body is updated: record the
working plan via `update_plan`, delegate research to exploration agents.

`build.md`/`explore.md` are unchanged — default-false means they simply stop
advertising `update_plan`.

## Consequences

### Positive

- Plan authority is default-closed: a future user-defined agent gains it only by
  opting in with `owns_plan: true`, never by forgetting to opt out.
- The `plan` agent is physically read-only, and its read-only-ness propagates to
  its spawned children through the existing ancestor mask intersection.
- Zero new protocol message — the field rides `AgentProfile` like #116/#119, and
  the enforcement is a one-line gate in each of the two core call sites.

### Negative / neutral

- Enforcement is split from #116/#119's runtime location because built-ins never
  reach the runtime. This is inherent to the built-ins being session-state tools.
- Adding a real host tool to `plan` later (`rhai` #122, `call`/`bash` #121) is a
  one-line `plan.md` frontmatter change and stays consistent — the ancestor mask
  *intersects*, so children can only shrink. Caveat for then: raw `bash` drops
  plan's "makes no changes" guarantee from physical (mask) to ask-gated;
  capability-sandboxed `rhai` with a read-only capability set preserves it.

## Alternatives considered

- **Drop the built-in exemption + `disallowed_tools: [update_plan]` on
  build/explore.** Rejected: the mask is default-open (`tools: None` = inherit
  all), so every future user-defined agent would silently receive plan authority
  unless its author remembers to deny it. An *authority* default must not depend on
  each file opting out.
- **List `update_plan` in the `tools:` allowlist.** Rejected: forces a complete
  allowlist onto any plan-owner just to gain one built-in — conflating the physical
  tool mask (#116) with plan authority.
- **Enforce in the runtime like #116/#119.** Not possible: the built-ins don't
  round-trip to the runtime, so `tool_masked` never sees an `update_plan` call.

## References

- Issue #140: engine `update_plan` ownership (`owns_plan`, default-closed)
- Epic #111: agents/skills/system-prompt
- [ADR-0038](0038-physical-per-agent-tool-restriction.md): physical per-agent tool
  restriction (the `AgentProfile`-as-carrier seam this reuses; the ancestor mask
  intersection that propagates plan's read-only clamp to its children)
- [ADR-0040](0040-per-profile-spawn-control.md): per-profile spawn control (the
  sibling authority gate — together they enforce the hierarchy)
- [ADR-0024](0024-subagent-permission-gating.md): sub-agent permission gating
  (the ancestor clamp)
- [ADR-0039](0039-markdown-task-list.md): markdown task list (`update_tasks`, the
  unconditional built-in this leaves untouched)
- Follow-up: #141 (plan-accept flow — `build` consumes the plan a `plan` agent
  authored)
