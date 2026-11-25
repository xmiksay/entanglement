# 0040. Per-profile spawn control (can_spawn + spawnable-agents allowlist)

- Status: Accepted
- Date: 2026-07-11

## Context

Sub-agent spawning had two gates before this issue: the depth + per-root budget
([ADR-0023](0023-subagent-spawn-limits.md)) and the ancestor permission clamp
([ADR-0024](0024-subagent-permission-gating.md)). ADR-0024's spawn boundary was
**spawner-side only**: a `Subagent`-mode leaf (`explore`) was refused the spawn
capability, but nothing gated the spawn *target*. The `agent`/`agent_spawn`
roster (`ProfileRegistry::iter()`) disclosed **all** profiles and the `agent`
enum listed all names, so a model could spawn a `primary` (`build`, `plan`), and
an unknown `InMsg::Spawn` name silently resolved to `build` (the most-privileged
default) — a typo'd spawn *escalated*.

File-based agent definitions ([ADR-0034](0034-file-based-agent-definitions.md))
parse two spawn-control fields, `can_spawn` and `spawnable_agents`, but their
enforcement was deferred (they didn't even reach the core `AgentProfile`). #119
is the enforcement half — and, per planning, the enforcement half of the **agent
hierarchy**: `plan` spawns only exploration-type agents and is never spawnable,
`build` spawns everything except primaries, `explore` is a leaf.

## Decision

Make spawning a per-profile capability declared in the agent definition — both
*whether* a profile may spawn and *which* profiles it may spawn — layered in
front of the ADR-0023 budget and the ADR-0024 clamp. Four checks now run before
a child is minted:

1. **`can_spawn` gate** (this issue)
2. **target-side mode gate** (this issue) — spawnable ⇔ `mode ∈ {subagent, all}`
3. **`spawnable_agents` allowlist** (this issue)
4. depth cap + per-root budget (`SpawnGuard`, ADR-0023 — unchanged)
5. ancestor permission clamp (ADR-0024 — unchanged; still the outer bound)

### Fields + semantics ride the core `AgentProfile`

Same seam as [ADR-0038](0038-physical-per-agent-tool-restriction.md)'s tool mask.
`AgentProfile` gains `can_spawn: Option<bool>` and
`spawnable_agents: Option<Vec<String>>`, with three helpers:

- `may_spawn()` = `can_spawn.unwrap_or(mode != Subagent)` — a `Subagent` leaf
  defaults closed, every other mode open; an explicit `can_spawn` overrides.
- `spawn_target_allowed(name)` — `None` allowlist ⇒ open to any target.
- `spawnable_as_subagent()` = `mode ∈ {Subagent, All}` — the target-mode gate.

Core = semantics (the three helpers), runtime = enforcement. Because the
hierarchy falls out of **mode defaults**, `build`/`plan` become unreachable spawn
targets with *zero* frontmatter changes, and user-defined/redefined profiles slot
into the same rules — the hierarchy is a default, not a hardcoded invariant.

### Target-side gate is **structural**, via a per-profile spec seam

`EngineConfig` gains `profile_tool_specs: HashMap<String, Vec<ToolSpec>>`.
`run_turn` appends the active profile's entry (also filtered through
`advertises_tool`) after the #116 mask filter, so a session's advertised tools
are `tool_specs ∩ mask` + the profile's own spawn specs. The runtime's
`build_config` fills the table via `subagent::spawn_specs_for(profile, registry)`
— the `agent_spawn`/`agent`/`agent_poll` triple with the roster + `agent` enum
**scoped to exactly the profiles that profile may spawn** (its `spawnable_agents`
∩ the target-mode gate). The entry is empty when `!may_spawn()` or there are no
valid targets, so a non-spawning profile's model never sees the family at all,
and an out-of-list spawn is a **schema violation before it is an executor
refusal**. The `agent`/`agent_spawn`/`agent_poll` specs therefore move *out* of
the shared `tool_specs` and *into* `profile_tool_specs`; `ask_user` (every
profile may ask) stays shared.

### Refusal layering in `runtime::permission::spawn_refusal`

`spawn_refusal(spawner, target, registry)` runs in the executor before the
`SpawnGuard`, and layers, in order: `!may_spawn()` (absorbs the old
`spawn_capability_refusal`, same "cannot spawn" phrasing) → unknown target →
target not spawnable-mode → target outside `spawnable_agents`. Each returns a
clear `ToolOutput` refusal (the ADR-0023 pattern) with no child minted.

### Transitivity is deliberate

The allowlist is checked per spawning session against **its own** profile: A
allowed to spawn B does not imply A can spawn what B can spawn. Each hop is
re-checked against the spawner's list, and the ADR-0024 clamp keeps privileges
monotonically non-increasing down the tree.

### Supervisor hardening

`InMsg::Spawn` with an unknown agent name no longer falls back to `build` via
`ProfileRegistry::resolve` — the `Spawn` arm now `get()`s the profile and emits a
supervisor `Error` on miss, so a typo'd spawn is refused rather than escalated.
The lazy-`Prompt` default path keeps `resolve` (that fallback is a blank user
session, not a model-chosen target).

### TUI + built-ins

The TUI `/agent` picker and Tab-cycle become registry-driven, filtered to
`mode ∈ {primary, all}` — a `subagent` leaf like `explore` is a spawn target,
never a manual entry agent (the old hardcoded list leaked it). Built-in
`plan`/`build` **omit** `spawnable_agents`, so user-defined exploration agents
stay spawnable without editing a built-in. `SetAgent` stays ungated (user
authority) — an accepted risk: a user may manually switch a session to any
profile, spawn targets are the only model-reachable axis gated here.

## Consequences

### Positive

- The agent hierarchy is enforced structurally: a model cannot spawn `build`/
  `plan`, cannot spawn a profile off its allowlist, and a non-spawning profile
  never sees the `agent_*` family. Out-of-list spawns fail as schema violations.
- Zero new protocol *message*: the fields ride `AgentProfile` (like #116) and the
  specs ride a generic `profile_tool_specs` table — later per-profile features
  (e.g. `propose_plan`, #141) reuse the same seam.
- The typo-escalation bug is closed at the supervisor.
- The whole hierarchy falls out of mode defaults, so it holds for user-defined
  profiles too.

### Negative / neutral

- `profile_tool_specs` is keyed by profile *name*; a per-profile tool-name variant
  was rejected (it would break `tool_masked`'s by-name ancestor intersection), so
  the table holds whole `ToolSpec`s, resolved per active profile.
- `SetAgent` is ungated — a user with `SetAgent` authority can still switch a
  session's profile. This is deliberate (user authority ≠ model authority).
- The advertisement half is the session's own scope; the executor refusal is the
  hard boundary (same asymmetry as ADR-0038).

## Alternatives considered

- **Per-profile tool-name variants** (e.g. `agent_spawn@build`). Rejected: breaks
  `tool_masked`'s by-name ancestor intersection and the executor's name dispatch.
- **Core-side roster synthesis.** Rejected: core would need to read the registry
  and synthesize per-profile specs — an ADR-0006 layering violation. The runtime
  fills `profile_tool_specs`; core only appends by active-profile name.
- **Mutating the shared `tool_specs` on `AgentChanged`.** Racy (a mid-flight turn
  could see a half-swapped list) and stateful; the per-profile table is computed
  once at `build_config` and read immutably.
- **Keep the target roster unfiltered, gate only at the executor.** Half the
  value: the model still sees (and is tempted by) primaries and off-list agents in
  the enum. #119 wants the schema scoped.

## References

- Issue #119: runtime per-profile spawn control (`can_spawn` + spawnable-agents)
- Epic #111: agents/skills/system-prompt
- [ADR-0034](0034-file-based-agent-definitions.md): file-based agent definitions
  (parsed `can_spawn`/`spawnable_agents`, deferred enforcement)
- [ADR-0038](0038-physical-per-agent-tool-restriction.md): physical per-agent
  tool restriction (the `AgentProfile`-as-carrier seam this reuses)
- [ADR-0024](0024-subagent-permission-gating.md): sub-agent permission gating
  (spawner-side capability gate + ancestor clamp this layers in front of)
- [ADR-0023](0023-subagent-spawn-limits.md): sub-agent spawn limits (the
  refusal-`ToolOutput` pattern + `SpawnGuard`)
- Follow-ups: #140 (`owns_plan` / `update_plan` authority), #141 (plan-accept
  flow) — both reuse the `profile_tool_specs` seam.
