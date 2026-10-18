# Per-session engine §5 — Turn loop, tool round-trip & live switching

> Part of the [per-session engine module](../engine.md) of the [architecture overview](../../architecture.md).

## 5. Per-session engine (`session/`)

The turn loop lives in the `session/` split — `session/turn.rs` (the live
reasoning turn: `drive_turn`/`run_round`, owning the per-round setup that only
needs to run once — tool specs, the context-window gate, system prompt
resolution — plus the small driver loop that retries in place),
`session/round.rs` (`run_attempt`: one streamed attempt and the ADR-0118
ambiguous-stop retry decision, split out of `turn.rs` along that retry seam,
#436), `session/stream.rs` (one streamed round-trip), `session/turn_state.rs`
(the parked-turn state), `session/mcp_envelope.rs` (the ADR-0226 `mcp`
envelope unwrap), and `session/emit.rs` (outbound-event helpers), with
`session/replay.rs` holding the pure state reconstruction (its turn fold
state machine in `session/replay_pending.rs`). `session.rs` itself keeps the
session task's command loop (`session_loop`); its start-up (announce, model
pin, generation overlay) is `session/start.rs`, the `SessionCmd` set and the
capped deferred-command stash are `session/cmd.rs`, and the loop's
state-changing command handlers are `session/commands.rs`.

Each session is a tokio task — created by `Spawn`/`Resume`, or on first
message per the unknown-session policy ([bounds & compaction](bounds-and-compaction.md)) — owning: `Context` (message history +
token estimate), an LLM backend `llm: Box<dyn Llm>` (from
`EngineConfig::llm_factory`), the
active `Agent`, a per-session `seq`, and `turn: Option<TurnState>` — the
in-flight turn as **explicit, serde-serializable state** (#270,
[ADR-0061](../../adr/0061-parked-turn-state-batch-tool-resolution.md)): `Some`
while a turn is live (streaming or parked on unresolved tool calls), `None`
when idle.
The backend is a **plain `Box<dyn Llm>`, not a per-session handle**
([ADR-0062](../../adr/0062-collapse-llmsession-placeholder-newtype.md), collapsing
the former `LlmSession` placeholder): the *conversation history* stays in core's
`Context`, and the *connection* state (pool, retry, rate-limit budget) belongs to
the provider — but that state is keyed **per endpoint** and shared across
sessions (#217, [ADR-0050](../../adr/0050-per-endpoint-connection-pool-retry-rate-limit.md)),
so there is no honest session-scoped state to wrap. The factory hands core the
streaming backend directly.

Turn loop (`run_round`, driven by `drive_turn`): assemble `tools` — **every**
spec `EngineConfig.tool_specs` (or the per-session `tool_spec_resolver`) yields,
verbatim, with **no filtering**: there is no more per-agent tool mask to
filter with — [ADR-0207](../../adr/0207-permission-modes-replace-agent-borne-authority.md)
retired it along with the rest of `Agent`'s authority fields, and the
session's permission mode grades every call at dispatch instead — and the
skill mask is removed too ([ADR-0194](../../adr/0194-skills-are-additive-only.md):
skills are additive-only), so the advertised surface stays stable within a
session and the provider's prompt cache survives an overlay toggle or a
skill load. With `Agent` carrying no more authority, the array no
longer varies by agent either — `propose_plan`/`request_mode` are pushed
into the base `tool_specs` unconditionally and the `agent`/`agent_send` spawn
schema is now provably identical regardless of which session asks, so the
old per-profile `profile_tool_specs` append is retired (see [agents &
permissions](../agents/modes.md) §permission modes for the mode's
own `Deny`/`Ask` grading and the discovery tools).

**One advertised surface** ([ADR-0226](../../adr/0226-one-advertised-surface-mcp-envelope-registered-is-available.md), superseding ADR-0196 §1–§4 and
ADR-0204): there is no advertising mode, wire encoding or discovery knob. The
runtime's resolver yields the compiled-in kernel (`tool_names::KERNEL`: the
file/exec tools, `web_fetch`, `poll`/`ask_user`/`update_tasks`,
`propose_plan`/`request_mode`, `agent`/`agent_send`, `rhai`, the six
discovery tools `list_agents`/`list_models`/`list_skills`/`load_skill`/
`list_mcp`/`mcp_tools`, and the `mcp` envelope), snapshotted at the session's
first resolution and returned byte-identical every later round, on every
wire. Nothing else is advertised: MCP tools are reached only through
`mcp(server, tool, args)`, their schemas delivered by `mcp_tools` as a tool
result; `glob`/`grep`/`call`/`http` are script-only `rhai` bindings. Core
still advertises
whatever the `tool_spec_resolver` yields, re-consulted every round; that it
never changes is the runtime's snapshot, not a core rule. The system prompt
carries no skill roster (`list_skills` does) and one static tool-conventions
line.

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
*ambiguous* stop instead retries in place (ADR-0118, detailed in [bounds & compaction](bounds-and-compaction.md)).
Batch calls thereby execute **concurrently**, not serially in call order
(#270, [ADR-0061](../../adr/0061-parked-turn-state-batch-tool-resolution.md));
a stale, duplicate, or unknown `ToolResult` is dropped with a debug trace.
`ToolResult`'s `is_error`/`duration_ms` fields (#636,
[ADR-0176](../../adr/0176-structured-tool-result-is-error-and-duration-fields.md))
and `exit_code` (#681,
[ADR-0186](../../adr/0186-exit-code-joins-the-structured-tool-result-side-channel.md))
ride straight through to the emitted `ToolOutput` — display-only, so they never
feed `Context`; the model still sees only the text. The #636 hand-audit covers
the runtime-owned `poll` route too (#695, the remainder ADR-0176 deferred):
unknown-handle, refused-`kill`, and script-terminal-error polls set
`is_error`; every state report from a poll that ran — including a job exiting
nonzero, whose status rides `exit_code` orthogonally — stays `false`.

**`mcp` calls are unwrapped for display and dispatch, never in `Context`**
([ADR-0226](../../adr/0226-one-advertised-surface-mcp-envelope-registered-is-available.md), carrying forward ADR-0204's mechanism, `session/mcp_envelope.rs`).
Core has no policy here: the only trigger is "this round's advertised specs
contain a spec named `MCP_TOOL`". When that holds, each `mcp` call in the
batch is unwrapped before its `ToolCall`/`ToolExec` pair is emitted. `server`
and `tool` must be non-empty strings (`tool` not `mcp` itself); a missing
`args` becomes `{}`; `args` given as a JSON string is parsed and must yield an
object; an object is used as is. Anything else is not unwrapped. The events
name the inner tool `mcp__<server>__<tool>` (characters outside
`[A-Za-z0-9_-]` mapped to `_`, the runtime's registered spelling), `input` is
the inner args serialized compactly, and
`envelope` carries the emitted outer name and raw input. `TurnState` keeps the
pending calls in this unwrapped form, plus an `envelopes` map by call id, so a
re-offer (resume or timer) and the resolving `ToolOutput` carry the same
envelope. The assistant message pushed into `Context` keeps the model's emitted
`mcp` call byte for byte (name, raw input, id, `provider_meta`). Rewriting it
to the inner name would shift the cached prefix (and, under ADR-0204's
`invoke`, made GLM-5.2/5.3 re-discover on the next turn). Logs from before
ADR-0226 carrying `invoke` envelopes still replay — the envelope is
name-agnostic. The result still pairs by call id. Replay rebuilds the
emitted call from `envelope` when present, so a resumed session sends the same
bytes. `ToolCallDelta` fragments stay as streamed, since they arrive before the
call is assembled.

**Replay rebuilds the live history byte for byte** ([ADR-0202](../../adr/0202-prompt-cache-discipline-anchors-deferral-replay-compaction-date.md)).
`Session::replay` folds events through `TurnFold` (`session/replay_pending.rs`),
which mirrors each live commit point instead of batching per turn: one assistant
message per model round (text, then its `ReasoningBlock`/`SearchResult` blocks,
then the round's calls with their `provider_meta`, envelope calls rebuilt from
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
([ADR-0049](../../adr/0049-plan-task-tools-as-runtime-state-tools.md)), and the
former plan-authority tools (`propose_plan`/`update_tasks`, #513) are now
ordinary runtime state/orchestration tools carried on `tool_specs` — the
former per-profile `profile_tool_specs` append is retired
([ADR-0207](../../adr/0207-permission-modes-replace-agent-borne-authority.md)),
so both are advertised unconditionally to every session.
Each round-trip's `Finish` is priced against
`EngineConfig.pricing` (effective model = `session.model` (a live switch) else
`profile.model` else `default_model`),
folded into the session's `SessionUsage`, and emitted as `OutEvent::Usage`; a
`StopReason::MaxTokens` also emits a truncation-warning `Error` (✅ #192,
[ADR-0055](../../adr/0055-usage-cost-and-stop-reason-surfacing.md)). Permission dispatch and approval no longer run
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
[ADR-0063](../../adr/0063-realtime-model-provider-switch.md)): an idle `SetModel {
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
[ADR-0081](../../adr/0081-per-profile-model-pinning-and-rebind-on-set-agent.md),
narrowed by [ADR-0207](../../adr/0207-permission-modes-replace-agent-borne-authority.md)
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

**Spawn-time model choice** (✅ #725): `InMsg::Spawn { model: Some(ModelChoice
{ provider, model }) }` picks the model a session starts on — the only way to
get the *first* round on it, since a follow-up `SetModel` queues behind the
initiating prompt. The supervisor resolves it through the same
`model_resolver`, under the session's effective user, **before** recording any
state for the new session; `start_session` then binds it via `rebind` in the
pin's slot, so it out-ranks the agent's pin (ADR-0081's session memory >
static pin — exactly what a `SetModel` before turn one would do) and the pin
is never applied. `SessionStarted.model` already announces it, and its
`ModelChanged` right after is what persistence logs and replay re-folds — no
new event or record shape. Failure is deliberately **stricter** than a
`SetModel` (which keeps the old binding) or the best-effort pin: an
unresolvable choice, or an engine with no resolver, refuses the spawn with a
seq-0 supervisor `Error` (the unknown-`agent` refusal shape, #119) and leaves
the id free for a corrected retry — running a turn on a model the caller
didn't ask for would be a silent cost/quality swap no later event can undo.

**Live generation-parameter changes + per-profile persistence** (#374,
[ADR-0094](../../adr/0094-reasoning-effort-and-per-profile-generation-persistence.md))
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
[ADR-0095](../../adr/0095-tui-set-show-generation-persist-on-confirmation.md))
mirror the `/model` picker's own persist-on-confirmation logic (`tui/app/model_picker.rs`).

Setup errors (the initial `stream()` call)
surface as `Error` + `Done` with no partial to commit. A **mid-stream** failure
is handled to keep the committed context aligned with what the user saw (#181,
[ADR-0057](../../adr/0057-mid-stream-error-partial-commit-and-retry.md)):
Reasoning arrives on two rails and only one reaches `Context`
([ADR-0160](../../adr/0160-extended-thinking-round-trip.md)): `LlmEvent::Reasoning`
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
while the engine is busy is never silently dropped. `SetMode`,
`SetSessionMeta` and the `ChildSpawned`/`ChildClosed` lineage mirror are the
exception: they never ride the stash — inside the streaming loop (pre-stream
wait included) and while parked alike they apply the moment they are
dequeued (`session::immediate`), so a mid-stream `/mode research` grades the
tool calls that very round is emitting — see the tool-round-trip section
above. The stash drains only when a turn ends, so letting the narrator's
per-tool-call `SetSessionMeta` in used to fill it to its 64-command cap
during a long turn, and the user's next `Prompt` was refused. Mid-stream the
user-issued deferrable commands (`Prompt`/`SetModel`/`SetGeneration`/
`SetToolOverlay`/`Oneshot`) hit the same cap as the idle loop's; lifecycle
commands (`Hibernate`, `Pause`/`Unpause`, `ToolResult`) are never dropped. A stashed **`Prompt` is additionally
*folded into the live turn*** (#182,
[ADR-0058](../../adr/0058-mid-turn-prompt-folds-into-live-turn.md)): at the top of each inner-loop iteration —
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

---

Next: [§5 — Re-offer, idle TTL, loop bounds & compaction](bounds-and-compaction.md)
