# 0038. Physical per-agent tool restriction (allowlist/denylist mask)

- Status: Accepted
- Date: 2026-07-11

## Context

File-based agent definitions ([ADR-0034](0034-file-based-agent-definitions.md))
parse a `tools` allowlist and a `disallowed_tools` denylist per agent, but #114
onward deferred their **enforcement** — the fields reached neither the core
`AgentProfile` nor any gate. The only per-agent restriction shipped so far is the
`permission` profile ([ADR-0003](0003-agent-and-permission-profiles.md)) with
ancestor clamping ([ADR-0024](0024-subagent-permission-gating.md)): a `Deny` rule
refuses a tool *at dispatch*, but the tool schema is still **advertised** to the
model. The restricted agent is a persona told "don't write", not one for which
`write` does not exist.

Claude Code's `Explore` agent has no `Write`/`Edit` tool *present* at all — a hard
boundary, not a nudge. Issue #116 asks for the same: an agent's `tools`/
`disallowed_tools` must (a) filter the `ToolSpec`s advertised to the model **and**
(b) be enforced in the runtime executor, so a masked call is refused even if the
model hallucinates it.

The open design question the issue posed: where is the seam? Core sources tool
schemas per turn from `EngineConfig.tool_specs` ([#61]); the mask must reach the
turn's `tools` list. Two candidate seams were named — a per-session `tool_specs`
override the runtime pushes into the engine, or a filter message.

## Decision

Put the mask on the core `AgentProfile` and enforce it on both sides of the
core↔runtime seam. Restriction is **orthogonal to permission**: `tools` controls a
tool's *advertisement + existence*; `permission` still grades `Allow`/`Ask`/`Deny`
among the tools that survive the mask.

### The mask lives on `AgentProfile` (no new protocol surface)

`AgentProfile` gains `tools: Option<Vec<String>>` (allowlist, `None` ⇒ inherit
all) and `disallowed_tools: Vec<String>` (denylist, applied after the allowlist),
plus `advertises_tool(name) -> bool` = `registry ∩ allowlist − denylist`. The
profile already travels per-session — resolved from the registry at
`SessionStarted`, `SetAgent`, and `Spawn` — so **no per-session override table and
no new message** are introduced. This is the rejected-alternatives axis of the
seam question: the profile *is* the per-session carrier of policy (system prompt,
model, permission), so the mask belongs there next to `permission`.

### (a) Advertisement — core filters specs at turn time

`run_turn` filters `EngineConfig.tool_specs` by `profile.advertises_tool(name)`
before appending the `update_plan`/`update_tasks` built-ins (which are
session-state tools, not host tools, and are **never** masked). A masked tool's
schema never reaches the model. This is a per-session filter of shared config, not
a per-session copy of it.

### (b) Enforcement — the runtime executor refuses a masked call at dispatch

The `tool_runner` already folds each session's active `AgentProfile` and its
`SpawnGuard` parent links. A new `runtime::permission::tool_masked` walks the
session + ancestor chain and returns `true` if **any** profile in the chain does
not advertise the tool — the mask **intersects down the ancestor chain**, exactly
mirroring ADR-0024's privilege ceiling (a child never gains a tool an ancestor
lacked). The check runs **first**, before the `agent_spawn`/`agent`/`agent_poll`/
`ask_user` interceptions and before permission resolution, so it uniformly masks
host tools *and* the runtime-owned orchestration tools, for `Primary` profiles as
well as sub-agents. A masked call gets a plain `ToolOutput` ("tool `x` is not
available to this agent (restricted by profile)") and never runs.

Ordering matters: placing `tool_masked` first is what lets a `Primary` agent that
masks `agent_spawn` actually be refused it — a check placed *after* the spawn
interception would let such an agent spawn anyway.

### `explore` becomes the reference read-only agent

The `explore` built-in (file and core fallback) gains `tools: [read, glob, grep]`
— no `edit`/`write`, no `bash`, no `agent_spawn`. Its existing `permission` denies
still stand; the mask is the stronger, physical boundary. ADR-0024's Subagent-leaf
capability gate is unchanged and still governs any `Subagent`-mode profile that
*does* advertise the spawn tools.

### Advertisement is not ancestor-clamped; enforcement is

Core only holds the session's own profile, not its ancestors, so the
advertisement filter (a) uses the session's own mask only. The runtime
enforcement (b) is the hard boundary and *does* clamp down the chain. This
asymmetry is deliberate and harmless: a child advertised a tool an ancestor masks
would still have any call refused at dispatch — the same shape as permission,
where an `Ask`/`Deny`-graded tool is still advertised.

## Consequences

### Positive

- A restricted agent's model never sees a masked tool's schema, and a
  hallucinated masked call is refused before it can run — the physical boundary
  #116 asked for.
- Zero new protocol surface: the mask rides the `AgentProfile` that already
  travels per session; no override table, no filter message. Core makes no policy
  decision — it filters by a field the profile carries, the runtime enforces.
- The enforcement mirrors ADR-0024's ancestor clamp, reusing the same
  `SpawnGuard` chain and single-threaded executor loop — one more pure function of
  state the executor already holds.
- `can_spawn`/`spawnable_agents` remain the only deferred frontmatter; the tool
  mask is now live.

### Negative / neutral

- Advertisement (a) is filtered by the session's own mask only, so a child could
  be *advertised* a tool an ancestor masks; enforcement (b) still refuses it. A
  minor UX wart (model sees a tool it can't use), identical to how permission
  already advertises `Ask`/`Deny` tools.
- The mask is name-based (exact tool-name match), like the permission patterns; no
  globbing beyond the allowlist/denylist sets. Sufficient for the fixed host-tool
  roster.
- Two built-in refusal messages now exist for a blocked tool ("not available" for
  a mask, "denied" for permission). They are distinct on purpose — a mask means
  the tool does not exist for the agent, a permission `Deny` means it exists but is
  refused.

## Alternatives considered

- **Per-session `tool_specs` override pushed into the engine.** The runtime
  computes each session's effective spec list and hands core a per-session copy.
  Rejected: it duplicates the shared `tool_specs`, needs a new message or a
  session-keyed table in core, and puts the ancestor-clamp knowledge (which core
  lacks) on the wrong side of the seam. The profile already carries the mask for
  free.
- **A filter message the runtime sends to mutate the turn's tools.** Adds a
  protocol variant and a mid-turn mutation point for a policy that is static per
  session. Rejected for the same reason — the profile is already the static
  per-session policy carrier.
- **Enforce only at dispatch, leave advertisement alone.** Half the value: the
  model still burns tokens on masked schemas and is tempted to call them. #116
  explicitly wants the schema withheld.
- **Advertise-only, no dispatch enforcement.** A hallucinated or replayed call
  would slip through. The executor is the trust boundary, so enforcement must live
  there too.
- **Fold the mask into the `permission` profile as a fourth `Hidden` grade.**
  Conflates two orthogonal axes (existence vs. approval). Keeping `tools` separate
  from `permission` matches ADR-0034's frontmatter and Claude Code's model.

## References

- Issue #116: runtime physical per-agent tool restriction
- Epic #111: agents/skills/system-prompt
- [ADR-0034](0034-file-based-agent-definitions.md): file-based agent definitions
  (parsed the `tools`/`disallowed_tools` fields, deferred enforcement)
- [ADR-0024](0024-subagent-permission-gating.md): sub-agent permission gating +
  privilege ceiling (the ancestor-clamp pattern this reuses)
- [ADR-0003](0003-agent-and-permission-profiles.md): agent + permission profiles
- [ADR-0006](0006-core-dependency-hygiene-gate.md): core dependency hygiene
