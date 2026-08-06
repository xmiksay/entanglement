# 0173. Watcher-driven plan-file-changed notice

- Status: Accepted
- Date: 2026-08-06
- Amends: [0145](0145-one-plan-tool-file-backed-plans-and-blocking-review-loop.md)
  "Consequences" (closes the "No watcher-driven 'plan updated by user'
  transcript notice" item). Issue
  [#627](https://github.com/xmiksay/entanglement/issues/627) (orig.
  [#513](https://github.com/xmiksay/entanglement/issues/513), tracked by the
  #396 ledger epic via [#624](https://github.com/xmiksay/entanglement/issues/624)),
  part of [#624](https://github.com/xmiksay/entanglement/issues/624).

## Context

[0145](0145-one-plan-tool-file-backed-plans-and-blocking-review-loop.md)
shipped `propose_plan`'s staleness guard: a session-scoped content-hash
binding (`entanglement-runtime/src/plan_files.rs`, `PlanFileRegistry`) that
refuses a `path`-mode resubmit if the bound file changed since the session
last touched it. The guard is deliberately passive — it only *checks* at the
next `propose_plan(path=...)` call — so a user who edits the plan file in
their own editor between phases gets no signal at all until (and unless) the
agent happens to resubmit that exact path. 0145's own "Consequences" named
this a deliberate deferral: a live, watcher-driven notice was future work,
logged in the deferred-work-ledger's row 7.

The obvious reuse candidate is `watch.rs`'s #329 definitions watcher
(`spawn_watcher`/`LiveDefinitions`) — the only filesystem watch already in the
runtime. But that watcher's whole shape is built around one specific reload
action: re-run the skill/agent loaders and swap the result into runtime-held
mirrors, gated by a fingerprint restricted to definition/config file shapes
(`*.md`/`*.yml`/`.env`) that's tuned to skip its own loaders' read-back
(`reload()` reads every watched file every pass, which would otherwise
perpetually re-trigger the watch on some filesystems). A plans-folder watch
needs none of that machinery — no loader to re-run, no `LiveDefinitions` to
swap, no notion of "reloaded" at all. It needs exactly one thing:
"filesystem changed under this directory" debounced into one callback. That
primitive already exists inside `watch.rs`, factored out from day one
(`spawn_debounced_watcher`, independently unit-tested), and bolting the
plans-folder path onto `LiveDefinitions`'s fingerprint/reload cycle would
conflate two semantically unrelated reload actions in one module for no
benefit.

## Decision

### A dedicated `plan_watch` module, reusing only the debounce primitive

`entanglement-runtime/src/plan_watch.rs` is new, small, and independent of
`watch.rs`'s definitions-reload semantics. `spawn_debounced_watcher` — until
now a private `fn` inside `watch.rs` — is exposed `pub(crate)` so this module
can call it directly:

```rust
pub fn spawn_plans_watcher(
    holly: &Holly,
    root: PathBuf,
    plan_files: Arc<PlanFileRegistry>,
) -> Option<tokio::task::JoinHandle<()>>
```

It watches a single directory, `<root>/.entanglement/plans/`, at the same
500ms debounce window the definitions watcher uses. On a debounced firing it
re-hashes every file `plan_files.snapshot()` currently has a binding for and
compares against that binding's last-known hash — a pure, independently
unit-tested `out_of_band_changes` function, mirroring `watch.rs`'s own
`fingerprint`/`definitions_changed` split. A mismatch means the file changed
by something other than this runtime's own tracked writes (an in-session
`edit`/`write` already refreshed the registry's hash via the existing
`FileChange` listener, so it never mismatches here) — i.e. a genuine
out-of-band, almost certainly user-made, edit.

### Sharing the registry, not duplicating detection

Deliberately **not** a second, independent staleness check: `spawn_plans_watcher`
takes the exact same `Arc<PlanFileRegistry>` instance the tool executor's
`propose_plan` dispatch arm and its `FileChange` listener already read/write
(`tool_runner::spawn_tool_executor_with_policy` now takes `plan_files` as a
parameter instead of constructing it internally, specifically so `main.rs` can
hand the identical instance to both). On a detected mismatch the watcher does
two things atomically, in this order: (1) `plan_files.record(...)` the fresh
hash — self-healing the registry immediately, so the guard's *next*
`propose_plan(path=...)` check sees the file as current, not stale — then (2)
emits the notice. Self-healing first means a user who edits the file and later
lets the agent resubmit it gets exactly one signal (the live notice), not two
(the live notice *and* a redundant hard refusal repeating the same fact the
head was already told).

### Wire shape: a new session-scoped `OutEvent::PlanChanged`

```rust
PlanChanged { session: SessionId, seq: u64, path: String, hash: String }
```

Session-scoped and seq-bearing (via `Holly::emit_for_session`, minting a fresh
seq off the session's shared counter — the same mechanism `propose_plan.rs`
already uses for `Plan`/`ToolRequest`), not the engine-global shape `Throttle`
uses. A plan file inherently belongs to one session's transcript (and,
per `path` mode, is arbitrary — nothing ties it to a fixed naming
convention the way `content` mode's `<short-session-id>.md` does), so
routing it as ordinary session-scoped wire data — automatically persisted by
`persistence.rs` (any event with `.session().is_some()` is appended) and
automatically fanned to *every* head (TUI, `serve`, `pipe`, `run --format
json`), not just the TUI — is the more consistent design than the
definitions watcher's own bespoke `mpsc<String>` toast channel, which bypasses
`Holly`/`OutEvent` entirely and is therefore invisible to every head but the
TUI. `Session::replay`'s fold treats it like `SkillActive`: no `Context`
mutation, caught by the wildcard `_ => {}` arm — a head reconstructs its own
display state from the log, core has nothing to reconstruct.

The TUI folds it into `session_view::reducer.rs`'s `apply_event` exactly like
`SkillActive`/`AmbiguousRetry`: a durable `ToolOutput`-shaped transcript
notice (`record_status`'s reuse pattern), seq-deduped against
`last_seen_seq`, not a transient toast — because unlike a definitions reload
(ambient, no action needed) this is actionable information about a specific
plan file the user or agent is actively working with, so it belongs in the
reviewable scrollback. `run.rs`'s one-shot text renderer gained a matching
one-line arm for non-TUI heads.

## Consequences

- A user editing a bound plan file out of band now gets a live signal instead
  of only a delayed refusal at the *next* `propose_plan(path=...)` call — and
  that later call is no longer even refused, since the watcher already
  self-healed the registry.
- `spawn_tool_executor_with_policy` gains one new parameter
  (`plan_files: Arc<PlanFileRegistry>`) instead of constructing its own
  internally; the two convenience wrappers (`spawn_tool_executor`,
  `spawn_tool_executor_with_hooks`) construct a private, unshared instance —
  byte-identical behavior for their ~30 test-only callers, none of which wire
  up a plans watch.
- `watch.rs`'s `spawn_debounced_watcher` is now `pub(crate)`, a second
  concrete caller alongside its own `spawn_watcher` — proof it was already a
  clean, definitions-decoupled primitive, not just a private implementation
  detail.
- Same known v1 limitation as the definitions watcher: `.entanglement/plans/`
  must exist at watch-start (`notify` can't watch a path that isn't there
  yet); a project's first-ever `propose_plan(content=...)` call creates the
  directory but the watch needs a process restart to pick up a directory
  created after startup. Acceptable for the same reason the definitions
  watcher accepts it — a restart is already how any live-reload gap in this
  codebase self-heals, and a project's plans folder, once created, persists.
- [docs/deferred-work-ledger.md](../deferred-work-ledger.md) row 7 moves to
  Resolved.

## Rejected alternatives

- **Bolt the plans-folder path onto `watch.rs`'s existing `spawn_watcher`/
  `LiveDefinitions`.** Rejected per the Context above: two unrelated reload
  actions (re-run loaders + swap mirrors vs. re-hash a registry + emit a wire
  notice) sharing one fingerprint/reload cycle would make `reload()` and its
  anti-self-trigger fingerprint guard try to serve two masters, and a plans
  edit would have to fabricate a fake "definitions reloaded" story it doesn't
  have.
- **A bespoke `mpsc<String>` toast channel**, mirroring the definitions
  watcher's own `reload_tx`/`reload_rx` plumbing. Rejected: that channel
  bypasses `Holly`/`OutEvent` entirely, so only the TUI (which happens to hold
  the receiving end) ever sees it — `serve`/`pipe`/`run` would have no way to
  observe a plan file changing out from under a session they're driving. An
  ordinary session-scoped `OutEvent` costs nothing extra and reaches every
  head for free.
- **Poll `PlanFileRegistry` on a timer instead of watching the filesystem.**
  Rejected as strictly worse than what already exists: `notify` push
  notifications are cheaper and lower-latency than any polling interval short
  enough to feel "live", and the debounce primitive was already sitting there
  unused for this purpose.
- **An engine-global `OutEvent` (no `session`), mirroring `Throttle`.**
  Rejected: unlike an LLM-endpoint throttle transition (which has no single
  owning session by construction — an endpoint is shared across every
  session using that provider), a plan file *does* have exactly one owning
  session (the one bound to it in `PlanFileRegistry`) — discarding that and
  making every head re-derive "which session does this notice belong to"
  would be strictly worse than just carrying it on the wire.
