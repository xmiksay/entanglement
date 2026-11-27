# Embedding §4 — Custom tool execution / policy

> Part of the [embedding guide](../embedding.md).

## 4. Custom tool execution / policy

### Constructing the executor — `ToolExecutor::builder`

The runtime tool executor (permission dispatch, approvals, host-tool
execution) is built with one builder (#724). The two required inputs go into
`builder(..)` itself, so a half-configured executor can't be spawned; every
other seam is an optional setter with a default, so a seam added in a later
release never breaks your call site:

```rust,ignore
use entanglement_runtime::tool_runner::ToolExecutor;

let executor = ToolExecutor::builder(&holly, tools.shared()) // Holly + SharedRegistry
    .agents(catalog)
    .resolver(Arc::new(TenantResolver { .. }))
    .grants(Arc::new(TenantGrants { .. }))
    .mcp_scopes(scopes)
    .spawn(); // tokio::task::JoinHandle<()>, abort it to shut down
```

| Setter | Default when not called |
| --- | --- |
| `agents(AgentCatalog)` / `live_agents(Arc<RwLock<AgentCatalog>>)` | `AgentCatalog::new()` (the built-in `general` agent only) |
| `live_skills(Arc<RwLock<Arc<SkillRegistry>>>)` | empty registry |
| `base(PermissionProfile)` | allow-all ceiling (no clamp — same as an absent `permissions:`) |
| `resolver(Arc<dyn PermissionResolver>)` | `ModeResolver` over the builder's final `perm_modes`/`mode_table`/`tools`/`base`, root-relative when `escape_root` is set |
| `grants(Arc<dyn GrantStore>)` | `InMemoryGrantStore` — `Always` grants last for the executor's lifetime; the library never writes a grants file (#966) |
| `mode_table(Arc<ModeTable>)` | `ModeTable::builtin()` |
| `perm_modes(..)` / `active(..)` | private maps. **A custom `.resolver(..)` (or sandbox resolver) that reads a mode map must get the same `Arc` via `.perm_modes(..)`** — otherwise the executor folds `ModeChanged` into its private map and your resolver silently grades from a map that never changes |
| `escape_root(EscapeRoot)` | strict containment; plans resolve against the cwd |
| `jobs` / `retained` / `scripts` / `plan_files` | private registries (share them with your `BashTool`/plans watcher if you wire those) |
| `mcp_scopes(Arc<McpScopes>)` | none — single-user MCP |
| `http_gate(Arc<HttpGate>)` | no `http` tool — set it to enable the script `http()` binding; the builder registers the tool over this gate (ADR-0227) |
| `workdirs(SessionWorkdirs)` | private — share it with `glob`/`grep` so they list relative to a session's workdir (ADR-0224) |
| `catalog(Arc<Catalog>)` | none — `list_models` and `agent`'s `model` validation report "no catalog configured" |
| `discovery(DiscoverySurface)` | `DiscoverySurface::default()` (private advertising state, empty MCP roster) |
| `attendance(Attendance)` | private — nobody watches, so a mode's `question_timeout` alone decides. Share it with your heads: a head that answers cards registers a `Watcher` (`watch_all()`, or `watcher()` + `attend(&session)`) for as long as it is attached, and that session and its sub-agents then park every `Ask` with no deadline (ADR-0247); pass the same value to `serve::router` |

**Persisted grants are the head's.** The library keeps grants in memory; a
head that wants `Always` approvals to outlive the process implements
`GrantStore` over its own storage and passes it — `skutter` passes its
file-backed `state::FileGrantStore` (the managed `grants.yml`) from the
`state::Stores` it builds at boot. The same holds for MCP state: `AvailableMcp` keeps OAuth
credentials and last-seen tool names in memory unless the head passes
`.with_token_store(..)` / `.with_tool_cache(..)`, and a live `/mcp
add|remove` changes only the in-memory `ServerConfigs` — persist it on
`OutEvent::McpChanged` if you want it to survive a restart.

**Undo/rewind is opt-in too** ([ADR-0212](../adr/archive/0212-undo-rewind-via-per-write-file-backups.md)).
Build the file tools with backups and hand core the matching reverter, all
from the `SessionStore` your persistence tap writes (a revert reads its turns
from that log):

```rust,ignore
let backups = FileBackups::new(store.clone());
cfg.file_reverter = Some(backups.reverter());         // before Holly::spawn
let tools = host_tools_with(root, HostToolOptions {
    extra_roots: extra,
    backups: Some(backups.clone()),
    ..HostToolOptions::default()
});
let holly = Holly::spawn(cfg);
backups.track_roots(&holly);                          // lineage, write ancestry + busy sessions
backups.set_busy_probe(detached_work_probe(jobs, scripts)); // refuse while detached work runs
```

`track_roots` also installs the probe that stamps each backed-up write
with the live tree it was made in, so a compaction successor's `/rewind`
reaches its moved sub-agents' writes
([ADR-0236](../adr/archive/0236-rewind-follows-adopted-sub-agents.md)); without it,
changes fall back to the logs' latest parent edges.

`FileBackups::new` uses the default caps (8 MiB per file, 256 MiB per root
session). For your own, build it with
`FileBackups::with_backup_limits(store, BackupLimits { enabled, max_file_bytes,
max_root_bytes })` — your own values; the library reads no config file
(`skutter` maps its `config.yml` `undo:` block onto it) — and call `backups.set_limits(..)` on any clone to
change them live; the next recorded write sees the new limits, and backups
already taken stay restorable even with `enabled: false`.

**Session working directories are opt-in too** ([ADR-0224](../adr/archive/0224-session-working-directory.md)).
Give core the runtime's resolver and share one `SessionWorkdirs` between the
search tools and the executor; with no resolver every `SetWorkdir`/
`Spawn.workdir` is refused:

```rust,ignore
cfg.workdir_resolver = Some(workdir::resolver(root.clone(), Some(store.clone())));
let workdirs = SessionWorkdirs::default();
let tools = host_tools_with(root, HostToolOptions {
    extra_roots: Some(store),
    workdirs: Some(workdirs.clone()),
    ..HostToolOptions::default()
});
ToolExecutor::builder(&holly, tools.shared()).workdirs(workdirs) /* … */;
```

Wrap your own call dispatch in `exec_scope::with_exec_seq(tool_exec.seq, ..)`
if you don't use `ToolExecutor` — that is what places a change in its turn
when request ids repeat.

Without it `InMsg::RevertFiles` answers with a recoverable `Error`. With a
custom `RecordSink` (no file log), write your own `FileReverter`.

**Forking needs no wiring** ([ADR-0223](../adr/archive/0223-non-compacting-session-fork.md)).
Send `InMsg::ForkSession { session, at_turn }` over `Holly::send` (it is
wire-refused). The fork announces itself with `SessionStarted` and then
`SessionForked`, which carries its seed history, so whatever sink you use
already persists a fork log that replays exactly. A `{tenant}:` prefix carries
over: the fork's id is minted in its source's lineage.

**Migrating from the positional constructors** (deprecated in 0.7, removed
after): `spawn_tool_executor(h, t, a, b)` →
`ToolExecutor::builder(h, t.shared()).agents(a).base(b).spawn()`;
`spawn_tool_executor_with_policy(h, t, …)` maps each positional argument onto
the setter of the same name (`agents` → `live_agents`, `skills` →
`live_skills`, an `Option` argument → its setter only when `Some`; its former
`advertising_inputs` argument is now `catalog: Option<Arc<Catalog>>`). Drop
any setter whose argument was just the default above.

### The policy seams

`.resolver(..)` and `.grants(..)` (#311,
[ADR-0079](../adr/archive/0079-pluggable-permission-resolver-and-grant-store.md)) are the
seam: two trait objects instead of the CLI's file-backed defaults, so a
per-tenant DB-backed policy plugs in without forking the executor
(spawn bounds, `rhai`, the plan/tasks tools all stay shared). A
resolver can also wrap another to add a veto — `skutter`'s `pre_tool_use`
hooks are exactly that (`skutter::hooks::HookResolver`, #964):

```rust
#[async_trait]
pub trait PermissionResolver: Send + Sync {
    async fn resolve(&self, session: &SessionId, tool: &str, input: &str) -> Grade;
}
/// `Allow | Ask | Deny(Option<String>)`; `From<Permission>`.
pub enum Grade { Allow, Ask, Deny(Option<String>) }
#[async_trait]
pub trait GrantStore: Send + Sync {
    fn is_granted(&self, session: &SessionId, tool: &str, arg: Option<&str>, mode: &str) -> bool;
    async fn record(
        &self,
        session: &SessionId,
        tool: &str,
        arg: Option<&str>,
        scope: ApprovalScope,
        mode: &str,
    );
    fn forget_session(&self, session: &SessionId);
}
```

`mode` is the permission mode the approval was earned in. The executor passes
the mode the call was graded under to both `is_granted` and `record`, so key your
grants by it: a grant matches only the mode it was recorded under, and one
earned in `build` never upgrades a call in `research`
([ADR-0207](../adr/archive/0207-permission-modes-replace-agent-borne-authority.md) §8).
A 0.6.0 store gains the parameter on both methods
([migration](migrations/agents-modes-and-permissions.md#mode-scoped-grants-adr-0207-8-634)).

A `Grade::Deny(Some(reason))` reaches the model verbatim as the call's error
result; a `Deny(None)` gets the `tool X denied by mode M — use /mode to
switch` text, which points at the wrong fix for a veto of your own, so give
one a reason (`Grade::deny(..)`, #964).

`resolve` is called once per session in a call's ancestor chain; the executor
takes the least-privileged grade across that chain itself, so your resolver
only ever decides a *single* session's own grade — the sub-agent privilege
ceiling ([ADR-0024](../adr/archive/0024-subagent-permission-gating.md)) stays in the
ladder on top of whatever you return. Spawning is asked about too: `agent`/
`agent_send` are graded like any tool, so a resolver that denies names it
doesn't know also refuses every spawn
([ADR-0232](../adr/archive/0232-spawning-is-graded-bookkeeping-control-tools-non-maskable.md)).
`poll`/`ask_user`/`update_tasks`/`request_mode` and the discovery tools are
never asked about; `propose_plan` is (by its `plan` class). A tenant-keyed resolver is a couple of
lines:

```rust
struct TenantResolver {
    tenants: HashMap<&'static str, PermissionProfile>,
    // The live registry the executor dispatches against — the same
    // `shared()` handle passed to `ToolExecutor::builder`, never a clone.
    tools: SharedRegistry,
}

#[async_trait]
impl PermissionResolver for TenantResolver {
    async fn resolve(&self, session: &SessionId, tool: &str, input: &str) -> Grade {
        // Each tool declares its own argument (#962), so ask the registry.
        let arg = {
            let tools = self.tools.read().expect("tool registry lock poisoned");
            permission_arg(&tools, tool, input)
        };
        match self.tenants.get(tenant_of(session)) {
            Some(profile) => profile.resolve(tool, arg.as_deref()).into(),
            // Fail closed on an unknown tenant, and say why.
            None => Grade::deny(format!("tool `{tool}` denied: unknown tenant")),
        }
    }
}
```

Read-lock the registry inside `resolve`, as the library's `ModeResolver`
does: the executor builder registers `http` at `spawn` and a lazy MCP connect
registers its tools later, so a resolver holding a `ToolRegistry` clone gets no
argument for them and a scoped rule such as `http(POST *)` never matches — the
call falls through to the tool's bare rules (#962).

`resolve` is `async` (a real embedder hits its DB) but
`GrantStore::is_granted(session, tool, arg, mode)` is deliberately **sync** —
the executor consults it inline before prompting, so it must be a fast
in-memory/cached check. The write side (`record(.., scope, mode)`, called on
every non-`Once` approval and persisting only an `ApprovalScope::Always` one)
is async and is where a DB write belongs; the corresponding read then goes through your
`PermissionResolver` on the *next* call, not through `is_granted` — the trait
split exists so the store's `is_granted` only has to cover the fast default
grants the CLI needs, not your DB-backed ones.

**Snapshot-cache pattern for sync resolvers** ([ADR-0076](../adr/archive/0076-per-session-dynamic-tool-specs.md)/[ADR-0078](../adr/archive/0078-per-turn-dynamic-system-prompt.md),
the same shape #311 reuses): the sibling `tool_spec_resolver` and
`system_prompt_resolver` seams on `EngineConfig` are plain sync `Fn`s consulted
on the turn's hot path, so they can't do I/O either. The documented pattern for
all three is an embedder-owned `Arc<RwLock<HashMap<SessionId, T>>>` hydrated
from your store on a slower cadence (a background refresh task, or on write),
with the hot-path closure only ever reading the cache. Apply the same shape to
a `PermissionResolver` if your DB round-trip is too slow to eat on every tool
call despite `resolve` being `async`.
[`examples/embedded_lifecycle.rs`](../../entanglement-runtime/examples/embedded_lifecycle.rs)
wires both `tool_spec_resolver` and `system_prompt_resolver` this way, over one
shared cache keyed by `SessionId`.

---

Previous: [§3 — Custom persistence](persistence.md) · Next: [§5–§7 — Approvals across restart, MCP & multi-user](approvals-mcp-multi-user.md)
