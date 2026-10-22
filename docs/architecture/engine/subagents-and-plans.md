# Per-session engine §5 — Sub-agents, ask-user, plan acceptance & `rhai`

> Part of the [per-session engine module](../engine.md) of the [architecture overview](../../architecture.md).

**Sub-agent spawn** (✅ #60, [ADR-0022](../../adr/0022-subagent-spawn.md), builds on the
[ADR-0021](../../adr/0021-hierarchical-session-model.md) tree). The model calls a
runtime-owned `agent { agent, prompt, background? }` tool (renamed from
`spawn_agent`, ✅ #120,
[ADR-0033](../../adr/0033-agent-tool-family-and-blocking-agent.md); the separate
`agent_spawn` tool it was later split into is retired again, ✅ #606,
[ADR-0161](../../adr/0161-unified-async-work-background-flag-and-one-poll.md) —
one tool, `background: bool` picks the return shape). The runtime executor
intercepts it before per-tool permission resolution (it starts a session rather
than touching a host resource), mints a child `SessionId`, and sends `InMsg::Spawn { session: child, parent, agent,
prompt }`. The **supervisor** records `parent_links[child] = parent` and starts
the child `session_loop` under the requested profile with the prompt queued — so
the child's `SessionStarted` carries the parent link and the tree-walk helpers
(`children_of` / `root_of`) reflect reality. `background: true` is
**non-blocking** (✅ #89,
[ADR-0026](../../adr/0026-async-subagent-spawn-and-poll.md), supersedes ADR-0022's
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
`poll { handle, timeout_secs? }` (✅ #605, [ADR-0161](../../adr/0161-unified-async-work-background-flag-and-one-poll.md),
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
[ADR-0155](../../adr/0155-errored-subagent-turn-parks-for-steering.md)): the
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
same ownership bookkeeping serving both the parent-scoped (#618) descendant
check above and the listing, merged with an equivalent job-registry snapshot by
`entanglement_runtime::operations::list_operations` — see
[protocol](../protocol.md) and [gates & host tools](../gates-and-host-tools.md).
Refusals (depth, budget, capability) are identical regardless of `background`
— one shared guard path.

**Sub-agent follow-up** (✅ #609, [ADR-0162](../../adr/0162-agent-send-supervising-a-sub-agent.md)).
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
the supervisor's own refusal (`holly/supervisor/spawn.rs`'s lazy-`Prompt` path knows a session
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
([ADR-0207](../../adr/0207-permission-modes-replace-agent-borne-authority.md)
§6, superseding [ADR-0023](../../adr/0023-subagent-spawn-limits.md)/[ADR-0040](../../adr/0040-per-profile-spawn-control.md)):
the old per-profile `can_spawn`/`spawnable_agents` gates and the
process-global `MAX_SPAWN_DEPTH`/`MAX_SPAWNS_PER_ROOT` constants are gone —
**any agent may spawn any registered agent**. `agent`/`agent_send` are
`Capability::Control` but graded like any tool before they launch
([ADR-0232](../../adr/0232-spawning-is-graded-bookkeeping-control-tools-non-maskable.md)): an overlay deny or a mode `Deny` refuses with an
error result and starts no child, and the built-in modes class-allow
`control`. Past that grade, two mode facts bound the session's whole
spawn sub-tree: `max_depth` (nesting, root = 0) and `max_agents` (concurrent
children per root — held from spawn until the child's answer arrives, never
a cumulative budget), both `Option<u32>` on the mode's `Limits` — undefined
means unlimited — enforced by `SpawnGuard::try_spawn`, which still folds
parent links from `SessionStarted` and replies with a clear refusal
`ToolOutput` naming the limit instead of starting a child.
`runtime::permission::spawn_refusal(target, registry)` is reduced to the one
check left: does `target` resolve to a real, registered agent. Spawn stays
**mode-gated** in a different sense than before, via `ancestor_chain`: a
pluggable, embedder-supplied `PermissionResolver` (§permission modes in
[agents & permissions](../agents-and-permissions.md)) can still vary per
session (a per-user ceiling, say), so `tool_runner::resolve_effective` folds
every session in the chain to the least-privileged grade
(`Deny < Ask < Allow`) rather than trusting only the leaf — for the built-in
`ModeResolver`, every session in one spawn tree already shares the
identical mode (§6), so this clamp is a no-op there and matters only for a
custom resolver. Filesystem isolation (a separate child root) and
bidirectional session-to-session messaging are still deferred (see
ADR-0022/0024).

**Roster disclosure** (✅ #112, [ADR-0034](../../adr/0034-file-based-agent-definitions.md),
unscoped by [ADR-0207](../../adr/0207-permission-modes-replace-agent-borne-authority.md)
§6, superseding [ADR-0040](../../adr/0040-per-profile-spawn-control.md)).
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

**Ask-user prompt** (✅ #90, [ADR-0027](../../adr/0027-ask-user-interactive-prompt.md);
v2 #488, [ADR-0127](../../adr/0127-ask-user-v2-multi-question-envelope.md);
draft-until-submit #518, [ADR-0143](../../adr/0143-ask-user-draft-until-submit.md)).
The model calls a runtime-owned `ask_user { questions: [{question, options,
multi_select}] }` tool — one call can batch several questions, each optionally
`multi_select`; a typed "Other" answer is unconditional (no `allow_free_form`
flag to opt into it, dropped in v2). The runtime executor (`ask_user.rs`)
intercepts it on `ToolExec` — before permission resolution, like `agent`
— emits a single dedicated `OutEvent::UserQuestion` carrying the whole
`questions` array and parks at `WaitingAnswer` (#160,
[ADR-0072](../../adr/0072-protocol-warts-settled-before-serve.md): a question is not
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
(✅ #141/#513, [ADR-0042](../../adr/0042-plan-acceptance-via-propose-plan-approval-roundtrip.md)/[ADR-0145](../../adr/0145-one-plan-tool-file-backed-plans-and-blocking-review-loop.md),
[ADR-0207](../../adr/0207-permission-modes-replace-agent-borne-authority.md)
§7 **retires** [ADR-0138](../../adr/0138-sponsored-build-child-and-propose-plan-cycle.md)
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
[ADR-0173](../../adr/0173-watcher-driven-plan-file-changed-notice.md),
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
`build`, `[n]`/`[e]` reject with a reason the model reads. A bare accept (no
`mode`) goes to `auto`, since accepting a plan usually means "go do it". Since
[ADR-0228](../../adr/0228-rhai-path-and-args-skill-tools-frontmatter-removed.md)
`auto` runs everything except its deny list (destructive and publishing
commands) and its prompt list (deletions such as `gh … delete`, `cargo yank`,
recursive `rm`), within its run budgets and `question_timeout` — the approver
who wants each unlisted call seen picks `[b]` `build` instead. An
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
[ADR-0046](../../adr/0046-rhai-sandboxed-script-tool.md)). The model calls
`rhai { script, timeout? }` to run multi-step logic in one call — the sanctioned
replacement for shelling out to `python3`/`node`. The engine
(`script/bindings.rs`, `rhai::Engine::new_raw()` + the IO-free `StandardPackage`) has **no**
filesystem/network/process/env access, and its module resolver serves only
discovered, precompiled modules by bare name (so `import` can't escape,
[ADR-0248](../../adr/0248-importable-rhai-library-modules.md)); `eval` is disabled. It is resource-bounded by construction:
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
[ADR-0207](../../adr/0207-permission-modes-replace-agent-borne-authority.md)
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

---

Previous: [§5 — Revert, fork, workdir, single-shot ops, ids, stop & pause](session-ops.md)
