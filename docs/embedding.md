# Embedding `entanglement` — a multi-tenant guide

How to build a **custom head** on top of the engine: one process serving many
users, each isolated, without forking anything. This is the path
`entanglement-runtime --no-default-features` exists for
([ADR-0025](adr/0025-runtime-cargo-feature-gates.md)) — the lean library gives
you the tool-execution loop, permission dispatch, sub-agent spawn, and
persistence machinery with zero CLI/TUI/transport weight. Everything below is exercised by compiling, runnable examples — the
multi-tenant policy seam in
[`entanglement-runtime/examples/embedded.rs`](../entanglement-runtime/examples/embedded.rs)
(`cargo run -p entanglement-runtime --example embedded --no-default-features`),
the dynamic-resolver and hibernate/resume lifecycle seams in
[`entanglement-runtime/examples/embedded_lifecycle.rs`](../entanglement-runtime/examples/embedded_lifecycle.rs)
(`cargo run -p entanglement-runtime --example embedded_lifecycle
--no-default-features`), and the streamable-HTTP MCP transport in
[`entanglement-runtime/examples/mcp_http.rs`](../entanglement-runtime/examples/mcp_http.rs)
(`cargo run -p entanglement-runtime --example mcp_http --features mcp-http`) —
none need a provider key, running against the built-in `EchoLlm`. `make
lint`/`make check-lean` run `clippy --all-targets` over all three on every
change (`mcp_http.rs` only under `lint`, since `mcp-http` is compiled out of
the lean `--no-default-features` build the other two target — see §6 below),
so the snippets quoted here can't silently drift from the real API.

Until this guide, the only wiring reference was `main.rs`/`run.rs` — written
for a single-user CLI. Multi-tenancy is not a separate mode the engine knows
about; it falls out of five ordinary decisions an embedder makes about its own
process, covered in order below. Part of #307/#315.

## 1. One engine, many tenants

Run **one `Holly`**, not one per tenant. `Holly::spawn` is a supervisor task
plus two broadcast channels ([`entanglement-core/src/holly.rs`](../entanglement-core/src/holly.rs));
sessions are cheap tasks keyed by `SessionId`
(`entanglement-core/src/holly/supervisor.rs`, the `supervisor` loop), created
on first message per the unknown-session policy (§3). Spinning up a whole
engine per user buys you nothing the session key doesn't already give you, and
it multiplies every piece of shared state (the provider connection pool,
[ADR-0050](adr/0050-per-endpoint-connection-pool-retry-rate-limit.md); the
definitions watcher, [ADR-0084](adr/0084-runtime-live-reload-and-managed-file-locking.md))
by tenant count for no isolation benefit — `SessionId` is a plain string
(`pub struct SessionId(pub String)`), so the isolation you actually need is a
**naming convention**, not a process boundary.

**Convention: `{tenant}:{uuid}`.** Mint every root session as
`format!("{tenant}:{}", SessionId::new_uuid())`. The tenant is recovered by
splitting on the first `:`:

```rust
fn tenant_of(session: &SessionId) -> &str {
    session.0.split_once(':').map_or(&session.0[..], |(t, _)| t)
}
```

Two kinds of session are minted by the engine, not by your head: a sub-agent
child (the runtime's `agent` tool) and a **compaction successor** — every
compaction forks one, a fresh *root* (`parent: None`, `predecessor: Some(source)`,
[ADR-0205](adr/0205-every-compaction-forks-a-successor-session.md)). Both go
through `EngineConfig::id_gen` with an `IdContext` naming their lineage, and
the default `DefaultIdGen` copies the parent's — else the predecessor's —
prefix up to and including the first `:` ([ADR-0211](adr/0211-idgen-context-and-tenant-prefix-inheritance.md)):
the child or successor of `acme:s-6a70…` is `acme:s-6a71…`. So `tenant_of`
holds for the whole lineage **by construction, with the default generator**.
A root with no `:` mints unprefixed descendants, as before.

If you install your own `IdGen` (a DB sequence, UUIDs, a different tenant
encoding), the prefix is yours to keep: `next(kind, ctx)` receives
`ctx.parent`/`ctx.predecessor`/`ctx.user`, and `ctx.inherited_prefix()` gives
the default's answer to reuse. A generator that ignores the context breaks
prefix filtering for every child and successor. When your head mints an id in
a lineage itself, call `Holly::next_id_in(kind, &IdContext::child_of(&parent))`
rather than `next_id`, which mints with an empty context.

**Ownership filtering on `subscribe()`.** `Holly::subscribe()` is one
broadcast fan-out of **every** tenant's events — there is no
`subscribe_session`/`subscribe_tenant` filter built into core, and none is
needed: the filter is a one-line predicate over the plain-string `SessionId`,
and different embedders want different scopes (exact session, a whole root's
subtree, a whole tenant), so a single core helper would either be too narrow or
grow parameters for each shape. Apply the filter once, at the point you relay
an event out to a tenant's transport (a WS socket, an SSE stream) — never
forward an event before checking it:

```rust
let mut sub = holly.subscribe();
loop {
    let ev = match sub.recv().await {
        Ok(ev) => ev,
        Err(RecvError::Lagged(_)) => continue, // §5 below
        Err(RecvError::Closed) => break,
    };
    if ev.session() != Some(&session) {
        continue; // another tenant's event on the shared fan-out
    }
    relay_to_tenant_socket(ev).await;
}
```

`OutEvent::session()` returns `None` for the supervisor-global query replies
(`SessionList`, `QuestionList`, `OperationList`, `PendingList`, `McpList` —
[ADR-0072](adr/0072-protocol-warts-settled-before-serve.md)), so the filter
above passes none of them. Several of these replies **do carry tenant data**,
and an unscoped query covers every session in the process:

- `SessionList.sessions`: every live session.
- `QuestionList.questions`: every open `ask_user` question text.
- `OperationList.operations`: every background job's command line.
- `PendingList.approvals`: every parked tool call's full `input`, i.e. plan
  bodies, file contents and commands
  ([ADR-0217](adr/0217-list-pending-query-for-parked-approvals.md)).

A multi-tenant head that forwards these queries over `send_from_wire` **must**:

1. scope the query itself before sending it: force `ListPending.root` (or
   `ListQuestions`/`ListOperations`' `session`) to a session the connection
   owns, **or** filter the reply's payload by session ownership; and
2. deliver the reply only to the connection that sent its `correlation_id`.
   Replies are broadcast to every subscriber, so relaying one to all
   connections leaks it even when the query was scoped.

## 2. A custom head over the ABI: the trust split

`Holly` exposes two inbound entry points, and picking the wrong one is a
privilege-escalation bug, not a style choice
([ADR-0069](adr/0069-trusted-untrusted-wire-frame-split.md)):

- **`Holly::send`** — the privileged, in-process path. Use it for anything
  *your embedder itself* decided to send: a `Prompt` you constructed from a
  request your auth layer already validated, a `ToolResult` your own executor
  produced, an internal `Spawn`.
- **`Holly::send_from_wire`** — for anything **deserialized from a client's
  bytes** (a WebSocket frame, an HTTP body). It enforces `InMsg::wire_allowed()`
  and refuses the runtime-authored trio (`ToolResult`, `Spawn`, `Resume`) with a
  `WireError`, because a forged `ToolResult` resolves a parked turn on
  `request_id` alone — bypassing both tool execution and permission dispatch —
  and a forged `Spawn` bypasses the tool path's spawn-refusal gate.

The rule of thumb: **who typed this frame?** If it's a value your server code
built after checking the caller's identity/tenant, `send` it. If it's bytes a
client sent that merely deserialize into an `InMsg`, always go through
`send_from_wire` — even in a trusted-tenant, authenticated-client setup, because
the split isn't about trusting the *user*, it's about not letting a client
socket masquerade as your own executor. `entanglement-runtime`'s own `pipe`
head is the reference: every stdin line that parses as an `InMsg` goes through
`send_from_wire` (`entanglement-runtime/src/pipe.rs`) — only the fallback case,
where `pipe` itself decides to wrap an unparsable line as a `Prompt` it
constructed, uses plain `send`. Same discipline applies even though `pipe` is a
trusted local process; it's what keeps a `serve`-style head safe once its input
actually is a remote client.

Multi-tenant addition on top of the ABI split: your head must also stamp or
validate the tenant prefix on every inbound frame's `SessionId` before handing
it to either `send` method — the trust split stops a client from forging
*privileged variants*, but nothing in core stops an authenticated tenant A from
addressing tenant B's session id if your head doesn't check. That check belongs
to your auth layer, not to `entanglement-core`, for the same reason the tenant
convention above is a naming idiom and not a core type: core has no concept of
"tenant" to enforce. With `UnknownSession::Resume` (§3) this check also guards
**evicted** sessions: an unchecked frame naming another tenant's hibernated id
would revive it — with that tenant's history and `user` (hence provider key) —
so validate ownership before `send_from_wire`, not only for live ids.

## 3. Custom persistence: tap → sink → lazy `resume`

The reference persistence layer
(`entanglement-runtime/src/persistence.rs`,
[ADR-0020](adr/0020-event-sourced-session-persistence.md)) is already split
into a fixed *tap* and a pluggable *sink* (#313), so a DB-backed embedder swaps
one trait impl instead of forking the subscriber:

```rust
pub trait RecordSink: Send + Sync {
    fn append(&self, root: &SessionId, record: &LogRecord) -> anyhow::Result<()>;
    // Defaulted: the top-level message only, control chars stripped, ≤200 chars.
    fn public_error(&self, error: &anyhow::Error) -> String { /* … */ }
}
```

`spawn_persistence_subscriber_with_sink(holly, sink)` owns everything you don't
want to reimplement: routing every record to its **root** session (a spawned
child's turns fold into the root's stream via `SessionStarted { root, parent }`),
biasing inbound `In` records ahead of their causally-later `Out` records so
replay pairs them correctly (`pair_records`), and writing a `LogPayload::Gap`
tombstone into every known root when the tap's broadcast receiver lags —
`RecvError::Lagged` — so a resume can never silently fold an incomplete
history.

**Gap ownership (#723): return `Err` for every record you lose.** An `Err`
from `append` means "this record is gone", and the tap marks it for you: it
appends a `Gap { dropped }` tombstone to that root, and if the tombstone write
fails too it holds the gap and writes it before that root's next record
(counting records it had to hold back as further loss). A sink that loses a
record *after* returning `Ok` — e.g. its writer task's `INSERT` fails later —
has hidden the loss from the tap and must write its own `Gap` row for that
root, **at the hole**: once an insert fails, the writer holds back that root's
later rows (counting them into the gap) until the `Gap` row lands — the same
rule the tap follows. An async writer that keeps inserting puts rows after an
unmarked hole, i.e. inside what `resumable_prefix` (below) keeps.

**Outages are wire-visible (#767).** The first failed `append` of an outage
broadcasts `OutEvent::PersistenceDegraded { root, error }`, and the landed
tombstone `PersistenceRecovered { root, lost }`. Both are **engine-global**
(no session routing), so every subscriber sees every root's outage. `error`
is `public_error(&e)`, never the raw chain (which `tracing` still logs in
full). Override `public_error` if even your top-level message can name a host,
DSN or path. A multi-tenant head must filter these events by **root
ownership** before forwarding them to a connection, as it already does for
session-bearing events.

**The one constraint that matters for a real (DB/network) sink:
`append` must not block.** The tap reads `Holly`'s outbound broadcast, which is
lossy under back-pressure; a sink that awaits a slow write starves that
receiver and *manufactures* the very `Gap` tombstones this design exists to
avoid. Put a bounded channel + dedicated writer task behind `append` and
return immediately — drop past the bound and surface it as an `Err`, don't
await:

```rust
struct DbSink { tx: mpsc::Sender<(SessionId, LogRecord)> } // writer task drains this

impl RecordSink for DbSink {
    fn append(&self, root: &SessionId, record: &LogRecord) -> anyhow::Result<()> {
        self.tx
            .try_send((root.clone(), record.clone()))
            .map_err(|_| anyhow::anyhow!("db sink backlog full, dropping record"))
    }
}
```

**Resume is lazy — you decide when to pay for it.** `Holly::resume(root,
records)` takes whatever `Vec<(Option<InMsg>, OutEvent)>` your store hands
back (your own `pair_records`-equivalent over your DB rows, or the shared
`session_store::pair_records` if you keep the same `LogRecord` shape) and
replays it into a fresh session task — nothing is loaded until a tenant
actually reconnects to that session id. There is no background hydration: an
idle tenant costs nothing beyond the rows already in your store.

**Cut the hole before you resume: `session_store::resumable_prefix`.** Don't
re-implement gap truncation over your rows. Hand the helper one item per
stored row — `Ok(LogRecord)` for a row that decoded, `Err(_)` for one that
didn't — and it returns a `ResumablePrefix { records, truncation }`: every
record before the first `Gap` tombstone or undecodable row, plus
`Option<Truncation>` (`Gap { index, dropped }` / `Corrupt { index }`) saying
where and why it cut. A `Retired` record (a frame an older release logged,
#720) is not a hole and stays in the prefix. Truncating is *your* policy
choice; the in-tree heads stay strict and refuse a log with any gap
(`check_for_resume`, #104).

**After a truncated resume, remove the hole before the session writes
again.** The resumed session appends its new rows *after* the old tombstone,
so the next load would cut at that same first gap and silently discard
everything done since this resume. Either delete the rows from
`truncation.index()` onward before resuming under the same id (shown below),
or resume the prefix into a **fresh** session id and retire the old one.

```rust
use entanglement_runtime::session_store::{pair_records, resumable_prefix};

let rows = db_rows.iter().map(|row| serde_json::from_value::<LogRecord>(row.payload.clone()));
let prefix = resumable_prefix(rows);
if let Some(cut) = prefix.truncation {
    tracing::warn!("resuming {root} from its intact prefix, cut at {cut:?}");
    // `db_rows[cut.index()..]` — the tombstone/corrupt row and everything after.
    db.delete_rows_from(&root, db_rows[cut.index()].id).await?;
}
holly.resume(root, pair_records(&prefix.records)).await?;
```

**Let the engine resume on demand: `EngineConfig::unknown_session`**
([ADR-0210](adr/0210-unknown-session-policy.md)). By default a routable
message (`Prompt`, `ToolResult`, `Stop`, `SetModel`, …) for an id with no live
task lazily spawns a **blank** root (`UnknownSession::SpawnBlank`) — right for
a single-user CLI, wrong for you: after an idle-TTL hibernation (or a
restart) the tenant's next `Prompt` would run on an empty context with
`user = None`. Don't mirror liveness off the broadcast and call
`Holly::resume` before every send — the idle sweep can evict between your
check and your send. Install a loader instead and keep sending plain
`Prompt`s:

```rust
use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use entanglement_core::{InMsg, OutEvent, SessionId, SessionLoader, UnknownSession};
// `pair_records` lives in entanglement-runtime; a core-only embedder pairs its
// own rows into the same `(Option<InMsg>, OutEvent)` shape.
use entanglement_runtime::session_store::pair_records;

struct DbLoader { db: Db }

#[async_trait]
impl SessionLoader for DbLoader {
    async fn load(&self, id: &SessionId)
        -> anyhow::Result<Option<Vec<(Option<InMsg>, OutEvent)>>> {
        // Bound your I/O: every message for `id` queues behind this call.
        let rows = tokio::time::timeout(Duration::from_secs(5), self.db.log_for(id)).await??;
        Ok(rows.map(|rows| pair_records(&rows)))  // None = no such session
    }
}

let cfg = EngineConfig {
    unknown_session: UnknownSession::Resume(Arc::new(DbLoader { db })),
    ..cfg
};
```

`Some(records)` resumes the session (its spawn sub-tree too, ADR-0112, with
its `user` intact) and then delivers the message; `None` refuses with a
seq-`0` `Error` naming the session, as does `UnknownSession::Refuse` without
asking a loader; `Err` surfaces as an `Error` and spawns nothing. Messages
that arrive while a load is in flight queue behind that one load and deliver
in order. `Holly::is_live(&id)` answers "is it running right now" cheaply,
for display — not for check-then-send.

**Ownership of a live session: `Holly::session_info(&id)`**
([ADR-0211](adr/0211-idgen-context-and-tenant-prefix-inheritance.md)). A
resolver deciding whether a session's tool call belongs to a user (a
permission seam, a per-user MCP scope) reads the engine's own directory
synchronously — `SessionInfo { user, parent, predecessor, agent, root, .. }`
— instead of mirroring `SessionStarted` off the broadcast, which races the
first tool call. It is `Some` from before the session's `SessionStarted`
until the engine processes its `CloseSession` or hibernation, `None` for an
unknown, closed or hibernated id, and a resume (explicit or through the
loader above) re-registers the same lineage and `user` from the log. A
compaction successor reports its `predecessor` and the `user` it inherited
from it, and is persisted under its **own** root id (it is a root), so your
sink receives its records keyed by the successor id — tenant-prefixed by the
default generator (§1), which is what lets your loader find it again. For
routing *events*, key on the prefix rather than `session_info`: a session's
trailing `SessionEnded` arrives after its directory entry is gone.

**The loader is reachable from the wire.** `Prompt`, `SetModel` and the other
covered frames are wire-allowed, so a client relayed via `send_from_wire`
triggers your `load` for **any** id it names — one loader task per distinct
id, no concurrency cap in core, and the per-id queue of frames waiting on a
load is unbounded. Worse, a resumed session restores its `user` from its own
log: a frame naming another tenant's *evicted* session would bring it back with
that tenant's history and provider key (under `SpawnBlank` it got a blank
session with `user = None`). The rule from §2 therefore applies to evicted ids
too: **validate tenant ownership of the id in your head before
`send_from_wire`.** The loader gets no provenance and needs none if the head
does; bound its I/O (and rate-limit, if your store needs it) inside `load`.

[`examples/embedded_lifecycle.rs`](../entanglement-runtime/examples/embedded_lifecycle.rs)
wires a `RecordSink` (an in-memory stand-in for a DB), reads its own log back
through `pair_records`, and drives the full round-trip end to end.

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
| `agents(AgentCatalog)` / `live_agents(Arc<RwLock<AgentCatalog>>)` | `AgentCatalog::new()` (built-in profiles) |
| `live_skills(Arc<RwLock<Arc<SkillRegistry>>>)` | empty registry |
| `base(PermissionProfile)` | allow-all ceiling (no clamp — same as an absent `permissions:`) |
| `resolver(Arc<dyn PermissionResolver>)` | `ModeResolver` over the builder's final `perm_modes`/`mode_table`/`tools`/`base`, root-relative when `escape_root` is set |
| `grants(Arc<dyn GrantStore>)` | **in-memory only** — `Always` grants never touch `grants.yml` (#747/#775) |
| `mode_table(Arc<ModeTable>)` | `ModeTable::builtin()` |
| `perm_modes(..)` / `active(..)` | private maps. **A custom `.resolver(..)` (or sandbox resolver) that reads a mode map must get the same `Arc` via `.perm_modes(..)`** — otherwise the executor folds `ModeChanged` into its private map and your resolver silently grades from a map that never changes |
| `hooks(Hooks)` | no hooks |
| `escape_root(EscapeRoot)` | strict containment; plans resolve against the cwd |
| `jobs` / `retained` / `scripts` / `plan_files` | private registries (share them with your `BashTool`/`CallTool`/plans watcher if you wire those) |
| `mcp_scopes(Arc<McpScopes>)` | none — single-user MCP |
| `workdirs(SessionWorkdirs)` | private — share it with `glob`/`grep` so they list relative to a session's workdir (ADR-0224) |
| `catalog(Arc<Catalog>)` | none — `list_models` and `agent`'s `model` validation report "no catalog configured" |
| `discovery(DiscoverySurface)` | `DiscoverySurface::default()` (private advertising state, empty MCP roster) |

**Persisted grants are opt-in.** A single-user head that wants `Always`
approvals written to the managed `grants.yml` must say so —
`.grants(Arc::new(DefaultGrantStore::load()))`, exactly as `skutter` does in
`services.rs`. Leaving the setter out keeps grants for the executor's lifetime
only.

**Undo/rewind is opt-in too** ([ADR-0212](adr/0212-undo-rewind-via-per-write-file-backups.md)).
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
backups.track_roots(&holly);                          // lineage + busy sessions
backups.set_busy_probe(detached_work_probe(jobs, scripts)); // refuse while detached work runs
```

`FileBackups::new` uses the default caps (8 MiB per file, 256 MiB per root
session). For your own, build it with
`FileBackups::with_backup_limits(store, BackupLimits { enabled, max_file_bytes,
max_root_bytes })` — or pass `Config::load(root)?.undo` to honor a
`config.yml` `undo:` block — and call `backups.set_limits(..)` on any clone to
change them live; the next recorded write sees the new limits, and backups
already taken stay restorable even with `enabled: false`.

**Session working directories are opt-in too** ([ADR-0224](adr/0224-session-working-directory.md)).
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

Wrap your own call dispatch in `file_backup::with_exec_seq(tool_exec.seq, ..)`
if you don't use `ToolExecutor` — that is what places a change in its turn
when request ids repeat.

Without it `InMsg::RevertFiles` answers with a recoverable `Error`. With a
custom `RecordSink` (no file log), write your own `FileReverter`.

**Forking needs no wiring** ([ADR-0223](adr/0223-non-compacting-session-fork.md)).
Send `InMsg::ForkSession { session, at_turn }` over `Holly::send` (it is
wire-refused). The fork announces itself with `SessionStarted` and then
`SessionForked`, which carries its seed history, so whatever sink you use
already persists a fork log that replays exactly. A `{tenant}:` prefix carries
over through `IdContext::successor_of`.

**Migrating from the positional constructors** (deprecated in 0.7, removed
after): `spawn_tool_executor(h, t, a, b)` →
`ToolExecutor::builder(h, t.shared()).agents(a).base(b).spawn()`;
`spawn_tool_executor_with_hooks` adds `.hooks(hooks)`;
`spawn_tool_executor_with_policy(h, t, …)` maps each positional argument onto
the setter of the same name (`agents` → `live_agents`, `skills` →
`live_skills`, an `Option` argument → its setter only when `Some`; its former
`advertising_inputs` argument is now `catalog: Option<Arc<Catalog>>`). Drop
any setter whose argument was just the default above.

### The policy seams

`.resolver(..)` and `.grants(..)` (#311,
[ADR-0079](adr/0079-pluggable-permission-resolver-and-grant-store.md)) are the
seam: two trait objects instead of the CLI's file-backed defaults, so a
per-tenant DB-backed policy plugs in without forking the executor
(spawn bounds, hooks, `rhai`, the plan/tasks tools all stay shared):

```rust
#[async_trait]
pub trait PermissionResolver: Send + Sync {
    async fn resolve(&self, session: &SessionId, tool: &str, input: &str) -> Permission;
}
#[async_trait]
pub trait GrantStore: Send + Sync {
    fn is_granted(&self, session: &SessionId, tool: &str, arg: Option<&str>) -> bool;
    async fn record(&self, session: &SessionId, tool: &str, arg: Option<&str>, scope: ApprovalScope);
    fn forget_session(&self, session: &SessionId);
}
```

`resolve` is called once per session in a call's ancestor chain; the executor
takes the least-privileged grade across that chain itself, so your resolver
only ever decides a *single* session's own grade — the sub-agent privilege
ceiling ([ADR-0024](adr/0024-subagent-permission-gating.md)) stays in the
ladder on top of whatever you return. A tenant-keyed resolver is a couple of
lines:

```rust
struct TenantResolver { tenants: HashMap<&'static str, PermissionProfile> }

#[async_trait]
impl PermissionResolver for TenantResolver {
    async fn resolve(&self, session: &SessionId, tool: &str, input: &str) -> Permission {
        let arg = permission_arg(tool, input);
        self.tenants
            .get(tenant_of(session))
            .map(|profile| profile.resolve(tool, arg.as_deref()))
            .unwrap_or(Permission::Deny) // fail closed on an unknown tenant
    }
}
```

`resolve` is `async` (a real embedder hits its DB) but `GrantStore::is_granted`
is deliberately **sync** — the executor consults it inline before prompting, so
it must be a fast in-memory/cached check. The write side
(`record`, only reached on an `ApprovalScope::Always` approval) is async and is
where a DB write belongs; the corresponding read then goes through your
`PermissionResolver` on the *next* call, not through `is_granted` — the trait
split exists so the store's `is_granted` only has to cover the fast default
grants the CLI needs, not your DB-backed ones.

**Snapshot-cache pattern for sync resolvers** ([ADR-0076](adr/0076-per-session-dynamic-tool-specs.md)/[ADR-0078](adr/0078-per-turn-dynamic-system-prompt.md),
the same shape #311 reuses): the sibling `tool_spec_resolver` and
`system_prompt_resolver` seams on `EngineConfig` are plain sync `Fn`s consulted
on the turn's hot path, so they can't do I/O either. The documented pattern for
all three is an embedder-owned `Arc<RwLock<HashMap<SessionId, T>>>` hydrated
from your store on a slower cadence (a background refresh task, or on write),
with the hot-path closure only ever reading the cache. Apply the same shape to
a `PermissionResolver` if your DB round-trip is too slow to eat on every tool
call despite `resolve` being `async`.
[`examples/embedded_lifecycle.rs`](../entanglement-runtime/examples/embedded_lifecycle.rs)
wires both `tool_spec_resolver` and `system_prompt_resolver` this way, over one
shared cache keyed by `SessionId`.

## 5. Approval-across-restart semantics

A turn that ends on tool calls **parks** as explicit session state
(`Session.turn: Option<TurnState>`,
[ADR-0061](adr/0061-parked-turn-state-batch-tool-resolution.md)) rather than
living only on an async stack — this is what makes it safe to persist a
mid-turn session and pick it up later, in a different process, after a crash
or a deploy. Three things follow that a custom executor/resolver must honor:

- **Re-offer on resume, at-least-once.** `Holly::resume` re-emits every
  pending call as a fresh `ToolExec` (same `request_id`, new `seq`). If your
  previous process instance already ran that call and the result never made it
  to your (possibly async/batched) sink, it runs again after resume — your tool
  implementations must tolerate a repeat, or dedupe themselves.
- **Idempotency is keyed on `request_id`, not the tool + args.** The reference
  executor tracks an in-flight `HashSet<String>` per session and skips a
  `ToolExec` whose `request_id` it's already running
  ([ADR-0071](adr/0071-parked-turn-reoffer-timer.md)) — this is what makes the
  *in-process* re-offer timer (60s of silence on a parked turn) safe without a
  restart. A custom executor needs the equivalent: track in-flight
  `request_id`s yourself if you don't reuse `ToolExecutor::builder`'s
  executor as-is.
- **Single-instance assumption.** Nothing in the protocol coordinates *two*
  processes racing to resolve the same parked turn — `Holly::resume` assumes
  the caller is the sole authority resuming that session id. If your deployment
  can run more than one instance against the same store, you need your own
  lock/lease per session id before calling `resume`; entanglement doesn't
  arbitrate that for you.

**`Holly::hibernate` is the embedder-driven counterpart to all of this**
([ADR-0077](adr/0077-session-hibernation-evictable-resumable.md)): it tears
down a session's in-memory state — same as a crash, but on purpose, to cap
memory across many idle tenants — without tombstoning the id, so a later
`resume` rebuilds it from your log exactly like the restart path above,
re-offering any pending call the same way. `EngineConfig.idle_ttl`
([ADR-0090](adr/0090-idle-ttl-auto-hibernation.md)) automates the sweep if you'd
rather not call `hibernate` yourself.
[`examples/embedded_lifecycle.rs`](../entanglement-runtime/examples/embedded_lifecycle.rs)
drives the whole `hibernate` → `pair_records` → `resume` → continue round-trip
against its own `RecordSink`.

## 6. Attaching MCP servers programmatically

The YAML `mcp:` config section (`mcp::AvailableMcp::from_config`, connected
lazily on first use — [ADR-0067](adr/0067-mcp-client-as-runtime-tool-provider.md),
[ADR-0226](adr/0226-one-advertised-surface-mcp-envelope-registered-is-available.md)) is one static
`command`/`url` per server, shared by every session — not what a multi-tenant
embedder needs when each tenant has their own remote server or their own
per-user auth token. `mcp::HttpClient::connect(server, url, headers)`
([ADR-0080](adr/0080-mcp-streamable-http-transport.md), behind the `mcp-http`
feature) is public for exactly this: build a client straight from a token you
already looked up, wrap it in `McpClient::Http`, and register its tools with
`McpTool::new(client, server, def)` into any `ToolRegistry` — the same runtime
tool-provider path a YAML-configured server goes through, with zero core
change and no config file in the middle.

```rust
let http = HttpClient::connect("acme", &url, &per_tenant_headers).await?;
let client: Arc<McpClient> = Arc::new(McpClient::Http(http));
for def in client.list_tools().await? {
    registry.register(McpTool::new(client.clone(), "acme", def));
}
```

[`examples/mcp_http.rs`](../entanglement-runtime/examples/mcp_http.rs) compiles
this path on every `make lint` run (it needs `mcp-http`, on by default, so it
can't ride the lean `--no-default-features` build the other two examples
target); set `MCP_HTTP_URL` to actually connect it to a server.

Hand-registering like this shares one client across every session. When each
*user* needs their own servers and credentials routed per session, use the
managed seam instead — `mcp::scoped::McpScopes` in §7 (#684) does the caching,
lazy connecting, and per-session discovery/dispatch replacement for you.

## 7. Multi-user: per-user providers, permissions, tokens

Multi-user mode is an **embedder API**, and by
[ADR-0181](adr/0181-userid-leaves-the-runtime-crate.md) the runtime crate
never names `UserId` (`make userid` enforces it): the type lives in
`entanglement-provider` (re-exported by core, riding `InMsg::Spawn { user }`),
and every per-user seam is something *you* implement and hand in. You already
know which user each session belongs to — you chose `user` when you sent the
session's `Spawn` — so keep your own `HashMap<SessionId, UserId>` (or derive
it from a session-id convention like §1's `{tenant}:{uuid}`) and close your
seam implementations over it. Per seam:

- **Providers, API keys, RPM/concurrency budgets** —
  [`entanglement_provider::multi_user`](adr/0184-provider-hosted-multi-user-seams.md):
  implement `UserProviderStore` over your tenant storage and wire
  `build_user_model_resolver(store, http_client, web_search)` onto
  `EngineConfig::model_resolver`. Each user gets their own `Catalog` + keys
  (never `std::env`); two users sharing one literal key each get their own
  admission slice ([ADR-0175](adr/0175-per-user-admission-gate-on-a-shared-literal-key.md)).
  To start a session on a user-chosen model, put it on the session's own
  `Spawn` — `model: Some(ModelChoice::new(provider, model))` (#725). It
  resolves through that same resolver under the session's user before the
  first round, wins over the agent's pin, and refuses the spawn with an
  `Error` if it can't resolve. Don't send a follow-up `SetModel` for this (it
  queues behind the initiating prompt, so turn one runs on the default) and
  don't pin a sentinel agent model plus a side map keyed by session — both
  race concurrent creates.
- **Permission ceiling + grants** — implement the #311
  `PermissionResolver`/`GrantStore` traits directly over your session→user
  map ([`examples/embedded.rs`](../entanglement-runtime/examples/embedded.rs)
  compiles the per-tenant shape). Compose least-privilege the way the
  in-tree resolver does: resolve the process-global profile first, then
  clamp by the user's own ceiling (`PermissionProfile::clamp_to_base`); key
  `Always`-scope grants by the user so one user's grant structurally cannot
  reach another's session (`Session` scope stays per-session — a session
  belongs to exactly one user).
- **Aux-model pins** — no module either
  ([ADR-0183](adr/0183-narrate-purpose-and-per-user-aux-pins.md)): snapshot
  the user's pins via `AuxModelStore::in_memory` and close your multi-user
  `ModelResolver` over the user before constructing a plain
  `AuxLlmRegistry`.
- **OAuth-protected LLM endpoints** —
  [ADR-0189](adr/0189-oauth-for-llm-provider-endpoints.md): a catalog entry
  carrying an `oauth:` block sends an `Authorization: Bearer` from an
  `AccessTokenSource` instead of a static key. Register each user's source on
  their context:
  `ctx.with_token_source("myproxy", Arc::new(StoredTokenSource::new("myproxy",
  user_scoped(store, user))))` — the same `UserTokenStore` your MCP
  credentials live in works here (mint the tokens with `WebFlow` exactly as
  for an MCP server). An `oauth:` entry with no registered source is a hard
  resolve error; with one, no static key is needed.
- **MCP OAuth credentials** — implement
  `entanglement_provider::oauth::UserTokenStore` over your storage;
  `user_scoped(store, user)` presents one user's slice as the plain
  per-connection `TokenStore` every auth consumer takes
  ([ADR-0184](adr/0184-provider-hosted-multi-user-seams.md)). Minting the
  tokens from a web app is `WebFlow`
  ([ADR-0187](adr/0187-mcp-oauth-web-redirect-flow-for-embedders.md)) — two
  handlers on your own domain:

  ```rust,ignore
  // Handler 1 — "connect kb" button:
  let pending = WebFlow::begin("kb", mcp_url, &oauth_cfg, None,
                               "https://app.example/oauth/mcp/callback",
                               "my-app").await?;
  store_pending(pending.state(), serde_json::to_string(&pending)?); // TTL ~10 min
  Redirect::to(pending.authorize_url())
  // Handler 2 — the callback, possibly on another replica:
  let pending: PendingWebAuthorization = load_and_delete_pending(&params.state)?;
  let auth = pending.complete(&params.code, &params.state).await?;
  user_token_store.save(&user, "kb", &auth)?;
  ```

- **Per-user MCP connections** — `mcp::scoped::McpScopes`
  ([ADR-0188](adr/0188-session-keyed-per-user-mcp-scopes.md)) routes a scoped
  session's `mcp__*` calls over that user's own servers and credentials
  (replace semantics — the global MCP set disappears for scoped sessions;
  same-named servers across users stay distinct via the `(scope key, server)`
  connection cache). Close an `McpScopeResolver` over your session→user map,
  and hand the `McpScopes` to `ToolExecutor::builder(..).mcp_scopes(scopes)`
  (leaving the setter out keeps single-user behavior). MCP schemas are never
  advertised ([ADR-0226](adr/0226-one-advertised-surface-mcp-envelope-registered-is-available.md)), so there is no spec overlay to wire: a scoped
  session's `list_mcp`/`mcp_tools` resolve through its scope and its
  `mcp(...)` calls dispatch over it.

  ```rust,ignore
  let scopes = McpScopes::new(Arc::new(move |session| {
      let user = users.get(session)?;                    // your own map
      Some(McpScope {
          key: user.0.clone(),                           // opaque cache key
          servers: my_db.mcp_servers_of(user),           // the config `mcp:` shape
          token_store: Some(user_scoped(store.clone(), user.clone())),
      })
  }), http_client.clone(), secret_env);
  ```

  The resolver runs per discovery read and per MCP dispatch — keep it an
  in-memory lookup, never I/O. Optionally await `scopes.prewarm(&session)`
  between `Spawn` and the first `Prompt` so `list_mcp` already shows the
  scope's tools (otherwise the first `mcp_tools`/`mcp(...)` connects); call
  `scopes.evict_scope(key)` on logout or whenever a user's server set or
  credentials change (the cache keys on `key` alone). An `oauth:` server
  whose slice holds no token fails the call with
  ``MCP server `name` requires authorization for this user; …`` — catch that
  string shape to prompt the user through the `WebFlow` handlers above.
  Capability hints (`capabilities:` on a scope's server entries) don't reach
  the in-tree profile expansion, which is process-global — fold
  `mcp::capability_index(&scope.servers)` into your own per-user
  `PermissionResolver` if you grade by capability.

## Migrating to one advertised surface (ADR-0226, #818)

[ADR-0226](adr/0226-one-advertised-surface-mcp-envelope-registered-is-available.md) removed the advertising modes and MCP's three states. What an
embedder has to change:

- **Removed types/consts:** `ToolAdvertising`, `Discovery`, `INVOKE_TOOL`,
  `TOOL_SEARCH_CALL_TOOL`, `McpServerState`, `ToolSpec.defer_loading` (drop
  the field from any struct literal), `tool_advertising::AdvertisingInputs`
  (and its pin/repin/encoding API), `mcp::McpEnableTool`, `mcp::connect` (there
  is no startup connect), `McpScopes::overlay_specs` (MCP schemas are never
  advertised).
- **Added:** `MCP_TOOL` (`"mcp"`, re-exported by core) — the envelope name
  core unwraps; `discover::{mcp_spec, list_*_spec, mcp_tools_spec}`;
  `mcp::McpToolCache` (the managed `mcp-tools.yml`); `McpScopes::rows`.
- **Renamed/reshaped:** `AvailableMcp::partition(..) -> (startup, roster)` →
  `AvailableMcp::from_config(..) -> roster`. It reads OAuth credentials from
  the managed `mcp-tokens.yml` by default (`.with_token_store(..)` pins
  another store), but records no last-seen tool names until you chain
  `.with_tool_cache(McpToolCache::managed())` (as `skutter` does); the free
  `mcp::needs_auth` → `AvailableMcp::needs_auth`; `ToolExecutorBuilder::
  advertising_inputs(..)` → `.catalog(Arc<Catalog>)` (the deprecated
  `spawn_tool_executor_with_policy` takes `catalog: Option<Arc<Catalog>>` in
  the same slot); `subagent::agent_specs()` takes no argument (the spec is
  session-invariant); `tool_advertising::surface::SurfaceSources` is
  `{ tools, advertising, agent_specs }`; `PromptContext.skills` →
  `tool_conventions: Option<&'static str>` (the skill roster left the prompt
  for `list_skills`); `McpServerConfig.state` → `description` (`disabled` is
  the one opt-out); `ProviderMcpServer.state` → `description` + `disabled`.
- **Retired config/catalog keys** (ignored with one warning each):
  `config.yml` `tool_advertising:`, `discovery:`, `mcp.<name>.state:`;
  `providers.yml` provider/model `discovery`/`tool_advertising` and a bundled
  server's `state`.
- **Wire:** `McpServerStatus.state` stays serde-defaulted for old logs but is
  always `None`; heads should read `connected` and `auth`.
  `OutEvent::McpChanged` gains a serde-defaulted `error: Option<String>` — set
  when an `McpAdd`/`McpRemove` was refused or failed (a match on the variant
  needs `..` or the new field). Old logs carrying
  `invoke` envelopes still deserialize and replay.

## TLS and the rustls crypto provider

`entanglement-provider` talks TLS through rustls with the **ring** provider and
the **bundled webpki roots**, configured explicitly on every client it builds
(`client::client_builder()`; [provider](architecture/provider.md)). To keep
aws-lc out of the tree it builds reqwest 0.13 with `rustls-no-provider`, and
Cargo's feature unification carries that into *your* build. Two cases bite:

- **You also enable `rustls/aws_lc_rs`** — e.g. stock reqwest 0.13 with its
  default features. Both providers are now compiled, so rustls can no longer
  pick one implicitly (`CryptoProvider::from_crate_features()` returns
  `None`): any code of yours relying on that implicit provider —
  `rustls::ClientConfig::builder()`, tokio-tungstenite's or sqlx's rustls
  paths — panics. Your own `reqwest::Client::new()` still works (reqwest's
  aws-lc feature names its provider explicitly).
- **You use reqwest 0.13 with `default-features = false` and no TLS
  feature.** Our `rustls-no-provider` unifies `__rustls` into your reqwest, so
  your `reqwest::Client::new()` now *panics* at construction for want of a
  provider — previously such a client only failed HTTPS at request time.

The general cure, covering both cases and every rustls user in the process:

```rust
// Early in main, before any client is built. Process-global, first caller
// wins (returns whether this call installed it); the engine never calls it.
entanglement_provider::client::install_default_crypto_provider();
```

Building your reqwest clients from
`entanglement_provider::client::client_builder()` (ring + webpki roots, no
global state) is an alternative **for reqwest clients only** — it fully cures
case (b), but in case (a) your other rustls users (tokio-tungstenite, sqlx,
`rustls::ClientConfig::builder()`) still need the install above or an explicit
provider of their own. An embedder on stock reqwest 0.13
compiles a second TLS provider (aws-lc-rs) next to ours — that's expected.
`rustls-platform-verifier` is compiled too (pulled by `rustls-no-provider`) but
never used: our clients never consult the OS trust store.

## Pinning a dependency

Until now there were no tags, so a `Cargo.toml` git dependency had nothing
stable to pin beyond a raw commit (`rev = "…"`). Starting from this guide,
notable merges get tagged `v0.1.x`:

```toml
entanglement-runtime = { git = "https://github.com/xmiksay/entanglement", tag = "v0.1.0", default-features = false }
```

Prefer `tag =` over `rev =` in a site-style consumer — it survives a
`cargo update` re-resolve intentionally (a `rev` pin is silently frozen either
way, but a tag documents *which* release you're on).

Every tagged release also publishes to crates.io (see
[`releasing.md`](releasing.md)), so once the crate you need has a release out,
a plain registry dependency works too and needs no `git`/`tag` at all:

```toml
entanglement-runtime = { version = "0.1", default-features = false }
```
