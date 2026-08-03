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
         | AnswerQuestion{session,request_id,answers:[[string]],answer?}  // ask_user answer(s) → runtime (#90, #488); answers = one inner vec per question; legacy answer:"…" still deserializes, folds to [[answer]] in seam::Decision::from_inmsg
         | RetractQuestion{session,request_id}   // withdraw an open ask_user question without cancelling the turn (#515, ADR-0146) — the orchestrator still replies (withdrawal note), unlike Stop's silent unwind
         | ReplaceQuestion{session,request_id,questions:[Question]}   // swap an open ask_user question's content in place; re-parks under the same request_id, not terminal (#515, ADR-0146)
         | Stop{session}
         | PauseSession{session}   // hold at Paused — no cancel, no eviction; deferred-until-safe mid-stream (#516, ADR-0144)
         | ResumeSession{session}   // lift a PauseSession hold; continues a drained-but-undriven parked batch with no re-prompt (#516, ADR-0144)
         | SetAgent{session,agent}   // switch profile; may be followed by ModelChanged/Error if the profile pins a model (#323, ADR-0081)
         | SetModel{session,provider,model}   // live model/provider switch, no restart (#218, ADR-0063)
         | SetGeneration{session,overrides:GenerationParams}   // partial generation-knob merge, no restart, always acks; no-override = query (#374/#376, ADR-0094/0095)
         | SetSessionMeta{session,name?,action?,if_unset=false}   // display metadata merge: None leaves a field, Some("") clears; applied IMMEDIATELY, never stashed; always acks with SessionMetaChanged (ADR-0151); if_unset=true applies `name` only when the session has none yet — the session-title generator's guard against clobbering a `/name` or a name restored by resume (#553)
         | SetToolOverlay{session,entries:[ToolOverlayEntry{pattern,allow,deny}]}   // replace the session's live tool overlay — enable entries exist past the agent mask (graded Ask|Allow), deny entries withdraw even profile-advertised tools (#539, ADR-0149); full replacement, empty clears; trusted-only, wire-refused
         | Oneshot{session,op,args}   // single out-of-band LLM op outside the turn loop; op="compact" today (#324, ADR-0082)
         | Spawn{session,parent:Option,predecessor:Option,agent,prompt,user?}   // start a session: parent=Some → child sub-agent (#60); parent=None → root, predecessor=Some(source) is the /compact successor (ADR-0110); user = owning user for multi-user deployment (#522, ADR-0147)
         | ListSessions{correlation_id}   // supervisor-global query; opaque echo token, not a session (#160, ADR-0072)
         | ListQuestions{correlation_id,session?}   // supervisor-global query; every open ask_user question, or one session's when session is set → QuestionList reply (#515, ADR-0146)
         | McpList{correlation_id}   // supervisor-global query; live MCP servers → McpList reply (#375)
         | McpAdd{name,config:McpServerSpec}   // hot-connect + persist to config.yml → McpChanged (#375); trusted-only, wire-refused (ADR-0124)
         | McpRemove{name}   // hot-disconnect + persist removal → McpChanged (#375); trusted-only, wire-refused (ADR-0124)
         | McpAuth{name,action}   // OAuth Connect|Check|Disconnect for an MCP server → McpAuthChanged (ADR-0153); trusted-only, wire-refused — a forged Connect opens a browser and mints a durable credential
         | BashEnable{grade:BashGrade}   // hot-register bash, graded Ask|Allow{pattern?} → BashChanged (#498, ADR-0133); trusted-only, wire-refused
         | BashDisable   // hot-unregister bash → BashChanged (#498, ADR-0133); trusted-only, wire-refused
         | ReplayFrom{session,correlation_id,after_seq}   // late-subscriber history fetch → History (#160, ADR-0072)
         | CloseSession{session}   // explicit destroy → SessionEnded, tombstones the id (#21)
         | HibernateSession{session}   // trusted-only: evict memory, NO tombstone → SessionHibernated, resumable (#318, ADR-0077)
         | Resume{session,records}   // internal, not serialized (#[serde(skip)]); replay log → session (§6b)

OutEvent = SessionStarted{session,parent?,predecessor?,profile,model?,root,ts,user?}   // lifecycle, no seq; predecessor = /compact source this session succeeds (ADR-0110); user = owning user in multi-user deployment (#522)
         | SessionEnded{session,ts}           // lifecycle, no seq
         | SessionHibernated{session,ts}      // lifecycle, no seq; memory evicted, id NOT tombstoned (#318, ADR-0077)
         | SessionList{correlation_id,sessions:[SessionInfo]}   // reply to ListSessions, no seq/session (#160, ADR-0072); SessionInfo = {session,parent?,profile,root,profile_detail?,user?}
         | QuestionList{correlation_id,questions:[PendingQuestion]}   // reply to InMsg::ListQuestions, no seq/session (#515, ADR-0146); PendingQuestion = {session,request_id,questions:[Question]}
         | McpList{correlation_id,servers:[McpServerStatus]}   // reply to InMsg::McpList, no seq/session (#375); McpServerStatus.state?: "enabled"|"allowed" + available-unconnected entries (#542, ADR-0152); McpServerStatus.auth? = OAuth posture (ADR-0153)
         | McpChanged{name,action}   // MCP server hot-added/removed, no seq; reply to McpAdd/McpRemove (#375)
         | McpAuthChanged{status}   // MCP OAuth state change, no seq/session; reply to McpAuth — a Connect emits twice, interim authorize_url then outcome (ADR-0153)
         | BashChanged{enabled,grade?}   // bash live-registered/unregistered, no seq; reply to BashEnable/BashDisable (#498, ADR-0133)
         | Throttle{endpoint,throttled,in_flight,cap,waiters,shared_leases?,retry_in_ms?,pacing_in_ms?}   // LLM endpoint throttle transition, no seq/session — per-endpoint not per-session (#517, ADR-0141); emitted only on enter/exit, not every poll
         | History{correlation_id,session,events:[OutEvent]}   // reply to ReplayFrom; content past the cursor, no seq (#160, ADR-0072)
         | Status{session,state}              // point-in-time, no seq
         | AgentChanged{session,agent,profile_detail?}   // point-in-time, no seq; detail = posture (#189)
         | ModelChanged{session,provider,model,context_window?}   // point-in-time, no seq; reply to SetModel, or a SetAgent model pin (#218, ADR-0063; #323, ADR-0081)
         | GenerationChanged{session,generation:GenerationParams}   // point-in-time, no seq; full effective params, reply to SetGeneration (incl. "/show") or a SetAgent generation overlay (#374/#376, ADR-0094/0095)
         | SessionMetaChanged{session,name?,action?}   // point-in-time, no seq; full merged display metadata, reply to SetSessionMeta; persisted + replay-folded by overwrite, head-folded — not mirrored into SessionInfo (ADR-0151)
         | ToolOverlayChanged{session,entries:[ToolOverlayEntry]}   // point-in-time, no seq; full effective overlay, reply to SetToolOverlay; persisted + replay-folded by overwrite (#539, ADR-0149)
         | Plan{session,seq,content,path}          // markdown prose snapshot, runtime-emitted (#231)
         | TextDelta{session,seq,text}
         | ReasoningDelta{session,seq,text}   // reasoning/thinking stream (#54)
         | ToolCallDelta{session,seq,request_id,tool,delta}   // streamed tool-arg fragment; display-only, before the assembled ToolCall (#194)
         | ToolCall{session,seq,request_id,tool,input}      // display-only, every call (before exec)
         | ToolRequest{session,seq,request_id,tool,input}   // Ask prompt, from runtime (#59)
         | ToolExec{session,seq,request_id,tool,input,agent}   // core → runtime: dispatch it (#58/#59); agent = active profile name for authoritative gating (#156)
         | UserQuestion{session,seq,request_id,questions:[Question]}  // ask_user prompt(s), one call → one event (#90, #488); questions flattens onto the wire (Questions newtype); legacy question/options/allow_free_form still deserializes into a one-element vec
         | ToolOutput{session,seq,request_id,tool,output,content?:[ContentPart]}   // output = display text; content carries an image result for faithful replay (#221)
         | TaskList{session,seq,content}      // full outline snapshot (markdown)
         | Usage{session,seq,input_tokens,output_tokens,cached_input_tokens,cache_write_tokens,cost_usd?}  // per-round-trip usage + cost (#192)
         | Error{session,seq,message}
         | Done{session,seq}
         | Compacted{session,seq,summary,kept,auto}   // compaction summary ready; auto:false (default) → source untouched, head forks into a new session (#324, ADR-0082 → ADR-0101); auto:true → in-place mutation the live engine already applied (#398, ADR-0103)
         | FileChange{session,seq,path,change_kind,hash}   // file-change audit: runtime executor emits on edit/write/apply_patch; hash = sha256(after) (#202, ADR-0060, #455)
         | SkillActive{session,seq,skill_id?,allowed_tools?}   // wire-facing posture only: the active skill's scope (tool mask); core neither interprets nor enforces it. Mirrors FileChange: a fresh per-session seq (#157), no core replay-fold semantics (a head just tracks the latest value). (#400, ADR-0106)
         | AmbiguousRetry{session,seq,nudge}   // ambiguous LLM stop → bounded in-place retry: persisted boundary so replay reconstructs the partial round + nudge, not one merged assistant message (ADR-0118)
         | SearchResult{session,seq,part}   // persisted provider-side web-search block (ContentPart::ProviderSearch); replay folds it into the assistant Message's content like TextDelta (#481, ADR-0131)
         | ReasoningBlock{session,seq,part}   // persisted extended-thinking block (ContentPart::Reasoning); same replay fold as SearchResult. The *persistence* rail for reasoning — ReasoningDelta above is the *display* rail and is never folded into Context; both fire for the same thinking. Capture is unconditional; whether the block is sent back is ModelEntry::replay_thinking (ADR-0160)
```

`AnswerQuestion` mirrors `Approve`/`Reject`: the supervisor drops it off the
inbound fan-out (core never routes it) and the `ask_user` executor consumes it
(§8, [ADR-0027](../adr/0027-ask-user-interactive-prompt.md)). `Question`
(`{question, options:[QuestionOption], multi_select}`) supersedes the v1
`question`/`options`/`allow_free_form` triple (#488,
[ADR-0127](../adr/0127-ask-user-v2-multi-question-envelope.md)): `ask_user` can
batch several questions into one call, a typed "Other" answer is unconditional
(no flag to opt into it), and `multi_select` lets a question accept more than
one picked option — `answers` carries one inner vec per question, in call
order.

**List/retract/replace an open question** (#515,
[ADR-0146](../adr/0146-ask-user-list-retract-replace.md)). `RetractQuestion`/
`ReplaceQuestion` join `AnswerQuestion` at the same supervisor bypass and the
same runtime consumer (`seam::Decision::from_inmsg` maps all three off the
inbound fan-out) — both carry a mandatory `session` but are core-oblivious
exactly like `AnswerQuestion`, never routed to a session task. Retract
withdraws the parked question *without* cancelling the rest of the turn: the
`ask_user` orchestrator still owes the model a `ToolResult`, so it replies
with an explicit withdrawal note instead of `Stop`'s silent unwind (sound
there only because the whole turn is being cancelled anyway). Replace is
**not terminal** — `run_ask_user` loops: a replace re-emits `UserQuestion`
with the revised `questions` under the *same* `request_id` and re-parks,
resolved only by the eventual answer or retract, so core never sees a second
round-trip for what is, from its perspective, one unchanged tool call.
`ListQuestions{correlation_id,session?}` mirrors `ListSessions`/`McpList`: a
session-less snapshot query (`session()` is `None` for it — the optional
`session` field is a result *filter*, not a routing target) answered by a
runtime-owned `OpenQuestions` registry (`entanglement-runtime/src/questions.rs`)
that `ask_user` keeps in sync with `PendingDecisions`, since the generic
`PendingDecisions` map carries no question content to answer the query with.
All four variants are wire-allowed: list is read-only, and retract/replace are
no more privileged than answering, which is already wire-allowed — the `serve`
head's per-connection approval-ownership gate (#402, ADR-0107) covers all
three decision variants alike.

**Trusted/untrusted frame split** (#155, [ADR-0069](../adr/0069-trusted-untrusted-wire-frame-split.md)).
`InMsg` has two entry points. `Holly::send` is **privileged in-process**: an
embedder holding a `Holly` (a head, the runtime tool executor) authors any
frame. `Holly::send_from_wire` is the **untrusted** path a wire head (stdio
`pipe`, WebSocket `serve`) calls after deserializing a line — it enforces the
`InMsg::wire_allowed()` allowlist and refuses (`WireError::Privileged`, not
routed) the trusted-only variants: `ToolResult` (a forged one resolves
a parked turn on `request_id` alone, bypassing execution *and* permission),
`Spawn` (bypasses the tool path's `spawn_refusal` gate, #119), `Resume`
(internal, `#[serde(skip)]`), `HibernateSession` (an embedder memory-eviction
control — a wire head must not evict another session's in-memory state, #318),
`McpAdd`/`McpRemove` (#472,
[ADR-0124](../adr/0124-wire-refused-mcp-mutation-and-stdio-key-scrub.md),
reversing #375's wire tier: an unapproved `McpAdd` spawns an arbitrary local
subprocess, and with the `serve` origin gate opt-in-off a hostile web page
could drive it cross-origin — the read-only `McpList` stays wire-allowed, and
the TUI `/mcp` path is unaffected since it sends over the privileged
`Holly::send`), `McpAuth`
([ADR-0153](../adr/0153-mcp-server-oauth.md), sharpening ADR-0124: a forged
`Connect` opens a browser and mints a durable credential, a forged
`Disconnect` destroys one, and even `Check` mutates state by refreshing — so
unlike the read-only `McpList`, none of the three actions is wire-allowed),
and `BashEnable`/`BashDisable`
([ADR-0133](../adr/0133-live-bash-enablement-graded-by-permission.md), #498,
same rationale as `McpAdd`/`McpRemove`: a blanket-`Allow` live-enable hands the
model a full shell with no approval prompt, so a wire frame must never grant
it — the TUI `/bash` command likewise sends over `Holly::send`), and
`SetToolOverlay` (#539,
[ADR-0149](../adr/0149-per-session-tool-overlay.md), the `BashEnable`
rationale again: it injects tools past the agent mask, optionally graded
`allow` with no approval prompt — the TUI `/enable`/`/disable` commands send
over `Holly::send`). `wire_allowed`
is an explicit exhaustive allowlist `match`
(ADR-0124), so a new variant is wire-refused until deliberately opted in — a
compile error to skip, mirroring `session()`/`variant_name()`. The executor
folds a completed tool round-trip back
over the named privileged handle `Holly::submit_tool_result` (used by
`seam::reply_content`, the single fold-back site). Under the local single-user
`serve` scope ([ADR-0048](../adr/0048-serve-head-local-trust-model.md)) this is
robustness/UX — which cooperating local client owns a frame — not defence against
a remote attacker; the WS head routes every inbound frame through
`send_from_wire` and implements per-connection `Approve` ownership (#402,
[ADR-0107](../adr/0107-ws-per-connection-approval-ownership.md)).

**Session lifecycle** (✅ #21, [ADR-0028](../adr/0028-session-lifecycle-enumeration-and-backpressure.md)).
`ListSessions` and `CloseSession` are **supervisor-global**: the supervisor
answers/acts on them directly rather than routing to a session task.
`ListSessions` returns one `SessionList` snapshot of the live
`SessionInfo{session,parent?,profile,root,profile_detail?,user?}` set — a reconnecting
head enumerates in one round-trip instead of folding the whole broadcast. Both
the query and the reply carry an opaque **`correlation_id`** the head mints and
the reply echoes — not an overloaded `SessionId` (#160, [ADR-0072](../adr/0072-protocol-warts-settled-before-serve.md)),
so `InMsg::session()`/`OutEvent::session()` return `Option<&SessionId>` and are
`None` for these session-less queries (a head's event router drops a `None`
rather than keying a phantom per-session view). `profile_detail`
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
to un-polled `agent { background: true }` children, ADR-0026 — collected via `poll`, #605.) Session ids are single-use: after
`SessionEnded`, mint a fresh id — `SessionId::new_uuid()` (kept name for
call-site compatibility; it mints the ADR-0164 `s-<hex>` scheme, not an actual
UUID) or, when a `Holly` handle is in scope, `Holly::next_id(IdKind::Session)`
to go through the engine's *configured* [`IdGen`](../adr/0164-short-sortable-kind-tagged-ids.md)
— rather than reuse a closed id (which would restart `seq` at 0). The supervisor routes to sessions with a
non-blocking `try_send` + bounded retry, shedding to a saturated session rather
than parking its single loop and stalling every other session.

**Session hibernation** (#318, [ADR-0077](../adr/0077-session-hibernation-evictable-resumable.md))
is a **third lifecycle state** between `live` and the terminal `closed`
tombstone. `HibernateSession{session}` (trusted-only — an embedder memory-eviction
control, not wire-allowed; `Holly::hibernate` is the wrapper) tears the session
task + its spawn sub-tree down (the same cascade `CloseSession` uses) and drops
each `Context`, but records **no** tombstone in the `closed` set — the map entry is
removed (memory released, gone from `ListSessions`) yet the id stays **resumable**:
a later `Holly::resume(id, records)` rebuilds it from the embedder's event log
exactly like the restart path, re-offering a turn parked mid-approval
([ADR-0061](../adr/0061-parked-turn-state-batch-tool-resolution.md)/[ADR-0071](../adr/0071-parked-turn-reoffer-timer.md)).
The task emits a distinct lifecycle `SessionHibernated{session,ts}` (no `seq`) so
heads/persistence taps tell eviction from termination; the runtime executor
releases its per-session bookkeeping on it as on `SessionEnded`. Hibernating a
turn **parked on approval** is safe (re-offer); a turn **mid-stream** is
*stop-then-hibernate* — the supervisor's command-sender drop cancels the round
(ADR-0017 cancel semantics), and its uncommitted text-only tail is discarded
exactly as `Session::replay` drops such a tail, so resume is lossless w.r.t. the
log. `closed` ids stay terminal (`resume` still refuses them); the embedder is
expected to `resume` before re-prompting a hibernated id. Core snapshots nothing —
rebuild is the embedder's log replay, keeping the no-DB-in-core boundary intact.
An **optional idle-TTL sweep** now drives `HibernateSession` automatically
(#363, [ADR-0090](../adr/0090-idle-ttl-auto-hibernation.md)): `EngineConfig.idle_ttl`
(`None` by default — eviction stays embedder-driven when unset) arms a
supervisor-level poll that auto-hibernates a **settled** root (and its whole
spawn sub-tree) once idle past the TTL — see the engine doc for the mechanism.

**Session pause** (#516, [ADR-0144](../adr/0144-pause-resume-a-hold-between-cancel-and-hibernate.md))
is a hold `Stop` and `HibernateSession` don't cover: `Stop` destroys the
in-flight round, `HibernateSession` evicts memory — neither is "hold this
session's next piece of work without losing it or evicting it."
`PauseSession{session}`/`ResumeSession{session}` (both wire-allowed, same
trust tier as `Stop`) drive a `Session.paused: bool` that is **not**
persisted/replayed (like `Stop`'s cancel — a hibernate/resume cycle always
comes back unpaused). Two holds depending on what the session was doing when
paused: an **idle** session defers its next `Prompt`/`SetAgent`/`SetModel`/
`SetGeneration`/`Oneshot` onto the existing turn-stash queue; a **parked**
batch keeps folding arriving `ToolResult`s into `Context` as normal (stashing
them would deadlock — the stash only drains once the turn goes idle, which
needs every pending result resolved first) but does not re-enter `drive_turn`
once the batch drains, so the same round resumes with no new prompt once
`ResumeSession` arrives. A session **mid-stream** when paused is unaffected
until the round reaches its next safe point (turn end or park) — `Pause`/
`Unpause` are ordinary `SessionCmd`s, so a mid-stream arrival rides the exact
generic stash-and-replay mechanism `SetAgent`/`SetModel` already use
(`session/stream.rs` needed no change). `Stop`/`HibernateSession` always take
priority over a pause and neither clears it: a `Stop`'d-but-still-paused
session reports `AgentState::Paused`, not `Done`, until an explicit
`ResumeSession`. A new `AgentState::Paused` (ADR-0139's dedicated-state
precedent) is never observed while genuinely streaming, for the reason above.

**Late-subscriber history fetch** (#160, [ADR-0072](../adr/0072-protocol-warts-settled-before-serve.md)).
A head that connected after a turn started asks
`ReplayFrom{session,correlation_id,after_seq}` for the events it missed. Because
the event log is the **runtime's** persistence seam (core holds no log), this is
answered *out-of-core*: a runtime history responder (spawned beside the
persistence subscriber, `history.rs`) reads it off the inbound fan-out — like the
supervisor answers `ListSessions`, just runtime-side — and broadcasts one
`History{correlation_id,session,events}` snapshot of every persisted content event
whose `seq` exceeds `after_seq` (via the seq-less `Holly::emit_history`, keeping
the raw sender closed). The query and reply are transient — neither is persisted
nor folded on replay. Delivery is a `correlation_id`-matched broadcast; sending
the reply to only the requesting socket is the WS `serve` head's concern (#153).

- **Session-multiplexed** like the `agent` reference's `task_id`: one connection
  routes many sessions by `SessionId`.
- **Monotonic `seq`** on content events so a head can dedupe against replayed
  history (`agent`'s pattern); lifecycle/query frames (`Status`, `AgentChanged`,
  `SessionList`, `History`, …) carry no `seq`. `OutEvent::seq()` returns
  `Option<u64>` — `None` for those — so the real seq-`0` sentinel below is a
  distinct `Some(0)`, not confused with "no seq" (#160, [ADR-0072](../adr/0072-protocol-warts-settled-before-serve.md)).
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

**Single-shot session ops — `InMsg::Oneshot`** (#324, [ADR-0082](../adr/0082-single-shot-session-ops-and-persisted-compaction.md)).
A generic **wire envelope** — `{session, op: String, args: Value}` — for a single
out-of-band LLM call outside the turn loop, not a plugin registry: the
genericity lives in the wire shape, so a future op needs no new `InMsg`
variant/`wire_allowed`/`SessionCmd`, just a new `match` arm in
`session::ops::run_oneshot`. `"compact"` (session compaction via LLM
summarization) is the first and only op today; an unknown `op` is a
recoverable `Error`. Wire-allowed (mutates only the caller's own session) and
deferred while a turn is live via the same stash gate as `SetAgent`/`SetModel`
— a oneshot never runs concurrently with a turn, which is what lets it reuse
the session's `&mut Llm` handle directly instead of racing the turn loop's
inbox `select!`. On success it emits the **persisted, seq-bearing**
`OutEvent::Compacted{session,seq,summary,kept,auto}` — persistence and
`ReplayFrom` history cover it for free (both are variant-agnostic over any
`seq()`-bearing event). **Copy-on-write (ADR-0101), forking a *successor*
(ADR-0110):** the source session's `Context` is **never mutated** — the summary
rides only in the event, and the head forks it into a **new root** session via
`InMsg::Spawn` (`parent = None`, `predecessor = Some(source)`, agent = source
profile, prompt = summary), then **closes the source** with `InMsg::CloseSession`
so its interactive session is retired (the user moves forward into the compacted
successor; the source's log is preserved). A truncated summary
(`StopReason::MaxTokens`) is refused outright (`Error`, never forked). The
successor is a *root*, not a child of the source, precisely so closing the source
doesn't cascade onto it. `Session::replay`'s `Compacted` fold is a **no-op** for
`auto: false` — a resumed *source* would recover its full pre-compaction history,
but the head now closes the source (closed ids are single-use), so that undo is
no longer reachable interactively (ADR-0110 amends ADR-0101's implicit undo); the
history survives only as the persisted log. `kept` (#397, ADR-0102) is how many
trailing messages ride verbatim inside `summary` rather than being paraphrased
— clamped to the nearest safe turn boundary by `Context::safe_kept`; `0` (the
default) means the whole history was summarized with no verbatim tail,
matching every pre-#397 record.

`auto` (#398, [ADR-0103](../adr/0103-auto-summarize-on-context-overflow.md))
tells the two mutation semantics sharing this variant apart: `false` (the
default, every pre-#398 record) is the copy-on-write report above; `true` is
`session/turn.rs`'s automatic in-place compaction on context overflow — a turn
mid-flight has no head to fork into, so it mutates the live `Context` via
`Context::apply_compaction` directly instead. `Session::replay` folds `auto:
true` by replaying that same `apply_compaction` call so a resumed session's
history matches the live one, rather than treating it as a no-op like the
manual path.

**Live generation-parameter changes — `InMsg::SetGeneration`** (#374,
[ADR-0094](../adr/0094-reasoning-effort-and-per-profile-generation-persistence.md)).
`SetGeneration{session,overrides:GenerationParams}` merges a **partial**
`GenerationParams` (temperature / max-output / thinking-budget / the new
`reasoning_effort`) onto the session's current one via
`GenerationParams::apply_overrides` — a `None` field in `overrides` leaves the
corresponding field untouched, so `/set temperature 0.7` only touches
`temperature`. Unlike `SetModel` there is no resolver to fail against (a pure
local merge, no network/catalog lookup), so it always succeeds and **always**
emits `OutEvent::GenerationChanged{session,generation}` with the full merged
result — even when nothing actually changed — so a head can rely on the reply
alone to confirm the write landed. The merged result is also recorded into
`Session.profile_generation` keyed by the active profile (the
generation-parameter analogue of `Session.profile_models`, #323/ADR-0081), so a
later `SetAgent` switch back to that profile re-applies it. Deferred (stashed)
while a turn is live, like `SetAgent`/`SetModel`. See the engine doc for the
`SetAgent`/session-start overlay precedence and the runtime doc for the
per-profile persisted store.

**Settable session display metadata — `InMsg::SetSessionMeta`**
([ADR-0151](../adr/0151-settable-session-metadata.md)).
`SetSessionMeta{session,name?,action?,if_unset=false}` merges a
human-readable display `name` (a session title, e.g. derived from the first
prompt) and/or the current `action` ("what the agent is doing now") onto
`Session.name`/`Session.action` — a `None` field leaves the stored value
untouched, `Some("")` clears it. Pure metadata: nothing in the engine reads
it. Unlike `SetGeneration` it is applied **immediately, never stashed** (the
`ChildSpawned` pattern) — `action`'s whole purpose is to change mid-turn, so
a set while the turn is parked on tool calls acks right away; a mid-*stream*
arrival still rides the generic stash like every command, since the session
task is single-threaded. Always acks with
`OutEvent::SessionMetaChanged{session,name?,action?}` carrying the **full
merged** values (both fields, not just the touched ones) — persisted and
replay-folded by overwrite (last write wins), like `GenerationChanged`.
Wire-allowed (cosmetic, session-scoped, no privilege). Heads fold it into
their session views (the `AgentChanged` pattern); it is deliberately **not**
mirrored into `SessionInfo`/`SessionList`, whose supervisor directory records
creation-time facts only. The TUI's `/name <text>` sets `name` for the active
session (the sidebar title updating is the confirmation) and prefers `name`
over the short id and `action` over the first-prompt snippet in the sidebar
and sessions modal; `skutter sessions` and the resume modal recover the name
from the log's last `SessionMetaChanged` record. `action` has no in-tree
producer yet — the intended consumer is an external namer/status writer
sending the wire message. `if_unset` (#553) is the auto session-title
generator's guard: it always sets it `true`, so the fold applies `name` only
when `Session.name` is still `None` — a late generated title silently no-ops
(still acking, with the unchanged state) against a name set by `/name` (in
either arrival order) or restored on `Resume` from a prior process's log; the
generator itself also folds a `Resume`'s replayed `SessionMetaChanged`
history off the inbound fan-out to seed its per-process "already titled" set,
so an already-named resumed session skips the aux call on its next prompt too
(see [heads & persistence §6d](heads-and-persistence.md)).

**Live tool overlay — `InMsg::SetToolOverlay`** (#539,
[ADR-0149](../adr/0149-per-session-tool-overlay.md)).
`SetToolOverlay{session,entries:[ToolOverlayEntry{pattern,allow,deny}]}`
**replaces** the session's live tool overlay: `*`/`?` patterns (the ADR-0148
mask semantics) that override the active profile's
`tools:`/`disallowed_tools:` mask in both directions — an enable entry makes
matching tools *exist* regardless of the mask (`mcp__chessbase__*` for a
server, a literal name for one tool), a `deny: true` entry *withdraws* them
even when the profile advertises them (deny > enable > profile,
`ToolOverlayEntry::disposition`). Full replacement, not a merge (an empty
list clears); like
`SetGeneration` there is nothing to fail against, so it always succeeds and
always emits `OutEvent::ToolOverlayChanged` with the full effective list —
which is also what persistence logs and `Session::replay` folds back (by
overwrite), so a resumed session keeps its overlay. Session-scoped by design:
it survives `SetAgent` (overriding the profile is its point) and dies with
the session. `allow: false` (default) grades matching calls `Ask`; `allow:
true` grades them `Allow` — the grade replaces the profile chain's resolution
on the runtime's generic dispatch route, still clamped by the config
permission ceiling (a deny entry has no grade; it removes the tool ahead of
any permission decision). Mask disposition is per ancestor-chain link, so a
parent's overlay also covers its spawn sub-tree. Trusted-only (wire-refused,
the `BashEnable` rationale); the TUI drives it via `/enable`/`/disable`, the
bare-`/enable` session-tools checklist dialog (the overlay as a diff against
the profile mask), and the `/mcp` panel's `e`/`d` server keys.
Stash-deferred while a turn is live, like `SetAgent`/`SetModel`.

## 4. Structured outputs (orthogonal to profiles) — [ADR-0004](../adr/0004-structured-plan-and-task-events.md)

Two artifacts the engine owns and re-emits as **full snapshots** on every change
(the `agent`/`design` "snapshot on change" pattern — idempotent, trivial to
render/dedupe):

- **Plan** — markdown strategy prose (`OutEvent::Plan { content, path }`, `path`
  added #513/[ADR-0145](../adr/0145-one-plan-tool-file-backed-plans-and-blocking-review-loop.md) —
  the plan is a **file** now, `.entanglement/plans/<short-id>.md` by default;
  `#[serde(default)]` on `path` keeps a pre-#513 persisted log replayable).
- **TaskList** — markdown task outline, typically a `- [ ]`/`- [x]` checklist
  (`OutEvent::TaskList`). Plain `content` like the plan (✅ #142,
  [ADR-0039](../adr/0039-markdown-task-list.md), supersedes ADR-0004's structured
  `Vec<TaskItem>`): the outline is **user-facing progress info** — the engine
  never consumed the item structure and the list is not fed back to the model,
  so the per-item id/status JSON envelope was pure model overhead.

Both are written by **runtime state/orchestration tools** the model calls —
`propose_plan(content: Option<String>, path: Option<String>)` (✅ #141/#513,
exactly one of the two — file-backed, not an in-memory snapshot; see the
engine doc's "Plan acceptance" section) and `update_tasks { content }`
(markdown, ✅ #231,
[ADR-0049](../adr/0049-plan-task-tools-as-runtime-state-tools.md)). Neither is
an engine built-in: `update_tasks` round-trips via `ToolExec`/`ToolResult`
like any host tool, resolving through the ordinary `Allow`/`Ask`/`Deny` path +
#116 mask, and the runtime executor emits its `OutEvent::TaskList` snapshot
after handling the result (the engine holds no task state) — `propose_plan`
additionally force-parks on `Ask` unconditionally (see the engine doc), since
its `OutEvent::Plan` snapshot is only one part of a larger approval +
sponsored-build-child flow. Plan authorship is default-closed via explicit
tool-mask allowlist membership: `propose_plan` is advertised only to a
profile that names it (an inherit-all profile never gets it); `update_tasks`
rides the shared specs. A read-only agent can mutate neither (mask +
permission), which is the #175 fix.

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
