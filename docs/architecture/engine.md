# entanglement Architecture — Per-session engine

> Part of the [architecture overview](../architecture.md). The *why* behind each choice is in the [decision log](../adr/README.md).

## 5. Per-session engine (`session.rs`)

Each session is a lazily-spawned tokio task owning: `Context` (message history +
token estimate), an `LlmSession` handle (from `EngineConfig::llm_factory`), the
active `AgentProfile`, the `TaskList`, the `Plan`, and a per-session `seq`.
The `LlmSession` is a **provider-owned session/connection handle**
([ADR-0007](../adr/0007-streaming-llm-and-provider-crate.md)): the *conversation
history* stays in core's `Context`, but the *connection* state (pool, retry,
rate-limit budget) belongs to the provider. The factory hands core a pooled
session handle that wraps the streaming backend.

Turn loop: send `LlmRequest { system, model, messages, tools }` → consume the
streamed `LlmEvent`s (emit `TextDelta` per `Text` chunk, gather `ToolCall`s,
note `Finish`) → for each tool call, run built-ins inline or hand host tools to
the runtime (emit `ToolExec`, park on `ToolResult`) → loop until the model
returns no tool calls → `Done`. Permission dispatch and approval no longer run
here — the runtime tool executor owns them (§3, §8, ✅ #59). The tool-result
wait parks the task on its inbox; any non-matching message (e.g. a new prompt) is
stashed and processed after the turn. Setup/mid-stream backend errors surface as
`Error` + `Done` without committing a partial assistant message. The same
stash discipline applies inside the streaming loop and between tool calls
(ADR-0018): mid-turn `try_recv` polls route `Stop` to interrupt and push every
other queued command (`Prompt`, `SetAgent`, …) onto the replay stash, so a
follow-up sent while the engine is busy is never silently dropped.

**Stop is cancel-semantics, not destroy** (ADR-0017). `InMsg::Stop` interrupts
the in-flight turn (the streaming loop and tool dispatch poll `try_recv` for
it; the tool-result wait returns cancelled) but does *not* evict the
session from the supervisor map or end its task. The session's `Context` is
preserved across a Stop+Prompt round-trip — Esc-in-approval or a stray Stop
between turns no longer causes amnesia. The supervisor map entry is only
removed on global inbox close (engine shutdown).

**Sub-agent spawn** (✅ #60, [ADR-0022](../adr/0022-subagent-spawn.md), builds on the
[ADR-0021](../adr/0021-hierarchical-session-model.md) tree). The model calls a
runtime-owned `agent_spawn { agent, prompt }` tool (renamed from `spawn_agent`,
✅ #120, [ADR-0033](../adr/0033-agent-tool-family-and-blocking-agent.md)). The
runtime executor
intercepts it (bypassing per-tool approval, like core's built-ins), mints a
child `SessionId`, and sends `InMsg::Spawn { session: child, parent, agent,
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
`OutEvent::UserQuestion` and parks at `WaitingApproval`. The head renders the labelled choices
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
standard `OutEvent::ToolRequest`. **Approve** records the plan (`InMsg::SetPlan`)
and folds `ToolOutput("plan accepted by the user")` back; **reject + reason**
folds `tool \`propose_plan\` rejected: <reason>` back and records no plan. On
approve the head *additionally* runs the **handoff** — pure head policy, zero new
protocol surface, so pipe/WS heads implement it identically:

1. mint a fresh `SessionId::new_uuid()`;
2. `SetAgent { session: new, agent: "build" }` — lazy session creation starts a
   **root** `build` session;
3. `Prompt { session: new, text: wrap(plan) }` — the accepted plan verbatim as the
   first user message;
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
enforced inside the engine, not by aborting the blocking task. No exec bindings
(`bash`/`call`) in v1 — that would escape the sandbox.
