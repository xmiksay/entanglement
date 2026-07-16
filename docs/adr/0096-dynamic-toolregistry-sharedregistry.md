# 0096. Dynamic `ToolRegistry`: `SharedRegistry` replaces value-ownership in the executor

- Status: Accepted
- Date: 2026-07-16
- Builds on [0059](0059-tool-trait-and-registry-live-in-the-runtime.md) (`Tool`/`ToolRegistry` live in the runtime), [0067](0067-mcp-client-as-runtime-tool-provider.md) (MCP tools registered into the registry), [0076](0076-per-session-dynamic-tool-specs.md) (`EngineConfig.tool_spec_resolver`, the seam this ADR finally wires up), and [0079](0079-pluggable-permission-resolver-and-grant-store.md)/[0084](0084-runtime-live-reload-and-managed-file-locking.md) (the `Arc<RwLock<..>>` live-mirror pattern this ADR applies to `ToolRegistry`). Phase 3 of the live-MCP-management umbrella (Part B). Issue #372.

## Context

`ToolRegistry` (`entanglement-runtime/src/tools.rs`) was a plain owned
`HashMap` with no `remove` — a write-once vocabulary built at startup
(`build_config`), then **moved by value** into
`tool_runner::spawn_tool_executor_with_policy`. Nothing after that point could
add or drop a tool: an MCP server connected at startup stayed registered for
the life of the process, and there was no way to attach one later or retract a
dead one without a restart.

Schema *advertisement* already had the seam this needed:
`EngineConfig.tool_spec_resolver` ([0076](0076-per-session-dynamic-tool-specs.md))
lets an embedder replace the advertised tool list per session, consulted fresh
every turn (`entanglement-core/src/session/turn.rs`). But `skutter`
(`entanglement-runtime/src/main.rs`) never wired it — `cfg.tool_specs` was set
once from a snapshot of the registry and baked into `EngineConfig`, which is
then moved into `Holly::spawn` and cloned per session
(`entanglement-core/src/holly.rs:454/532/586`) with no reload path. So even if
the *registry* could gain a tool at runtime, nothing would tell the model.

This blocks live MCP server add/remove (issue #4): retracting a server means
dropping its tools from both the dispatch table and the advertised schema
without tearing down the engine.

## Decision

**`SharedRegistry = Arc<std::sync::RwLock<ToolRegistry>>` replaces
value-ownership of `ToolRegistry` in the executor's low-level entry point, and
`EngineConfig.tool_spec_resolver` is wired to read through the same handle.**

```rust
pub type SharedRegistry = Arc<std::sync::RwLock<ToolRegistry>>;

impl ToolRegistry {
    pub fn unregister(&mut self, name: &str) -> Option<Arc<dyn Tool>>;
    pub fn unregister_prefix(&mut self, prefix: &str); // drop `mcp__<server>__*` in one call
    pub fn contains(&self, name: &str) -> bool;
    pub fn names(&self) -> Vec<String>;                // for a future `/mcp list`
    pub fn shared(self) -> SharedRegistry;
}
```

`spawn_tool_executor_with_policy` — the general entry point `main.rs` and a
multi-tenant embedder call directly — now takes `tools: SharedRegistry`
instead of `ToolRegistry` by value. The two convenience wrappers,
`spawn_tool_executor`/`spawn_tool_executor_with_hooks`, keep their historical
owned-`ToolRegistry` signature (mirroring the existing `wrap_profiles` pattern
for `ProfileRegistry`) and call `.shared()` internally — every existing test
and the `embedded.rs` example that only needs a scratch, never-mutated
registry is untouched.

### Per-dispatch snapshot, not a held lock

Each `ToolExec` dispatch takes a brief read lock and clones an **owned**
`ToolRegistry` snapshot before spawning its detached task:

```rust
let tools = tools.read().unwrap().clone();
```

The clone is cheap — `ToolRegistry` holds `Arc<dyn Tool>` values, so cloning
the map is a handful of refcount bumps, not a deep copy — and it means the
lock is held only for the snapshot, never across a tool's `.await` (a `bash`
call, an MCP round-trip). `dispatch`/`run_and_reply`/`run_rhai` keep their
existing `&ToolRegistry`/`ToolRegistry` parameter types; only the two call
sites in the executor's dispatch loop changed from a sync `ToolRegistry::clone`
to a `SharedRegistry` read-lock-then-clone.

### `std::sync::RwLock`, not `tokio::sync`

The lock is a **synchronous** `std::sync::RwLock`, the same choice already
made for `spawn_tool_executor_with_policy`'s `profiles: Arc<RwLock<ProfileRegistry>>`
parameter ([0084](0084-runtime-live-reload-and-managed-file-locking.md)). This
is load-bearing, not a style preference: `EngineConfig.tool_spec_resolver` is a
plain sync `Fn(&SessionId) -> Vec<ToolSpec>` ([0076](0076-per-session-dynamic-tool-specs.md))
consulted on the turn's hot path, specifically so it never blocks on I/O. A
`tokio::sync::RwLock` would force the resolver closure to either block a
runtime worker thread (`blocking_read`) or become `async` — the latter
contradicts 0076's explicit design. An in-memory registry read is fast enough
that a brief sync lock is never a bottleneck, in the executor loop or the
resolver.

### Wiring `tool_spec_resolver` — behavior-neutral by construction

`main.rs` wraps the registry once, right after `build_config` returns and
before `Holly::spawn` consumes `EngineConfig`:

```rust
let tools = tools.shared();
let resolver_tools = tools.clone();
let runtime_owned_specs = [update_tasks_spec(), ask_user_spec(), rhai_spec()];
engine_config.tool_spec_resolver = Some(Arc::new(move |_session: &SessionId| {
    let mut specs = resolver_tools.read().unwrap().specs();
    specs.extend(runtime_owned_specs.iter().cloned());
    specs
}));
```

Three tool specs (`update_tasks`, `ask_user`, `rhai`) are runtime-intercepted
state tools, not real `ToolRegistry` entries (0059's `Tool` trait doesn't cover
them) — `cfg.tool_specs` included them via three explicit `.push()`s in
`build_config`. The resolver reproduces that exact composition so the very
first turn advertises byte-identical schemas to before this change: **no user-
visible behavior change today**. Every *subsequent* registry mutation (a
future `#4` MCP add/remove) is picked up on the *next* turn for free, with no
`EngineConfig` reload and no engine restart — the resolver is a live view over
`tools`, not a snapshot taken once.

`cfg.tool_specs` itself is left set to its startup snapshot (used only for the
`/agent` picker's tools-checklist roster, `main.rs`) — it is no longer
consulted at turn time once the resolver is present (0076's "replace, not
merge" semantics), so leaving it stale there is harmless.

## Consequences

- **(+)** Unblocks #4 (live MCP add/remove): a future ops module holding a
  `SharedRegistry` clone can `register`/`unregister`/`unregister_prefix` at
  any time, and the next turn on every affected session sees the change with
  no restart.
- **(+)** Zero behavior change today — same specs, same dispatch, same tests,
  verified by the full existing suite passing unmodified.
- **(+)** Minimal call-site churn: only `spawn_tool_executor_with_policy`'s two
  production callers (`main.rs`, `examples/embedded.rs`) and one test
  (`tests/policy_seam.rs`) needed a `.shared()` wrapper; the ~30 test call
  sites through the convenience wrappers are untouched.
- **(−)** A registry write (a future MCP add/remove) briefly excludes readers
  across every session's next dispatch — acceptable since registration is rare
  compared to dispatch frequency, same trade-off already accepted for
  `profiles`.
- **(−)** The runtime-owned pseudo-tool specs (`update_tasks`/`ask_user`/
  `rhai`) are now assembled in two places conceptually (the resolver closure
  here, plus `build_config`'s three `.push()`s for the startup snapshot) —
  both use the same three constructor functions, so they can't drift in
  practice, but it is duplicated wiring rather than a single source of truth.

## Alternatives considered

- **`tokio::sync::RwLock` throughout.** Rejected: forces the sync
  `tool_spec_resolver` closure into either `blocking_read` (risks stalling a
  runtime worker thread from inside the turn's hot path) or `async` (breaks
  0076's explicit no-I/O-on-the-turn-path contract). `std::sync::RwLock` held
  only briefly, never across an `.await`, has none of that risk.
- **Change all three `spawn_tool_executor*` signatures to `SharedRegistry`.**
  Rejected: would force every one of the ~30 integration test call sites
  (none of which need live mutation) to wrap a scratch registry, for no
  behavioral gain — the same reasoning `wrap_profiles` already established for
  `ProfileRegistry`'s convenience wrappers.
- **Merge `cfg.tool_specs` and the resolver's runtime-owned-specs list into one
  source of truth in `build_config`.** Considered, deferred: would mean
  threading an extra return value out of `build_config` (already returning a
  5-tuple) for three lines saved. Left as a follow-up if `build_config` grows
  another reason to be restructured.
