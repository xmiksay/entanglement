# entanglement Architecture — Wire protocol & structured outputs

> Part of the [architecture overview](../architecture.md). The *why* behind each choice is in the [decision log](../adr/README.md).

## 2. Wire protocol (`protocol.rs`) — [ADR-0002](../adr/0002-session-multiplexed-protocol.md)

One set of serde-tagged types crosses every transport:

```
#[serde(tag = "kind", rename_all = "snake_case")]
InMsg    = Prompt{session,content:[ContentPart]} | Approve{session,request_id,scope?}  // approval →
         //   content: [{type:text,text} | {type:image,source:{type:base64,media_type,data}}]; legacy `text:"…"` still deserializes (#197, ADR-0064)
         | Reject{session,request_id,reason?}                         // runtime, not core (#59)
         //   scope: once (default) | session | always  — persisted grants (#174, ADR-0052)
         | ToolResult{session,request_id,content:[ContentPart]}   // runtime → core: tool ran (#58)
         //   content: text, or an image block when `read` opens an image (#221); legacy `output:"…"` still deserializes
         | AnswerQuestion{session,request_id,answer}  // ask_user answer → runtime (#90)
         | Stop{session}
         | SetAgent{session,agent}   // switch profile (plan/task state is a runtime tool now, #231)
         | SetModel{session,provider,model}   // live model/provider switch, no restart (#218, ADR-0063)
         | Spawn{session,parent,agent,prompt}   // start a child session (sub-agent) (#60)
         | ListSessions{session}   // supervisor-global query; session = correlation id (#21)
         | CloseSession{session}   // explicit destroy → SessionEnded (#21)
         | Resume{session,records}   // internal, not serialized (#[serde(skip)]); replay log → session (§6b)

OutEvent = SessionStarted{session,parent?,profile,model?,root,ts}   // lifecycle, no seq
         | SessionEnded{session,ts}           // lifecycle, no seq
         | SessionList{session,sessions:[SessionInfo]}   // reply to ListSessions, no seq (#21)
         | Status{session,state}              // point-in-time, no seq
         | AgentChanged{session,agent,profile_detail?}   // point-in-time, no seq; detail = posture (#189)
         | ModelChanged{session,provider,model,context_window?}   // point-in-time, no seq; reply to SetModel (#218, ADR-0063)
         | Plan{session,seq,content}          // markdown prose snapshot, runtime-emitted (#231)
         | TextDelta{session,seq,text}
         | ReasoningDelta{session,seq,text}   // reasoning/thinking stream (#54)
         | ToolCallDelta{session,seq,request_id,tool,delta}   // streamed tool-arg fragment; display-only, before the assembled ToolCall (#194)
         | ToolCall{session,seq,request_id,tool,input}      // display-only, every call (before exec)
         | ToolRequest{session,seq,request_id,tool,input}   // Ask prompt, from runtime (#59)
         | ToolExec{session,seq,request_id,tool,input}      // core → runtime: dispatch it (#58/#59)
         | UserQuestion{session,seq,request_id,question,options,allow_free_form}  // ask_user prompt (#90)
         | ToolOutput{session,seq,request_id,tool,output,content?:[ContentPart]}   // output = display text; content carries an image result for faithful replay (#221)
         | TaskList{session,seq,content}      // full outline snapshot (markdown)
         | Usage{session,seq,input_tokens,output_tokens,cached_input_tokens,cache_write_tokens,cost_usd?}  // per-round-trip usage + cost (#192)
         | Error{session,seq,message}
         | Done{session,seq}
         | FileChange{session,seq,path,change_kind,hash}   // file-change audit: runtime executor emits on edit/write; hash = sha256(after) (#202, ADR-0060)
```

`AnswerQuestion` mirrors `Approve`/`Reject`: the supervisor drops it off the
inbound fan-out (core never routes it) and the `ask_user` executor consumes it
(§8, [ADR-0027](../adr/0027-ask-user-interactive-prompt.md)).

**Trusted/untrusted frame split** (#155, [ADR-0069](../adr/0069-trusted-untrusted-wire-frame-split.md)).
`InMsg` has two entry points. `Holly::send` is **privileged in-process**: an
embedder holding a `Holly` (a head, the runtime tool executor) authors any
frame. `Holly::send_from_wire` is the **untrusted** path a wire head (stdio
`pipe`, the future WS `serve`) calls after deserializing a line — it enforces the
`InMsg::wire_allowed()` allowlist and refuses (`WireError::Privileged`, not
routed) the three runtime-authored variants: `ToolResult` (a forged one resolves
a parked turn on `request_id` alone, bypassing execution *and* permission),
`Spawn` (bypasses the tool path's `spawn_refusal` gate, #119), and `Resume`
(internal, `#[serde(skip)]`). The executor folds a completed tool round-trip back
over the named privileged handle `Holly::submit_tool_result` (used by
`seam::reply_content`, the single fold-back site). Under the local single-user
`serve` scope ([ADR-0048](../adr/0048-serve-head-local-trust-model.md)) this is
robustness/UX — which cooperating local client owns a frame — not defence against
a remote attacker; the WS head's `send_from_wire` call and per-connection
`Approve` ownership are deferred to #153.

**Session lifecycle** (✅ #21, [ADR-0028](../adr/0028-session-lifecycle-enumeration-and-backpressure.md)).
`ListSessions` and `CloseSession` are **supervisor-global**: the supervisor
answers/acts on them directly rather than routing to a session task.
`ListSessions` returns one `SessionList` snapshot of the live
`SessionInfo{session,parent?,profile,root,profile_detail?}` set — a reconnecting
head enumerates in one round-trip instead of folding the whole broadcast; its
`session` field is a correlation id the reply echoes. `profile_detail`
(**#189**, optional) carries the active profile's resolved posture — `mode`, the
#116 tool mask (`tools`/`disallowed_tools`), and the `PermissionProfile` rules —
so a head renders the permission posture without re-reading the agent `.md`
layers. It rides `AgentChanged` on every switch and each live `SessionInfo`;
`None` only on the resume path's fallback, where the replay log preserves the
profile *name* alone. Pair it with the runtime's per-resolution `debug!`
(`tool=… rule=Allow|Ask|Deny source=own|ancestor <id>`) when tracing *why* a
sub-agent's tool was clamped. `CloseSession` drops the session's command
channel so its task exits and emits `SessionEnded` — the explicit destroy `Stop`
(cancel-semantics, ADR-0017) does not perform. It **cascades** over the spawn
sub-tree (**#180**): the supervisor walks the child→parent links and closes every
transitive descendant alongside the target, so a spawned sub-agent is never left
orphaned — running with no consumer for its answers and burning provider tokens.
(This is the explicit-destroy path only; a parent `Stop` still does *not* cascade
to un-polled `agent`/`agent_poll` children, ADR-0026.) Session ids are single-use: after
`SessionEnded`, mint a fresh `SessionId::new_uuid()` rather than reuse a closed
id (which would restart `seq` at 0). The supervisor routes to sessions with a
non-blocking `try_send` + bounded retry, shedding to a saturated session rather
than parking its single loop and stalling every other session.

- **Session-multiplexed** like the `agent` reference's `task_id`: one connection
  routes many sessions by `SessionId`.
- **Monotonic `seq`** on content events so a head can dedupe against replayed
  history (`agent`'s pattern); lifecycle frames (`Status`, `AgentChanged`)
  carry no `seq`.
- **`(session, seq)` is unique across every authored content event** (#157). The
  seq comes from **one per-session counter** (`Arc<AtomicU64>`), shared by the
  core session task and the runtime through a supervisor-held registry: a session
  task registers its counter on start / removes it on exit, and a runtime service
  authoring an event for a *parked* session — an approval `ToolRequest`/
  `UserQuestion`, a `Plan`/`TaskList` snapshot, a `FileChange` — mints a **fresh**
  seq from that same counter via `Holly::emit_for_session` instead of reusing the
  parked `ToolExec` seq (the pre-#157 defect that split authorship across crates
  and made a strict `seq > last` dedupe drop every approval prompt). The seq-less
  `Status` transitions the runtime emits around a parked call go through
  `Holly::emit_status`; the raw outbound sender is no longer exposed.
  - **Supervisor lifecycle errors are the one exemption**: an `Error` the
    supervisor emits for an id with **no live session** (a refused resume/spawn of
    a closed/unknown id, a saturated *dead* channel) has no counter to draw from,
    so it carries `seq == 0` — a value core never mints, so it can't collide with
    content — and a head renders it **unconditionally** (the seq-`0` bypass)
    rather than dropping it under a `seq > last` dedupe (ex-#159, the reason
    supervisor-shed errors were invisible in the TUI). A supervisor error for a
    session that *is* still live (e.g. its channel saturated) mints a real seq
    from the live counter and takes its ordered place in that stream.

## 4. Structured outputs (orthogonal to profiles) — [ADR-0004](../adr/0004-structured-plan-and-task-events.md)

Two artifacts the engine owns and re-emits as **full snapshots** on every change
(the `agent`/`design` "snapshot on change" pattern — idempotent, trivial to
render/dedupe):

- **Plan** — markdown strategy prose (`OutEvent::Plan`).
- **TaskList** — markdown task outline, typically a `- [ ]`/`- [x]` checklist
  (`OutEvent::TaskList`). Plain `content` like the plan (✅ #142,
  [ADR-0039](../adr/0039-markdown-task-list.md), supersedes ADR-0004's structured
  `Vec<TaskItem>`): the outline is **user-facing progress info** — the engine
  never consumed the item structure and the list is not fed back to the model,
  so the per-item id/status JSON envelope was pure model overhead.

Both are written by **runtime state tools** the model calls — `update_plan
{ content }` and `update_tasks { content }` (both markdown, ✅ #231,
[ADR-0049](../adr/0049-plan-task-tools-as-runtime-state-tools.md)). They are
**not** engine built-ins: they round-trip via `ToolExec`/`ToolResult` like any
host tool, resolve through the ordinary `Allow`/`Ask`/`Deny` path + #116 mask, and
the runtime executor emits the `OutEvent::Plan`/`OutEvent::TaskList` snapshot after
handling the result (the engine holds no plan/task state). Plan authorship is
default-closed via explicit tool-mask allowlist membership: `update_plan` is
advertised only to a profile that names it (an inherit-all profile never gets it);
`update_tasks` rides the shared specs. A read-only agent can mutate neither (mask
+ permission), which is the #175 fix.

This is why `entanglement` has *both* the opencode agent-profile axis *and* structured
events: profiles control **what the agent is instructed/permitted to do**;
structured events give every head a native plan/task panel to render.

**Usage & cost** (✅ #192, [ADR-0055](../adr/0055-usage-cost-and-stop-reason-surfacing.md)).
The provider normalizes each round-trip's terminal `LlmEvent::Finish` to
`{ stop_reason: StopReason, usage: Usage }` — `StopReason` collapses both wire
vocabularies (`EndTurn | ToolUse | MaxTokens | StopSequence | Other`), and `Usage`
splits the token counts so each maps to one catalog pricing dimension without
double-counting (`input_tokens` is the *uncached* input; the OpenAI client
subtracts its cache reads out of `prompt_tokens`, Anthropic already separates
them). The engine prices the round-trip via `ModelPricing::cost_usd` (effective
model = `profile.model` else `EngineConfig.default_model`, looked up in
`EngineConfig.pricing`), folds it into the session's `SessionUsage` running total,
and emits `OutEvent::Usage` — the **per-round-trip delta**, so a head sums deltas
for its own total. `cost_usd` is `None` when no catalog pricing covers the model.
A `MaxTokens` finish additionally emits a recoverable `OutEvent::Error`
(truncation warning) — the reply still commits, but no longer silently. Because
`cost_usd` is a float, `OutEvent` (and `InMsg`, via `Resume`) are `PartialEq` but
not `Eq`.
