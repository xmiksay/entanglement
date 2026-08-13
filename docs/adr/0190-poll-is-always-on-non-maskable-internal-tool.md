# 0190. `poll` is an always-on, non-maskable internal tool

- Status: Accepted
- Date: 2026-08-13
- Amends: [ADR-0161](0161-unified-async-work-background-flag-and-one-poll.md)
  (§3's "poll bypasses permission" — extended: `poll` is also exempt from the
  #116 advertisement mask), [ADR-0076](0076-per-session-dynamic-tool-specs.md)
  (the resolver contract: it must include the always-on tools)
- Relates to: [ADR-0038](0038-physical-per-agent-tool-restriction.md) (the #116
  mask this carves an exemption into)

## Context

`poll` (#605, [ADR-0161](0161-unified-async-work-background-flag-and-one-poll.md))
is the single collection mechanism for all async work in the engine — it joins
background `bash`/`call`/`rhai` jobs ([ADR-0185](0185-rhai-joins-background-and-poll.md))
and sub-agent handles ([ADR-0162](0162-agent-send-supervising-a-sub-agent.md)),
and lists a session's pending operations when called without a handle. ADR-0161
§3 already established that `poll` **starts nothing and touches no host
resource** — it reads state a previously-graded launch produced — so it is
intercepted before permission resolution and carries no grade of its own.

It is a crucial internal orchestration primitive, not a host tool a profile
author should be able to withdraw. But it failed on **two** counts:

### Bug 1 — `poll` was never advertised to the model in production

The runtime builds the advertised surface twice. The static
`EngineConfig.tool_specs` pushes `poll_spec()` alongside `update_tasks`/
`ask_user`/`rhai`, but that list is now only the `/agent` tools-checklist
roster. What the model actually sees comes from `tool_spec_resolver`
([ADR-0076](0076-per-session-dynamic-tool-specs.md)), which core consults fresh
every turn and whose output *replaces* the static list for the session. The
resolver rebuilt the surface from registry tools + a `runtime_owned_specs` vec
that listed `update_tasks`/`ask_user`/`rhai` — **but not `poll`**. So `poll`'s
schema never reached any real LLM under a resolver-equipped engine (every
shipped head). Tests missed it because they use `EngineConfig` without a
resolver (the static-list fallback, which *did* carry `poll`) and scripted LLMs
that emit hardcoded `poll` calls regardless of advertisement; the executor's
name-based interception (`tool_runner.rs`) executes a `poll` call that arrives
whether or not its spec was advertised.

### Bug 2 — even when advertised, `poll` was maskable away

The #116 profile mask ([ADR-0038](0038-physical-per-agent-tool-restriction.md))
filters every shared spec through a profile's `tools`/`disallowed_tools`. A
profile that omits `poll` from its `tools:` allowlist — or lists it in
`disallowed_tools:` — withdraws it. But `poll` only collects work the profile
*already* authorized creating: `bash`/`call`/`rhai`/`agent` launchers are the
graded capability decisions, and each is independently maskable. Withdrawing
`poll` strands background jobs and sub-agent handles the profile's own launchers
produced — it reduces operability, not capability.

`poll`'s siblings sit on the other side of this line: `agent`/`agent_send` are
per-profile gated because *spawning* is a real capability decision, and
`ask_user` is an interactive surface a headless batch profile might legitimately
suppress. `poll` is neither.

## Decision

`poll` becomes a named always-on internal tool in core, exempt from both the
resolver-omission and the profile mask.

### 1. `ALWAYS_ADVERTISED_TOOLS: &["poll"]` in core

A small named constant of always-on internal tools lives in
`entanglement-core/src/protocol.rs`, alongside the `AgentProfile` mask helpers.
The advertisement filter in `run_round` (`session/turn.rs`) short-circuits on
`AgentProfile::is_always_advertised(name)` **before** consulting the profile
mask *or* the session overlay (#539): an always-on tool is advertised
unconditionally and cannot be withdrawn even by an explicit overlay deny entry.

The constant is deliberately a short, narrow list. Each entry is an exemption
from the #116 physical restriction, so adding one removes a profile-author
control and should be a deliberate decision (widening is a one-line change to
the constant, with this ADR as the precedent).

### 2. The resolver reproduces the full runtime-owned roster

`tool_spec_resolver` in `entanglement-runtime/src/main.rs` now includes
`poll::poll_spec()` in its `runtime_owned_specs` vec alongside
`update_tasks`/`ask_user`/`rhai`, mirroring the static `cfg.tool_specs` the
resolver replaces. The resolver is the one seam core consults per turn; a
tool whose spec the resolver omits is invisible to the model regardless of any
mask exemption, because the exemption filters the specs it is *given* — it does
not synthesize specs. So the two changes are coordinated: advertise `poll`
unconditionally **and** make sure its spec reaches the filter.

### 3. Dispatch-side correctness is already covered

The runtime dispatch (`tool_runner.rs`) does **not** re-check the profile mask
for the `agent`/`agent_send`/`poll`/`ask_user` interception family — it trusts
the advertised surface and routes by name. So once `poll` is advertised, a
`poll` call always executes; no dispatch-side change is needed. (The `tool_masked`
enforcement gate that *does* run before interception is itself now a no-op for
`poll`, since the same filter that exempts it from advertisement is the one that
determines whether it reaches the model — a hallucinated `poll` call under a
masking profile would be the one place a masked tool executes, but `poll`
starts nothing, so this is harmless and consistent with ADR-0161 §3.)

## Consequences

### Positive

- `poll` is always reachable by every profile, fixing the live bug where the
  planning agent (and any real LLM under a resolver-equipped engine) saw no
  `poll` tool and could not collect background handles.
- A profile author can no longer accidentally strand async work by omitting
  `poll` from an allowlist — the one footgun ADR-0161's `explore` bug
  illustrated (a read-only agent that could start a job but had nothing to read
  it) is structurally closed.
- The exemption seam is named and narrow, with room to grow deliberately.

### Negative / neutral

- `poll` is no longer withdrawable by any profile, even one that has no
  business starting background work. This is acceptable because `poll` is inert
  without a handle to collect, and the launchers that produce handles are still
  maskable.
- The `ALWAYS_ADVERTISED_TOOLS` constant is a second source of truth alongside
  the runtime-owned specs list — the resolver must list a tool's spec for it to
  be advertised at all, and the constant exempts an advertised tool from the
  mask. The two must agree: a future always-on tool added to the constant but
  omitted from the resolver would be exempt from a mask it never faces.
- The `tool_masked` dispatch gate is technically bypassed for `poll` (a masked
  `poll` call would execute). This is consistent with ADR-0161 §3's "poll starts
  nothing" rationale and produces no observable capability leak.

## Alternatives considered

- **Widen the constant to include `ask_user` now.** `ask_user` is also
  runtime-owned and non-host, but it is an interactive surface a headless batch
  profile might legitimately suppress. Kept to `["poll"]` per the planning
  scope; widening later is one line.
- **Put the exemption in the resolver rather than the mask.** A resolver could
  inject `poll` unconditionally. Rejected: the resolver is an embedder-owned
  seam (ADR-0076), and a multi-tenant embedder's resolver is explicitly allowed
  to vary the surface per session — relying on every resolver implementation to
  remember `poll` reproduces Bug 1. The exemption belongs in core, where the
  mask lives.
- **Make `poll` a per-profile tool (`profile_tool_specs`) like `agent`.** That
  would make `poll` opt-in, the opposite of the goal: `poll` must be present
  wherever a launcher is, and per-profile gating would require every profile
  that lists `bash`/`call`/`rhai`/`agent` to also remember `poll`.

## References

- [ADR-0161](0161-unified-async-work-background-flag-and-one-poll.md): the
  unified async-work model `poll` joins — §3 established `poll` bypasses
  permission; this extends the principle to the advertisement mask
- [ADR-0076](0076-per-session-dynamic-tool-specs.md): the resolver contract this
  relies on (must include always-on tools) and Bug 1's home
- [ADR-0038](0038-physical-per-agent-tool-restriction.md): the #116 mask this
  carves an exemption into
- [ADR-0162](0162-agent-send-supervising-a-sub-agent.md),
  [ADR-0185](0185-rhai-joins-background-and-poll.md): the handle kinds `poll`
  collects
