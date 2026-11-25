# 0086. `RecordSink` — pluggable persistence append target

- Status: Accepted
- Date: 2026-07-15
- Splits the persistence tap's *what to persist* (event-sourcing logic: route
  each record to its root session, tombstone broadcast-lag gaps) from its
  *where to persist* (JSONL file), the seam multi-tenant embedding needs (#307)
  so a Postgres-backed embedder doesn't fork the subscriber. Part of #307.
  Issue #313.

## Context

`spawn_persistence_subscriber` coupled the event tap — subscribing `Holly`'s
in/out broadcasts, building `LogRecord`s, tombstoning gaps on broadcast lag
(#104) — to one append target: a JSONL file under the `session_store` layout.
An embedder persisting elsewhere (the site: a Postgres `assistant_events`
table) had to copy the whole subscriber to change one line, and would then
have to track upstream gap/lag fixes by hand instead of inheriting them.

## Decision

`RecordSink` (`entanglement-runtime::persistence`):

```rust
pub trait RecordSink: Send + Sync {
    fn append(&self, root: &SessionId, record: &LogRecord) -> anyhow::Result<()>;
}
```

`spawn_persistence_subscriber_with_sink(holly, sink: Arc<dyn RecordSink>)` is
the seam; the existing `spawn_persistence_subscriber(holly, cwd)` becomes a
thin wrapper over it + a new `FileSink` (the same file-backed behavior,
`session_store::append` under the hood) — the default entry point's signature
and behavior are unchanged. The tap keeps all its logic — routing to root,
`record_gap`'s tombstone-on-`RecvError::Lagged` — so every sink inherits it for
free; only the append call is swapped.

`append` is **synchronous** by design: the tap runs the broadcast receive loop
directly, and a sink that blocks on it (network, DB) starves the receiver and
manufactures the very `Gap` tombstones the trait exists to avoid. A blocking
backing store must put a bounded channel + dedicated writer task behind
`append` and return immediately (drop-and-error past the bound, never await).
This is documented on the trait, not enforced by the type — the same
"sync boundary, documented buffering contract" shape as `tool_spec_resolver`
[0076](0076-per-session-dynamic-tool-specs.md)/`system_prompt_resolver`
[0078](0078-per-turn-dynamic-system-prompt.md)'s sync `Fn` + snapshot-cache
guidance.

`session_store::read`/`pair_records` stay file-side only — `Holly::resume`
already accepts records from anywhere, so no matching read-side trait is
needed; a DB-backed embedder reads its own store and hands `resume` the
records directly.

## Consequences

- File behavior is byte-identical (existing persistence/replay tests pass
  unchanged); a DB-backed sink is a ~20-line `impl RecordSink` plus its own
  buffering, not a subscriber fork.
- Gap/lag fixes to the shared tap benefit every sink automatically.
- Rejected: an `async` `append` (the tap's receive loop is not async-blocked on
  persistence today; forcing it would slow every sink, including the common
  file case, for the rare DB one), a generic `Write`-based sink (JSONL framing
  is `session_store`'s concern, not every embedder's), a read-side
  `RecordSource` trait mirroring `RecordSink` (no embedder need identified —
  `resume` already takes plain records).
