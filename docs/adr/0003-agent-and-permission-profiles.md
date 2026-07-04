# 0003. Agent + permission profiles (opencode-style)

- Status: Accepted
- Date: 2026-07-04

## Context

We need Build / Plan / Explore modes: the same engine, but with different
instructions and different capabilities (Plan shouldn't edit files; Explore is
read-only).

Research into OpenCode (its [agents doc](https://opencode.ai/docs/agents/)) shows
how it models this: **"plan" is a built-in primary *agent***, not a system
message and not a structured output. An agent is a config bundle
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
wins** with a `*` wildcard (opencode's semantics). The turn loop dispatches:

- `Allow` → run the tool immediately, emit `ToolOutput`.
- `Ask` → emit `ToolRequest`, park at `WaitingApproval` until `Approve`/`Reject`.
- `Deny` → emit `ToolOutput("…denied by permission profile")`, never run.

## Consequences

- **(+)** Parity with the proven opencode model; users already know Build/Plan.
- **(+)** Custom profiles via `ProfileRegistry::insert` (config-driven, no code).
- **(+)** One permission mechanism drives the entire approval flow — there is no
  separate "is this safe?" path.
- **(−)** A profile-config surface to document and validate.

## Alternatives considered

- **A structured `mode` enum on messages** (`Prompt { mode: Plan, ... }`).
  Rejected: less expressive than opencode's full config bundle, and couples mode
  to each message rather than to the session.
- **Hardcode Plan as read-only inside the engine.** Rejected: not configurable,
  no custom agents, and we'd reinvent profiles later.
- **Per-tool permission as a flat allowlist, no `Ask`.** Rejected: `Ask` is the
  load-bearing human-in-the-loop behavior; a binary allow/deny loses it.
