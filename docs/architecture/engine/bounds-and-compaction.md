# Per-session engine §5 — Re-offer, idle TTL, loop bounds & compaction

> Part of the [per-session engine module](../engine.md) of the [architecture overview](../../architecture.md).

**Parked-turn re-offer timer** (✅ #274,
[ADR-0071](../../adr/0071-parked-turn-reoffer-timer.md)). `OutEvent::ToolExec` rides
the lossy outbound `broadcast`, so the runtime executor can lag
(`RecvError::Lagged`), drop an offer, and strand the parked turn with no
in-process recovery — restart + `Holly::resume` was the only cure. So while
parked the session loop bounds its `rx.recv()` with
`tokio::time::timeout(EngineConfig.reoffer_interval, …)` (default 60s; `None`
disables it). After that much *silence* — no `ToolResult` arriving — it
**re-offers** every `TurnState::pending` call via the same `emit_tool_exec` the
resume path uses (same `request_id`, fresh `seq`), then loops; the batch draining
retires the timer. This is sound **only** because the runtime executor is
idempotent by `request_id` (a per-session in-flight set, cleared on the resolving
`ToolOutput`): a re-offer to a call it is still running is a no-op there, not a
double-run. At-least-once, exactly like resume.

**Optional idle-TTL auto-hibernation sweep** (✅ #363,
[ADR-0090](../../adr/0090-idle-ttl-auto-hibernation.md)). `EngineConfig.idle_ttl:
Option<Duration>` (`None` by default — the ADR-0077 stance that eviction stays
embedder-driven) arms a supervisor-level sweep, not another per-session timer:
`holly::supervisor` wraps its `rx.recv()` in a `tokio::select!` with a
`tokio::time::interval` at `max(idle_ttl / 4, 30s)` — a coarse eviction poll, not
a scheduler — that is simply absent from the `select!` when `idle_ttl` is `None`,
so the feature off is byte-identical to the pre-#363 code path. Each session task
publishes its own settledness to a shared `ActivityRegistry`
(`Arc<Mutex<HashMap<SessionId, Option<tokio::time::Instant>>>>`, the same
sharing pattern as `SeqRegistry`): `None` while `Session::turn.is_some()` (mid-turn
*or* parked on a tool/approval/question result — core's single settledness
signal, no runtime `AgentState` needed), `Some(instant)` from the moment it last
became settled. A missing entry defaults to unsettled — the sweep only ever
evicts a session it can positively prove is at rest. Each tick judges every
**root** by its whole spawn sub-tree (`collect_subtree`): every member must be
settled, and the sub-tree's idle clock starts at the *latest* member's settle
time, so one parked child pins its whole ancestry live regardless of how long
the root itself has sat idle. A qualifying root hibernates through the same
`hibernate_subtree` helper `InMsg::HibernateSession` uses — the identical
teardown, `OutEvent::SessionHibernated`, and resumability (#318, ADR-0077) as a
manual eviction. Deliberately **stricter** than manual `HibernateSession`
(which is stop-then-hibernate): a timer must never cancel live work, so the
sweep only touches a session already at rest, never one mid-stream.
`entanglement-runtime` exposes this as the `idle_ttl_secs` `config.yml` setting
(#401, [ADR-0105](../../adr/0105-expose-idle-ttl-via-runtime-config.md)) — whole
seconds, copied onto `EngineConfig.idle_ttl` in `build_config` alongside
`max_turns`; one engine-global setting shared by every head (`Holly::spawn`
runs once before the subcommand match), mainly useful for a long-lived
multi-session `skutter serve`. Unset (the default) stays `None`, byte-identical
to before this config surface existed.

**Unknown-session policy** (✅ #721,
[ADR-0210](../../adr/0210-unknown-session-policy.md)). A routable message
(`Prompt`, `ToolResult`, `Stop`, `Pause`/`ResumeSession`, `SetMode`,
`SetModel`, `SetGeneration`, `SetSessionMeta`, `SetToolOverlay`, `Oneshot`)
addressed to an id with no live task goes through
`EngineConfig.unknown_session` (`holly/supervisor/unknown.rs`):
`SpawnBlank` (default) lazily creates a blank root under the default agent —
the single-user convenience, still refusing a closed id and a known
hibernated sub-agent child (#639); `Refuse` emits an `Error` naming the
session (seq `0` for an id that never started; a retired id's continues its
seq high-water, ADR-0239) and spawns nothing; `Resume(Arc<dyn SessionLoader>)` asks the
embedder's async loader for the records `Holly::resume` consumes and runs the
ordinary resume path (sub-tree cascade included) before delivering, `None`
refusing and `Err` erroring. The load runs on its own task and reports back
over a supervisor-owned channel, so a slow store never stalls routing;
`pending_loads[id]` queues every further message for that id behind the one
in-flight load and replays them through the normal handler in arrival order;
`CloseSession` abandons it. `Holly::is_live` reads the per-session seq
registry's live counters (registered before `SessionStarted`, retired before
`SessionEnded`/`SessionHibernated`; a retired id's seq high-water does not
count, [ADR-0239](../../adr/0239-retired-session-keeps-a-seq-high-water.md)) — advisory; the policy is the race-free
way to address a possibly-evicted id.

**Loop bounds — `max_turns` and context-over-limit** (`session/round.rs` for
the per-attempt cap, `session/turn.rs` for the once-per-round context-window
gate). The turn is capped at `EngineConfig.max_turns` rounds (default 200; user-configurable
via `config.yml`, [ADR-0089](../../adr/0089-user-configurable-max-turns.md)), one
round = one LLM round-trip that may fan out into tool calls, counted on
`TurnState::iterations` and reset per prompt (#177 — a fresh `TurnState` per
`Prompt`; a folded mid-turn prompt does not reset it), so a model wedged in a
tool loop can't run forever while a legitimate long session (many prompts) is
never capped. Resume resets the counter too (a runaway guard, not a quota —
ADR-0061). The trip ends the turn through `emit_turn_error`
(`session/emit.rs`) — the same `Error` + `Done` + `Status` triple a backend
error fires — so a one-shot head awaiting `Done` exits, and replay sees the
turn boundary exactly where the live engine drew it (it used to emit only the
`Error`, #177).

**Ambiguous-stop retry — `max_ambiguous_stop_retries`** (`session/round.rs`,
[ADR-0118](../../adr/0118-ambiguous-stop-reason-bounded-retry.md)). A round that
ends with empty `tool_calls` is classified by `StopReason::is_confident_stop`
(#433 — an exhaustive method on `StopReason` itself, in
`entanglement-provider/src/llm/types.rs`, so a new variant is a compile error until
it's explicitly classified, rather than a non-exhaustive `matches!` in
`round.rs` silently defaulting it to ambiguous):
`EndTurn`/`MaxTokens`/`StopSequence` are deliberate and end the turn as above
(`MaxTokens` also fires its truncation-warning `Error`, ADR-0055, unchanged).
Everything else reaching this point — a bare `None` (the stream closed with no
`finish_reason` ever observed, e.g. a provider like Ollama dropping the
connection mid-generation), `Other`, or a contradictory `ToolUse` with zero
actual tool calls (a tool call dropped for malformed JSON) — is *ambiguous*:
instead of ending the turn, core commits whatever partial text streamed, pushes
a short synthetic user-role nudge into `Context`, and returns
`RoundAttempt::AmbiguousRetry` so `run_round`'s driver loop calls
`run_attempt` again **in place** — no new park, no round-trip through the
runtime tool executor, and (#436) no re-running the per-round setup
`run_round` already resolved once (`system_prompt_resolver`, the
context-window/auto-compact gate) — only the cheap per-attempt work
(iteration count, mid-turn prompt fold) repeats. Two consequences of that bare
context mutation are made sound explicitly. **(1) The nudge is persisted.** The retry
emits a seq-bearing `OutEvent::AmbiguousRetry { nudge }` — like `Compacted`,
part of the event-sourced log — so `Session::replay` folds the exact boundary
(flush the partial assistant round, then push the nudge) instead of merging
both rounds' `TextDelta`s into one assistant message and dropping the nudge; a
resumed session then continues from the history the live model actually saw
(the load-bearing "event log is the persistence seam" invariant, ADR-0061).
Its non-delta arrival also delimits the re-streamed text, so a head (the TUI
transcript, `subagent/answer.rs`'s answer collector) starts a fresh segment rather
than concatenating consecutive rounds. **(2) An *empty* ambiguous round commits
nothing** — a stream that died before any text would otherwise push
`content: []`, which the strict clients (`anthropic/request.rs`,
`gemini/request.rs`) drop, leaving the retry request with two adjacent user
turns the provider
rejects with a 400. Core skips that empty commit, and the strict clients also
coalesce adjacent same-role turns (`coalesce_same_role`), so the nudge landing
next to the original prompt stays well-formed. The retry count lives on
`TurnState::ambiguous_retries`, capped by
`EngineConfig.max_ambiguous_stop_retries` (default 2) and reset to 0 by any
round that produces a confident outcome — real tool calls or a deliberate
stop — so only a *persistently* ambiguous model exhausts the budget. A retry
round still increments `TurnState::iterations`, so `max_turns` above remains
the hard outer backstop regardless of this knob (including set to 0, which
disables the retry outright — a true opt-out that stays silent, restoring the
pre-ADR-0118 behavior). Exhausting a *non-zero* budget emits a distinct warning
`Error` ("model stop was ambiguous ... response may be incomplete") followed
by the normal `Done`/`Status::Done`, rather than silently succeeding as
before; a zero budget skips the warning and emits only the `Done`/`Status::Done`. Separately, before each iteration core checks
`Context::within_limit()` against the **model's real context window** (#178). The
budget is `INPUT_BUDGET_FRACTION` (0.85) of the active model's catalog
`context_window` — threaded runtime → `EngineConfig.context_window` →
`Context::with_window` — reserving the rest for the reply and estimator slack;
an unknown model (`EchoLlm`, or an env-override id absent from the catalog) falls
back to the flat `CONTEXT_LIMIT_TOKENS` (180k). Over budget, core now tries three
recovery steps in order (#398,
[ADR-0103](../../adr/0103-auto-summarize-on-context-overflow.md)):
1. **Auto-summarize into a successor**, gated by `EngineConfig::auto_compact` (default
   `true`, exposed to users as `config.yml`'s `auto_compact:` — copied onto the
   engine config in `build_config` beside `max_turns`/`idle_ttl_secs`, the
   ADR-0105 wiring shape): `run_round` checks `Context::within_limit` once per
   round and, over budget, calls `session/turn.rs::enforce_context_window`,
   which runs these three steps in order; step 1 (`try_summary_fork`) calls the
   same `session/summarize.rs::summarize`
   the manual `"compact"` op ([session ops](session-ops.md)) uses — on the same aux-resolved backend too
   (`summarize::AuxBackend::for_summarize(cfg)` then
   `aux.resolve(&mut *s.llm, model, s.generation)`; see *Auxiliary models*
   in [session ops](session-ops.md)): an overflow recovery is a side transformation, so it runs on the
   pinned `summarize` model when one is set — requesting a small fixed keep-tail
   (`AUTO_COMPACT_KEEP_TAIL`, clamped to a safe turn boundary by
   `Context::safe_kept` exactly as #397/ADR-0102 does), then **forks a
   successor session** seeded with that summary plus the verbatim
   kept tail (`summarize::compose_report`) — the live `Context` is never
   mutated ([ADR-0205](../../adr/0205-every-compaction-forks-a-successor-session.md)
   amends ADR-0103's in-place design; a turn mid-flight no longer needs a head
   to fork into, because the engine forks for itself). On success it emits
   `OutEvent::Compacted { auto: true, mode: summary, .. }`. The request reuses the round's
   own system prompt and tool specs — `run_round` resolves both once, *before*
   this gate (`session/round_inputs.rs`), so a remote `system_prompt_resolver`
   is never fetched twice — and its shape follows the backend ([ADR-0202](../../adr/0202-prompt-cache-discipline-anchors-deferral-replay-compaction-date.md) §4,
   `session/compaction_request.rs`). On the session's own backend it is
   **structured**: that system prompt, those specs, the head messages verbatim
   plus one trailing instruction, and the session `cache_key` — byte-identical
   to a turn's request, with no tool-choice override (Anthropic invalidates its
   messages cache on a `tool_choice` change; z.ai accepts only `auto`). Only the
   instruction text forbids a tool call; a reply that calls one anyway is
   discarded and summarization re-runs once on the rendered transcript
   (`session/summary_attempt.rs`), both attempts' usage summed into the one
   `purpose: compaction` `Usage`. WHY: the head is a strict prefix of the live history, so tools →
   system → history are prompt-cache hits instead of a full-price re-read at
   the moment the context is largest, and the summarizer sees the real tool
   calls, results and thinking blocks rather than a capped rendering. A pinned
   aux model gets the **rendered** transcript (the summarizer's own system
   string, no tools, no cache key): a different model is a different cache
   namespace, and a foreign wire may reject the history's signed thinking
   blocks or tool-call ids. Budget rule: structured when head + prefix fit the
   real window (`Context::window()` minus `max_output_tokens` and a 2% margin —
   not the input limit, which the context exceeds by definition here, and
   mid-turn the kept tail collapses so the head *is* the whole context),
   rendered otherwise (guarded by the input limit), prune-only last.
2. **Fall back to the prune-only `Context::compact`** (placeholder-prune the
   oldest tool outputs, newest-first-preserved) when auto-summarize is
   disabled, its own guard trips (a rendered transcript or kept tail over the
   input limit, an LLM error, a tool call on the rendered attempt, a truncated
   summary), or the successor fork is unavailable (no engine handle). There
   is no post-summary fit check: the kept tail is guarded on its own, but
   summary + tail is not re-measured. It prunes a **clone** of the
   context and forks a successor seeded with the pruned transcript, emitting
   `OutEvent::Compacted { auto: true, mode: prune, .. }` (ADR-0205) — the
   source's own history is left exactly as its log describes. Prunes in one
   batch down to ~90% of the budget rather than stopping the instant the
   estimate dips under it (#566): a session sitting near the edge would
   otherwise re-trip this fallback every round or two as new content trickles
   in — busting a provider's cached prefix right when requests are largest.
3. **Refuse the turn** via `emit_turn_error` (a `"context window exceeded"`
   `Error` + `Done` + `Status`) if pruning also doesn't fit (or the fork is
   unavailable) — sending an over-window request just burns a paid
   round-trip and errors at the provider.

**Both compacting steps fork; neither mutates** (ADR-0205). Steps 1 and 2
each produce a successor session and retire the source unchanged at the fork
point; step 3 refuses the turn without touching anything. No path rewrites a
live session's history behind its own append-only log, so ADR-0202's
byte-equality guarantee now covers compacted sessions too and
`Session::replay`'s `Compacted` fold is a **no-op on every path** — there is
nothing to reconstruct. ADR-0121's silent in-place prune, and the live/replay
divergence it knowingly accepted, are retired with it: a head has to follow a
session id change, so the prune announces itself like any other compaction.

**The fork itself** (`session/fork.rs`) is one mechanism shared by both
automatic steps and the manual op ([session ops](session-ops.md)). It emits `Compacted` — the source's
last content event — then sends the same two frames the TUI used to send
head-side for `/compact`: `InMsg::Spawn { parent: None, predecessor:
Some(source), agent: <source profile>, prompt: <seed> }` and
`InMsg::CloseSession { source }` (ADR-0110's successor-closes-predecessor
lifecycle, now universal). A session task cannot mint a session, so it asks the
supervisor over a dedicated session→supervisor channel whose frames the
supervisor handles exactly like inbox ones — fan-out included, which is what
lets the persistence tap synthesize the successor's seed prompt from its
`Spawn` (ADR-0113) so the successor's own log replays to the history it started
live with. Delivery runs on a detached task, so a session task never blocks
while the supervisor may be waiting to route into that very session, and the
frames go out only after the source has finished writing its own log tail.

**The successor inherits background work** ([ADR-0231](../../adr/0231-compaction-successor-inherits-background-work.md)).
The runtime moves the predecessor's background jobs, scripts and retained
outputs to the successor when it sees `SessionStarted { predecessor }`, and
moves a background script's parked approval cards to it ([ADR-0238](../../adr/0238-compaction-moves-a-background-scripts-card.md)). See
[gates & host tools](../gates/host-tools-contracts.md) §poll.

**The successor keeps the spawn tree** ([ADR-0234](../../adr/0234-live-sub-agents-survive-compaction.md)).
The supervisor places the successor where its source was: `Spawn.parent` is
`None` on the fork, and a successor whose source had a parent inherits it, so
a sub-agent that compacts stays under its parent (its mode is still the
source's). While handling that `Spawn` it also moves the source's live direct
children under the successor (`holly/supervisor/relink.rs`: `parent_links`,
the directory's `SessionInfo.parent`, `ChildSpawned`, `OutEvent::ParentChanged`).
The fork sends `CloseSession` after the `Spawn` on the same channel and the
supervisor handles one frame at a time, so the cascade that follows finds only
the source; no child can be caught between the two. Closed children already
left `parent_links`; hibernated ones have no task and are retired as before.
A `Spawn` naming a parent with a live successor joins the successor, even
while the source is still registered between the fork's two frames, so a
sub-agent launched in that window is not caught by the cascade.
The source → successor chain lives in the shared `SessionDirectory`
(`Holly::successor_of`). A moved child keeps logging to the predecessor's
root file; the successor's file gets an adoption pointer that resume follows,
so a cold resume of the successor restores it (see
[heads & persistence](../heads-and-persistence.md)).

**The successor keeps the live binding** (#768/#776). The successor is the
same session continuing, so a live `SetModel`/`SetGeneration`/`SetMode`/
`SetToolOverlay` on the source survives the fork rather than the successor restarting on its agent's
pin. The mode is inherited by the supervisor from the predecessor (ADR-0207
§6). The model and generation ride beside the `Spawn` on the fork channel as
an in-process `Inherited { model, generation, mode, overlay }`
(`session/fork.rs`), never as
`Spawn.model`: that field is *re-resolved* and refuses the spawn on failure
(#725, [turn loop](turn-loop.md)), and by then the source is being retired, so a refusal would
lose the session. `Inherited.model` is the source's already-bound
`ResolvedModel` (`Session::bound`, kept by `bind`/`rebind` and replay's
`ModelChanged` fold) with the source's live generation, so the successor
binds it with **no resolution at all** and cannot fail, even if that model
has stopped resolving since (it was resolvable when the source bound it). It
takes the pin's slot, so the pin is never applied; a source on the startup
default carries no model and the successor starts the way the source did.
The successor's log records the binding as an ordinary `ModelChanged`, then
the inherited params as a `GenerationChanged`, which replay folds after the
model's fresh-resolution params. That `GenerationChanged` is also recorded as
the profile's session memory, so the persisted `generation_resolver` default
does not override it. A resumed session's corrective `ModelChanged`
re-announce keeps the replayed generation rather than resetting it to the
catalog's. When the two differ, it also re-logs a `GenerationChanged` after
itself. Without that, the corrective record would be the last write in the
log, and the *next* resume would fold back to the catalog's params.

The **tool overlay** (ADR-0149) is mode-scoped (ADR-0207): an actual mode
switch drops it wholesale. Core now applies that rule to `Session::tool_overlay`
too, both live (`immediate.rs`) and in replay's `ModeChanged` fold, exactly as
the runtime's dispatch-side mirror always did. Without it, a successor could
inherit an entry the runtime had already forgotten. `start_session` applies
the inherited overlay only when the successor starts in the mode it was set
under (`Inherited.mode == initial_mode`). The two can differ only if the
supervisor applied a `SetMode` that the source never processed before
forking; in that case the overlay is dropped. The overlay is logged as
`ToolOverlayChanged` after the successor's `ModeChanged`, so neither replay
nor the runtime reads it as cleared by a switch. Carrying it never changes
the successor's tools array: the overlay is consulted at dispatch only, and
an enable never edits the advertised surface ([ADR-0226](../../adr/0226-one-advertised-surface-mcp-envelope-registered-is-available.md)).

**A resumed session's overlay is re-announced and enforced again.** This
covers any session, a successor included. The runtime grades dispatch off its
own copy of the overlay. That copy is built only from `ToolOverlayChanged`
and forgotten on `SessionHibernated`/`SessionEnded`, and a fresh process never
had it. So on resume, `start_session` re-emits the overlay that replay
rebuilt, as a `ToolOverlayChanged` right after the same-mode `ModeChanged`
re-announce, and that re-announce is not a switch, so the runtime keeps the
overlay. Without this, a user's deny entry stopped applying after
hibernate→resume (fail-open) and an enable entry was lost (#776).

**A mid-turn fork carries the turn.** The window gate runs between rounds, so
the turn's tool batch is always drained when it fires — `drive_turn` is only
re-entered once `TurnState::pending` empties — and the successor continues the
work from its seed prompt while the source's log ends exactly at the fork. A
tool result still in flight when the fork happened would name the retired
source; the supervisor redirects `ToolResult` frames along the source→successor
chain it records from each fork's `Spawn`, so a late result resolves against
the session that took the turn over instead of hitting the closed-id refusal
and surfacing an error for work nobody cancelled.

So both the turn-limit trip and the context-refusal *end* a turn — the former
on an `Error` with no `Done` (the #177 gap), the latter on the full
`emit_turn_error` triple; the #192 `max_tokens` truncation `Error` remains a
recoverable warning that runs on to its normal `Done`.

---

Previous: [§5 — Turn loop, tool round-trip & live switching](turn-loop.md) · Next: [§5 — Revert, fork, workdir, single-shot ops, ids, stop & pause](session-ops.md)
