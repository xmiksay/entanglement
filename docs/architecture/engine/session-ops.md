# Per-session engine §5 — Revert, fork, workdir, single-shot ops, ids, stop & pause

> Part of the [per-session engine module](../engine.md) of the [architecture overview](../../architecture.md).

**File revert — `InMsg::RevertFiles` (`session/revert.rs`, #731,
[ADR-0212](../../adr/0212-undo-rewind-via-per-write-file-backups.md)).** Deferred
exactly like `SetModel`/`Oneshot` (stashed while a turn is live or the session
is paused), so a revert never races the turn's own tool writes. At idle the
session awaits the runtime's `EngineConfig::file_reverter` seam (core does no
file I/O; `None` → a recoverable `Error`), emits the seq-bearing
`FilesReverted` and — only when something was restored — appends a
user-role note to `Context` (`revert_note`: what was restored/skipped, that
exec-tool changes were not reverted, re-read before editing). `Session::replay`
folds `FilesReverted` into the same note and never calls the seam, so a
resume never re-reverts. The note is folded *before* any prompt paired with
the event: a `Prompt` sent while the seam was still running is logged ahead
of `FilesReverted` (and so pairs with it), yet the live session applied the
revert first.

**User fork: `InMsg::ForkSession` (`session/user_fork.rs`, #786,
[ADR-0223](../../adr/0223-non-compacting-session-fork.md)).** This fork does not
compact anything. It is deferred like `RevertFiles`, so it is only ever cut at
rest: no parked batch and no half-streamed round. At idle the session reads
`ctx.prefix(at_turn)`. `Context` records the index of every prompt pushed
through `push_user_content`, live and on replay, so turn `n` is the `n`-th
prompt (ADR-0212's numbering), and the cut falls just before turn `n + 1`.
That is a turn boundary unless turn `n + 1` was a mid-turn steering prompt.
Such a prompt is folded in after the previous round's tool results, so the cut
keeps every call paired with its result but ends on those results. It then mints the fork's id in its own lineage (tenant prefix,
ADR-0211) and sends, over the same session→supervisor channel compaction uses,
a root `Spawn` with no `predecessor`, an empty prompt (no turn is queued), its
own user and its mode. `Inherited` rides alongside with the live backend,
generation and overlay, as for a compaction successor, plus a `seed`: the
verbatim history. `start_session` applies the hand-over, then
`user_fork::seed_fork` seeds `ctx` and emits `SessionForked { forked_from,
at_turn, context }`. Replay folds that event with `Context::seed`, which
*prepends*, so a log that recorded the fork's first prompt ahead of the event
still rebuilds the live order. The fork's own prompts count from 1, so `/fork
0` on a fork keeps exactly its seed. The source emits nothing and keeps
running. The fork has no children (it is a root), and its undo history is empty
(backups are per root).

**Working directory — `InMsg::SetWorkdir` / `Spawn.workdir`
(`session/workdir.rs`, `holly/supervisor/workdir.rs`, #764,
[ADR-0224](../../adr/0224-session-working-directory.md)).** Core carries
`Session::workdir` as an opaque absolute path and does no file I/O: the
supervisor validates a `SetWorkdir` the moment it arrives through
`EngineConfig::workdir_resolver(current, dir, approve)` — `current` is its own
per-session cache of the latest validated workdir, so a relative `dir`
resolves against what the user last saw — and refuses a bad one with a
supervisor `Error` (a real seq from the session's counter); `None` (no resolver) refuses every change. The session applies it
deferred like `SetModel` (so one turn's calls share one workdir) and acks with
`WorkdirChanged`, which replay folds by overwrite. A spawn's workdir is its own
`Spawn.workdir` (validated against the parent's, no approval; a bad one refuses
the spawn), else what a successor/fork carries on `Inherited`, else the
parent's cached one; existing children are never moved. On resume the
supervisor re-validates the replayed workdir (no approval) and falls back to
the project root with an `Error` when it no longer resolves
(`revalidate_resumed_workdir`). `start_session` announces it on a fresh start
and on resume. Every `ToolExec` carries it
(`emit::Caller`), so a re-offer runs where the original would have. The model
is told through the per-round **trailing notice** —
`[mode: …] [workdir: <dir> — …]`, rebuilt from `s.workdir` every round like
the mode notice, never pushed into `Context` and never in the system prompt.

**Single-shot ops — `InMsg::Oneshot` (`session/ops.rs`, #324,
[ADR-0082](../../adr/0082-single-shot-session-ops-and-persisted-compaction.md)).**
Separate from the [turn loop](turn-loop.md): `run_oneshot` never streams tool calls and
never parks — it either completes in one round-trip or fails cleanly. Routed
like `SetModel` (`SessionCmd::Oneshot`, deferred via the stash gate
while `s.turn.is_some()` — unlike `SetMode`, which applies immediately, #560),
so it only ever runs with no turn in flight — the
invariant that lets `compact_op` drive a bare `llm.stream(...)` (via
`session/summary_attempt.rs`'s small `drain` helper that drains the stream for
`Text` chunks + the `Finish` usage, noting any tool call) instead of going through
`session/stream.rs`'s inbox-racing `tokio::select!`. The backend it drives is
**aux-resolved**, not necessarily the session's own: `compact_op` first
resolves `summarize::AuxBackend::for_summarize(cfg)` and then
`aux.resolve(&mut *s.llm, model, s.generation)` — a `summarize` aux-model pin
([ADR-0154](../../adr/0154-per-purpose-auxiliary-models.md), next section) routes
the call to a one-shot pinned client; unset, the triple resolves straight back
to the session's own `llm`/`model`/`generation`. `"compact"` resolves the
session's system prompt and tool specs exactly as a turn round does
(`session/round_inputs.rs`) and summarizes via the shared
`session/summarize.rs::summarize`, which `session/turn.rs`'s auto-compact path
([bounds & compaction](bounds-and-compaction.md)) also calls — so it takes the same two request shapes ([ADR-0202](../../adr/0202-prompt-cache-discipline-anchors-deferral-replay-compaction-date.md) §4). On the
session's own backend it is **structured** (that system prompt + specs, the
head messages verbatim, one trailing no-tools instruction, the session
`cache_key`; a tool call falls back once to the rendered shape), reusing the
provider's cached prefix and giving the
summarizer full-fidelity history. On a pinned aux model — a different cache
namespace whose wire may reject the history's signed thinking blocks or
tool-call ids — or for a head that does not fit the real window, it is the
**rendered** plain-text transcript (each `Tool`-role message truncated
head+tail past ~2k chars) under the summarizer's own system string, with no
tools and no cache key. Structured when it fits the real window, rendered
otherwise; `args.instructions` is appended to the instruction either way. **Copy-on-write (ADR-0101), forking a successor (ADR-0110/0205):** the source
session's `Context` is **never mutated** — on success `compact_op` composes the
summary with the rendered kept-tail (`summarize::compose_report`, since the
successor's seed is a single flat prompt), emits
`Compacted{summary, auto: false, mode: summary}` and forks through the same
`session/fork.rs` the automatic paths use, then
`Usage`/`Done`/`Status::Done`, the ordinary terminal sequence so a one-shot head
still unblocks on `Done`. A truncated summary (`StopReason::MaxTokens`) is
refused outright (`Error`, never forked), and a rendered transcript that
overflows `s.ctx.limit()` is rejected before shipping a request the provider
would 4xx. On failure, the ordinary `emit_turn_error` triple runs and `Context`
is untouched. Model resolution and pricing mirror the turn loop: `s.model` →
`s.profile.model` → (pricing only) `cfg.default_model`.

**Auxiliary models — the `aux_llm_resolver` seam** (Issue 5,
[ADR-0154](../../adr/0154-per-purpose-auxiliary-models.md)). A side
transformation (compaction is core's only consumer today; the runtime's
session-title generator runs outside core entirely) may run on a
cheaper/faster model than the session's own. `EngineConfig.aux_llm_resolver:
Option<AuxLlmResolver>` (`Arc<dyn Fn(&str) -> Option<ResolvedModel>>`,
shaped like `generation_resolver`) resolves a **purpose string** to the
provider/model pinned for it — core knows only the string
(`session/summarize.rs::AUX_PURPOSE_SUMMARIZE`), never the runtime's
`AuxLlmRegistry` or the managed `aux-models.yml` behind the closure (see the
heads/persistence doc §6d). Reusing `ResolvedModel` is deliberate: the
one-shot client is built from the same `llm_factory` a `SetModel` switch
would use, so it inherits the warm per-endpoint pool (ADR-0050) instead of
opening its own. Both compaction paths — the manual `"compact"` op
(`session/ops.rs`) and the auto-summarize overflow path
(`session/turn.rs::enforce_context_window` → `try_summary_fork`) — resolve
`summarize::AuxBackend::for_summarize(cfg)` and then
`aux.resolve(&mut *s.llm, model, s.generation)`: `AuxBackend` owns the built
`Box<dyn Llm>` (one-shot, dropped when it goes out of scope), which is what
lets both call sites hand `summarize` a `&mut dyn Llm` outliving the borrow.
`resolve` also reports which arm it took (`BackendArm::Session` /
`PinnedAux`), which is what picks the request shape above (ADR-0202).
A `None` from the resolver — no resolver wired, an unset pin, or a pin the
catalog no longer knows — falls back **field-by-field to the session's own
`llm`/`model`/`generation`**: byte-identical to the pre-ADR-0154 behavior,
and strictly better than a fixed primary model, since a live `/model` switch
keeps applying to compaction whenever no pin is set.

**Id generation — the `id_gen` seam** ([ADR-0164](../../adr/0164-short-sortable-kind-tagged-ids.md)).
`EngineConfig.id_gen: Arc<dyn IdGen>` mints every session id, background-job
id, and runtime-minted request/correlation id — never `Option`, unlike the
resolver seams above, since there is always a scheme rather than an
opt-in override. `IdGen::next(kind: IdKind, ctx: &IdContext) -> String`
defaults to `DefaultIdGen`: `<kind>-<epoch-seconds hex><2-hex process salt><3-hex
counter>`, 15 characters (`s-`/`j-`/`r-`/`o-`/`x-` for `Session`/`Job`/
`Request`/`Output`/`Script` — the last per #637,
[ADR-0185](../../adr/0185-rhai-joins-background-and-poll.md)), with a
process-global `(last_second, counter)` pair (module statics, not per-instance
state) that makes "never twice" structural rather than probabilistic within
one process, and waits for the next second rather than repeating on the
4096/s counter budget. `Holly::next_id(kind)` clones the configured generator
out at spawn time so any in-process embedder/head mints through the *same*
generator (and, if overridden, the same policy) the engine itself would use.
Scope is deliberately narrow: a `ToolCall.id` is provider-supplied on the wire
and is never run through this — reformatting it broke Gemini (#444).
`SessionId::new_uuid()` keeps its name for call-site compatibility but now
mints the `s-` scheme, not a UUID; legacy UUID-form ids coexist indefinitely
since `SessionId` is an opaque `String` newtype and the two shapes cannot
collide.

**Lineage context and tenant prefixes** (✅ #722,
[ADR-0211](../../adr/0211-idgen-context-and-tenant-prefix-inheritance.md)). Every
mint carries a borrowed, `#[non_exhaustive]` `IdContext { parent, predecessor,
user }` (`IdContext::default()` / `child_of` / `successor_of` / `with_user`).
`DefaultIdGen` prepends `ctx.inherited_prefix()` — everything up to and
including the first `:` of the parent's id, else the predecessor's — so a
sub-agent child (the runtime's `agent` tool, `subagent.rs`, via
`Holly::next_id_in(kind, &IdContext::child_of(&parent))`) and a compaction
successor (`session/fork.rs`, `IdContext::successor_of(source)` with the
source's user) of `t1:s-…` are `t1:s-…` too; an unprefixed lineage mints the
plain 15-char form. `Holly::next_id_in` fills a missing `user` from the
lineage's directory entry. Jobs, scripts, retained outputs, request ids and
fresh roots mint with an empty context. An overriding `IdGen` owns the whole
format and may reuse `inherited_prefix`.

**`Holly::session_info(&SessionId) -> Option<SessionInfo>`** reads the
supervisor's live-session directory (`holly/directory.rs`, the same records
`ListSessions` snapshots, now shared behind a mutex instead of owned by the
loop): `user`, `parent`, `predecessor`, `agent`, `root`. `Some` from
registration (before `SessionStarted`) until the supervisor processes the id's
`CloseSession`/hibernation; `None` for unknown, closed and hibernated ids;
resume re-registers the same lineage and user from the log's
`SessionStarted`. It is the race-free ownership read for a resolver handling a
live session's tool call; event routing should key on the inherited prefix,
since a session's trailing `SessionEnded` arrives after its entry is gone.

**Stop is cancel-semantics, not destroy** (ADR-0017). `InMsg::Stop` interrupts
the in-flight turn (the streaming loop *races* it via `tokio::select!` so a
stalled stream can't delay cancel (#179); a **parked** turn is cancelled by
clearing its `TurnState` — the committed assistant message and any
already-arrived outputs stay in `Context`, and a late `ToolResult` for the
cancelled batch is dropped as stale) but does *not* evict the
session from the supervisor map or end its task. The session's `Context` is
preserved across a Stop+Prompt round-trip — Esc-in-approval or a stray Stop
between turns no longer causes amnesia. The supervisor map entry is only
removed on global inbox close (engine shutdown).

Clearing `TurnState` cancels the *turn*, but core never owns the executing tool,
so a `Stop` that lands while a `bash`/`call` command or a `rhai` script is
already running would leave that work going (✅ #167). The **runtime executor**
closes this: it registers each in-flight tool task per session
(`runtime::cancel::CancelRegistry`) and an inbound-fan-out watcher aborts every
one of them on that session's `Stop`. Aborting the async task drops its future —
which for `bash`/`call` fires the exec tools' process-group SIGKILL guard so
grandchildren don't orphan (matching the timeout path, #168) — while a `rhai`
task pairs the abort with a cooperative stop flag the (un-abortable
`spawn_blocking`) engine's progress callback polls, terminating it with an
uncatchable `ErrorTerminated` the script can't `try`/`catch` and continue past.

**Pause is a hold, not a cancel** (#516, [ADR-0208](../../adr/0208-pause-resume-a-hold-between-cancel-and-hibernate.md)).
`Session.paused: bool` (never persisted/replayed) is set by
`SessionCmd::Pause`/cleared by `SessionCmd::Unpause`. It gates two of the
existing gates rather than adding a new code path: every command that already
checks `s.turn.is_some()` to decide "defer onto the stash" (`Prompt`,
`SetModel`, `SetGeneration`, `Oneshot`) now checks
`s.turn.is_some() || s.paused` — so an *idle* paused session defers its next
`Prompt` exactly like a live turn defers a mid-turn one. `SetMode` is the one
command that checks neither (#560): a mode is a label, not something a paused
session's parked batch is actively using, so it applies immediately whether
paused or not. The stash-pop
condition at the top of the loop gained a matching `&& !s.paused` guard, or a
deferred command would be immediately popped back off the queue and
re-stashed (the same busy-loop the pre-existing "pop only when idle" comment
already warns about). A **parked** batch's `ToolResult` handling is not
gated the same way — an arriving result still resolves and folds into
`Context` immediately (gating it would deadlock: the stash only drains once
`s.turn` goes back to `None`, which needs every pending result resolved
first) — instead, the `drive_turn` call that would normally fire once the
batch drains (`TurnState::is_drained`) is skipped while paused, leaving
`s.turn` "drained but undriven" until `Unpause` drives it. A session
mid-stream when `Pause` arrives needs **no special handling in `stream.rs`**:
`Pause`/`Unpause` are ordinary `SessionCmd`s, so a mid-stream arrival is
`stash.push_back`'d by the same generic non-`Stop` branch `SetModel`
already rides, and applied once the round reaches its next safe
point. `Stop` and `Hibernate` are both unconditional regardless of `paused`
and neither clears it — `Stop`'s resting-state emit reports `Paused` (not
`Done`) if the session is still held.

---

Previous: [§5 — Re-offer, idle TTL, loop bounds & compaction](bounds-and-compaction.md) · Next: [§5 — Sub-agents, ask-user, plan acceptance & `rhai`](subagents-and-plans.md)
