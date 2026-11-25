# 0193. Two tool-call modes (`native` / `invoke`) + lazy tool discovery

- Status: Accepted
- Date: 2026-09-13
- Narrows: [ADR-0067](0067-mcp-client-as-runtime-tool-provider.md) — its
  rejection of a *single `mcp` dispatcher tool* ("strips per-tool
  schema/permission") stands for **execution**; `invoke` is an
  advertisement/encoding layer only, and both halves of that objection are
  answered structurally: per-tool schemas are preserved (the registry still
  registers real tools with full schemas, and `describe` serves each one's
  schema on demand) and permissions are preserved (the `invoke` envelope is
  unwrapped to the inner name *at the top of the dispatch ladder*, before
  every mask gate, so the whole permission pipeline grades the inner call).
- Relates to: [ADR-0192](0192-universal-advertisement-enforcement-at-dispatch.md)
  — its dispatch-time enforcement is **unchanged**; `invoke` mode is the mode
  that closes the dynamic seams ADR-0192 acknowledged and left open
  (`mcp__*` live-registry mutation, `mcp_enable`'s dynamic enum,
  profile-defining specs), by making the advertised array an immutable lean
  kernel instead of the full registry surface. Also extends
  [ADR-0190](0190-poll-is-always-on-non-maskable-internal-tool.md)'s
  always-on-internal-tool pattern to the discovery trio (see §3).
- Supersedes: nothing.

## Context

Two goals in tension, both load-bearing:

1. **Rich tool surface** — the agent can reach *every* registered capability:
   all built-ins, every MCP tool from every connected or connectable server,
   endpoints, skill-defined tools. A model that cannot even name a tool
   cannot choose it.
2. **Cache-stable history** — the advertised `tools` array is a prompt
   prefix; anything that mutates it mid-session (ADR-0192's motivating
   observation) re-bills the whole prompt at the uncached rate.

Post-ADR-0192 the array is session-stable *except* for the seams it
explicitly acknowledged and left open: `mcp__*` (live registry +
`spec_visible`, so an `mcp_enable` or `McpAdd` mid-session grows or shrinks
the array), `mcp_enable`'s dynamic roster `enum` (schema itself mutated by
enablement state), and the profile-defining specs (only across profiles, not
within a session). Every MCP enablement is still a cache bust. And the
full-surface advertisement itself is a cost: schemas for tools a model will
never use ride every first-turn prompt, and grow with the registry.

Meanwhile ADR-0067's rejection of a single MCP dispatcher was about
*execution*: one fat tool with a stringly-typed name would strip per-tool
schemas and launder permissions through one grade. That rejection stands —
but it was never about *advertisement shape*.

## Decision

Two tool-call modes, selected per model (catalog) with config/env override.

| Mode | Tools array | How dynamic tools reach the model |
|---|---|---|
| `native` (default) | full surface: all registered tools + dynamic MCP specs inline; mutates on add/remove (today's behavior, accepted cache cost) | a new spec appears in the array next turn |
| `invoke` | **immutable lean kernel** + `invoke(name, args)` + discovery meta-tools | **pull-only**: the model lists via `tools`/`skills`, inspects via `describe`, calls via `invoke`. No announcements, no rosters — nothing is pushed. |

### 1. Lean kernel (invoke mode)

High-frequency tools keep native schema fidelity in the array:
`read`, `edit`, `apply_patch`, `write`, `bash`, `poll`, `ask_user`,
`update_tasks`, `load_skill`, `invoke`, `describe`, `tools`, `skills`, plus
the profile-defining specs (`agent`/`agent_send` spawn enum, `propose_plan`)
which vary only across profiles (the ADR-0192 carve-out). Everything else —
`call`, `glob`, `grep`, `rhai`, `mcp_enable`/`mcp_add`/`mcp_remove`, all
`mcp__*`, endpoints, skill tools — stays **registered** (the
`SharedRegistry` is mode-independent) but invoke-reachable and discoverable
only: absent from the array, present in `tools` listings, callable through
`invoke`, inspectable through `describe`.

Because the kernel is a fixed list over mode-static inputs, the advertised
array is **byte-stable for the session's lifetime** — including across
`mcp_enable`, `McpAdd`, and skill loads. That is the point: invoke mode
closes ADR-0192's acknowledged dynamic seams rather than documenting them.

Models without native tool calling are out of scope for `invoke` (they have
no tool calls to route).

### 2. `invoke` is a transparent router, unwrapped at the top of the ladder

`invoke { name: string, args: object }` (absent `args` → `{}`). The envelope
is unwrapped **at the top of the `ToolExec` arm** — after the in-flight
`request_id` dedupe (ADR-0071) and the profile self-heal (ADR-0070), but
*before* the agent/overlay mask, the skill mask's former position, and every
later gate — and the `(tool, input)` pair is replaced in place with the
inner `(name, args)`. Mask, permission grade, hooks, interception, events,
and execution then all operate on the inner name, exactly as if the model
had called it natively.

The unwrap position is load-bearing, not incidental. Unwrapping any later —
e.g. just before `Intercept::classify` — would let an `invoke` reach tools
the mask withdrew, a permission hole. Unwrapping before the dedupe would
break the re-offer idempotency ADR-0071 established. The chosen position
preserves both invariants and every attribution: an `invoke` to a masked
tool declines with the same attributed `Declined by …` message a native
attempt gets (ADR-0192), because it *is* one by the time the ladder runs.

- **`invoke(invoke(...))` is rejected** — no self-nesting; the inner name
  must be a real tool name, and `invoke`/`describe` never appear as inner
  names (the discovery trio carries no arguments worth routing).
- **Parallelism is multiple `invoke` calls in one batch** — no batch
  parameter; ADR-0061's batch-emission concurrency already covers it.
- **History keeps the emitted `invoke(...)` call under the same `call.id`**:
  core folds the `invoke` `ToolCall` and its `ToolResult` as one round-trip;
  the executor's events (`ToolCall`/`ToolRequest`/`ToolOutput`) name the
  inner tool. No cross-mode re-encoding; a native-mode replay of an
  invoke-mode log is out of scope (recorded below).
- In native mode, a stray `invoke` call is an unknown tool.

### 3. Discovery meta-tools — always-on, non-maskable, both modes

`tools(filter?)`, `skills()`, `describe(name)` are internal tools in the
[ADR-0190](0190-poll-is-always-on-non-maskable-internal-tool.md) family:
always-on (advertised in both modes), non-maskable (no mask, overlay, or
profile can withdraw them), always-`Allow` (read-only introspection;
`describe` reads a schema, `tools`/`skills` read an index), and inert (they
touch no host resource and start nothing). This extends ADR-0190's pattern
from one tool (`poll`) to a trio — the same rationale: they only *surface*
work the profile already authorized reaching.

- **`tools(filter?)`**: a terse index (name + one-liner + source) over every
  dynamic source — MCP servers (with three-state status; an `allowed`-tier
  server shows "enable with `mcp_enable`" — listing *its* tools requires the
  model to enable it first; no auto-connect side effects), endpoints, skill
  tools, and the unadvertised built-ins (`call`, `glob`, `grep`, `rhai`, MCP
  management). Always live — computed from the registry at call time, never
  cached — which is what makes announcements unnecessary: there is no
  staleness window to announce across.
- **`skills()`**: the skill index (name + one-liner), replacing the
  system-prompt skills roster in invoke mode.
- **`describe(name)`**: description + parameter schema + one usage example
  for any tool, including MCP (live registry lookup, the server's
  `inputSchema`). Resolves through the session-scoped registry view
  (`overlay_registry_for_call`, [ADR-0188](0188-session-keyed-per-user-mcp-scopes.md))
  so per-user MCP scopes resolve correctly for the asking session.

Pull-only: **no announcements**. Nothing is ever pushed to the model about
tool availability; the model asks (`tools`), inspects (`describe`), and acts
(`invoke`). This is the discovery counterpart of "no rosters in the prompt."

### 4. MCP management stays runtime infrastructure — mode-uniform, still graded

`mcp_enable`/`mcp_add`/`mcp_remove` move conceptually next to the
runtime-owned roster (`poll`, `ask_user`, `update_tasks`) — they are the
runtime's own plumbing ("skutter tools"), not agent tools — but unlike the
discovery trio they **stay maskable and permission-graded**: `mcp_add`
spawns processes, and `mcp_enable`'s grade is `Ask` so each enablement is
user-approved. They are advertised and callable in both modes (invoke-reachable
in invoke mode, via `tools`/`describe`/`invoke`).

`mcp_enable`'s schema becomes **static (`server: string`) in both modes** —
the live roster lives in `tools`/`describe` now, not in a schema enum —
removing the last native-mode cache seam ADR-0192 didn't list. In invoke
mode its result says "enabled, N tools — list with `tools`"; nothing is
announced.

### 5. Mode selection and plumbing

- `ModelEntry.tool_call: Option<ToolCallMode>` (`native | invoke`), the same
  `Option<Enum>` catalog pattern as `thinking_style`/`thinking_format`.
  Precedence: env (`ENTANGLEMENT_TOOL_CALL_MODE`) > `config.yml`
  `tool_call_mode` > catalog > default `native`.
- **Mode is per-session, resolved at session start** from that session's
  initial model, held in a session→mode map in the runtime (the resolver and
  executor are engine-global and session-multiplexed; per-profile model pins
  mean concurrent sessions can differ). A live `SetModel` **keeps** the
  session's mode (logged when the new model's catalog preference differs) —
  switching mode mid-session would bust the cache the mode exists to
  protect, and would strand half-emitted history. Subagents resolve their
  own mode at spawn. No mid-session switching.
- Invoke-mode system prompt: rosters (skills, dynamic tools) are **dropped**
  for a one-line pointer at `tools`/`skills`/`describe`. Native mode keeps
  today's prompt sections.

### 6. Wrong-args declines (both modes)

Three cases, one wording family: (a) user denial → the denial reason only;
(b) malformed arguments → reason + the tool's schema + one example — a bad
`invoke` envelope yields `invoke`'s own definition, wrong inner args yield
the inner tool's definition, wrong MCP args yield the server's
`inputSchema`; (c) runtime failure → the failing output verbatim, nothing
appended. The mechanism is pre-dispatch validation of the input against the
tool's advertised `ToolSpec` schema (subsuming the MCP required-param
pre-check), so an arg-parse failure stops being indistinguishable from a
runtime failure at the executor boundary. Detailed in the architecture docs
(`gates-and-host-tools.md`); the mechanism is common to both modes and not
load-bearing for the mode split, so it is recorded here as a consequence.

## Consequences

### Positive

- Invoke mode reaches every registered tool with a **byte-stable** advertised
  array and system prompt — `mcp_enable`, `McpAdd`, skill loads, and dynamic
  MCP schemas no longer bust the provider prompt cache, closing the seams
  ADR-0192 explicitly left open.
- Prompt weight drops with the surface: first-turn token cost scales with
  the lean kernel (~15 specs), not the registry (which grows without bound
  as MCP servers attach).
- Discovery is always-live and mode-uniform: native-mode models can use
  `tools`/`describe` too, and the always-on trio cannot be withdrawn by a
  profile that would strand the model without it.
- Permissions, masks, hooks, interception, events, and decline attribution
  are all unchanged in substance — the unwrap happens before all of them, so
  every existing guarantee composes.

### Negative / neutral

- Invoke-mode models pay a discovery round-trip (`tools` → `describe` →
  `invoke`) before first use of an unadvertised tool — two extra round-trips
  the native mode doesn't pay, per tool. Accepted: this is the explicit
  rich-surface/cache-stability trade.
- Native mode keeps the full-surface advertisement and its mutation cost on
  add/remove (accepted cache cost; unchanged from today). The seams ADR-0192
  acknowledged remain open in native mode by design — `invoke` is the mode
  for closing them, not a mandate.
- History asymmetry: an invoke-mode session's log carries `invoke(...)`
  envelopes as emitted; cross-mode replay (a native-mode resume of an
  invoke-mode log, or vice versa) is out of scope for v1. A resume re-resolves
  the session's mode from its initial model, per §5.
- `invoke`'s own schema (one spec) plus the trio's (three more) are new
  always-on schema weight in native mode (~4 small specs). Accepted.
- The session→mode map is a new piece of runtime state that must be
  consulted by the resolver and by `SetModel`; it is derived state (from the
  initial model), so it introduces no persistence.

## Alternatives considered

- **A single `mcp` dispatcher tool for everything** (ADR-0067's rejected
  shape, generalized). Rejected for the same reasons ADR-0067 gives: it
  strips per-tool schemas and launders permissions through one grade —
  unless you unwrap before the gates, which is exactly what `invoke` does;
  but then the dispatcher adds nothing over `invoke` + full registration.
- **Invoke as execution-level indirection** (router decides grades). Rejected:
  the router must be transparent or it becomes a permission seam. The unwrap
  position makes it a no-op for enforcement.
- **Push-based discovery** (announce new tools into the conversation when
  they appear — a synthetic system message or a roster refresh). Rejected:
  it mutates history mid-session (cache bust), reintroduces the roster the
  invoke-mode prompt drops, and needs staleness tracking that always-live
  `tools` makes unnecessary.
- **Session-frozen filtered advertisement** (ADR-0192's rejected
  alternative, applied to the full surface). Rejected for its reason: an
  `mcp_enable` would silently stop working until restart. The lean kernel +
  live `tools` gets the cache stability without freezing anything.
- **`invoke` batch parameter** (one call, N inner calls). Rejected:
  ADR-0061's batch-emission already gives parallelism, and a batch envelope
  complicates the request-id/dedupe/history fold for no observed need.
- **Mid-session mode switching.** Rejected: it would bust the cache the mode
  protects and strand half-encoded history; `SetModel` keeps the mode.

## References

- [ADR-0067](0067-mcp-client-as-runtime-tool-provider.md): the single-dispatcher
  rejection this narrows — execution-level, stands unchanged
- [ADR-0192](0192-universal-advertisement-enforcement-at-dispatch.md):
  dispatch-time enforcement, unchanged; the acknowledged dynamic seams invoke
  mode closes
- [ADR-0190](0190-poll-is-always-on-non-maskable-internal-tool.md): the
  always-on internal-tool pattern the discovery trio extends
- [ADR-0188](0188-session-keyed-per-user-mcp-scopes.md): the session-scoped
  registry view `describe` must resolve through
- [ADR-0176](0176-structured-tool-result-is-error-and-duration-fields.md):
  the `is_error` channel declines ride
- [ADR-0071](0071-parked-turn-reoffer-timer.md)/[ADR-0070](0070-authoritative-tool-exec-profile-and-fail-closed-fallback.md):
  the dedupe and self-heal the unwrap position sits after
- [ADR-0061](0061-parked-turn-state-batch-tool-resolution.md): batch emission
  — why no `invoke` batch param is needed
