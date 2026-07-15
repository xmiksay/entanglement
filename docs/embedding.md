# Embedding `entanglement` — a multi-tenant guide

How to build a **custom head** on top of the engine: one process serving many
users, each isolated, without forking anything. This is the path
`entanglement-runtime --no-default-features` exists for
([ADR-0025](adr/0025-runtime-cargo-feature-gates.md)) — the lean library gives
you the tool-execution loop, permission dispatch, sub-agent spawn, and
persistence machinery with zero CLI/TUI/transport weight. Everything below is
exercised by a compiling, runnable example:
[`entanglement-runtime/examples/embedded.rs`](../entanglement-runtime/examples/embedded.rs)
(`cargo run -p entanglement-runtime --example embedded --no-default-features`,
no provider key required — it runs against the built-in `EchoLlm`). `make
lint`/`make check-lean` run `clippy --all-targets` over it on every change, so
the snippets quoted here can't silently drift from the real API.

Until this guide, the only wiring reference was `main.rs`/`run.rs` — written
for a single-user CLI. Multi-tenancy is not a separate mode the engine knows
about; it falls out of five ordinary decisions an embedder makes about its own
process, covered in order below. Part of #307/#315.

## 1. One engine, many tenants

Run **one `Holly`**, not one per tenant. `Holly::spawn` is a supervisor task
plus two broadcast channels ([`entanglement-core/src/holly.rs`](../entanglement-core/src/holly.rs));
sessions are cheap, lazily-spawned tasks keyed by `SessionId`
(`entanglement-core/src/holly.rs`, the `supervisor` loop). Spinning up a whole
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

A spawned sub-agent's id is minted by the runtime's `agent`/`agent_spawn` tool,
not by your head, but it stays inside the same tenant's tree — child sessions
never cross the parent's namespace (spawn is always relative to a running
session, `entanglement-core/src/holly.rs`'s `Spawn` arm), so filtering by root
prefix already covers the whole subtree.

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

`OutEvent::session()` returns `None` for the handful of supervisor-global
query replies (`SessionList`, `History`'s correlation reply —
[ADR-0072](adr/0072-protocol-warts-settled-before-serve.md)); those never carry
tenant data on their own, but if your embedder answers `ListSessions` you must
still filter its `sessions: Vec<SessionInfo>` payload by tenant before it
reaches a client, since a raw reply lists every session in the process.

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
"tenant" to enforce.

## 3. Custom persistence: tap → sink → lazy `resume`

The reference persistence layer
(`entanglement-runtime/src/persistence.rs`,
[ADR-0020](adr/0020-event-sourced-session-persistence.md)) is already split
into a fixed *tap* and a pluggable *sink* (#313), so a DB-backed embedder swaps
one trait impl instead of forking the subscriber:

```rust
pub trait RecordSink: Send + Sync {
    fn append(&self, root: &SessionId, record: &LogRecord) -> anyhow::Result<()>;
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

## 4. Custom tool execution / policy

`spawn_tool_executor_with_policy` (#311,
[ADR-0079](adr/0079-pluggable-permission-resolver-and-grant-store.md)) is the
seam: it drives two trait objects instead of the CLI's file-backed defaults,
so a per-tenant DB-backed policy plugs in without forking the ~350-line
executor (spawn/mask gating, hooks, `rhai`, the plan/tasks tools all stay
shared):

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
  `request_id`s yourself if you don't reuse `spawn_tool_executor_with_policy`
  as-is.
- **Single-instance assumption.** Nothing in the protocol coordinates *two*
  processes racing to resolve the same parked turn — `Holly::resume` assumes
  the caller is the sole authority resuming that session id. If your deployment
  can run more than one instance against the same store, you need your own
  lock/lease per session id before calling `resume`; entanglement doesn't
  arbitrate that for you.

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
