# 0076. Per-session dynamic tool specs (`EngineConfig.tool_spec_resolver`)

- Status: Accepted
- Date: 2026-07-15
- Refines the per-turn spec assembly of [0040](0040-per-profile-spawn-control.md) (per-profile specs) and rides the [0067](0067-mcp-client-as-runtime-tool-provider.md) MCP tool provider. Hard blocker for multi-tenant embedding (#307). Issue #308.

## Context

`EngineConfig` is the immutable input an embedder hands `Holly::spawn`, and its
advertised tool surface is engine-global: `tool_specs` is one list for every
session, and `profile_tool_specs` varies it only by *profile name*, not by
*session*. Both are the same across every session a single engine supervises.

That is fine for a single-user head, but it forecloses **multi-tenant
embedding** — one `Holly` serving many users. Two problems, both unexpressible
today:

- **Cross-tenant leakage.** User A's MCP-server tools (#198,
  [0067](0067-mcp-client-as-runtime-tool-provider.md)) are discovered from *A's*
  per-user config. With a global `tool_specs`, those schemas would be advertised
  to user B's sessions on the same engine.
- **Per-session restriction.** A site policy like "this session may only see
  MCP servers in `enabled_mcp_server_ids`" has nowhere to live — the tool
  surface can't be narrowed for one session while another keeps the full set.

The only workaround is **one engine per user**, which forfeits shared
supervision, multiplies executors and persistence taps, and forces an engine
respawn on every MCP-server edit. A per-session seam is needed instead.

## Decision

**Add an optional per-session resolver to `EngineConfig`, consulted at
turn-build time, whose output replaces the engine-global base specs for that
session — under the same profile mask.**

```rust
pub type ToolSpecResolver = Arc<dyn Fn(&SessionId) -> Vec<ToolSpec> + Send + Sync>;

pub struct EngineConfig {
    // ...
    pub tool_spec_resolver: Option<ToolSpecResolver>,
}
```

### Where it is consulted

`run_round` (`entanglement-core/src/session/turn.rs`) assembles the advertised
specs at the top of every LLM round-trip. The base list is now:

```rust
let base_specs = match &cfg.tool_spec_resolver {
    Some(resolve) => resolve(session),      // per-session
    None          => cfg.tool_specs.clone(), // engine-global (unchanged default)
};
```

everything downstream is untouched. Because it is consulted **fresh every
turn**, an embedder that mutates its backing store (a user editing their MCP
servers) sees the new surface on the **next turn** — no engine respawn.

### Semantics — replace, then mask

- **Replace, not merge.** When the resolver is present its output *replaces* the
  static `tool_specs` for that session. Composition is the embedder's job: if it
  wants the global set plus per-user extras, it concatenates them inside the
  closure. Merge-in-core would force one policy on every embedder and make "this
  tenant sees *fewer* tools than the global set" impossible to express.
- **`profile_tool_specs` still append.** The per-profile specs
  ([0040](0040-per-profile-spawn-control.md)) — the spawnable `agent_*` roster and
  the plan-authorship tools ([0049](0049-plan-task-tools-as-runtime-state-tools.md))
  — are layered on top of the resolved base exactly as before. The resolver
  varies the *host* tool surface per session; it does not touch profile-scoped
  tooling.
- **The profile mask still applies on top.** Both the resolved base and the
  appended profile specs are filtered through `AgentProfile::advertises_tool`
  (the #116 allow/deny mask). The resolver **widens discovery, it never bypasses
  masking**: a session under a read-only `explore` profile cannot be handed
  `edit` by a resolver — the mask filters it out after resolution. This keeps
  the mask the single physical restriction it has always been; the resolver only
  changes *what is discovered before* it.

### Sync `Fn`, snapshot-cache pattern

The closure is deliberately synchronous — the turn path must not block on I/O.
The documented pattern is an embedder-owned snapshot cache
(`Arc<RwLock<HashMap<SessionId, Vec<ToolSpec>>>>`) hydrated from its store out of
band; the resolver just reads the current snapshot. This keeps core free of any
store/async concern while still reflecting edits on the next turn.

`None` (the default) is a pure no-op: every session keeps the engine-global
`tool_specs`, so nothing changes for a single-user head.

## Consequences

- **Multi-tenant embedding on one `Holly`.** Different sessions advertise
  disjoint tool surfaces; a per-user MCP-server set or a site restriction is now
  expressible without one engine per user — shared supervision, one executor,
  one persistence tap.
- **Edits land on the next turn, not on respawn.** The resolver is consulted per
  turn, so mutating the backing store is reflected without tearing down the
  engine.
- **No protocol / wire change.** The resolver is a construction-time knob on
  `EngineConfig` (like `model_resolver`, [0063](0063-realtime-model-provider-switch.md),
  and the web-search config, [0075](0075-provider-side-web-search-mvp.md)); the
  frozen wire ([0072](0072-protocol-warts-settled-before-serve.md)) is untouched.
- **Masking invariant preserved.** The resolver sits *before* the mask, so it
  can never grant a tool the profile denies — the security posture is unchanged.
- The `Fn` runs on the hot turn path; a resolver that does real work (I/O, lock
  contention) would stall the turn. The contract is "read a snapshot"; the doc
  comment says so.

## Alternatives rejected

- **Merge resolver output into `tool_specs` in core.** Simpler to reason about
  for the additive case, but forces one composition policy on every embedder and
  makes a *narrowing* per-session surface (fewer tools than global) impossible.
  Replace + embedder-composes is strictly more general.
- **A per-session `EngineConfig` field baked at spawn** (e.g. a
  `HashMap<SessionId, Vec<ToolSpec>>`). Static — it can't reflect a mid-session
  MCP-server edit without a respawn, which is the exact cost we're removing.
- **An `async` resolver.** Would let the closure hit the store directly, but puts
  I/O on the turn path and complicates the seam. The snapshot-cache pattern gets
  freshness without blocking the turn.
- **One engine per tenant.** The status quo. Forfeits shared supervision,
  multiplies executors/persistence taps, and respawns on every config edit — the
  problem this ADR exists to remove.
- **Resolving *after* the mask (letting the resolver override masking).** Would
  turn the resolver into a permission bypass, collapsing the physical-restriction
  invariant the mask guarantees. The resolver is discovery, not policy.
