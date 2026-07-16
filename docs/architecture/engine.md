# entanglement Architecture — Per-session engine

> Part of the [architecture overview](../architecture.md). The *why* behind each choice is in the [decision log](../adr/README.md).

## 5. Per-session engine (`session/`)

The turn loop lives in the `session/` split — `session/turn.rs` (the live
reasoning turn: `drive_turn`/`run_round`), `session/stream.rs` (one streamed
round-trip), `session/turn_state.rs` (the parked-turn state), and
`session/emit.rs` (outbound-event helpers), with `session/replay.rs` holding the
pure state reconstruction.

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
batch drains → rounds repeat until the model returns no tool calls → `Done`.
Batch calls thereby execute **concurrently**, not serially in call order
(#270, [ADR-0061](../adr/0061-parked-turn-state-batch-tool-resolution.md));
a stale, duplicate, or unknown `ToolResult` is dropped with a debug trace.
**Every** tool call takes the runtime round-trip; core holds no executable tools
and runs nothing inline — the built-ins were removed in #231
([ADR-0049](../adr/0049-plan-task-tools-as-runtime-state-tools.md)), and the
former plan-authority tools (`update_plan`/`update_tasks`) are now ordinary
permission-gated runtime state tools carried on `tool_specs`/`profile_tool_specs`.
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
if the stream drops *before any* `TextDelta`/`ReasoningDelta` is shown, core
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
While parked there is no racing to do: the session loop itself is the receiver,
handling `ToolResult`/`Stop`/`Prompt` directly against the pending `TurnState`.

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

**Loop bounds — `max_turns` and context-over-limit** (`session/turn.rs`). The
turn is capped at `EngineConfig.max_turns` rounds (default 200; user-configurable
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
known robustness gap (see #177). Separately, before each iteration core checks
`Context::within_limit()` against the **model's real context window** (#178). The
budget is `INPUT_BUDGET_FRACTION` (0.85) of the active model's catalog
`context_window` — threaded runtime → `EngineConfig.context_window` →
`Context::with_window` — reserving the rest for the reply and estimator slack;
an unknown model (`EchoLlm`, or an env-override id absent from the catalog) falls
back to the flat `CONTEXT_LIMIT_TOKENS` (180k). Over budget, core now tries three
recovery steps in order (#398,
[ADR-0103](../adr/0103-auto-summarize-on-context-overflow.md)):
1. **Auto-summarize in place**, gated by `EngineConfig::auto_compact` (default
   `true`): `try_auto_compact` calls the same `session/summarize.rs::summarize`
   the manual `"compact"` op below uses, requesting a small fixed keep-tail
   (`AUTO_COMPACT_KEEP_TAIL`, clamped to a safe turn boundary by
   `Context::safe_kept` exactly as #397/ADR-0102 does), then applies the result
   via `Context::apply_compaction` — **mutating the live session's `Context` in
   place**, the fundamental split from the manual op's copy-on-write (ADR-0101):
   a turn mid-flight has no head to fork into. On success it emits
   `OutEvent::Compacted { auto: true, .. }`.
2. **Fall back to `Context::compact`** (placeholder-prune the oldest tool
   outputs, newest-first-preserved) when auto-summarize is disabled, its own
   guard trips (an oversized transcript/tail, an LLM error, a truncated
   summary), or the result still doesn't fit.
3. **Refuse the turn** via `emit_turn_error` (a `"context window exceeded"`
   `Error` + `Done` + `Status`) if pruning also doesn't fit — sending an
   over-window request just burns a paid round-trip and errors at the provider.

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
invariant that lets `compact_op` call `s.llm.stream(...)` directly (via
`session/summarize.rs`'s small `oneshot_text` helper that drains the stream for
`Text` chunks + the `Finish` usage) instead of going through
`session/stream.rs`'s inbox-racing `tokio::select!`. `"compact"` renders the
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

**Sub-agent spawn** (✅ #60, [ADR-0022](../adr/0022-subagent-spawn.md), builds on the
[ADR-0021](../adr/0021-hierarchical-session-model.md) tree). The model calls a
runtime-owned `agent_spawn { agent, prompt }` tool (renamed from `spawn_agent`,
✅ #120, [ADR-0033](../adr/0033-agent-tool-family-and-blocking-agent.md)). The
runtime executor
intercepts it before per-tool permission resolution (it starts a session rather
than touching a host resource), mints a child `SessionId`, and sends `InMsg::Spawn { session: child, parent, agent,
prompt }`. The **supervisor** records `parent_links[child] = parent` and starts
the child `session_loop` under the requested profile with the prompt queued — so
the child's `SessionStarted` carries the parent link and the tree-walk helpers
(`children_of` / `root_of`) reflect reality. Spawn is **non-blocking** (✅ #89,
[ADR-0026](../adr/0026-async-subagent-spawn-and-poll.md), supersedes ADR-0022's
synchronous relay): `agent_spawn` replies to the parent *immediately* with the
child handle (`agent_id`) instead of parking the turn on the child's `Done`, so
one turn can launch several sub-agents that then run concurrently. The launch
task keeps watching the child and records its final answer + duration into a
shared `AgentRegistry` (`runtime::agent_poll`) keyed by the handle. The parent
collects a result with a second runtime-owned tool, `agent_poll { agent_id,
timeout_secs }` — also intercepted before permission resolution (it starts no
session and touches no host resource): it blocks up to `timeout_secs` for that
child and returns its answer (with elapsed time) as the tool `ToolOutput`, or a
still-running status on timeout so the model can poll again or do other work.
For the single-delegation case, a third tool `agent { agent, prompt }` (✅ #120,
[ADR-0033](../adr/0033-agent-tool-family-and-blocking-agent.md)) **blocks**: it runs
the exact `agent_spawn` launch path (same guard, clamp, `Spawn`), then parks on
the child's `Done` and folds its answer directly into the `ToolOutput` — one call
instead of launch-then-poll. It still records into the `AgentRegistry`, so a
parent `Stop` while parked leaves the child collectable via `agent_poll`.
Refusals (depth, budget, capability) are identical across `agent` and
`agent_spawn` — one shared guard path.
All three reuse the #58 round-trip, so core's turn loop needs no notion of a
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
The `agent`/`agent_spawn` tool descriptions carry one `name: description` line per
spawnable agent, and the `agent` argument's schema constrains the name to an
`enum` — so the model learns *who it may spawn* at the call site, and
`description` is the one field of a definition ever exposed to a parent. The
roster + enum are now **per-profile**: `subagent::spawn_specs_for` scopes them to
exactly the profiles the spawning profile may target (its `spawnable_agents` ∩ the
target-mode gate), and the whole `agent_*` triple lives in
`EngineConfig.profile_tool_specs` (empty when the profile may not spawn), so a
`primary` like `build`/`plan` is never advertised as a target and an out-of-list
spawn is a schema violation before an executor refusal. The related supervisor
wart is fixed too: an `InMsg::Spawn` naming an unknown profile now emits a
supervisor `Error` instead of silently resolving to the `build` default. (The
#116 tool mask restricts each agent's *tool* set — a different axis than which
agents it may spawn.)

**Ask-user prompt** (✅ #90, [ADR-0027](../adr/0027-ask-user-interactive-prompt.md)).
The model calls a runtime-owned `ask_user { question, options, allow_free_form }`
tool. The runtime executor (`ask_user.rs`) intercepts it on `ToolExec` — before
permission resolution, like `agent_spawn` — emits a dedicated
`OutEvent::UserQuestion` and parks at `WaitingAnswer` (#160,
[ADR-0072](../adr/0072-protocol-warts-settled-before-serve.md): a question is not
a permission decision, so it is distinct from the `WaitingApproval` an `Ask` tool
raises). The head renders the labelled choices
Claude-style (the TUI adds a `PendingQuestion` interaction state alongside
`ApprovalMode`, with an "Other" entry that opens free-text input) and replies
`InMsg::AnswerQuestion { request_id, answer }`. Like `Approve`/`Reject`, the
supervisor drops it off the inbound fan-out and the executor consumes it, then
folds the answer (the picked label or typed text, verbatim) back as the
`ask_user` `ToolOutput` — reusing the #58 round-trip, so core needs no new turn
logic. A `Stop` while pending unwinds silently (core cancels the turn). The
non-interactive `run` head auto-answers (first option, else a canned note) so it
never parks; `pipe` forwards the question and accepts the answer as-is.

**Plan acceptance — `propose_plan` + the handoff recipe** (✅ #141,
[ADR-0042](../adr/0042-plan-acceptance-via-propose-plan-approval-roundtrip.md)). The
plan agent calls a runtime-owned `propose_plan { plan }` to finalize. The executor
(`propose_plan.rs`) intercepts it on `ToolExec` — after the #116 mask check, same
family as `ask_user` — and **force-parks it on the `Ask` path unconditionally** (a
profile can never `Allow` it; user approval *is* the semantics), emitting a
standard `OutEvent::ToolRequest`. **Approve** folds `ToolOutput("plan accepted by
the user")` back (the engine holds no plan state to record now, #231 — the working
plan was already surfaced via `update_plan`); **reject + reason** folds `tool
\`propose_plan\` rejected: <reason>` back. On
approve the head *additionally* runs the **handoff** — pure head policy, zero new
protocol surface, so pipe/WS heads implement it identically:

1. mint a fresh `SessionId::new_uuid()`;
2. `SetAgent { session: new, agent: "build" }` — lazy session creation starts a
   **root** `build` session;
3. `Prompt { session: new, content: [text wrap(plan)] }` (via `InMsg::prompt`) —
   the accepted plan verbatim as the first user message;
4. switch the head's active view to the new session.

The build session is a **root, not a child** of the plan session: a parent link
would clamp `build` to `plan`'s read-only tool set (#116) + the ADR-0024 permission
ceiling (it could never `edit`/`write`), drain the plan root's ADR-0023 spawn
budget, and mis-model accept — which is a transfer of authority *from the user*, a
root. The plan session stays alive after accept; a later re-propose mints another
fresh build session. One-shot `run`/`pipe` can't park an approval, so they
auto-reject `propose_plan` with a "non-interactive head" reason (the plan agent
still learns the outcome in-band and can revise).

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
shared `tool_specs`, and a profile gates it like any tool (a read-only `explore`
with `tools: [read, glob, grep]` never sees it). The executor intercepts `rhai`
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
