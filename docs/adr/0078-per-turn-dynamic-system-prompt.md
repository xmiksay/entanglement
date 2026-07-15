# 0078. Per-turn dynamic system prompt (`EngineConfig.system_prompt_resolver`)

- Status: Accepted
- Date: 2026-07-15
- Sibling of [0076](0076-per-session-dynamic-tool-specs.md) (per-session tool specs): the same construction-time-resolver seam, applied to the system prompt instead of the tool surface. Hard blocker for multi-tenant embedding (#307). Issue #310.

## Context

The system prompt is fixed at engine spawn. A session runs under an
`AgentProfile`, and its `system_prompt` field is read at stream time
(`session/stream.rs`) to fill `LlmRequest.system`. Profiles come from the layered
definition loader and don't change for the life of a `Holly`.

That is fine for a coding-agent head, but it forecloses an embedder **whose
prompt is user-editable content** — e.g. a site that serves its system prompt
from a CMS page (`system/prompt`). Today such an embedder can only pick up an
edit by **respawning the engine**, which also tears down every live session. The
prompt is dynamic; the profile is static — there is no seam between them.

## Decision

**Add an optional per-turn resolver to `EngineConfig`, consulted at turn-build
time, whose `Some` return overrides the active profile's `system_prompt` for that
turn.**

```rust
pub type SystemPromptResolver =
    Arc<dyn Fn(&SessionId, &AgentProfile) -> Option<String> + Send + Sync>;

pub struct EngineConfig {
    // ...
    pub system_prompt_resolver: Option<SystemPromptResolver>,
}
```

### Where it is consulted

`run_round` (`entanglement-core/src/session/turn.rs`) resolves the prompt once at
the top of every LLM round-trip and hands it to `stream_round`, which was reading
`s.profile.system_prompt` directly:

```rust
let system_prompt: String = cfg
    .system_prompt_resolver
    .as_ref()
    .and_then(|resolve| resolve(session, &s.profile))
    .unwrap_or_else(|| s.profile.system_prompt.clone());
// ... stream_round(..., &specs, &system_prompt)
```

Because it is consulted **fresh every turn**, an embedder that mutates its
backing store (an admin editing the CMS prompt page) sees the new prompt on the
**next turn** — no engine respawn, no session teardown.

### Semantics — override, else fall back

- **`Some(prompt)` overrides; `None` falls back.** A present resolver returning
  `None` for a turn (or an absent resolver entirely) uses the profile's own
  `system_prompt`. The override is opt-in *per turn*, so an embedder can key it on
  session, profile, or its own state and leave the rest on the static prompt.
- **The resolver sees `(&SessionId, &AgentProfile)`.** It receives the running
  session's *own* id and resolved profile — so a sub-agent turn (researcher /
  page-writer) is resolved against *that child's* profile, keeping per-profile
  prompts working. An embedder that wants tenant context keys off the root
  session id instead. Passing the profile also lets a resolver compose — prepend
  a per-tenant preamble to `profile.system_prompt` rather than replace it.

### Sync `Fn`, snapshot-cache pattern

The closure is deliberately synchronous — the turn path must not block on I/O.
The documented pattern (same as [0076](0076-per-session-dynamic-tool-specs.md))
is an embedder-owned snapshot cache (`Arc<RwLock<HashMap<SessionId, String>>>`)
hydrated from its store out of band; the resolver just reads the current
snapshot. Core stays free of any store/async concern while still reflecting edits
on the next turn.

`None` (the default) is a pure no-op: every turn keeps the profile's static
prompt, so nothing changes for a single-user head.

## Consequences

- **Prompt edits land on the next turn, not on respawn.** A CMS-backed prompt is
  live-editable without tearing down sessions — the exact cost this seam removes.
- **Sub-agent prompts keep working.** The resolver is handed the child's
  profile, so per-profile prompts are unaffected; an embedder that doesn't wire
  the resolver is byte-for-byte unchanged.
- **No protocol / wire change.** A construction-time knob on `EngineConfig` (like
  `model_resolver`, [0063](0063-realtime-model-provider-switch.md), and
  `tool_spec_resolver`, [0076](0076-per-session-dynamic-tool-specs.md)); the
  frozen wire ([0072](0072-protocol-warts-settled-before-serve.md)) is untouched.
- The `Fn` runs on the hot turn path; a resolver that does real work (I/O, lock
  contention) would stall the turn. The contract is "read a snapshot"; the doc
  comment says so.

## Alternatives rejected

- **A `system_prompt` field on `InMsg::Prompt` / `SetAgent`.** Puts the prompt on
  the wire and makes an untrusted head able to set the system prompt — a
  privilege the frame split ([0069](0069-trusted-untrusted-wire-frame-split.md))
  deliberately withholds. The resolver is an in-process construction-time knob,
  never serialized.
- **A mutable prompt field on `Session` updated by a new `InMsg`.** Adds a wire
  message and session state for what is really an embedder-owned lookup; still
  can't reflect an external CMS edit without the embedder pushing every change in.
  A pull-per-turn resolver is simpler and always fresh.
- **An `async` resolver.** Would let the closure hit the CMS directly, but puts
  I/O on the turn path. The snapshot-cache pattern gets freshness without blocking.
- **Respawn the engine on prompt edit.** The status quo. Tears down every live
  session on one prompt change — the problem this ADR exists to remove.
- **Reuse `tool_spec_resolver`'s `Fn(&SessionId)` signature.** Would drop the
  profile, breaking per-profile sub-agent prompts and the compose-from-profile
  pattern. The extra `&AgentProfile` arg is cheap and load-bearing.
