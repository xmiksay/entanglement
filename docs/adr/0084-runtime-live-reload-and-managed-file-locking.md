# 0084. Runtime-side live reload (inotify) and advisory locking for managed files

- Status: Accepted
- Date: 2026-07-15
- Builds on the "live registry mutation" rejection of
  [0081](0081-per-profile-model-pinning-and-rebind-on-set-agent.md) (the
  precedent this ADR follows: core holds `EngineConfig.profiles` immutably for
  the process lifetime — no seam is added here either), the local trust
  boundary of [0047](0047-local-trust-boundary.md) (definitions are trusted,
  layered, embedded < user < project), the approval-scope grants file of
  [0052](0052-approval-scope-and-persisted-grants.md) and the managed env-file
  writer of [0073](0073-managed-env-file-writer-and-key-surfaces.md) (the two
  managed-file precedents this locks), and the pluggable policy seam of
  [0079](0079-pluggable-permission-resolver-and-grant-store.md) (the
  `Arc<RwLock<..>>` swap point this reuses for the profile registry). Issue
  #329.

## Context

Two related gaps in the runtime, both about a long-running `skutter` process
(TUI, `serve`) drifting from what's on disk:

1. **No live reload.** Agent/skill/provider-catalog/config definitions are
   loaded once at startup ([0034](0034-file-based-agent-definitions.md),
   [0036](0036-skill-discovery-and-registry.md)). Editing an agent's
   frontmatter or adding a skill mid-session requires a restart to take
   effect — annoying during iteration on a profile, and impossible for a
   long-lived `serve` process.
2. **No write locking.** `grants.yml` (#174), the managed `.env` (#220), and
   `agent-models.yml` (#323) are each read-modify-written by
   `FileGrantStore::record`/`env_key::set_key`/`AgentModelStore::set`. Two
   `skutter` instances against the same project (e.g. a `serve` head plus a
   `tui` head, or two terminal tabs) doing load → mutate → write concurrently
   can silently clobber each other's update: the second writer's `self.always`
   (or `self.agents`) was read *before* the first writer's change landed, so
   its write overwrites that change with stale in-memory state.

## Decision

### Locking (`config::lock`)

A new `entanglement-runtime/src/config/lock.rs` module:
`with_locked_file(path, f)` acquires an exclusive `fd-lock::RwLock` on
`path`'s **sibling** `<name>.lock` file (never the target file itself — that
one is replaced wholesale by `atomic_write`'s temp-file-then-rename, which
would silently drop a lock held on the old inode), creating the parent dir and
lock file if missing, then runs `f` under the held lock. `f` is expected to
**re-read the target's current on-disk state itself** — the lock only
serializes callers, it caches nothing — so a write merges against the latest
state, not a snapshot taken before the lock was acquired.

All three managed read-modify-write cycles route through it, converging on
**read-current → merge → write → adopt the merged result into memory**:

- `grants::FileGrantStore::record` (`Always` scope) now calls a `persist`
  that, under the lock, re-reads the file's current `Always` set, inserts the
  new key, writes it, and sets `self.always` to the merged result — so this
  process's own `is_granted` reads stay consistent with what's now on disk,
  including a concurrent writer's own addition.
- `config::agent_models::AgentModelStore::set` follows the identical shape;
  the write itself is factored into a free `persist_map(path, &agents)` so the
  locked closure doesn't need `&self`.
- `config::env_key::set_key` wraps its existing read → `upsert` → `atomic_write`
  body in the lock (path resolution and value validation stay outside the
  lock — fail fast before blocking on it).

`write_grants` (grants.rs) is migrated off a raw `std::fs::write` onto the
shared `atomic_write` (already used by `env_key`/`agent_models`), so all three
managed files now share one write primitive.

Each store also gains a `reload()` (`FileGrantStore`/`DefaultGrantStore`/
`AgentModelStore`) — a plain re-read from disk, no lock needed (a torn read of
a file mid-`atomic_write`'s rename is impossible: the rename is atomic, a
reader always sees either the old or the new complete file) — which the
definitions watcher below calls to pick up another instance's write.

### Live reload (`watch`)

A new `entanglement-runtime/src/watch.rs`, split into a pure primitive and the
concrete reload logic:

- `spawn_debounced_watcher(paths, debounce, on_change)` is the primitive:
  watches every **existing** path (a path that doesn't exist at watch-start is
  skipped — `notify` cannot watch a path that isn't there yet; a directory
  created after startup needs a restart, a documented v1 limit) via
  `notify_debouncer_mini::new_debouncer`, which already runs its own
  background thread (notify's watch API is blocking) and batches every raw
  filesystem event landing inside one `debounce` window into a single
  `handle_event` call. The handler forwards one signal per batch over a tokio
  `mpsc` channel; a tokio task relays each signal into `on_change`, holding
  the `Debouncer` guard alive for the task's lifetime (it stops on drop, i.e.
  on `JoinHandle::abort`). Returns `None` (spawns nothing) when no candidate
  path exists — independently unit-tested without touching any real
  definition loader.
- `watch_paths(cwd)` resolves what to watch: every candidate agent/skill dir
  `layers::candidate_dirs` would read, `${config_dir}/entanglement/` (covers
  the provider catalog, user config, and all three managed files in one
  recursive watch), `<root>/.entanglement/` (the project config layer), and
  the parent dir of any `ENTANGLEMENT_*_FILE`/`*_DIR` override — deduplicated
  via a `BTreeSet`.
- `spawn_watcher(cwd, live, notice)` wires the two together: on a debounced
  change it calls `reload(cwd, live)`, which re-runs `skills::load_registry` +
  `agents::load_registry`, reloads the grants/agent-models stores from disk
  and re-applies the agent-models overlay onto the freshly-loaded profiles,
  and — **only if every step succeeds** — swaps the result into `live`. A
  malformed edit mid-save must not crash a long-running watcher or wipe a
  previously-good in-memory registry (unlike startup, which fails fast): the
  reload logs and keeps serving the last-known-good state instead.

`LiveDefinitions` is the swap target — **the runtime's own mirrors**, not
core's `EngineConfig`:

```rust
pub struct LiveDefinitions {
    pub profiles: Arc<RwLock<ProfileRegistry>>,
    pub skills: Arc<RwLock<Arc<SkillRegistry>>>,
    pub agent_models: Arc<Mutex<AgentModelStore>>,
    pub grants: Arc<DefaultGrantStore>,
}
```

Three call sites now read through these live handles instead of an owned
snapshot:

- `tool_runner::spawn_tool_executor_with_policy`'s `profiles` parameter is now
  `Arc<RwLock<ProfileRegistry>>` (was a plain `ProfileRegistry`); its three
  `profiles.get(name)` lookups (on `SessionStarted`, `AgentChanged`, and the
  `ToolExec` self-heal, #156) and the `spawn_refusal` chain check take a brief
  read lock, so permission grade + spawn-target resolution always sees the
  current registry. The two convenience wrappers
  (`spawn_tool_executor`/`_with_hooks`) keep their historical owned-registry
  signature (wrapping it in `Arc::new(RwLock::new(..))` internally) for
  existing callers/tests that need no live reload.
- `skills::load_skill::LoadSkillTool`'s `registry` field is now
  `Arc<RwLock<Arc<SkillRegistry>>>` (was `Arc<SkillRegistry>`) — matching
  `LiveDefinitions::skills`'s exact type so `main.rs` shares the same handle
  with no adapter. The `run` handler clones the current `Arc<SkillRegistry>`
  out from under a brief read lock (cheap — a refcount bump) before resolving
  the skill name, so it never holds the lock across the `async` call.
- The TUI's `/agent` picker + Tab-cycle roster (`App::available_profiles` /
  `primary_profile_order`) and `/model` persistence (`App::agent_models`) are
  threaded the same live handles; a new `App::refresh_profiles` re-derives the
  roster from a freshly reloaded registry (the startup derivation in
  `App::new` and this reload path share one `primary_order` helper). `tui()`
  gains a `reload_rx: UnboundedReceiver<String>` arm in its `tokio::select!`
  loop that calls `refresh_profiles` + records a status line
  (`App::record_reload_status`) on each reload notice.

**What does *not* change**: core's `EngineConfig.profiles` (baked into
`Holly::spawn` once) and the per-session system prompt/tool-mask it derives
stay pinned to what was loaded at process start, for the lifetime of every
session already running. This is the ADR-0081 precedent applied identically:
core holds the registry immutably; adding a live-mutation seam there would be
a concurrency hazard for no gain, since the switch this issue actually needs
— "the next new session, and the next explicit `SetAgent`/picker pick, sees
fresh state" — is fully served by the runtime's own mirrors, which the
runtime *already* re-resolves fresh per call (the `ToolExec` self-heal, #156;
the TUI picker only reads its roster when opened). A currently-running turn's
resolved system prompt/tool-mask is unaffected by a reload; only the next
`SetAgent`/new session picks up the edit.

## Consequences

- Two `skutter` instances against the same project no longer silently lose an
  `Always` grant, a provider-key write, or a `/model` pin to a race — the
  losing writer's change is now merged in, not clobbered.
- Editing an agent/skill file, or another instance touching `grants.yml`/
  `agent-models.yml`, is visible to a long-running `tui`/`serve` process
  within one debounce window (500ms in production) without a restart, for
  every surface that consults the runtime's mirrors (permission grade, the
  `/agent` picker, `load_skill`).
- Known v1 limits, both documented rather than solved: (a) a directory that
  doesn't exist at watch-start needs a restart to be picked up once created;
  (b) a currently-running turn's system prompt/tool mask is unaffected by a
  reload — only the *next* `SetAgent`/session sees it, per the precedent
  above.

## Rejected alternatives

- **Live-mutate `EngineConfig.profiles` in core.** Rejected per the
  [0081](0081-per-profile-model-pinning-and-rebind-on-set-agent.md) precedent:
  core holds the registry immutably for the process lifetime, and mutating
  the shared registry live would add a concurrency hazard (a turn reading it
  mid-mutation, or reconciling with an in-flight `SetAgent`) for no gain the
  runtime's own mirrors don't already cover.
- **A wire-level reload message / new `InMsg`.** Rejected: this is a
  runtime-internal mirror swap, not an engine state transition — no core
  protocol change is needed, and adding one would put reload timing on the
  wire for something no head-to-engine contract requires.
- **Poll instead of inotify.** Rejected: a poll loop wastes CPU scanning
  unchanged directories indefinitely and adds reload latency bounded by the
  poll interval, where `notify`'s inotify backend delivers events immediately
  and is already a pure-Rust, hygiene-gate-clean dependency (`make
  check-lean` confirms `notify`/`notify-debouncer-mini`/`fd-lock` drag in
  nothing on the `LEAN_FORBIDDEN` list).
