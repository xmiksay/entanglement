# 0081. Per-agent-profile provider/model pinning, and rebind on `SetAgent`

- Status: Accepted
- Date: 2026-07-15
- Builds on the realtime model/provider switch of
  [0063](0063-realtime-model-provider-switch.md) (the `model_resolver` seam this
  reuses), the file-defined agent profiles of
  [0034](0034-file-defined-agent-profiles.md), the local trust boundary of
  [0047](0047-local-trust-boundary.md) (the managed store is trusted, like the
  grants file [0052](0052-approval-scope-and-persisted-grants.md) and the env
  file [0073](0073-managed-env-file-writer-and-key-surfaces.md)), and the frozen
  wire of [0069](0069-trusted-untrusted-wire-frame-split.md) /
  [0072](0072-protocol-warts-settled-before-serve.md). Issue #323 (part of #302).

## Context

Agent frontmatter carries `model:` but no `provider:`, and `SetAgent` (core
`session.rs`) only swapped `s.profile` — it never rebuilt `s.llm`,
`s.generation`, or the context-window budget. So a profile-pinned model only
took effect *within the startup provider endpoint*: a `plan` profile pinning a
model on a different provider was impossible, and even a same-provider pin was
only a per-request `model` field, never a real backend switch. Meanwhile a live
`/model` choice (`SetModel`, the full re-bind path of
[0063](0063-realtime-model-provider-switch.md)) was session-only and never
persisted.

The goal: each profile (Plan, Build, a cheap-model explore) carries its own
`provider`+`model`; switching to it re-binds the session's backend; and picking
a model via `/model` while a profile is active **persists** for that profile
across restarts.

## Decision

**The rebind lives in core's `SetAgent` handler**, driven off the existing
`model_resolver` seam ([0063](0063-realtime-model-provider-switch.md)); the
**runtime** decides *which* model a profile pins (persisted file > frontmatter)
and injects it into the assembled `AgentProfile`. Core stays policy-free — its
addition is pure mechanism.

### Protocol (core)

- `AgentProfile` gains `provider: Option<String>` (`#[serde(default)]`,
  back-compat) beside the existing `model`. A profile with **both** set is a
  *model pin*, exposed as `AgentProfile::model_pin() -> Option<(&str, &str)>`.
  `model` alone → `None` (the legacy request-level fallback, documented, no
  rebind); `provider` without `model` is a **loud load error** in the runtime
  frontmatter parser (a provider with nothing to run is meaningless).

### Engine (core)

- `Session` gains `provider: Option<String>` (the provider the live backend is
  bound to — the **no-op guard** so an already-correct binding isn't rebuilt)
  and `profile_models: HashMap<String, (String, String)>` (per-profile session
  memory of `/model` choices).
- The `SetModel` success arm is factored into `Session::rebind(...)`: rebuild
  `llm`, retarget `model` + `generation` + context window, track `provider`,
  emit `ModelChanged`. `SetModel` also records the choice into `profile_models`.
- `SetAgent` computes the target pin as **`profile_models[name]` (memory) >
  `profile.model_pin()` (static)** and rebinds only when it differs from the
  live binding — so a pin-less profile with no memory keeps the current binding
  (**no `ModelChanged`**), and a live override survives an agent switch. The
  `AgentChanged` is emitted first regardless; a resolver error surfaces the same
  `Error` as `SetModel` and keeps the old binding.
- **Session start** applies the starting profile's pin when no model is bound
  yet (`s.model.is_none()`), covering the default `build` session and spawned
  sub-agents (a cheap-model `explore` becomes possible). Best-effort:
  warn-not-`Error` on failure, matching replay.
- **Replay** tracks the active profile across the fold and reconstructs
  `profile_models` + `provider` from each `ModelChanged` record, so a resumed
  session re-binds and re-applies per-profile memory exactly like the live one.

One `SetAgent` may now be followed by a `ModelChanged` (pin applied) or an
`Error` (pin resolve failed), both already folded in log order.

### Storage (runtime)

- A **managed, not layered** file `${config_dir}/entanglement/agent-models.yml`
  (override `ENTANGLEMENT_AGENT_MODELS_FILE`), sibling of the grants + env files,
  shape `agents: { build: { provider: zai, model: glm-5.2 } }` (a `BTreeMap` for
  stable output, grants-style header comment). `AgentModelStore { load, get,
  set, apply }`: `apply` overlays persisted pins onto the loaded
  `ProfileRegistry` at startup (**persisted file > frontmatter**), consulted
  before the engine builds. Missing/malformed → empty + warn (fail-open); a
  write failure is logged, never fatal. `atomic_write` is extracted out of
  `config::env_key` into a shared `config::atomic`; `env_key` delegates.

### TUI persist-on-confirmation

The `/model` picker's Enter sends `SetModel` and records
`pending_model_persist = (agent, provider, model)`; the **matching**
`ModelChanged` for the active session (same provider+model) commits the atomic
write + a transcript status line; an `Error` clears the pending without writing.
A `ModelChanged` caused by a `SetAgent` pin application has no pending recorded,
so it never writes.

## Consequences

- One locus (core `SetAgent`) covers Tab cycle, the `/agent` picker, `--agent`,
  spawn, and wire clients; replay stays consistent for free; mid-turn deferral
  is inherited from the existing `SetAgent` stash.
- Sub-agents can pin cheaper/faster models per role without any head change.
- The precedence chain (session memory > static pin > current binding) means a
  live `/model` choice wins per profile for the session and, once persisted,
  across restarts — while a pin-less profile never clobbers a live override.
- MVP limits: `model:` without `provider:` stays request-level (no rebind) —
  documented as legacy; auto-migration of an old `model`-only frontmatter into a
  pin is out of scope.

## Rejected alternatives

- **Head-driven follow-up `SetModel`** after each `SetAgent`. Every head (TUI,
  wire, spawn, `--agent`) would have to re-implement the pin lookup and race the
  agent switch; replay would need a synthesized follow-up. Putting it in core's
  `SetAgent` makes one mechanism serve all entry points and keeps replay honest.
- **A layered `config.yml` section** for the pins. The runtime rewrites this
  file on every `/model` confirmation; a layered, hand-edited, `deny_unknown_fields`
  config is the wrong home (same reasoning as the grants file,
  [0052](0052-approval-scope-and-persisted-grants.md)). A managed sibling file
  keeps machine-written state out of the user's config.
- **Live registry mutation** (rewriting `EngineConfig.profiles` at runtime on a
  `/model` pick). Core holds the registry immutably; the switch already has a
  clean seam (`model_resolver` + per-session `Session` fields), so mutating the
  shared registry would add a concurrency hazard for no gain. The pin lives on
  `Session`, applied through the resolver, exactly like `SetModel`.
