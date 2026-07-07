# 0003. Agent + permission profiles (opencode-style)

- Status: Accepted
- Date: 2026-07-07

## Context

We need Build / Plan / Explore modes: the same engine, but with different
instructions and different capabilities (Plan shouldn't edit files; Explore is
read-only).

OpenCode ([agents doc](https://opencode.ai/docs/agents/)) models this well:
**"plan" is a built-in primary *agent***, not a system message and not a
structured output. An agent is a config bundle
`{ mode, model, prompt (→ system prompt), permission, temperature }`. The `plan`
agent is `build` with a different system prompt plus a restricted permission
profile (`edit: ask`, `bash: ask`). The user switches Build↔Plan with Tab. The
"plan" content is just the agent's text response.

## Decision

A session runs under exactly one `AgentProfile`:

```rust
struct AgentProfile {
    name: String,
    mode: AgentMode,            // Primary | Subagent
    system_prompt: String,
    model: Option<String>,
    permission: PermissionProfile,
}
```

Switch with `InMsg::SetAgent { agent }`; the engine resolves the name from a
`ProfileRegistry` and emits `AgentChanged`. Built-ins ship: `build`
(all-allow), `plan` (ask, read allow), `explore` (deny, read/glob/grep allow);
users add their own.

`PermissionProfile` resolves `Allow | Ask | Deny` per tool, **last-matching-rule
wins** with a `*` wildcard (opencode's semantics):

- `Allow` → run the tool immediately, emit `ToolOutput`.
- `Ask` → emit `ToolRequest`, park at `WaitingApproval` until `Approve`/`Reject`.
- `Deny` → emit `ToolOutput("…denied by permission profile")`, never run.

**Where dispatch runs.** The `AgentProfile` *shape* is a protocol type owned by
`entanglement-core`, but the **permission decision and the approval wait are a
runtime concern** ([ADR-0006][0006], [ADR-0010][0010]): the runtime holds the
active profile, resolves `Allow|Ask|Deny` for each tool request the engine
emits, runs the tool on `Allow`, prompts the user on `Ask`
([ADR-0014][0014]), and returns the outcome to the loop. This keeps the tool
set and permission policy out of the pure engine — an embedder supplies its own.
The `Ask` round-trip rides the existing `ToolRequest`/`Approve`/`Reject`
protocol frames, so the wire contract ([ADR-0002][0002]) is unchanged.

## Consequences

- **(+)** Parity with the proven opencode model; users already know Build/Plan.
- **(+)** Custom profiles via `ProfileRegistry::insert` (config-driven, no code).
- **(+)** One permission mechanism drives the entire approval flow — there is no
  separate "is this safe?" path.
- **(+)** Because dispatch lives in the runtime, permission policy is pluggable
  per embedder without touching core.
- **(−)** A profile-config surface to document and validate.
- **(−)** The engine and runtime must agree on the tool-request/outcome protocol
  for *every* tool, not only `Ask` ones (the price of moving execution out of
  core, [ADR-0010][0010]).

## Alternatives considered

- **A structured `mode` enum on messages** (`Prompt { mode: Plan, ... }`).
  Rejected: less expressive than opencode's full config bundle, and couples mode
  to each message rather than to the session.
- **Hardcode Plan as read-only inside the engine.** Rejected: not configurable,
  no custom agents, and we'd reinvent profiles later.
- **Per-tool permission as a flat allowlist, no `Ask`.** Rejected: `Ask` is the
  load-bearing human-in-the-loop behavior; a binary allow/deny loses it.
- **Decide permissions inside core.** Rejected: it binds the engine to a fixed
  tool set and permission policy and pulls execution I/O into the pure layer
  ([ADR-0006][0006]).

[0002]: 0002-session-multiplexed-protocol.md
[0006]: 0006-core-dependency-hygiene-gate.md
[0010]: 0010-single-head-crate-and-bash-opt-in.md
[0014]: 0014-tool-approval-inline-modal.md
