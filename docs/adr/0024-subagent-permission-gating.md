# 0024. Sub-agent spawn permission gating and privilege ceiling

- Status: Accepted
- Date: 2026-07-09

## Context

[ADR-0022](0022-subagent-spawn.md) shipped `spawn_agent` bypassing the permission
profile like core's `update_plan`/`update_tasks` built-ins, and explicitly
deferred isolation/permissions for sub-sessions. [ADR-0023](0023-subagent-spawn-limits.md)
bounded the spawn *tree* (depth + fan-out) but left the *authority* of a spawn
untouched. Two holes remained (issue #77):

1. **Any active profile can spawn — including read-only `explore`.** A restricted
   profile could escalate by spawning a `build` child that writes freely,
   sidestepping its own `Deny`/`Ask` rules.
2. **A child runs under its own profile with no relationship to the parent's
   authority.** A less-privileged parent could spawn a fully-privileged child
   against the *same* working tree, so "read-only" was not actually enforced
   across a delegation.

The issue framed two axes: gate spawn as a per-profile capability, and/or give
sub-sessions an isolated root / restricted tool set.

## Decision

Two runtime-only gates, layered on the existing `Allow | Ask | Deny` dispatch
(#59). Both live in the tool executor's single-threaded loop (`tool_runner.rs`)
and are folded from the same lifecycle events as permission dispatch and the
`SpawnGuard` (#76) — **zero core surface**. The pure policy lives in a new
`runtime::permission` module.

### Gate A — spawn is a capability of the profile (keyed on `AgentMode`)

Only `Primary`-mode profiles may call `spawn_agent`. A `Subagent`-mode leaf
(`explore`, and any custom leaf profile) is **refused** before `SpawnGuard`
charges its budget, with a plain refusal `ToolOutput` ("a read-only sub-agent
profile cannot spawn further sub-agents. Do the work directly."). This answers
"should spawn be a per-profile capability" — yes, and `AgentMode` already carries
the leaf/non-leaf distinction, so no new field is introduced. Read-only `explore`
can no longer spawn at all, closing hole (1) at its most direct point.

### Gate B — privilege ceiling: a child is never more privileged than its ancestors

When the runtime resolves a tool's permission for a session, it clamps the
result to the **least-privileged** `for_tool` across the session and its whole
ancestor chain, ordered `Deny < Ask < Allow`. A root has no ancestors, so it
resolves to its own profile — single-session behavior is unchanged. A child
spawned under `build` (allow-all) below a `plan` parent (read-only + ask)
therefore gets `plan`'s ceiling: it may read, but an `edit` becomes `Ask`, not a
silent `Allow`. The child can never touch the shared working tree in a way an
ancestor couldn't, closing hole (2) regardless of the profile the child was
spawned under.

The ancestor chain comes from the `SpawnGuard`'s parent links (already folded
from `SessionStarted`); the runtime already tracks each session's active profile
for dispatch. So the clamp is a pure function of state the executor already
holds.

### Isolation (separate filesystem root) — deferred, not needed for this hole

The child still shares the parent's `root` and host-tool `ToolRegistry`. A
separate per-child root/worktree is **not** required to close the escalation
concern: Gate B already bounds a child's authority over the shared tree to its
parent's. A sandboxed scratch root is a heavier change (per-child registry /
worktree lifecycle) with its own failure modes, and is left to a future
security-focused ADR alongside the still-open `bash` sandbox
([ADR-0010](0010-single-head-crate-and-bash-opt-in.md)).

## Consequences

### Positive

- A read-only profile can neither spawn (Gate A) nor launder writes through a
  child (Gate B); "explore is read-only" now holds across delegation.
- Both gates are pure runtime state — no protocol variant, no core change,
  consistent with the three-layer split (ADR-0006/0010). Core still has no notion
  of a "child session".
- Refusals and clamps surface through the existing `ToolOutput` / `ToolRequest`
  paths, so every head (stdio, TUI, future WS) handles them with no new code.

### Negative / neutral

- The capability gate is coarse: it keys on `AgentMode`, so a would-be
  spawn-capable leaf profile must be `Primary`. Acceptable — a leaf that spawns
  is a contradiction in the mode's meaning.
- The ceiling clamps but never *loosens*; a child under a permissive parent still
  runs its own (possibly stricter) profile, which is the intended direction.
- No filesystem isolation yet: a `build` child of a `build` root still writes the
  shared tree freely (as before). That is in-scope authority, not escalation.

## Alternatives considered

- **Make `spawn_agent` a normal permission-gated host tool.** Gives per-profile
  gating for free but `explore`/`plan` would `Deny`/`Ask` a primitive that only
  coordinates sessions, and it still needs special execution. Rejected in
  ADR-0022; Gate A on `AgentMode` gives the capability distinction without the
  approval-prompt awkwardness.
- **Refuse (not clamp) a child more privileged than its parent.** Simpler to
  state but blocks the legitimate "plan delegates a build sub-task, but keep it
  read-only" flow. Clamping is strictly safer and more useful — the child still
  runs, just bounded.
- **Isolated filesystem root per child now.** Real isolation, but a large change
  (per-child registry, worktree lifecycle, path translation) for a hole Gate B
  already closes. Deferred.
- **Track the ceiling in core's supervisor.** It owns the real `parent_links`,
  but pulling permission policy into core violates ADR-0006; the runtime already
  has every fact from the outbox.

## References

- Issue #77: runtime isolation / permission gating for sub-agent spawn
- [ADR-0022](0022-subagent-spawn.md): sub-agent spawn (deferred this gating)
- [ADR-0023](0023-subagent-spawn-limits.md): spawn recursion / fan-out limits
- [ADR-0021](0021-hierarchical-session-model.md): hierarchical session model
- [ADR-0006](0006-core-dependency-hygiene-gate.md): core dependency hygiene
- [ADR-0003](0003-agent-and-permission-profiles.md): agent + permission profiles
