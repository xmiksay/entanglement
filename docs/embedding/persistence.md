# Embedding §3 — Custom persistence

> Part of the [embedding guide](../embedding.md).

## 3. Custom persistence: tap → sink → lazy `resume`

The reference persistence layer
(`entanglement-runtime/src/persistence.rs`,
[ADR-0020](../adr/0020-event-sourced-session-persistence.md)) is already split
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
history. A session keeps the root it started under for life; when a parent's
compaction moves a live sub-agent under the successor, your sink also gets a
`LogPayload::Adopted { from }` row for the successor's root, naming the root
whose stream holds the sub-agent's history. To resume that root with its
moved sub-agents, prepend the named root's records for that sub-tree, as
`SessionStore::read_tree` does for files. `RecordSink::touch(from)` is called
alongside, so a sink with time-based retention can keep the named root
([ADR-0234](../adr/0234-live-sub-agents-survive-compaction.md)).

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
([ADR-0210](../adr/0210-unknown-session-policy.md)). By default a routable
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
([ADR-0211](../adr/0211-idgen-context-and-tenant-prefix-inheritance.md)). A
resolver deciding whether a session's tool call belongs to a user (a
permission seam, a per-user MCP scope) reads the engine's own directory
synchronously — `SessionInfo { user, parent, predecessor, agent, root, .. }`
— instead of mirroring `SessionStarted` off the broadcast, which races the
first tool call. It is `Some` from before the session's `SessionStarted`
until the engine processes its `CloseSession` or hibernation, `None` for an
unknown, closed or hibernated id, and a resume (explicit or through the
loader above) re-registers the same lineage and `user` from the log. A
compaction successor reports its `predecessor` and the `user` it inherited
from it. A root's successor is persisted under its **own** root id, so your
sink receives its records keyed by the successor id — tenant-prefixed by the
default generator (§1), which is what lets your loader find it again. For
routing *events*, key on the prefix rather than `session_info`: a session's
trailing `SessionEnded` arrives after its directory entry is gone. `parent`
is the one field that can change: when a session compacts, its live sub-agents
move under the successor (`OutEvent::ParentChanged`, ADR-0234), and
`Holly::successor_of(id)` names the session a compacted id became
(`Holly::lineage_head(id)` the id it started as). The runtime's executor
treats every session that starts with a `parent` as that parent's sub-agent:
when it has not launched it itself (a resumed sub-agent, or a child your head
spawned directly with `InMsg::Spawn { parent }`), it registers it so the
parent's `poll`, `agent_send` and `ListOperations` reach it.

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

[`examples/embedded_lifecycle.rs`](../../entanglement-runtime/examples/embedded_lifecycle.rs)
wires a `RecordSink` (an in-memory stand-in for a DB), reads its own log back
through `pair_records`, and drives the full round-trip end to end.

---

Previous: [§1–§2 — Many tenants & a custom head](tenants-and-heads.md) · Next: [§4 — Custom tool execution / policy](tool-execution.md)
