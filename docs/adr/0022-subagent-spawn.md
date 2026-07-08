# 0022. Sub-agent spawn and parent→child answer relay

- Status: Accepted
- Date: 2026-07-08

## Context

[ADR-0021](0021-hierarchical-session-model.md) modeled the session tree — a
`parent: Option<SessionId>` on every session, tree-walk helpers
(`children_of` / `root_of`), and nested TUI rendering — but explicitly deferred
the mechanism that *creates* children. The supervisor's `parent_links` map was
declared and read but never written, so every lazily-spawned session was a root;
`AgentMode::Subagent` existed (the `explore` profile uses it) but no code path
spawned one. This is issue #60: give the engine a spawn path plus message passing
between parent and child, actually populating the tree.

Two axes had to be decided: **how a spawn is requested** and **how the child's
result flows back to the parent**.

## Decision

### Spawn request — a new `InMsg::Spawn`, driven by a runtime `spawn_agent` tool

`InMsg::Spawn { session: child, parent, agent, prompt }` is added to the protocol
(`session` is the *child's* id). The **supervisor** (`holly.rs`) handles it
specially — like `Resume`, it is never routed to a session as a command:

1. `parent_links.insert(child, Some(parent))` — the one write that makes the
   existing tree-walk helpers reflect reality;
2. resolve the requested `agent` profile (falling back to the default `build`);
3. spawn the child `session_loop` with `parent = Some(parent)` so its
   `SessionStarted` carries the link (`root: false`);
4. queue the initial `SessionCmd::Prompt(prompt)`.

A duplicate `Spawn` for a live child id is a no-op.

The model-facing trigger is a **runtime-owned `spawn_agent { agent, prompt }`
tool**. It is *not* a `ToolRegistry` tool (it touches no host resource); the
runtime tool executor intercepts it on `ToolExec`, **before** permission
resolution, so it bypasses the permission profile exactly like core's
`update_plan` / `update_tasks` built-ins. Its schema is advertised to the model
via `EngineConfig.tool_specs` (appended in the head's `build_config`).

### Answer relay — child's final text as a synthetic tool result

The runtime executor, on intercepting `spawn_agent`, subscribes to the outbox
*before* sending `Spawn` (so the child's `Done` cannot race ahead of the
watcher), sends the `Spawn`, then watches the child's events — accumulating its
assistant `TextDelta`s until the child's `Done`. It then replies to the **parent**
with `InMsg::ToolResult { request_id, output: child_answer }`.

This reuses the #58 tool round-trip end to end: the parent's turn loop already
parks on `ToolResult` and folds the output into `Context` as an ordinary
`ToolOutput`. **Core's turn loop needs no notion of a "child session"** — the
whole feature lands in the runtime plus one supervisor branch.

### Explicitly deferred

Per ADR-0021's deferred list, out of scope here (follow-up issues, not blockers):
isolation / permissions for sub-sessions, recursion / fan-out limits, and
`apply_diff` re-enable. Full bidirectional session-to-session messaging is also
deferred — the one-shot parent→child→parent relay is sufficient for v1; a general
protocol lands only if it proves insufficient.

## Consequences

### Positive

- `parent_links` is finally populated; `children_of` / `root_of` and the nested
  TUI rendering reflect real spawns, not just replayed logs.
- Minimal core surface: one protocol variant + one supervisor branch. Spawn
  orchestration, the tool schema, and the answer relay all live in the runtime,
  honoring the three-layer split (ADR-0006/0010).
- The child is a first-class session: its own tools round-trip through the same
  executor, its events persist and replay like any other.

### Negative / neutral

- No recursion limit yet: a sub-agent can itself call `spawn_agent`. Bounded per
  session by `MAX_TURNS`, but an unbounded spawn tree is possible until a limit
  lands (deferred).
- Bypassing the permission profile means any active profile (even read-only
  `explore`) may spawn. Acceptable for v1; gating is part of the deferred
  isolation work.
- A lagging outbox watcher could miss the child's `Done`; the relay breaks out
  and returns what it has rather than parking the parent forever.

## Alternatives considered

- **`spawn_agent` as a core built-in** (handled in `session.rs` like
  `update_plan`). Rejected: the session loop cannot spawn sessions (the
  supervisor owns the map) and would have to watch child events itself — pulling
  orchestration back into core against the three-layer direction.
- **`spawn_agent` as a normal permission-gated host tool.** Gives per-profile
  gating for free, but it still needs special execution (it orchestrates
  sessions, not files), and `explore`/`plan` would `Deny`/`Ask` a primitive that
  only coordinates sessions. Bypassing matches the built-in precedent; profile
  gating is deferred to the isolation work.
- **Full bidirectional session-to-session messaging.** More general, but needs
  its own protocol design; deferred until the one-shot relay proves insufficient.

## References

- Issue #60: runtime inter-session agent messaging / subagent spawn
- [ADR-0021](0021-hierarchical-session-model.md): hierarchical session data model
- [ADR-0010](0010-single-head-crate-and-bash-opt-in.md): single head crate
- ADR-0006: core dependency hygiene; #58/#59: tool exec + permission dispatch in runtime
