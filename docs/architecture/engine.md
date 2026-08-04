# entanglement Architecture — Per-session engine

> Part of the [architecture overview](../architecture.md). The *why* behind each choice is in the [decision log](../adr/README.md).

## 5. Per-session engine (`session/`)

The turn loop lives in the `session/` split — `session/turn.rs` (the live
reasoning turn: `drive_turn`/`run_round`, owning the per-round setup that only
needs to run once — tool specs, the context-window gate, system prompt
resolution — plus the small driver loop that retries in place),
`session/round.rs` (`run_attempt`: one streamed attempt and the ADR-0118
ambiguous-stop retry decision, split out of `turn.rs` along that retry seam,
#436), `session/stream.rs` (one streamed round-trip), `session/turn_state.rs`
(the parked-turn state), and `session/emit.rs` (outbound-event helpers), with
`session/replay.rs` holding the pure state reconstruction.

Each session is a lazily-spawned tokio task owning: `Context` (message history +
token estimate), an LLM backend `llm: Box<dyn Llm>` (from
`EngineConfig::llm_factory`), the
active `AgentProfile`, a per-session `seq`, and `turn: Option<TurnState>` — the
in-flight turn as **explicit, serde-serializable state** (#270,
[ADR-0061](../adr/0061-parked-turn-state-batch-tool-resolution.md)): `Some`
while a turn is live (streaming or parked on unresolved tool calls), `None`
when idle.
The backend is a **plain `Box<dyn Llm>`, not a per-session handle**
([ADR-0062](../adr/0062-collapse-llmsession-placeholder-newtype.md), collapsing
the former `LlmSession` placeholder): the *conversation history* stays in core's
`Context`, and the *connection* state (pool, retry, rate-limit budget) belongs to
the provider — but that state is keyed **per endpoint** and shared across
sessions (#217, [ADR-0050](../adr/0050-per-endpoint-connection-pool-retry-rate-limit.md)),
so there is no honest session-scoped state to wrap. The factory hands core the
streaming backend directly.

Turn loop (`run_round`, driven by `drive_turn`): send `LlmRequest { system,
model, messages, tools }` → consume the streamed `LlmEvent`s (emit `TextDelta`
per `Text` chunk, gather `ToolCall`s, fold `Finish`) → if the reply carries
tool calls, **emit the whole batch up front** — the per-call (`ToolCall`,
`ToolExec`) pair for every call — record it as `TurnState::pending`, and
*return to the session loop* (`RoundOutcome::Parked`); the loop resolves each
`InMsg::ToolResult` against the pending set (**any order** — outputs fold into
`Context` on arrival, in arrival order) and re-enters `drive_turn` when the
batch drains → rounds repeat until the model returns no tool calls **and a
confident stop** → `Done`. A round that returns no tool calls with an
*ambiguous* stop instead retries in place (ADR-0118, detailed below).
Batch calls thereby execute **concurrently**, not serially in call order
(#270, [ADR-0061](../adr/0061-parked-turn-state-batch-tool-resolution.md));
a stale, duplicate, or unknown `ToolResult` is dropped with a debug trace.
**Every** tool call takes the runtime round-trip; core holds no executable tools
and runs nothing inline — the built-ins were removed in #231
([ADR-0049](../adr/0049-plan-task-tools-as-runtime-state-tools.md)), and the
former plan-authority tools (`propose_plan`/`update_tasks`, #513) are now
ordinary permission-gated runtime state/orchestration tools carried on
`tool_specs`/`profile_tool_specs`.
Each round-trip's `Finish` is priced against
`EngineConfig.pricing` (effective model = `session.model` (a live switch) else
`profile.model` else `default_model`),
folded into the session's `SessionUsage`, and emitted as `OutEvent::Usage`; a
`StopReason::MaxTokens` also emits a truncation-warning `Error` (✅ #192,
[ADR-0055](../adr/0055-usage-cost-and-stop-reason-surfacing.md)). Permission dispatch and approval no longer run
here — the runtime tool executor owns them (§3, §8, ✅ #59). While parked, the
session loop stashes a `Prompt`/`SetAgent`/`SetModel` for the live turn's fold
site / replay-after-turn; only the stash gate differs from idle (the stash is
popped only between turns).

**Live model/provider switch** (✅ #218,
[ADR-0063](../adr/0063-realtime-model-provider-switch.md)): an idle `SetModel {
provider, model }` re-resolves via `EngineConfig.model_resolver` (a
runtime-supplied `Fn(&str,&str) -> Result<ResolvedModel,_>` capturing the catalog
+ warm per-endpoint client, #217), rebuilds `Session::llm`, and retargets the
per-session `model` (overrides `profile.model` on the request + in pricing) +
`generation` + the `Context` window budget — no restart. Emits `ModelChanged`
(unknown provider / missing key → `Error`); deferred mid-turn like `SetAgent`, and
replay re-applies it to re-bind a resumed session. That success arm is factored
into `Session::rebind`, shared by the live switch and the pin paths below.

**Per-profile model pinning** (✅ #323,
[ADR-0081](../adr/0081-per-profile-model-pinning-and-rebind-on-set-agent.md))
reuses that same `rebind`: a `SetAgent` to a profile carrying a **model pin**
(`AgentProfile::model_pin()` — both `provider` and `model` set) re-binds the
backend to it, so switching agents can switch endpoints. The rebind lives in
core's `SetAgent` handler (one locus for Tab cycle / `/agent` / `--agent` /
spawn / wire) and at **session start** for a pinned starting profile (guarded on
`Session.provider`/`model` so a child already on its pinned endpoint doesn't
rebuild). Precedence: per-session memory (`Session.profile_models`, a `/model`
choice recorded under a profile) **>** the static pin **>** keep the current
binding — so a pin-less profile with no memory emits no `ModelChanged`, and a
live override survives an agent switch. `SetAgent` emits `AgentChanged` first
regardless; a resolver failure surfaces the same `Error` as `SetModel` and keeps
the old binding. Replay reconstructs `profile_models`/`provider` from the folded
`ModelChanged` records.

**Live generation-parameter changes + per-profile persistence** (#374,
[ADR-0094](../adr/0094-reasoning-effort-and-per-profile-generation-persistence.md))
mirrors the model pin above, but through a **separate** seam:
`EngineConfig.generation_resolver: Option<GenerationResolver>` (a
runtime-supplied `Fn(&str) -> Option<GenerationParams>`, keyed by profile
*name* rather than baked into `AgentProfile` — `GenerationParams`'s
`temperature: Option<f32>` has no total `Eq`, so it can't join
`AgentProfile`'s `PartialEq + Eq` derive the way the pin's `provider`/`model`
fields do). `Session.generation` starts at the catalog default
(`EngineConfig.generation`, resolved from the active model at session
creation, unchanged from #191) and layers on top of it, at both `SetAgent` and
session start, with the same three-tier precedence the pin uses: **session
memory** (`Session.profile_generation`, populated by a live `SetGeneration`
recorded under that profile — a **full** merged snapshot, not a diff) **>**
**the resolver's persisted value** (also a full snapshot) **>** **the current
binding, unchanged** (no `GenerationChanged` for a profile with neither).
Session start applies the persisted tier when `Session.profile_generation`
carries no entry yet for the starting profile (the generation analogue of the
pin's `Session.model.is_none()` guard). Replay reconstructs
`profile_generation` from folded `GenerationChanged` records exactly as it
reconstructs `profile_models` from `ModelChanged`. The runtime's persisted
store (`AgentGenerationStore`, a managed `agent-generation.yml` sibling of
`agent-models.yml`) is documented in the heads/persistence doc; unlike
`AgentModelStore` it has no `apply(&mut ProfileRegistry)` — there is nothing
on `AgentProfile` to overlay, so its `resolver(...)` builds the
`GenerationResolver` closure directly instead. The TUI `/set`/`/show` surface
and its persist-on-confirmation write to that store (#376,
[ADR-0095](../adr/0095-tui-set-show-generation-persist-on-confirmation.md))
mirror the `/model` picker's own persist-on-confirmation logic (`tui/app/pickers.rs`).

Setup errors (the initial `stream()` call)
surface as `Error` + `Done` with no partial to commit. A **mid-stream** failure
is handled to keep the committed context aligned with what the user saw (#181,
[ADR-0057](../adr/0057-mid-stream-error-partial-commit-and-retry.md)):
Reasoning arrives on two rails and only one reaches `Context`
([ADR-0160](../adr/0160-extended-thinking-round-trip.md)): `LlmEvent::Reasoning`
→ `OutEvent::ReasoningDelta` is the *display* rail, streamed and persisted but
deliberately never folded into history (it counts as "shown" for the retry rule
below); `LlmEvent::ContentBlock(ContentPart::Reasoning)` →
`OutEvent::ReasoningBlock` is the *replay* rail, committed into the assistant
`Message` alongside the round's text like a `ProviderSearch` block. The split
exists because Anthropic requires the signed thinking block back on a parked
turn's final assistant message, so a resumed session has to rebuild it — while
the rendered text must stay out of the token estimator and compaction.

If the stream drops *before any* `TextDelta`/`ReasoningDelta` is shown, core
transparently **re-requests once** (`STREAM_RETRIES = 1`) — a clean re-stream the
provider's own connect-level retry (ADR-0050) can't cover; if a delta was already
shown, core instead **commits the partial** assistant message with an appended
`\n\n[interrupted]` marker (streamed as a final `TextDelta` so display and
context stay identical) before the `Error` + `Done`, so the next turn's context
matches the display instead of continuing as if the model said nothing. Any
half-assembled tool calls are dropped (no `Finish` ⇒ possibly incomplete). The
same stash discipline applies inside the streaming loop and while the turn is
parked (ADR-0018): a mid-turn `Stop` interrupts, every other queued command
(`Prompt`, `SetAgent`, …) is pushed onto the replay stash, so a follow-up sent
while the engine is busy is never silently dropped. A stashed **`Prompt` is additionally
*folded into the live turn*** (#182,
[ADR-0058](../adr/0058-mid-turn-prompt-folds-into-live-turn.md)): at the top of each inner-loop iteration —
before the next model request — core drains every stashed `Prompt` into `ctx`
via `push_user`, so mid-turn guidance steers the running turn on the very next
round-trip (the same way a queued user message folds into the next request)
instead of only replaying as a fresh turn after `Done`. The fold site is reached
only when the previous round emitted tool calls (a reply with none ends the turn
first), so a prompt sent *after* the model's final answer still correctly starts
a new turn via the stash; non-`Prompt` commands stay stashed for the session
loop. **The streaming loop *races* the
inbox against the stream** with a `biased` `tokio::select!` (#179) — not a
`try_recv` polled only after each event yields — so a `Stop` preempts a
connected-but-silent provider immediately (dropping the stream aborts the
`reqwest` request) instead of blocking until the HTTP client's read timeout.
The **pre-stream phase races the inbox too** (#547): `session/stream.rs` pins
the `llm.stream()` call itself and `select!`s it against the inbox the same
way, so a `Stop` sent while the provider client is still parked on its
retry-after wait / pacing gate / cross-process shared gate / semaphores —
none of which used to be preemptible — cancels immediately instead of
waiting for the whole pre-stream phase to finish. While parked there is no
racing to do: the session loop itself is the receiver, handling
`ToolResult`/`Stop`/`Prompt` directly against the pending `TurnState`.

**Parked-turn re-offer timer** (✅ #274,
[ADR-0071](../adr/0071-parked-turn-reoffer-timer.md)). `OutEvent::ToolExec` rides
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
[ADR-0090](../adr/0090-idle-ttl-auto-hibernation.md)). `EngineConfig.idle_ttl:
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
(#401, [ADR-0105](../adr/0105-expose-idle-ttl-via-runtime-config.md)) — whole
seconds, copied onto `EngineConfig.idle_ttl` in `build_config` alongside
`max_turns`; one engine-global setting shared by every head (`Holly::spawn`
runs once before the subcommand match), mainly useful for a long-lived
multi-session `skutter serve`. Unset (the default) stays `None`, byte-identical
to before this config surface existed.

**Loop bounds — `max_turns` and context-over-limit** (`session/round.rs` for
the per-attempt cap, `session/turn.rs` for the once-per-round context-window
gate). The turn is capped at `EngineConfig.max_turns` rounds (default 200; user-configurable
via `config.yml`, [ADR-0089](../adr/0089-user-configurable-max-turns.md)), one
round = one LLM round-trip that may fan out into tool calls, counted on
`TurnState::iterations` and reset per prompt (#177 — a fresh `TurnState` per
`Prompt`; a folded mid-turn prompt does not reset it), so a model wedged in a
tool loop can't run forever while a legitimate long session (many prompts) is
never capped. Resume resets the counter too (a runaway guard, not a quota —
ADR-0061). **Beware:** the trip path emits **only** an
`OutEvent::Error` and returns — *not* the `Error` + `Done` + `Status` triple that
`emit_turn_error` (`session/emit.rs`) fires on a backend error — so a one-shot
head awaiting `Done` hangs when the turn limit trips. That missing-`Done` is a
known robustness gap (see #177).

**Ambiguous-stop retry — `max_ambiguous_stop_retries`** (`session/round.rs`,
[ADR-0118](../adr/0118-ambiguous-stop-reason-bounded-retry.md)). A round that
ends with empty `tool_calls` is classified by `StopReason::is_confident_stop`
(#433 — an exhaustive method on `StopReason` itself, in
`entanglement-provider/src/llm.rs`, so a new variant is a compile error until
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
transcript, `subagent.rs`'s answer collector) starts a fresh segment rather
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
[ADR-0103](../adr/0103-auto-summarize-on-context-overflow.md)):
1. **Auto-summarize in place**, gated by `EngineConfig::auto_compact` (default
   `true`, exposed to users as `config.yml`'s `auto_compact:` — copied onto the
   engine config in `build_config` beside `max_turns`/`idle_ttl_secs`, the
   ADR-0105 wiring shape): `try_auto_compact` calls the same `session/summarize.rs::summarize`
   the manual `"compact"` op below uses — on the same aux-resolved backend too
   (`summarize::AuxBackend::for_summarize(cfg)` then
   `aux.resolve(&mut *s.llm, model, s.generation)`; see *Auxiliary models*
   below): an overflow recovery is a side transformation, so it runs on the
   pinned `summarize` model when one is set — requesting a small fixed keep-tail
   (`AUTO_COMPACT_KEEP_TAIL`, clamped to a safe turn boundary by
   `Context::safe_kept` exactly as #397/ADR-0102 does), then applies the result
   via `Context::apply_compaction` — **mutating the live session's `Context` in
   place**, the fundamental split from the manual op's copy-on-write (ADR-0101):
   a turn mid-flight has no head to fork into. On success it emits
   `OutEvent::Compacted { auto: true, .. }`.
2. **Fall back to `Context::compact`** (placeholder-prune the oldest tool
   outputs, newest-first-preserved) when auto-summarize is disabled, its own
   guard trips (an oversized transcript/tail, an LLM error, a truncated
   summary), or the result still doesn't fit. Prunes in one batch down to
   ~90% of the budget rather than stopping the instant the estimate dips
   under it (#566): a session sitting near the edge would otherwise re-trip
   this fallback and mutate one more early message every round or two as new
   content trickles in — busting a provider's cached prefix right when
   requests are largest.
3. **Refuse the turn** via `emit_turn_error` (a `"context window exceeded"`
   `Error` + `Done` + `Status`) if pruning also doesn't fit — sending an
   over-window request just burns a paid round-trip and errors at the provider.

Step 2's prune mutates `Session.ctx` in place — like step 1 — but, unlike
step 1, **emits no `OutEvent`** (#450,
[ADR-0121](../adr/0121-prune-only-compact-stays-silent.md)): nothing records
that the prune happened, so `Session::replay` never replays it and a resumed
session briefly reconstructs the full, unpruned history the live session had
already discarded — a real but accepted live/replay divergence in the exact
request shape. It self-heals within one round-trip: `enforce_context_window`
runs before every round, so a resumed session still over budget just re-prunes
(or re-summarizes) on its very next turn and converges to where the live
session already was, and it never ships an over-window request in the
meantime. Recording it was rejected — `Context::compact` is a deterministic,
idempotent function of the existing log and the model's token budget alone
(no LLM call, nothing destroyed that a subsequent guard run can't re-derive),
unlike step 1's LLM-authored rewrite that *must* be recorded for replay to
reconstruct the same `Context`.

So both the turn-limit trip and the context-refusal *end* a turn — the former
on an `Error` with no `Done` (the #177 gap), the latter on the full
`emit_turn_error` triple; the #192 `max_tokens` truncation `Error` remains a
recoverable warning that runs on to its normal `Done`.

**Single-shot ops — `InMsg::Oneshot` (`session/ops.rs`, #324,
[ADR-0082](../adr/0082-single-shot-session-ops-and-persisted-compaction.md)).**
Separate from the turn loop above: `run_oneshot` never streams tool calls and
never parks — it either completes in one round-trip or fails cleanly. Routed
like `SetAgent`/`SetModel` (`SessionCmd::Oneshot`, deferred via the stash gate
while `s.turn.is_some()`), so it only ever runs with no turn in flight — the
invariant that lets `compact_op` drive a bare `llm.stream(...)` (via
`session/summarize.rs`'s small `oneshot_text` helper that drains the stream for
`Text` chunks + the `Finish` usage) instead of going through
`session/stream.rs`'s inbox-racing `tokio::select!`. The backend it drives is
**aux-resolved**, not necessarily the session's own: `compact_op` first
resolves `summarize::AuxBackend::for_summarize(cfg)` and then
`aux.resolve(&mut *s.llm, model, s.generation)` — a `summarize` aux-model pin
([ADR-0154](../adr/0154-per-purpose-auxiliary-models.md), next section) routes
the call to a one-shot pinned client; unset, the triple resolves straight back
to the session's own `llm`/`model`/`generation`. `"compact"` renders the
history as a plain-text transcript (each `Tool`-role message truncated
head+tail past ~2k chars so one oversized tool output can't blow the
summarizer's own context window), optionally appends `args.instructions`, and
asks the model to summarize it with a tool-less `LlmRequest` (`tools: &[]`) —
all via the shared `session/summarize.rs::summarize`, which `session/turn.rs`'s
auto-compact path above also calls. **Copy-on-write (ADR-0101):** the source
session's `Context` is **never mutated** — on success `compact_op` composes the
summary with the rendered kept-tail (`summarize::compose_report`, since the
fork's seed is a single flat string) and emits `Compacted{summary, auto: false}`
(a *report*; the head forks the summary into a new session) then
`Usage`/`Done`/`Status::Done`, the ordinary terminal sequence so a one-shot head
still unblocks on `Done`. A truncated summary (`StopReason::MaxTokens`) is
refused outright (`Error`, never forked), and an oversized transcript (one that
overflows `s.ctx.limit()`) is rejected before shipping a request the provider
would 4xx. On failure, the ordinary `emit_turn_error` triple runs and `Context`
is untouched. Model resolution and pricing mirror the turn loop: `s.model` →
`s.profile.model` → (pricing only) `cfg.default_model`.

**Auxiliary models — the `aux_llm_resolver` seam** (Issue 5,
[ADR-0154](../adr/0154-per-purpose-auxiliary-models.md)). A side
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
(`session/turn.rs::try_auto_compact`) — resolve
`summarize::AuxBackend::for_summarize(cfg)` and then
`aux.resolve(&mut *s.llm, model, s.generation)`: `AuxBackend` owns the built
`Box<dyn Llm>` (one-shot, dropped when it goes out of scope), which is what
lets both call sites hand `summarize` a `&mut dyn Llm` outliving the borrow.
A `None` from the resolver — no resolver wired, an unset pin, or a pin the
catalog no longer knows — falls back **field-by-field to the session's own
`llm`/`model`/`generation`**: byte-identical to the pre-ADR-0154 behavior,
and strictly better than a fixed primary model, since a live `/model` switch
keeps applying to compaction whenever no pin is set.

**Id generation — the `id_gen` seam** ([ADR-0164](../adr/0164-short-sortable-kind-tagged-ids.md)).
`EngineConfig.id_gen: Arc<dyn IdGen>` mints every session id, background-job
id, and runtime-minted request/correlation id — never `Option`, unlike the
resolver seams above, since there is always a scheme rather than an
opt-in override. `IdGen::next(kind: IdKind) -> String` defaults to
`DefaultIdGen`: `<kind>-<epoch-seconds hex><2-hex process salt><3-hex
counter>`, 15 characters (`s-`/`j-`/`r-` for `Session`/`Job`/`Request`), with a
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

**Pause is a hold, not a cancel** (#516, [ADR-0144](../adr/0144-pause-resume-a-hold-between-cancel-and-hibernate.md)).
`Session.paused: bool` (never persisted/replayed) is set by
`SessionCmd::Pause`/cleared by `SessionCmd::Unpause`. It gates two of the
existing gates rather than adding a new code path: every command that already
checks `s.turn.is_some()` to decide "defer onto the stash" (`Prompt`,
`SetAgent`, `SetModel`, `SetGeneration`, `Oneshot`) now checks
`s.turn.is_some() || s.paused` — so an *idle* paused session defers its next
`Prompt` exactly like a live turn defers a mid-turn one. The stash-pop
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
`stash.push_back`'d by the same generic non-`Stop` branch `SetAgent`/
`SetModel` already ride, and applied once the round reaches its next safe
point. `Stop` and `Hibernate` are both unconditional regardless of `paused`
and neither clears it — `Stop`'s resting-state emit reports `Paused` (not
`Done`) if the session is still held.

**Sub-agent spawn** (✅ #60, [ADR-0022](../adr/0022-subagent-spawn.md), builds on the
[ADR-0021](../adr/0021-hierarchical-session-model.md) tree). The model calls a
runtime-owned `agent { agent, prompt, background? }` tool (renamed from
`spawn_agent`, ✅ #120,
[ADR-0033](../adr/0033-agent-tool-family-and-blocking-agent.md); the separate
`agent_spawn` tool it was later split into is retired again, ✅ #606,
[ADR-0161](../adr/0161-unified-async-work-background-flag-and-one-poll.md) —
one tool, `background: bool` picks the return shape). The runtime executor
intercepts it before per-tool permission resolution (it starts a session rather
than touching a host resource), mints a child `SessionId`, and sends `InMsg::Spawn { session: child, parent, agent,
prompt }`. The **supervisor** records `parent_links[child] = parent` and starts
the child `session_loop` under the requested profile with the prompt queued — so
the child's `SessionStarted` carries the parent link and the tree-walk helpers
(`children_of` / `root_of`) reflect reality. `background: true` is
**non-blocking** (✅ #89,
[ADR-0026](../adr/0026-async-subagent-spawn-and-poll.md), supersedes ADR-0022's
synchronous relay): the call replies to the parent *immediately* with the
child handle (`agent_id`) instead of parking the turn on the child's `Done`, so
one turn can launch several sub-agents that then run concurrently. The launch
task keeps watching the child and records its final answer + duration into a
shared `AgentRegistry` (`runtime::agent_registry`) keyed by the handle and
scoped to the spawning parent (✅ #618): each entry also carries the parent
`SessionId` recorded at `register`, so a lookup only resolves for that same
parent — a session polling a handle it did not launch (even one it learned or
guessed) gets the same "unknown handle" `ToolOutput` a genuinely nonexistent
handle would (#605 adopts this error convention over `bash_output`'s former
return-it-as-text). The parent collects a result with the runtime-owned join tool,
`poll { handle, timeout_secs? }` (✅ #605, [ADR-0161](../adr/0161-unified-async-work-background-flag-and-one-poll.md),
replacing the former `agent_poll`/`bash_output` outright) — also intercepted
before permission resolution (it starts no session and touches no host
resource): it dispatches on the handle's kind prefix (a sub-agent handle is a
`s-` session id) to this same `AgentRegistry`, blocks up to `timeout_secs` for
that child and returns its answer (with elapsed time) as the tool
`ToolOutput`, or a still-running status on timeout so the model can poll
again or do other work. Or, with `timeout_secs: 0`, blocks until the child
completes (no caller-side bound, ADR-0123 — the same indefinite-wait path the
default blocking `agent` call takes). For the single-delegation case, the
**default** (`background` omitted or `false`) **blocks**: it runs the exact
`background: true` launch path (same guard, clamp, `Spawn`), then parks on
the child's genuine completion and folds its answer directly into the
`ToolOutput` — one call instead of launch-then-poll. It still records into the
`AgentRegistry`, so a parent `Stop` while parked leaves the child collectable
via `poll`. Both routes share `subagent::collect_child_answer`, which
does **not** treat a bare `Done` as final (✅ #562,
[ADR-0155](../adr/0155-errored-subagent-turn-parks-for-steering.md)): the
engine emits `Done` even for a turn that ended in `Error`
(`emit_turn_error`), so a `Done` with no accumulated text and an `Error` on
that turn clears the per-turn state and keeps watching instead of unblocking
the parent on a failed child — the child session is still alive and
steerable (prompting it "continue" starts a new turn), and the wait ends only
on a `Done` that carries a usable answer or the child's `SessionEnded`/
`SessionHibernated`. Each re-arm emits an explanatory `OutEvent::Error` on the
*parent* naming the child, so the user knows to steer it; `AgentState` stays
`WaitingAgent` throughout (no new lifecycle state).
Every `AgentRegistry` entry also carries the profile the child was launched
under (#607), and a `snapshot(parent)` method lists every child a session has
outstanding — completed entries included, since nothing here evicts them (the
model sees an unclaimed answer sitting there rather than losing track of it).
This backs the no-`handle` form of `poll` and the head-facing
`InMsg::ListOperations` (ADR-0161 §6): "what do I still have running," the
same ownership bookkeeping serving both the descendant check above and the
listing, merged with an equivalent job-registry snapshot by
`entanglement_runtime::operations::list_operations` — see
[protocol](protocol.md) and [gates & host tools](gates-and-host-tools.md).
Refusals (depth, budget, capability) are identical regardless of `background`
— one shared guard path.

**Sub-agent follow-up** (✅ #609, [ADR-0162](../adr/0162-agent-send-supervising-a-sub-agent.md)).
A child can be talked to more than once: `agent_send { agent_id, prompt,
background? }` sends `InMsg::Prompt` at an existing child (an `agent` launch
or a sponsored `propose_plan` build, whose reply now names its `agent_id`
too, ADR-0162 §5) instead of minting a new `InMsg::Spawn` — the child session
task stays alive after its turn ends, so the fresh prompt starts a new turn
on its accumulated context rather than losing it. No protocol change was
needed: `collect_child_answer` already ends its wait on any `Done` carrying
text, so a child that concludes "I'm blocked, advise" already unparks its
parent — only the reply verb was missing. The runtime executor intercepts
`agent_send` the same way as `agent` (before permission resolution, no
per-tool grade), but the gate is different: no spawn depth/budget check (this
sends into an *already-authorized* child, not a new one) but a mandatory
ownership + lifecycle check, `AgentRegistry::begin_send(poller, child)`,
resolved in one lock acquisition so there is no window between "is this
live" and "mint the follow-up's watch channel." Ownership generalizes the
`poll` descendant check (ADR-0161 §4) to a *write* verb — a handle is only
ever sendable by the session that launched it, the same "unknown agent_id"
message for a stranger's guess as for an outright-nonexistent id. Lifecycle
is the load-bearing half: `AgentRegistry` now tracks each tracked child's
session as `Live`/`Hibernated`/`Closed`, folded from the engine-wide
`SessionStarted`/`SessionHibernated`/`SessionEnded` broadcast (any session's
transition, not just this executor's own) so it stays current independent of
whether `agent_send` itself was ever called. A **closed** (tombstoned) child
refuses clearly (its id is spent, ADR-0028); a **hibernated** child refuses
*loudly* rather than silently — sending it a fresh `Prompt` would otherwise
fall into the supervisor's lazy-respawn path (`holly.rs`'s unknown-session
`Prompt` handling) and come back as a *blank* session wearing the right id,
discarding its context. Only a `Live` child is ever actually sent the
prompt; the default (blocking) path then reuses `collect_child_answer`
exactly like the blocking `agent` route — waiting for the child's *next*
`Done`, not its first — and `background: true` returns immediately, joined
later with `poll` on the same handle. `agent_send` is the reply half of an
escalation loop that needed no new mechanism: a child's blocked/errored
conclusion is an ordinary tool result the parent reads and decides what to
do with, then answers via `agent_send`, which parks again for the child's
next result — the parent is always the one investigating before it replies,
never answering blind.

Both reuse the #58 round-trip, so core's turn loop needs no notion of a
"child session". The runtime executor bounds the spawn
tree (✅ #76, [ADR-0023](../adr/0023-subagent-spawn-limits.md)): a `SpawnGuard`
folds parent links from `SessionStarted` and, before each spawn, refuses past a
depth cap (`MAX_SPAWN_DEPTH`) or a cumulative per-root budget
(`MAX_SPAWNS_PER_ROOT`) — replying with a clear refusal `ToolOutput` instead of
starting a child. Spawn is also **permission-gated** (✅ #77,
[ADR-0024](../adr/0024-subagent-permission-gating.md), `runtime::permission`): every
child's per-tool permission is clamped to the least-privileged rule across its
whole ancestor chain (`Deny < Ask < Allow`), so a child can never touch the
shared tree in ways a parent couldn't. Layered in front of that clamp and the
ADR-0023 budget is **per-profile spawn control** (✅ #119,
[ADR-0040](../adr/0040-per-profile-spawn-control.md), `spawn_refusal`): a profile
must `may_spawn` (a `subagent` leaf like `explore` defaults closed — this absorbs
ADR-0024's capability gate) and its *target* must be spawnable-mode
(`subagent`/`all`) and on its `spawnable_agents` allowlist. Filesystem isolation
(a separate child root) and bidirectional session-to-session messaging are still
deferred (see ADR-0022/0024).

**Roster disclosure** (✅ #112, [ADR-0034](../adr/0034-file-based-agent-definitions.md);
scoped ✅ #119, [ADR-0040](../adr/0040-per-profile-spawn-control.md)).
The `agent` tool description carries one `name: description` line per
spawnable agent, and the `agent` argument's schema constrains the name to an
`enum` — so the model learns *who it may spawn* at the call site, and
`description` is the one field of a definition ever exposed to a parent. The
roster + enum are now **per-profile**: `subagent::spawn_specs_for` scopes them to
exactly the profiles the spawning profile may target (its `spawnable_agents` ∩ the
target-mode gate), and the single `agent` spec lives in
`EngineConfig.profile_tool_specs` (empty when the profile may not spawn), so a
`primary` like `build`/`plan` is never advertised as a target and an out-of-list
spawn is a schema violation before an executor refusal. The related supervisor
wart is fixed too: an `InMsg::Spawn` naming an unknown profile now emits a
supervisor `Error` instead of silently resolving to the `build` default. (The
#116 tool mask restricts each agent's *tool* set — a different axis than which
agents it may spawn.)

**Ask-user prompt** (✅ #90, [ADR-0027](../adr/0027-ask-user-interactive-prompt.md);
v2 #488, [ADR-0127](../adr/0127-ask-user-v2-multi-question-envelope.md);
draft-until-submit #518, [ADR-0143](../adr/0143-ask-user-draft-until-submit.md)).
The model calls a runtime-owned `ask_user { questions: [{question, options,
multi_select}] }` tool — one call can batch several questions, each optionally
`multi_select`; a typed "Other" answer is unconditional (no `allow_free_form`
flag to opt into it, dropped in v2). The runtime executor (`ask_user.rs`)
intercepts it on `ToolExec` — before permission resolution, like `agent`
— emits a single dedicated `OutEvent::UserQuestion` carrying the whole
`questions` array and parks at `WaitingAnswer` (#160,
[ADR-0072](../adr/0072-protocol-warts-settled-before-serve.md): a question is not
a permission decision, so it is distinct from the `WaitingApproval` an `Ask` tool
raises). The head renders the labelled choices Claude-style (the TUI's
`PendingQuestion` interaction state, alongside `ApprovalMode`, models one
*call* — it walks its `questions` in order with checkboxes for a `multi_select`
question and an always-available "Other" entry that opens free-text input).
Every answer is a **draft, revisable until an explicit Submit** (#518): committing
a question (`Enter`/number-pick) writes that question's draft in place and steps
to the next one, or — once every question has a draft — to a terminal
review/submit step; `Left`/`Backspace` steps back to any earlier question to
revise it (its draft, including free text, reloads on screen), and the review
step's own `Enter` is the one explicit Submit that turns the drafts into
`InMsg::AnswerQuestion { request_id, answers: [[string]] }` — one inner vec per
question, in call order (`Esc` on the review step just steps back to revise,
sending nothing and leaving the call parked; a mid-question `Esc` still
interrupts the turn, unchanged). Like `Approve`/`Reject`, the supervisor drops
`AnswerQuestion` off the inbound fan-out and the executor consumes it, then folds
every answer (picked labels joined, or typed text, verbatim, one line per
question) back as the `ask_user` `ToolOutput` — reusing the #58 round-trip, so
core needs no new turn logic and the draft/review walk is entirely head-side
state with no wire change. A `Stop` while pending unwinds silently (core cancels
the turn). The non-interactive `run` head auto-answers every question (first
option, else a canned note) so it never parks; `pipe` forwards the questions and
accepts the answers as-is — neither has a draft step, since both resolve the
whole call in one shot.

**Plan acceptance, file-backed with a blocking review loop — `propose_plan`**
(✅ #141/#513, [ADR-0042](../adr/0042-plan-acceptance-via-propose-plan-approval-roundtrip.md),
amended by [ADR-0138](../adr/0138-sponsored-build-child-and-propose-plan-cycle.md)
and [ADR-0145](../adr/0145-one-plan-tool-file-backed-plans-and-blocking-review-loop.md)).
The plan agent calls a runtime-owned `propose_plan(content: Option<String>,
path: Option<String>)` — **exactly one** of the two. `content` materializes
(or overwrites) `.entanglement/plans/<short-session-id>.md`; `path` binds an
existing in-root `.md` file, refused if it's changed since the session last
touched it (a session-scoped content-hash staleness guard,
`entanglement-runtime/src/plan_files.rs`, kept fresh by `propose_plan` itself
plus a passive listener on the executor's `FileChange` audit for the
session's own `edit`/`write`). A malformed or stale call replies immediately
with **no** approval prompt. Otherwise the executor (`propose_plan.rs`)
intercepts it on `ToolExec` — after the #116 mask check, same family as
`ask_user` — and **force-parks it on the `Ask` path unconditionally, every
phase** (a profile can never `Allow` it; user approval *is* the semantics),
first emitting an `OutEvent::Plan { content, path }` snapshot for the plan
session's own display, then a standard `OutEvent::ToolRequest` carrying the
resolved `{content, path}` JSON regardless of which the model sent.

**Approve** spawns a **sponsored** `build` child of the plan session
(ADR-0138) — a parent-child link (result return, session-tree visibility)
whose permission resolution **stops at the child**: its own profile stands,
no ADR-0024 ancestor clamp, no ADR-0023 fan-out budget drain (sponsored
spawns are exempt, sequential and user-authorized). The `SpawnGuard`
mutation (sponsor check + `record_sponsored_start`) happens in the tool
executor's single-threaded loop before the detached task. The accepted plan
reaches the child verbatim as its first prompt (`wrap_plan`) and as its own
`OutEvent::Plan` snapshot; the plan session parks on `WaitingAgent`
(ADR-0139) and the task `.await`s the child's *genuine* completion
(`collect_child_answer`, which keeps waiting past an errored build turn with
no usable answer instead of concluding on top of a failed build — ADR-0155,
#562) — registered with `crate::cancel::CancelRegistry`
(#513), so a `Stop` on the plan session aborts this wait with no reply owed
and the child (an independent session) keeps running untouched — "detach" by
default; a head wanting the child stopped too sends it an ordinary second
`Stop` ("cascade", no new protocol surface). The build's answer folds back —
prefixed with the plan file's location — as the `propose_plan` tool result,
so the plan agent has the implementation outcome in context: it reviews it,
updates the plan file's checkboxes via `write`/`edit`, and `propose_plan`s
the next phase (`path`, reusing the same file) or stops. **Reject + reason**
folds `tool \`propose_plan\` rejected (plan file: <path>): <reason>` back,
still naming the file (materialized either way — rejection is about the
*proposal*, not the file). One-shot `run`/`pipe` can't park an approval, so
they auto-reject `propose_plan` with a "non-interactive head" reason (the
plan agent still learns the outcome in-band and can revise).

The build session is a sponsored **child**, not the pre-ADR-0138 disconnected
root: the parent link is what lets the answer fold back and the plan agent
cycle, and sponsorship (not inheritance) is what keeps it able to
`edit`/`write` despite `plan`'s own read-only mask. The handoff is entirely
**runtime** policy now — no head-side recipe, so pipe/WS heads get it for
free with zero head-specific code.

**Sandboxed script tool — `rhai`** (✅ #122,
[ADR-0046](../adr/0046-rhai-sandboxed-script-tool.md)). The model calls
`rhai { script, timeout? }` to run multi-step logic in one call — the sanctioned
replacement for shelling out to `python3`/`node`. The engine
(`script.rs`, `rhai::Engine::new_raw()` + the IO-free `StandardPackage`) has **no**
filesystem/network/process/env access and **no module resolver** (so `import`
can't escape); `eval` is disabled. It is resource-bounded by construction:
`max_operations`, `max_call_levels`, string/array/map size caps, and a wall-clock
timeout (default 5s, max 30s) via the `on_progress` interrupt — a runaway script
dies deterministically, never OOMs. `print(...)` is captured; the last-expression
value is serialized (JSON, display-form fallback), the whole output bounded to the
§8 32 KiB cap.

The only capabilities bound are the root-contained quintet as script functions —
`read`/`glob`/`grep`/`edit`/`write` (with the tools' overloads) — each
**delegating to the registered `Tool` impl** (so root containment + bounded output
come for free) and resolving permission **per call exactly like a `ToolExec`**:
`Deny` or a #116 mask throws a catchable script exception; `Allow` runs; `Ask`
parks the script on the standard `ToolRequest` → `Approve`/`Reject` round-trip,
**resolved once per function per run** (the first `edit` asks; approval covers the
rest). Because the bindings *are* the always-registered quintet, `rhai` is
precisely as privileged as those tools — so it is registered by default in the
shared `tool_specs`, and a profile gates it like any tool (a profile whose
`tools` allowlist omits `rhai` never sees it; the read-only `explore`/`research`
profiles advertise it at `Ask` grade instead). The executor intercepts `rhai`
before the generic dispatch (it needs the per-session profile state to snapshot
each binding's mask + clamped permission); its *own* Allow/Ask/Deny is resolved
the same way as any host tool. Rhai's engine is sync, so the script runs under
`spawn_blocking` and each binding crosses a small **bridge** — `mpsc` request +
`oneshot` reply — to the async resolver on the executor task; the timeout is
enforced inside the engine, not by aborting the blocking task. A session `Stop`
(#167) reaches the blocking engine the same way: it trips a cooperative flag the
progress callback polls, terminating the script with an uncatchable
`ErrorTerminated` (unlike a thrown binding error, a script can't `try`/`catch` it
and continue). No exec bindings (`bash`/`call`) in v1 — that would escape the
sandbox.
