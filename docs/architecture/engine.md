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
(the parked-turn state), `session/invoke_envelope.rs` (the ADR-0204 `invoke`
unwrap), and `session/emit.rs` (outbound-event helpers), with
`session/replay.rs` holding the pure state reconstruction (its turn fold
state machine in `session/replay_pending.rs`).

Each session is a lazily-spawned tokio task owning: `Context` (message history +
token estimate), an LLM backend `llm: Box<dyn Llm>` (from
`EngineConfig::llm_factory`), the
active `Agent`, a per-session `seq`, and `turn: Option<TurnState>` — the
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

Turn loop (`run_round`, driven by `drive_turn`): assemble `tools` — **every**
spec `EngineConfig.tool_specs` (or the per-session `tool_spec_resolver`) yields,
verbatim, with **no filtering**: there is no more per-agent tool mask to
filter with — [ADR-0207](../adr/0207-permission-modes-replace-agent-borne-authority.md)
retired it along with the rest of `Agent`'s authority fields, and the
session's permission mode grades every call at dispatch instead — and the
skill mask is removed too ([ADR-0194](../adr/0194-skills-are-additive-only.md):
skills are additive-only), so the advertised surface stays stable within a
session and the provider's prompt cache survives an overlay toggle or a
skill load. With `Agent` carrying no more authority, the array no
longer varies by agent either — `propose_plan`/`request_mode` are pushed
into the base `tool_specs` unconditionally and the `agent`/`agent_send` spawn
schema is now provably identical regardless of which session asks, so the
old per-profile `profile_tool_specs` append is retired (see [agents &
permissions](agents-and-permissions.md) §permission modes for the mode's
own `Deny`/`Ask` grading and the discovery pair's non-tool kinds).

**Two advertising modes** (`ToolAdvertising`,
[ADR-0196](../adr/0196-tool-search-and-lazy-discovery-replace-the-invoke-envelope.md),
superseding ADR-0193's `native`/`invoke` split): the surface above is what
**`Full`** mode advertises — the full registered surface with dynamic MCP
specs inline, mutating on add/remove (an accepted cache cost). **`ToolSearch`**
mode (**the default**) advertises instead an **immutable lean kernel**: the
high-frequency tools (`read`/`edit`/`apply_patch`/`write`/`bash`/`poll`/
`ask_user`/`update_tasks`/`load_skill`) plus the discovery pair
(`explore`/`describe`) plus `propose_plan`/`request_mode`/`agent`/
`agent_send` — the old ADR-0192 "profile-defining specs" carve-out no longer
applies: with `Agent` carrying no authority
([ADR-0207](../adr/0207-permission-modes-replace-agent-borne-authority.md)),
none of these vary across sessions any more either, so they are simply part
of the kernel now, not an exception to it. Everything
else (`call`/`glob`/`grep`/`rhai`, MCP management, all `mcp__*`, endpoints,
skill tools) stays **registered but unadvertised** — dispatchable by name the
moment the model calls it, discoverable via `explore`, schema-delivered via
`describe`. There is no router tool: a discovered tool is called exactly like
a kernel one, by its real name.

The mode is **per-session, resolved at session start** from the session's
initial model (`ModelEntry.tool_advertising`, precedence env
`ENTANGLEMENT_TOOL_ADVERTISING` > `config.yml` `tool_advertising` > catalog >
default `ToolSearch`) and held in a runtime-side session→mode map — the
resolver and executor are engine-global and session-multiplexed, and
per-profile model pins mean concurrent sessions can run different modes. A
live `SetModel` **keeps** the session's mode (logged when the new model's
catalog preference differs — switching mid-session would bust the cache the
mode exists to protect); subagents resolve their own mode at spawn.

Mechanically the mode is the resolver's input shape, not a core concept: core
still advertises whatever the `tool_spec_resolver` yields, re-consulted fresh
every round. Under `ToolSearch` mode's `client_side` wire encoding (OpenAI-
compat Chat Completions incl. z.ai, Ollama, Gemini), each `describe()` call
grows that resolver's output by appending the described tool's spec to a
session-keyed discovered set — **append-only, never removed**, so the
advertised array only ever grows and each discovery costs one cache
invalidation, not a continuous one. The `anthropic_native` and
`responses_native` encodings instead lean on each wire's own
`defer_loading`/`tool_search` primitive (see
[provider](provider.md) and [gates & host tools](gates-and-host-tools.md)
§Discovery and lazy tool search for the wire-level detail). In `ToolSearch`
mode the system prompt drops the skills/dynamic-tool rosters for a one-line
pointer at `explore`/`describe` (`Full` mode keeps today's sections).

The assembled tools go into `LlmRequest { system,
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
`ToolResult`'s `is_error`/`duration_ms` fields (#636,
[ADR-0176](../adr/0176-structured-tool-result-is-error-and-duration-fields.md))
and `exit_code` (#681,
[ADR-0186](../adr/0186-exit-code-joins-the-structured-tool-result-side-channel.md))
ride straight through to the emitted `ToolOutput` — display-only, so they never
feed `Context`; the model still sees only the text. The #636 hand-audit covers
the runtime-owned `poll` route too (#695, the remainder ADR-0176 deferred):
unknown-handle, refused-`kill`, and script-terminal-error polls set
`is_error`; every state report from a poll that ran — including a job exiting
nonzero, whose status rides `exit_code` orthogonally — stays `false`.

**`invoke` calls are unwrapped for display and dispatch, never in `Context`**
([ADR-0204](../adr/0204-invoke-fallback-for-client-side-discovery.md),
`session/invoke_envelope.rs`). Core has no policy here: the only trigger is
"this round's advertised specs contain a spec named `INVOKE_TOOL`". When that
holds, each `invoke` call in the batch is unwrapped before its
`ToolCall`/`ToolExec` pair is emitted. The rules are ADR-0193's: `name` must be
a non-empty string other than `invoke` or `responses_tool_search`; a missing
`args` becomes `{}`; `args` given as a JSON string is parsed and must yield an
object; an object is used as is. Anything else is not unwrapped. The events
name the inner tool, `input` is the inner args serialized compactly, and
`envelope` carries the emitted outer name and raw input. `TurnState` keeps the
pending calls in this unwrapped form, plus an `envelopes` map by call id, so a
re-offer (resume or timer) and the resolving `ToolOutput` carry the same
envelope. The assistant message pushed into `Context` keeps the model's emitted
`invoke` call byte for byte (name, raw input, id, `provider_meta`). Rewriting it
to the inner name made GLM-5.2/5.3 re-`describe` on the next turn, and it would
shift the cached prefix. The result still pairs by call id. Replay rebuilds the
emitted call from `envelope` when present, so a resumed session sends the same
bytes. `ToolCallDelta` fragments stay as streamed, since they arrive before the
call is assembled.

**Replay rebuilds the live history byte for byte** ([ADR-0202](../adr/0202-prompt-cache-discipline-anchors-deferral-replay-compaction-date.md)).
`Session::replay` folds events through `TurnFold` (`session/replay_pending.rs`),
which mirrors each live commit point instead of batching per turn: one assistant
message per model round (text, then its `ReasoningBlock`/`SearchResult` blocks,
then the round's calls with their `provider_meta`, `invoke` calls rebuilt from
`envelope`), each `ToolOutput` pushed on arrival, the ADR-0118 nudge after an
`AmbiguousRetry`, and no message at all for an empty reply — the live commit
skips an empty assistant message on every stop, confident or ambiguous, since
the log carries no stop reason and the strict wires drop one anyway. A `Prompt`
logged while a turn is live is stashed and folded at the next round's first
event, or after `Done` (ADR-0058). A resting `Status` (`Done`, or `Paused` on a
paused session) after a logged `InMsg::Stop` — or, without one, with a round or
batch open — is the cancel: the uncommitted stream is dropped, an
already-emitted batch is kept, and the turn ends even if nothing had streamed.
The
`ToolOutput.content` and `ToolCall.provider_meta` fields exist so nothing is
lost. `tests/it/replay_equality.rs` proves live and resumed requests are
identical across every round shape. The prune-only compaction divergence
(ADR-0121) is the one remaining gap.
**Every** tool call takes the runtime round-trip; core holds no executable tools
and runs nothing inline — the built-ins were removed in #231
([ADR-0049](../adr/0049-plan-task-tools-as-runtime-state-tools.md)), and the
former plan-authority tools (`propose_plan`/`update_tasks`, #513) are now
ordinary runtime state/orchestration tools carried on `tool_specs` — the
former per-profile `profile_tool_specs` append is retired
([ADR-0207](../adr/0207-permission-modes-replace-agent-borne-authority.md)),
so both are advertised unconditionally to every session.
Each round-trip's `Finish` is priced against
`EngineConfig.pricing` (effective model = `session.model` (a live switch) else
`profile.model` else `default_model`),
folded into the session's `SessionUsage`, and emitted as `OutEvent::Usage`; a
`StopReason::MaxTokens` also emits a truncation-warning `Error` (✅ #192,
[ADR-0055](../adr/0055-usage-cost-and-stop-reason-surfacing.md)). Permission dispatch and approval no longer run
here — the runtime tool executor owns them (§3, §8, ✅ #59). While parked, the
session loop stashes a `Prompt`/`SetModel` for the live turn's fold
site / replay-after-turn; only the stash gate differs from idle (the stash is
popped only between turns). `SetMode` is the one exception (#560): it applies
immediately even while parked — a mode is a label the runtime's tool-dispatch
gate reads on the *next* call, not something a live round is using, so there
is nothing to protect by waiting. This is the shape a `propose_plan` approval
actually hits — `SetMode` then `ToolResult`, both while parked on that very
call — and deferring it left the continuing turn's next tool call graded
under the mode the plan was written in, not the one just approved into.

**Live model/provider switch** (✅ #218,
[ADR-0063](../adr/0063-realtime-model-provider-switch.md)): an idle `SetModel {
provider, model }` re-resolves via `EngineConfig.model_resolver` (a
runtime-supplied `Fn(&str,&str) -> Result<ResolvedModel,_>` capturing the catalog
+ warm per-endpoint client, #217), rebuilds `Session::llm`, and retargets the
per-session `model` (overrides `profile.model` on the request + in pricing) +
`generation` + the `Context` window budget — no restart. Emits `ModelChanged`
(unknown provider / missing key → `Error`); deferred mid-turn (rebuilding the
backend under a live round would be incoherent — unlike `SetMode`, which
applies immediately, #560), and
replay re-applies it to re-bind a resumed session. That success arm is factored
into `Session::rebind`, shared by the live switch and the pin path below.

**Per-profile model pinning** (✅ #323,
[ADR-0081](../adr/0081-per-profile-model-pinning-and-rebind-on-set-agent.md),
narrowed by [ADR-0207](../adr/0207-permission-modes-replace-agent-borne-authority.md)
§9 — `SetAgent` itself is deleted, so this is now the *only* rebind locus)
reuses that same `rebind`: **at session start**, a profile carrying a **model
pin** (`Agent::model_pin()` — both `provider` and `model` set)
re-binds the backend to it, so an agent chosen at spawn can pin its own
endpoint (guarded on `Session.provider`/`model` so a resumed session already
on its pinned endpoint doesn't rebuild). Precedence: per-session memory
(`Session.profile_models`, a `/model` choice recorded under a profile) **>**
the static pin **>** keep the current binding — so a pin-less profile with no
memory emits no `ModelChanged`. `SessionStarted` emits `AgentChanged`
first regardless; a resolver failure surfaces the same `Error` as `SetModel`
and keeps the old binding. Replay reconstructs `profile_models`/`provider`
from the folded `ModelChanged` records.

**Live generation-parameter changes + per-profile persistence** (#374,
[ADR-0094](../adr/0094-reasoning-effort-and-per-profile-generation-persistence.md))
mirrors the model pin above, but through a **separate** seam:
`EngineConfig.generation_resolver: Option<GenerationResolver>` (a
runtime-supplied `Fn(&str) -> Option<GenerationParams>`, keyed by profile
*name* rather than baked into `Agent` — `GenerationParams`'s
`temperature: Option<f32>` has no total `Eq`, so it can't join
`Agent`'s `PartialEq + Eq` derive the way the pin's `provider`/`model`
fields do). `Session.generation` starts at the catalog default
(`EngineConfig.generation`, resolved from the active model at session
creation, unchanged from #191) and layers on top of it, at
session start (the only rebind locus now, mirroring the model pin above),
with the same three-tier precedence the pin uses: **session
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
`AgentModelStore` it has no `apply(&mut AgentCatalog)` — there is nothing
on `Agent` to overlay, so its `resolver(...)` builds the
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
(`Prompt`, `SetModel`, …) is pushed onto the replay stash, so a follow-up sent
while the engine is busy is never silently dropped. `SetMode` is the
exception: it never rides the stash — inside the streaming loop (pre-stream
wait included) and while parked alike it applies the moment it is dequeued
(`session::mode::apply_set_mode`, #560), so a mid-stream `/mode research`
grades the tool calls that very round is emitting — see the tool-round-trip
section above. A stashed **`Prompt` is additionally
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
ADR-0061). The trip ends the turn through `emit_turn_error`
(`session/emit.rs`) — the same `Error` + `Done` + `Status` triple a backend
error fires — so a one-shot head awaiting `Done` exits, and replay sees the
turn boundary exactly where the live engine drew it (it used to emit only the
`Error`, #177).

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
   and **forks a successor session** seeded with that summary plus the verbatim
   kept tail (`summarize::compose_report`) — the live `Context` is never
   mutated ([ADR-0205](../adr/0205-every-compaction-forks-a-successor-session.md)
   amends ADR-0103's in-place design; a turn mid-flight no longer needs a head
   to fork into, because the engine forks for itself). On success it emits
   `OutEvent::Compacted { auto: true, mode: summary, .. }`. The request reuses the round's
   own system prompt and tool specs — `run_round` resolves both once, *before*
   this gate (`session/round_inputs.rs`), so a remote `system_prompt_resolver`
   is never fetched twice — and its shape follows the backend ([ADR-0202](../adr/0202-prompt-cache-discipline-anchors-deferral-replay-compaction-date.md) §4,
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
   summary), or the result still doesn't fit. It prunes a **clone** of the
   context and forks a successor seeded with the pruned transcript, emitting
   `OutEvent::Compacted { auto: true, mode: prune, .. }` (ADR-0205) — the
   source's own history is left exactly as its log describes. Prunes in one
   batch down to ~90% of the budget rather than stopping the instant the
   estimate dips under it (#566): a session sitting near the edge would
   otherwise re-trip this fallback every round or two as new content trickles
   in — busting a provider's cached prefix right when requests are largest.
3. **Refuse the turn** via `emit_turn_error` (a `"context window exceeded"`
   `Error` + `Done` + `Status`) if pruning also doesn't fit — sending an
   over-window request just burns a paid round-trip and errors at the provider.

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
automatic steps and the manual op below. It emits `Compacted` — the source's
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

**Single-shot ops — `InMsg::Oneshot` (`session/ops.rs`, #324,
[ADR-0082](../adr/0082-single-shot-session-ops-and-persisted-compaction.md)).**
Separate from the turn loop above: `run_oneshot` never streams tool calls and
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
([ADR-0154](../adr/0154-per-purpose-auxiliary-models.md), next section) routes
the call to a one-shot pinned client; unset, the triple resolves straight back
to the session's own `llm`/`model`/`generation`. `"compact"` resolves the
session's system prompt and tool specs exactly as a turn round does
(`session/round_inputs.rs`) and summarizes via the shared
`session/summarize.rs::summarize`, which `session/turn.rs`'s auto-compact path
above also calls — so it takes the same two request shapes ([ADR-0202](../adr/0202-prompt-cache-discipline-anchors-deferral-replay-compaction-date.md) §4). On the
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
`resolve` also reports which arm it took (`BackendArm::Session` /
`PinnedAux`), which is what picks the request shape above (ADR-0202).
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
counter>`, 15 characters (`s-`/`j-`/`r-`/`o-`/`x-` for `Session`/`Job`/
`Request`/`Output`/`Script` — the last per #637,
[ADR-0185](../adr/0185-rhai-joins-background-and-poll.md)), with a
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

**Pause is a hold, not a cancel** (#516, [ADR-0208](../adr/0208-pause-resume-a-hold-between-cancel-and-hibernate.md)).
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
background? }` sends `InMsg::Prompt` at an existing child launched via `agent`
instead of minting a new `InMsg::Spawn` — the child session
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
*loudly* rather than silently at this layer too — belt-and-suspenders with
the supervisor's own refusal (`holly.rs`'s lazy-`Prompt` path knows a session
id is a spawned child via `parent_links`, which survives hibernation
specifically so it can refuse a known child instead of blank-respawning it
under `build`, ADR-0168/#639). Only a `Live` child is ever actually sent the
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
"child session". Spawning is bounded by the session's **permission mode**
now, not the agent
([ADR-0207](../adr/0207-permission-modes-replace-agent-borne-authority.md)
§6, superseding [ADR-0023](../adr/0023-subagent-spawn-limits.md)/[ADR-0040](../adr/0040-per-profile-spawn-control.md)):
the old per-profile `can_spawn`/`spawnable_agents` gates and the
process-global `MAX_SPAWN_DEPTH`/`MAX_SPAWNS_PER_ROOT` constants are gone —
`agent`/`agent_send` are `Capability::Control`, so spawning is never
permission-graded at all, and **any agent may spawn any registered agent**.
What bounds it instead is two mode facts applying to the session's whole
spawn sub-tree: `max_depth` (nesting, root = 0) and `max_agents` (concurrent
children per root), both `Option<u32>` on the mode's `Limits` — undefined
means unlimited — enforced by `SpawnGuard::try_spawn`, which still folds
parent links from `SessionStarted` and replies with a clear refusal
`ToolOutput` naming the limit instead of starting a child.
`runtime::permission::spawn_refusal(target, registry)` is reduced to the one
check left: does `target` resolve to a real, registered agent. Spawn stays
**mode-gated** in a different sense than before, via `ancestor_chain`: a
pluggable, embedder-supplied `PermissionResolver` (§permission modes in
[agents & permissions](agents-and-permissions.md)) can still vary per
session (a per-user ceiling, say), so `tool_runner::resolve_effective` folds
every session in the chain to the least-privileged grade
(`Deny < Ask < Allow`) rather than trusting only the leaf — for the built-in
`ModeResolver`, every session in one spawn tree already shares the
identical mode (§6), so this clamp is a no-op there and matters only for a
custom resolver. Filesystem isolation (a separate child root) and
bidirectional session-to-session messaging are still deferred (see
ADR-0022/0024).

**Roster disclosure** (✅ #112, [ADR-0034](../adr/0034-file-based-agent-definitions.md),
unscoped by [ADR-0207](../adr/0207-permission-modes-replace-agent-borne-authority.md)
§6, superseding [ADR-0040](../adr/0040-per-profile-spawn-control.md)).
The `agent` tool description carries one `name: description` line per
registered agent, and the `agent` argument's schema constrains the name to an
`enum` — so the model learns *who it may spawn* at the call site, and
`description` is the one field of a definition ever exposed to a parent. The
roster + enum are now **identical for every session** — `subagent::agent_specs`
is provably the same regardless of which profile asks, since there is no
more per-profile `spawnable_agents` to scope it by — so the `agent` spec
rides the shared, session-stable `tool_specs` directly, with no more
per-profile `profile_tool_specs` append: nothing about the array varies by
agent any more. The `agent` tool also gained a `model` parameter, validated
against the catalog, so a spawn can pin its child's model directly. The
related supervisor wart is fixed too: an `InMsg::Spawn` naming an unknown
profile emits a supervisor `Error` instead of silently resolving to a
default.

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

**Plan acceptance, file-backed, approval as a mode switch — `propose_plan`**
(✅ #141/#513, [ADR-0042](../adr/0042-plan-acceptance-via-propose-plan-approval-roundtrip.md)/[ADR-0145](../adr/0145-one-plan-tool-file-backed-plans-and-blocking-review-loop.md),
[ADR-0207](../adr/0207-permission-modes-replace-agent-borne-authority.md)
§7 **retires** [ADR-0138](../adr/0138-sponsored-build-child-and-propose-plan-cycle.md)
wholesale — no sponsored child, no permission root, no blocking build wait).
The plan agent calls a runtime-owned `propose_plan(content: Option<String>,
path: Option<String>)` — **exactly one** of the two. `content` materializes
(or overwrites) `.entanglement/plans/<short-session-id>.md`; `path` binds an
existing in-root `.md` file, refused if it's changed since the session last
touched it (a session-scoped content-hash staleness guard,
`entanglement-runtime/src/plan_files.rs`, kept fresh by `propose_plan` itself
plus a passive listener on the executor's `FileChange` audit for the
session's own `edit`/`write`). A malformed or stale call replies immediately
with **no** approval prompt. `propose_plan` carries `Capability::Plan`
(ADR-0207 §3/§7): a mode denying it (`research`/`build`/`auto` in the
built-in table) declines the call flat — naming the mode and the way out —
**before** any file is touched or an approval is ever parked; a mode
allowing it (`plan`'s own `Allow`) still force-parks unconditionally, since
approval *is* the tool's semantics and no mode may `Allow` past it.
Otherwise the executor (`propose_plan.rs`) intercepts it on `ToolExec` —
after the capability check above, same interception family as `ask_user` —
first emitting an `OutEvent::Plan { content, path }` snapshot for the plan
session's own display, then a standard `OutEvent::ToolRequest` carrying the
resolved `{content, path}` JSON regardless of which the model sent.

The staleness guard above only fires at the *next* `propose_plan(path=...)`
call; a dedicated, debounced plans-folder watch (#627,
[ADR-0173](../adr/0173-watcher-driven-plan-file-changed-notice.md),
`entanglement-runtime/src/plan_watch.rs`) surfaces the same out-of-band-edit
detection live instead. It reuses `watch.rs`'s `spawn_debounced_watcher`
primitive only — never its agent/skill/config `LiveDefinitions` reload, a
deliberately unrelated reload action — and shares the exact same
`PlanFileRegistry` instance as the staleness guard and the `FileChange`
listener: a debounced firing re-hashes every currently bound file, and a
mismatch both self-heals the registry (so the guard doesn't also refuse the
next resubmit) and emits a session-scoped `OutEvent::PlanChanged { path, hash
}`, which the TUI folds into the session's transcript as a durable notice.

**Approve** — the entire ADR-0138 mechanism this used to trigger is gone: no
`SpawnGuard` sponsor mutation, no child session, no `WaitingAgent` block, no
folded-back build report. The executor instead sends `InMsg::SetMode` on the
**plan session itself** and replies at once, naming the plan file and the mode
it switched to; the same turn continues, implementing the plan directly.

**Which mode is the approver's choice** (#560): `InMsg::Approve` carries an
optional `mode`. The prompt offers `[u]` accept → `auto`, `[b]` accept →
`build`, `[n]` reject. A bare accept (no `mode`) goes to `auto`, since
accepting a plan usually means "go do it" and `auto` is bounded by its budgets,
its timeout and a deny list covering every network-mutating command. An
unrecognised value fails *safe* to `build` rather than open to `auto` — a
mistyped cautious choice must not run a plan unattended. `propose_plan` may
*suggest* `build` or `auto` through its own optional `mode` argument; that only
pre-selects the option and never decides.

The switch takes effect **immediately**, not at turn end: `SetMode` is applied
the moment it is dequeued, so the tool calls the continuing turn makes are
graded under the new mode. (It used to be stashed while a turn was live, which
left the post-approval turn running in `plan` mode with `write` still denied.)
The model's next request carries `[mode: auto — changed from plan]` once, then
the plain notice. Core cascades the `SetMode` over the session's whole live
spawn sub-tree (§6: mode applies uniformly, no per-spawn override), so a plan
session with running children switches them too.

Headless `run` still **auto-rejects** every plan, even under `--mode auto`:
accepting a plan unattended would let a bounded run revise its own plan and
proceed, which is a different thing from executing a plan a person approved.

A multi-phase plan → build → review loop is: work in `build` or `auto`, `/mode
plan` to go back when the plan needs revising, edit the file, `propose_plan`
again. Going back is the user's action — `request_mode` only ever *widens*, so
it cannot move `build → plan`. **Reject + reason** folds `tool \`propose_plan\`
rejected (plan file: <path>): <reason>` back, still naming the file
(materialized either way — rejection is about the *proposal*, not the file);
the model revises and re-proposes in the same turn.

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
a `Deny` from the session's permission mode throws a catchable script
exception; `Allow` runs; `Ask` parks the script on the standard `ToolRequest`
→ `Approve`/`Reject` round-trip, **resolved once per function per run** (the
first `edit` asks; approval covers the rest). Because the bindings *are* the
always-registered quintet, `rhai` is precisely as privileged as those tools —
so it is registered by default in the shared `tool_specs`, and it is itself a
multi-`Capability` tool (`Read`+`Write`+`Exec`,
[ADR-0207](../adr/0207-permission-modes-replace-agent-borne-authority.md)
§3), so a mode denying any of those classes declines it, and `research`
mode's curated posture grades it `Ask` like the exec tools rather than
outright denying it. The executor intercepts `rhai`
before the generic dispatch (it needs the session's live mode + overlay state
to snapshot each binding's grade); its *own* Allow/Ask/Deny is resolved
the same way as any host tool. Rhai's engine is sync, so the script runs under
`spawn_blocking` and each binding crosses a small **bridge** — `mpsc` request +
`oneshot` reply — to the async resolver on the executor task; the timeout is
enforced inside the engine, not by aborting the blocking task. A session `Stop`
(#167) reaches the blocking engine the same way: it trips a cooperative flag the
progress callback polls, terminating the script with an uncatchable
`ErrorTerminated` (unlike a thrown binding error, a script can't `try`/`catch` it
and continue). No exec bindings (`bash`/`call`) in v1 — that would escape the
sandbox.
