# Heads & persistence §6b — Session persistence & resume

> Part of the [heads & persistence module](../heads-and-persistence.md) of the [architecture overview](../../architecture.md).

## 6b. Session persistence & resume — [ADR-0020](../../adr/0020-event-sourced-session-persistence.md) (`persistence` + `session_store`)

Sessions are event-sourced to disk, one JSONL file per **root** session under
`<data_dir>/entanglement/sessions/<safe-cwd>/<root_id>.jsonl` (`session_store`).
That directory is a `SessionStore` value handed to every reader/writer — the
persistence tap, the `ReplayFrom` responder, the startup prune, resume and the
TUI resume modal — rather than re-resolved from the data dir at each call
(#747): the in-tree heads build `SessionStore::for_cwd(cwd)` (the layout
above), an embedder or test pins `SessionStore::at(dir)`. Under `cfg(test)` the
data-dir resolution panics, so a unit test can never read, write or prune the
developer's real logs. `spawn_persistence_subscriber` (`persistence`) taps **both** directions of the
ABI — `holly.subscribe()` for `OutEvent`s and `holly.subscribe_inbound()` for
`InMsg`s — and appends each frame as a `LogRecord { ts, session, payload }` where
`payload` is `LogPayload::In(InMsg) | Out(OutEvent) | Gap { dropped } | Retired { kind }`
(a tombstone and a read-only compat marker, both below). Logging inbound messages is
what makes a session resumable: `Session::replay` reconstructs user turns from
the logged `InMsg::Prompt` records, so without them a resumed context holds only
assistant/tool messages and the model appears to forget the conversation.

- **Inbound is biased ahead of outbound** so a prompt lands on disk before the
  events it produces (`pair_records`, `session_store/pairing.rs`, queues the
  `Prompt`/`Stop` records replay reads first-in first-out and pairs each with a
  following `Out`, so a prompt logged just before a `ToolResult` is not lost).
  `InMsg::Resume` is skipped (it carries the whole prior log → recursion/bloat).
  `InMsg::Spawn` is still never persisted verbatim (`roots` can't resolve the
  child to its parent's root file until the child's own `SessionStarted`
  arrives — logging it earlier would create a stray child root). Its `prompt`
  **is** captured, though (#421,
  [ADR-0113](../../adr/0113-persistence-synthesizes-a-spawned-childs-initiating-prompt.md)):
  the tap caches it (`pending_spawn_prompts: HashMap<SessionId, String>`, keyed
  by child id) and, once that `SessionStarted` resolves `roots`, synthesizes an
  `InMsg::Prompt { session: child, .. }` record right after it — so replay
  reconstructs the task instruction that framed the child's first turn, not
  just its eventual reply. The cache entry is consumed on first use, so a
  resumed child's re-announced `SessionStarted` (resume never re-sends `Spawn`)
  never re-synthesizes or duplicates the record.
- **Spawned children fold into the root file** via a `roots` map built from
  `SessionStarted { root, parent }`, so each root file is a self-contained,
  replayable record of the whole session tree. A sub-agent's compaction
  successor starts under that sub-agent's parent, so it folds into the same
  root file like any sub-agent. **A session logs to the root it started under
  for life** ([ADR-0234](../../adr/0234-live-sub-agents-survive-compaction.md)):
  when a parent's compaction moves a live child under the successor
  (`OutEvent::ParentChanged`), the child's records, the move included, keep
  going to the predecessor's file, and the tap appends a
  `LogPayload::Adopted { from }` pointer (record session = the child) to the
  successor's root file, deferred until that root resolves and skipped when
  both are the same file. Moving the history instead would need the tap to
  re-read its sink, which `RecordSink` (append-only) cannot.
- **The `<env>` date is pinned per session** ([ADR-0202](../../adr/0202-prompt-cache-discipline-anchors-deferral-replay-compaction-date.md) §5,
  `env_date::EnvDatePins` on the shared `AdvertisingState`): the system-prompt
  resolver stamps the date a session first resolves and patches every later
  turn's `Date:` line back to it — even after midnight or a definitions reload
  re-bakes a newer one — so the cached system block stays byte-identical. The
  executor forgets the pin on `SessionEnded`/`SessionHibernated`; a resumed
  session re-pins to its resume day.
- **Resume** reads the root's tree (`SessionStore::read_tree`: the file, with
  each `Adopted` pointer resolved to the adopted sub-tree's records from the
  file it names, recursively, those records first — a session's records come
  from one file in order and a pointer always names an older file, so every
  session's history stays chronological; a pointer to a deleted or pruned file
  drops that sub-tree, including any post-resume records it wrote into this
  file, rather than resuming it from a partial history; a pointer the tap
  never wrote — it is held in memory until the successor's root resolves —
  is covered by also reading the resumed root's immediate predecessor's tree
  (with its pointers resolved) for sessions whose last `ParentChanged` names
  this tree — one hop, since
  only the newest compaction can lack its pointer; each file is read once
  per call; ADR-0234),
  `pair_records` builds the `(Option<InMsg>, OutEvent)`
  stream, and `Holly::resume` seeds a session from `Session::replay`. The CLI
  exposes `skutter run --resume <id>` and `skutter sessions` (lists past root
  sessions for the cwd); the TUI `/resume` modal restores the full visible
  transcript (`restore_from_records`) *and* reseeds engine context. **Resume
  cascades over the spawn sub-tree** (#415,
  [ADR-0112](../../adr/0112-resume-cascades-over-the-spawn-subtree.md)): since a
  root file already carries every spawned child's interleaved records
  (previous bullet), the supervisor doesn't stop at re-materializing the
  requested id — it walks that session's replay-reconstructed `children` and
  recursively `Session::replay`s + re-spawns each one still "live" in the log
  (a `SessionStarted` with no matching `SessionEnded`/`SessionHibernated`),
  mirroring `CloseSession`/`HibernateSession`'s teardown cascade in reverse. A
  parent resumed after a crash/hibernation can still reach and continue its
  sub-agents instead of them silently vanishing (touching an untouched child
  id would otherwise hit the lazy-`Prompt` path's known-child refusal,
  [ADR-0168](../../adr/0168-lazy-prompt-refuses-a-known-hibernated-child.md)/#639,
  rather than actually resuming it). Replay folds `ParentChanged`, so a moved
  child resumes under its successor parent, and the runtime's lifecycle loop
  registers each resumed sub-agent new to this process with the parent's
  `AgentRegistry` (under the handle the parent knew, `Holly::lineage_head`:
  the resume re-records every compaction lineage its log names) with a
  watcher for its next answer, so `poll`,
  `agent_send` and `ListOperations` reach it again. Resume and
  file-backup reads follow pointers (backups are still *written* per file;
  a revert reads them across the tree through `SessionStore::tree_files`
  and takes the changes whose recorded `ancestry` names its target,
  [ADR-0236](../../adr/0236-rewind-follows-adopted-sub-agents.md));
  `skutter sessions` and `ReplayFrom` stay per file.
  Both listings carry a **first-prompt snippet** (#327): `list_sessions` captures the
  first `InMsg::Prompt` in the same pass that finds `SessionStarted` (no extra
  I/O), truncates it to ~60 chars on a word boundary with `…`
  (`SessionMeta::first_prompt`), and both `skutter sessions` (DESCRIPTION column)
  and the `/resume` rows render it beside the full session id. The in-memory
  `ListSessions`/`SessionList` supervisor query is unaffected (no capture-at-spawn).
- **Mid-turn tails are resumable** (#271/#272,
  [ADR-0061](../../adr/0061-parked-turn-state-batch-tool-resolution.md)). A log
  ending after `ToolCall`/`ToolExec` with no matching `ToolOutput` replays into
  a **parked `TurnState`**: the completed assistant message commits, logged
  outputs fold, and the unanswered calls become `Session.turn.pending`. On
  resume the session **re-offers** each pending call as a fresh `ToolExec`
  (same `request_id`, fresh `seq`) — the tool executor, or any external
  resolver holding a `Holly` handle, answers it like a first offer;
  **at-least-once**, so a tool that ran before the crash but whose result never
  reached disk runs again. A drained tail (all results logged, next round never
  streamed) continues the turn directly; a text-only tail (mid-stream crash)
  stays dropped. This event-log + `Holly::resume` path is also the persistence
  seam for embedders of `entanglement-core`: records are serde values storable
  anywhere (a DB, a queue); the JSONL store here is the reference
  implementation. Resume restores every *replayable* piece of runtime-side
  state (tool overlay, permission grants) but **not** MCP tool registration —
  `ToolRegistry` is process-lifetime, never logged; a re-offered
  `mcp__<server>__*` call connects its server lazily at dispatch time, like any
  first call
  ([ADR-0201](../../adr/0201-unregistered-mcp-tool-dispatch-resolves-truthfully.md),
  [ADR-0226](../../adr/0226-one-advertised-surface-mcp-envelope-registered-is-available.md),
  §"MCP client" in [gates & host tools](../gates/mcp.md)).
- **Every compaction forks a successor — nothing is ever mutated in place**
  (#324, [ADR-0082](../../adr/0082-single-shot-session-ops-and-persisted-compaction.md)
  → [ADR-0101](../../adr/0101-compaction-forks-into-a-new-session-copy-on-write.md)
  → [ADR-0205](../../adr/0205-every-compaction-forks-a-successor-session.md)).
  All three paths — the `"compact"` op, auto-summarize on overflow, and the
  prune-only fallback — emit `OutEvent::Compacted{summary,kept,auto,mode}`, an
  ordinary seq-bearing content event, so they needed **zero** persistence-tap
  code: the tap already appends every `OutEvent` with `session().is_some()`
  regardless of variant, and the `ReplayFrom` history responder
  ("Late-subscriber history fetch" in [protocol §2](../protocol/sessions.md))
  already includes every event with `seq().is_some()`. The source session is
  never mutated: the summary rides in the event, the engine forks it into a
  **successor** session and retires the source, and `Session::replay`'s
  `Compacted` fold is a **no-op on every path** — a source's log still
  reconstructs the full pre-compaction history it actually held. A truncated
  summary is refused outright (`StopReason::MaxTokens` → `Error`) so it never
  forks either.
  **Keep-tail** (#397,
  [ADR-0102](../../adr/0102-compact-keep-tail-verbatim-in-the-fork-prompt.md)):
  `args.kept: u64` (optional, default `0`) requests that the last `kept`
  messages ride into the successor **verbatim** instead of being paraphrased
  into the summary. `Context::safe_kept` clamps the request to the nearest safe
  turn boundary — the tail must start at a `User` message, or a `Tool` reply
  could replay without its paired `Assistant` tool-call half, breaking
  providers' `tool_use`/`tool_result` pairing (ADR-0082's deferred-to-v1
  blocker). `compact_op` summarizes only the *head*, then appends the tail's
  rendered transcript after the summary — the composed text ships inside the
  same `summary` field and seeds the successor, so **no wire change** was
  needed. The TUI's `/compact [--keep N] [instructions]`
  (`tui::commands::parse_compact_args`) is the head-side entry point; the
  command-palette pick still defaults to `kept: 0`.
- **What a head must do about a compaction** (ADR-0205). The engine mints the
  successor, so a head neither forks nor closes anything — it **follows
  lineage**. The successor announces itself with
  `OutEvent::SessionStarted { predecessor: Some(source), .. }`, and a head that
  tracks one particular session (the one-shot `run` loop, the `agent` tool's
  child-answer collector, the TUI's active view) rebinds to the successor on
  that event, because the turn — and its `Done` — now land there. `Compacted`
  carries the seed text for a head that wants to show what the new session
  continues from, and `mode` (`summary` | `prune`) says whether there is a real
  summary behind it. The TUI records the seed on `Compacted`, opens the
  successor's view on `SessionStarted`, seeds its transcript, and switches to
  it **only if the session that compacted was the active one** — a background
  session overflowing must not yank the user's view. `pipe` and `serve` are
  pure relays and need no change: they forward both events and their client
  follows the same lineage.
- **The automatic paths are not an exception any more** (#398,
  [ADR-0103](../../adr/0103-auto-summarize-on-context-overflow.md), amended by
  [ADR-0205](../../adr/0205-every-compaction-forks-a-successor-session.md)). A
  turn overflowing its context window used to mutate `Context` directly,
  because it had no head to fork into; the engine now forks for itself, so the
  overflow paths mark the same wire event `OutEvent::Compacted { auto: true,
  .. }` and take the identical successor lifecycle. `mode` separates the two:
  `summary` for the LLM-summarized fork, and `prune` for the placeholder-prune
  fallback, whose `summary` field carries the pruned transcript that seeds the
  successor rather than an LLM-authored summary (ADR-0121's silence retired).
- **Pluggable append target — `RecordSink`** (#313,
  [ADR-0086](../../adr/0086-recordsink-pluggable-persistence-append-target.md)).
  The tap's *what to persist*
  (route each record to its root, tombstone lag gaps) is split from its *where to
  persist*: it appends every finished `LogRecord` through a
  `RecordSink { fn append(&self, root: &SessionId, record: &LogRecord) }`, and an
  embedder swaps in any target without forking the subscriber (and so tracks
  upstream gap/lag fixes for free). The default `FileSink` is the JSONL store
  above; `spawn_persistence_subscriber(holly, store)` is just
  `spawn_persistence_subscriber_with_sink(holly, Arc::new(FileSink::new(store)))`.
  `append` is **synchronous** — the file sink is one `writeln!`. A sink whose
  store can block (DB, network) must **not** block the tap: that starves the
  broadcast receiver and manufactures the very `Gap` tombstones the tap exists to
  avoid. Such a sink puts a bounded channel + writer task behind `append` and
  returns immediately, surfacing back-pressure as an `Err` (dropped past the
  bound) rather than awaiting. `SessionStore::read`/`pair_records` stay the
  file-side read helpers; resume already accepts records from anywhere
  (`Holly::resume(root, records)`), so no read-side trait is needed.
- **One-shot flush**: a `run` invocation ends the moment the turn does, so `main`
  aborts the tool executor and drops its `Holly` handle to close the broadcast
  channels, then awaits the persistence task so buffered events reach disk before
  the process exits.
- **Log integrity — never resume a hole** (#104). The persistence tap reads
  Holly's *lossy* broadcast, so a fast turn that outruns disk appends can drop a
  contiguous run of events (`RecvError::Lagged`) — a well-formed file whose
  history is silently incomplete. On lag the tap writes a `Gap { dropped }`
  tombstone into every known root file (a lag can't say *which* session lost
  records, so all are marked). A record the `RecordSink` *refuses* (`append`
  returns `Err`: full backlog, failed insert) is the same kind of hole (#723):
  the tap's `GapWriter` tombstones it as `Gap { dropped: 1 }` in its root, and
  if the tombstone write fails too it keeps the count pending per root and
  writes it before that root's next record — later records for the root count
  into the pending gap rather than land after an unmarked hole — with a final
  retry at tap shutdown; a lasting outage logs one error at its start and one
  warning on recovery (with the gap size), retries only at debug. Those same
  two transitions reach the heads (#767), on ADR-0141's `Throttle` model:
  `OutEvent::PersistenceDegraded{root,error}` on an outage's first failure and
  `PersistenceRecovered{root,lost}` when its tombstone lands — one pair per
  outage per root, none while the sink is healthy (a lag whose tombstone lands
  first try is loss, not an outage). A successful append also retries every
  *other* root's pending tombstone, so a root that ended or hibernated
  mid-outage still recovers once the sink heals. `error` is the sink's
  `RecordSink::public_error` (default: top-level message, control chars
  stripped, ≤200 chars; the full chain stays in `tracing`). Both are engine-global (no `seq`,
  `session()` is `None`, `root` is payload), so the tap — which skips
  session-less events — never writes its own signal into the failing sink.
  The tap emits through a `PersistenceSignal` (`Holly::persistence_signal`), a
  *weak* hold on the outbox rather than a `Holly` clone, so it never keeps the
  engine's channels open past shutdown. Heads: the TUI raises a `⚠ log`
  status-line counter (`status.attention`, `⚠ log×N` for several roots) until
  every root recovers, plus one transcript line per transition (in that
  root's view if open, else the focused one). A root that ends or hibernates
  while degraded leaves the badge with a "may end without a gap marker" note,
  and a lagged TUI receiver clears the badge with a note, since a missed
  `Recovered` cannot be re-derived; `run` prints
  one stderr line per transition (any format — stdout's NDJSON stays clean);
  `pipe`/`serve` forward both like any event (read-only, wire-allowed). The tap owns
  gap-marking for every `Err` and lag; a sink that loses a record after
  returning `Ok` must write its own `Gap` *at the hole*, holding that root's
  later rows back until it lands.
  `integrity_gap` detects it and both resume paths
  (`skutter run --resume`, the TUI modal) **refuse** rather than fold an
  incomplete context. An embedder that prefers resuming the intact head uses
  `session_store::resumable_prefix(rows)` instead: it takes one
  `Result<LogRecord, _>` per stored row (`Err` = undecodable) and returns the
  records before the first gap/corrupt row plus an `Option<Truncation>`
  (`Retired` is not a hole); it and `integrity_gap` share
  `LogPayload::gap_dropped` as the one definition of a hole. Before a session
  resumed from a truncated prefix writes again, the embedder deletes the rows
  from `Truncation::index()` onward (or resumes into a fresh id) — else the
  old tombstone stays the first gap and the next load drops the new work.
  `session_store::read_path` (behind `SessionStore::read`) likewise distinguishes a
  crash-truncated *tail* line (tolerated with a warning) from *interior*
  corruption (a hole → hard error), and `list_sessions` skips-and-warns per bad
  file instead of aborting the whole enumeration.
- **Log compatibility — logs from 0.6.0 onward load** (#720). A protocol
  change never strands a persisted log: a renamed field keeps its old
  spelling as a `serde(alias)` (`SessionStarted.profile` → `agent`), a removed
  field is ignored (no protocol type denies unknown fields), and a removed
  variant's tag is appended to core's `RETIRED_IN_MSG_KINDS`/
  `RETIRED_OUT_EVENT_KINDS` (`set_agent`, `bash_enable`/`bash_disable`,
  `bash_changed`). `LogPayload` decodes such a frame as `Retired { kind }` —
  every consumer skips it and `integrity_gap` does not count it, since it is
  an intentional retirement, not lost data; any *other* undecodable frame
  still fails as corruption. The live enums carry no catch-all, so the wire
  stays strict. Because `LogRecord` is the serde type, an embedder decoding
  its own rows inherits the same tolerance. `tests/fixtures/v0.6.0-session.jsonl`
  (recorded by the `v0.6.0` tag) pins it: it must load and resume its parked
  turn with history intact. Two semantic caveats: a retired `set_agent`'s own
  `AgentChanged` reply still folds last-write-wins, so the session resumes
  under the last agent it switched to (if still in the catalog); and a 0.6.0
  `Compacted { auto: true }` rewrote context in place, which replay ignores
  like every `Compacted`, so such a log resumes with its full pre-compaction
  history rather than the summary (a manual 0.6.0 compaction never rewrote,
  so it resumes exactly).
  A log naming a retired built-in *agent* (`build`/`explore`/`research`)
  resumes too ([ADR-0214](../../adr/0214-retired-agent-logs-resume-as-general.md),
  replacing ADR-0207 stage 6a's refusal; refined by
  [ADR-0215](../../adr/0215-resume-fallback-per-session-and-read-only-narrowing.md)).
  Each rebuilt session is judged on its last effective agent. One whose
  agent fell back resumes as the last agent the catalog still carries, else
  `general`. If it was read-only (`explore`/`research`), it is narrowed to
  the `research` mode unless `--mode` was explicit. That narrowing rides the
  replay as a synthetic `ModeChanged` (`ResumeCheck::paired_for_resume`), so
  a re-offered parked call is never graded under the wider mode first.
  `skutter run --resume` and the TUI print one `note:` line per fallback.
  The narrowing covers the forced session's rebuilt descendants too (a mode
  spans the spawn sub-tree, ADR-0207 §6). The CLI's own `--mode`/config
  `mode:` must not undo it: a `SetMode` on the root cascades over the whole
  tree and would widen a forced child until a re-narrowing landed (#757,
  [ADR-0216](../../adr/0216-resume-modes-ride-the-replay.md)). So
  `session_store::resume_with_mode` carries *every* mode in the replay when
  anything is forced — `research` inside a forced sub-tree, the configured
  mode everywhere else — and sends the ordinary cascading `SetMode` only when
  nothing is (an explicit `--mode` disables forcing, so it always cascades).
  On that replay path a non-forced session's model sees `[mode: X]` with no
  "changed from" transition, an accepted trade-off.
  The logic is the pure `session_store::check_for_resume`, which also
  refuses a gap; `SessionStore::load_for_resume` and `resume_with_mode` wrap it for the
  heads.
- **Deletion + startup auto-prune** (Issue 4, 15175e0). Session logs can be
  deleted explicitly — `d`/`Delete` on the TUI sessions modal removes the
  highlighted session's `.jsonl` (a **live** session is refused with a status
  line: the modal lists the live set, and deleting under one would orphan its
  view) — and age out automatically: `SessionStore::prune(retention_days)`
  runs **best-effort at startup** (`main.rs`, before any head
  spawns, so the resume modal and `skutter sessions` both reflect the pruned
  set), removing root files older by mtime than the retention window from
  **the current project's session dir only**. The window resolves
  `config.yml`'s `session_retention_days` > the embedded default `30` (the
  env override is retired, [ADR-0213](../../adr/0213-prune-env-vars-and-catalog-knobs.md)); an
  unreadable dir/entry is a warn-and-skip, never fatal. A root's per-write
  file backups (`backups/<root_id>/`, [ADR-0212](../../adr/0212-undo-rewind-via-per-write-file-backups.md))
  share its fate: `delete` removes them with the `.jsonl`, and `prune`
  removes every backup directory whose `.jsonl` is gone. `prune` is
  stat-only; a file moved sub-agents were adopted from stays fresh because
  the tap touches it when it writes the pointer and a live moved child keeps
  appending to it (ADR-0234). Once it does age out, resuming the adopter
  omits that sub-tree.

---

Previous: [§6c–§6d — Managed key file, onboarding & auxiliary models](keys-and-aux-models.md)
