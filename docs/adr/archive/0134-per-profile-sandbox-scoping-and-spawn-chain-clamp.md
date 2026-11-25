# 0134. Per-profile sandbox scoping for `bash`/`call`, with a spawn-chain confinement clamp

- Status: Accepted
- Date: 2026-07-23
- Amends: [ADR-0104](0104-bubblewrap-sandbox-for-bash-call.md)

## Context

Ledger row 1 ([#396](https://github.com/xmiksay/entanglement/issues/396) epic,
filed as [#479](https://github.com/xmiksay/entanglement/issues/479)), sourced
from [ADR-0104](0104-bubblewrap-sandbox-for-bash-call.md) §3 & "Negative":
"per-profile scoping is the tracked next step."

`ENTANGLEMENT_SANDBOX=bwrap` confined `bash`/`call` for the **whole process**:
`SandboxPolicy::from_env()` was read once in `main.rs` and baked into the two
exec tools as a fixed field at registry-construction time
(`register_default_tools`). A mixed run — a trusted `build` profile
unconfined beside an untrusted `explore` sub-agent confined — was not
expressible; the sandbox was all-or-nothing per `skutter` instance. ADR-0104
§3 named the exact blocker: `AgentProfile` is a wire type shared with every
head, and the tool registry a session dispatches through was profile-agnostic
— one `BashTool`/`CallTool` instance served every session. It also named the
seam that would make per-profile possible once it existed:
[ADR-0088](0088-session-aware-tool-execution.md)'s `Tool::run_for_session`,
which threads the caller's `SessionId` into every execution and has been
available since #360.

## Decision

### 1. `AgentProfile` gains an opaque `sandbox:` field

`sandbox: Option<String>` joins `AgentProfile` ([entanglement-core][protocol])
alongside `permission`: core carries and serializes it but never interprets
it, exactly like `PermissionProfile`'s `Allow`/`Ask`/`Deny` rules — the
runtime's `host::sandbox` module owns the `bwrap` mechanism and is the only
code that gives the string meaning. Frontmatter parsing lives in
`agents::build_profile`: `bwrap`/`bubblewrap` ⇒ confined, `none` ⇒ forced
unconfined, `inherit`/omitted ⇒ defer to the process-global
`ENTANGLEMENT_SANDBOX` default (the same "no pin" sentinel `model`/`provider`
already use). Any other value is a loud load error, matching every other
frontmatter key's strictness — never a silently-ignored typo.

This was the more direct alternative to a runtime-only sidecar table keyed by
profile name: the session→profile map (`tool_runner`'s `active:
Arc<Mutex<HashMap<SessionId, AgentProfile>>>`, already shared with
`ProfileResolver`) already gives every consumer the profile's `name` *and*
now its `sandbox` field for free — no second load-time table to keep in sync
with `ProfileRegistry`'s own layering (embedded < user < project).

### 2. The exec tools hold a resolver, not a fixed policy

`BashTool`/`CallTool` replace their `sandbox: SandboxPolicy` field with
`sandbox_resolver: Arc<dyn policy::SandboxResolver>`
(`fn resolve(&self, session: Option<&SessionId>) -> SandboxPolicy`), consulted
inside `run_impl` on every call — mirroring `PermissionResolver`'s shape but
synchronous (no `Ask` round-trip, no DB lookup a real embedder would need to
await for a sandbox decision). `session: None` is the plain
[`Tool::run`][ADR-0088] path (no live session — standalone use, most unit
tests).

A fixed `SandboxPolicy` is trivially its own resolver
(`impl SandboxResolver for SandboxPolicy`), so the pre-#479
`.with_sandbox(policy)` builder is unchanged and every existing call site
(tests, the `embedded` example) keeps compiling untouched.

Landing alongside [ADR-0133](0133-live-bash-enablement-graded-by-permission.md)'s
live bash enablement (#498) meant `bash_live::BashToolConfig` — the state a
later `/bash on` needs to build a fresh `BashTool` — also switched from a
fixed `sandbox: SandboxPolicy` field to `sandbox_resolver: Arc<dyn
SandboxResolver>`, wired from the same `SandboxConfig::resolver()` the
startup-registered pair uses. Without this, a live-enabled `bash` would have
frozen whatever `SandboxPolicy` was captured at startup, silently bypassing a
profile's `sandbox:` override the moment `/bash on` ran.
`.with_sandbox_resolver(resolver)` is the new builder the runtime head wires.

### 3. Precedence: profile setting > `ENTANGLEMENT_SANDBOX` env

`SandboxPolicy::resolve_profile_override(over: Option<&str>)` resolves a
profile's `sandbox` string against the process-global default: `None`
inherits the default unchanged; `bwrap`/`bubblewrap` confines (keeping the
default's `network` posture — no separate per-profile network knob); `none`
forces unconfined regardless of the env var. An unset per-profile key is thus
byte-identical to today under both `ENTANGLEMENT_SANDBOX` values — the env
stays the global default exactly as ADR-0104 shipped it.

### 4. Spawn-chain clamp: most-confined wins, mirroring ADR-0024

A confined parent must not spawn an unconfined child. `SandboxPolicy` gains a
confinement ordering — `SandboxBackend::None` (0) < `Bubblewrap` with network
shared (1) < `Bubblewrap` with network cut (2) — and `most_confined(self,
other)` picks the higher rank, the confinement-axis mirror of
[ADR-0024](0024-subagent-permission-gating.md)'s least-privileged-wins
permission ceiling.

Unlike the permission ceiling, this is **not** re-derived live on every call
by walking `SpawnGuard`'s parent chain — that would require sharing
`tool_runner`'s single-threaded `SpawnGuard` outside its dispatch loop (it is
today a plain, unlocked local variable; making it an `Arc<Mutex<..>>` shared
with two more consumers was a materially larger change for the same
observable result). Instead, a session's ancestor **floor** is computed once,
at `SessionStarted`, from its parent's *already-resolved* effective
confinement (`policy::record_session_sandbox`) and cached in a second shared
map alongside the session's own resolved policy
(`policy::SandboxConfig { base, own, floor }`). Composition is transitive by
construction: a grandchild's floor is folded from its parent's floor, which
was itself folded from the grandparent's — a multi-level spawn chain clamps
correctly with no explicit chain walk at read time
(`policy::resolve_sandbox` just does `own.most_confined(floor)`, an O(1)
lookup). `SandboxResolver::resolve` (what `BashTool`/`CallTool` call per
call) and `tool_runner`'s `SessionStarted` handler share the identical
`resolve_sandbox` helper, so the two can't drift.

The floor is frozen at spawn, not re-derived on a later `AgentChanged`
(`SetAgent`): a mid-session profile switch recomputes only the switching
session's own policy (`policy::record_own_sandbox`), never its ancestor
floor. A parent that later switches to a *more* confined profile does not
retroactively tighten an already-spawned child's floor. This is a deliberate
scope cut (see Alternatives) — the permission ceiling has the identical
property in practice, since `effective_permission`'s live ancestor walk reads
each ancestor's *current* profile from the same `active` map, but sandbox's
floor is a value snapshot rather than a walk for the reason above, so it
doesn't automatically pick up a later ancestor change the way permission
does.

### 5. Wiring: one `SandboxConfig` created early, shared into both halves

`register_default_tools` (constructs `BashTool`/`CallTool`, called early in
`main.rs::build_config`) and `spawn_tool_executor_with_policy` (constructs the
dispatch loop that folds lifecycle events, called later once the engine's
`Holly` exists) need the *same* `own`/`floor` maps — one writes, the other's
resolver reads. `policy::SandboxConfig { base: SandboxPolicy, own:
Arc<Mutex<HashMap<SessionId, SandboxPolicy>>>, floor: Arc<Mutex<HashMap<..>>>
}` is created once in `build_config` (`SandboxConfig::from_env()`, mirroring
`SandboxPolicy::from_env()`), threaded out through `build_config`'s return
tuple, and passed by reference into both: `sandbox_config.resolver()` into
`register_default_tools`, `sandbox_config` (cloned — every field is
`Copy`/`Arc`) into `spawn_tool_executor_with_policy`. `SandboxConfig::none()`
is the zero-wiring default every test helper, the `embedded` example, and the
four-/five-arg `spawn_tool_executor`/`_with_hooks` convenience wrappers pass —
byte-identical to pre-#479 behavior.

## Consequences

- **(+)** A mixed run is now expressible: two profiles in one process run
  `bash`/`call` confined and unconfined respectively (integration test in
  `host/bash.rs`, skippable when `bwrap` is absent, matching every existing
  sandbox test's skip convention).
- **(+)** Spawn-chain clamp is decided, enforced, and tested at both the pure
  resolver level (`policy.rs` unit tests: unseen-session fallback, own
  override, ancestor-floor clamp) and the population level
  (`record_session_sandbox`'s multi-level spawn-chain test, and the
  never-loosens-a-child's-own-stricter-choice test) — without needing a full
  engine turn loop to exercise `tool_runner`'s `SessionStarted` handler.
- **(+)** Unset per-profile key stays byte-identical to today under both
  `ENTANGLEMENT_SANDBOX` values — `resolve_profile_override(None)` is the
  identity function.
- **(+)** `AgentProfile`'s existing carry-opaque-data precedent (`permission`)
  extends cleanly to `sandbox` — no new sidecar table, no `load_registry`
  signature change, no new field on the dozens of existing `AgentProfile`
  test-literal construction sites beyond the one mechanical `sandbox: None`
  each already needed for the struct to keep compiling.
- **(−)** The ancestor floor is a value snapshot at spawn, not a live walk — a
  parent's later `SetAgent` to a more confined profile does not retroactively
  tighten an already-running child's floor (see Decision §4 and Alternatives).
- **(−)** No per-profile network-sharing override — `network` still comes
  from the process-global `ENTANGLEMENT_SANDBOX_NETWORK` only; a profile can
  select the backend (`bwrap`/`none`) but not independently ask for network
  access back once confined. Narrower surface to reason about for v1; revisit
  if a concrete profile needs network access under confinement while sibling
  profiles don't.

## Alternatives considered

- **Live ancestor-chain walk on every call**, sharing `tool_runner`'s
  `SpawnGuard` behind an `Arc<Mutex<..>>` so `SandboxResolver::resolve` could
  walk parent links exactly as `effective_permission` does. Rejected for v1:
  requires converting every one of `SpawnGuard`'s existing unlocked call sites
  inside the dispatch loop to take a lock, for a property (a parent's
  *later* profile switch retroactively re-tightening an already-spawned
  child) no acceptance criterion asked for. The frozen-floor design gets the
  spawn-time clamp — the actual ask — with a strictly smaller diff.
- **A runtime-only sidecar `HashMap<String, SandboxPolicy>` keyed by profile
  name**, leaving core's `AgentProfile` untouched. Rejected: `load_registry`
  would need a second return value (or a wrapper type) threaded through every
  caller (`main.rs`, `inspect`, the watcher's live-reload snapshot), while
  `AgentProfile` already carries one opaque, runtime-interpreted field
  (`permission`) — extending that existing precedent touches only the struct
  literal sites (mechanical) instead of every load-path signature.
- **Per-profile network override alongside the backend choice.** Rejected for
  v1 as unrequested scope: the issue's acceptance criteria are backend
  scoping (confined vs. not) and the spawn clamp, not a second per-profile
  knob; `network` staying process-global is also the more conservative
  default (a profile cannot silently reopen egress the operator cut
  globally).
- **Recompute the ancestor floor on every `AgentChanged` by re-walking to the
  root.** Rejected as unnecessary complexity for a case (a parent's mid-life
  `SetAgent` reaching down to retighten an already-spawned child) the
  permission ceiling itself doesn't obviously need either in practice — a
  spawned child's floor reflecting its parent's confinement *at spawn time*
  is a defensible, simpler-to-reason-about contract, and nothing in the issue
  asked for live re-tightening.

[protocol]: ../architecture/protocol.md
