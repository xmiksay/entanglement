# Wire protocol §2 — Message types, questions & queries

> Part of the [wire protocol module](../protocol.md) of the [architecture overview](../../architecture.md).

## 2. Wire protocol (`protocol.rs` + `protocol/`) — [ADR-0002](../../adr/0002-session-multiplexed-protocol.md)

One set of serde-tagged types crosses every transport:

```
#[serde(tag = "kind", rename_all = "snake_case")]
InMsg    = Prompt{session,content:[ContentPart]} | Approve{session,request_id,scope?,mode?,run_all?}  // approval →
         //   content: [{type:text,text} | {type:image,source:{type:base64,media_type,data}}]; legacy `text:"…"` still deserializes (#197, ADR-0064)
         | Reject{session,request_id,reason?}                         // runtime, not core (#59)
         //   scope: once (default) | session | always  — persisted grants (#174, ADR-0052)
         //   mode: build | auto, meaningful only when approving a propose_plan request (#560) — the approver's mode choice; None (a bare accept) resolves to auto in the runtime orchestrator, never evaluated by core
         //   run_all: bool (default false, omitted), meaningful only on a `rhai` launch card (ADR-0228): true = Run all (the script's nested calls force-allowed this run, explicit denies still win; never part of a grant), false = Ask per command — an older head's frame means Ask per command; the scope still applies (a Session/Always approval grants the script's path); wire-allowed like the rest of Approve
         | ToolResult{session,request_id,content:[ContentPart],is_error=false,duration_ms?,exit_code?}   // runtime → core: tool ran (#58)
         //   content: text, or an image block when `read` opens an image (#221); legacy `output:"…"` still deserializes
         //   is_error/duration_ms (#636, ADR-0176): structured side channel alongside content's still-unchanged text — denied/masked/refused/unknown-tool/errored calls, and an MCP result flagged isError (#727), set is_error; duration_ms is measured once, generically, around the whole host-tool dispatch
         //   exit_code (#681, ADR-0186): the numeric status when the call ran a process to completion (bash/call foreground, or a poll observing a job exit) — None for every other tool, a background launch, or a killed process (the [exit -1] text keeps its sentinel; the field stays honest). Orthogonal to is_error: nonzero exit still executed
         | AnswerQuestion{session,request_id,answers:[[string]],answer?}  // ask_user answer(s) → runtime (#90, #488); answers = one inner vec per question; legacy answer:"…" still deserializes, folds to [[answer]] in seam::Decision::from_inmsg
         | RetractQuestion{session,request_id}   // withdraw an open ask_user question without cancelling the turn (#515, ADR-0146) — the orchestrator still replies (withdrawal note), unlike Stop's silent unwind
         | ReplaceQuestion{session,request_id,questions:[Question]}   // swap an open ask_user question's content in place; re-parks under the same request_id, not terminal (#515, ADR-0146)
         | Stop{session}
         | PauseSession{session}   // hold at Paused — no cancel, no eviction; deferred-until-safe mid-stream (#516, ADR-0208)
         | ResumeSession{session}   // lift a PauseSession hold; continues a drained-but-undriven parked batch with no re-prompt (#516, ADR-0208)
         | SetMode{session,mode}   // trusted-only: switch the session's permission mode → ModeChanged, cascades over the whole live spawn sub-tree; applied IMMEDIATELY, turn live or paused or not — unlike SetModel/SetGeneration/SetToolOverlay, a mode is a label the runtime grades the *next* tool call against, not something a live round is using, so deferring it left a same-turn propose_plan approval executing its first call under the stale mode (#560 fix, ADR-0207 §12). SetAgent is gone (ADR-0207 §9) — an agent is chosen once, at session start or spawn, and is fixed for that session's life
         | SetModel{session,provider,model}   // live model/provider switch, no restart (#218, ADR-0063)
         | SetGeneration{session,overrides:GenerationParams}   // partial generation-knob merge, no restart, always acks; no-override = query (#374/#376, ADR-0094/0095)
         | SetSessionMeta{session,name?,action?,if_unset=false}   // display metadata merge: None leaves a field, Some("") clears; applied IMMEDIATELY, never stashed; always acks with SessionMetaChanged (ADR-0151); if_unset=true applies `name` only when the session has none yet — the session-title generator's guard against clobbering a `/name` or a name restored by resume (#553)
         | SetToolOverlay{session,entries:[ToolOverlayEntry{pattern,allow,deny,arg_pattern?}]}   // replace the session's live tool overlay — enable entries exist past the session's mode grade (graded Ask|Allow, optionally arg_pattern-narrowed, overriding even a mode Deny), deny entries withdraw even mode-allowed tools (#539, ADR-0149; arg_pattern per #611, ADR-0163 — the closed-table lazy built-in registration it added is retired by ADR-0195, bash registering at startup); now mode-scoped — dropped wholesale on a live SetMode (#560, ADR-0207 §8); full replacement, empty clears; trusted-only for an enable entry, but wire-allowed when every entry is deny-only (#634, ADR-0177); a deny entry for `rhai` withdraws the script launch like any tool's (ADR-0228); an entry naming `mcp`/a discovery tool/a script-only read variant (`read_raw`/`*_json`) matches nothing — the runtime warns (#843)
         | Oneshot{session,op,args}   // single out-of-band LLM op outside the turn loop; op="compact" today (#324, ADR-0082)
         | Spawn{session,parent:Option,predecessor:Option,agent,prompt,user?,model?:{provider,model},mode?,workdir?}   // start a session: parent=Some → child sub-agent (#60); parent=None → root, predecessor=Some(source) is the /compact successor (ADR-0110), which the supervisor places where its source was — under the source's parent when the source was a sub-agent — and to which it moves the source's live sub-agents (ADR-0234); a parent with a live successor resolves to it; user = owning user for multi-user deployment (#522, ADR-0147); model = the `ModelChoice` the session starts on (#725) — resolved via `model_resolver` under the effective user before the first round, wins over the agent's pin, logged as the session's first `ModelChanged`; unresolvable (or no resolver) → spawn refused with a supervisor `Error` (seq 0 for an id that never started or whose retired entry was evicted, #891; a retired id's continues its sequence, ADR-0239), never a silent default; mode = the permission mode the session starts in (#763, ADR-0218) — a head's `--mode`/config `mode:`, so a root announces one starting mode instead of a follow-up `SetMode` transition; wins over the agent's `default_mode` for a root, clamped to the parent's mode for a child (`EngineConfig::mode_ladder`), ignored by a compaction successor; a name off a non-empty ladder → spawn refused with an `Error` (seq 0 as above); no sponsored flag any more — ADR-0207 §7 retired the propose_plan sponsored-build-child handoff (ADR-0138) it disambiguated and the field left the wire with it (an old log's `sponsored` is an ignored unknown field on replay); workdir = the working directory it starts in (#764, ADR-0224), validated like SetWorkdir's dir (relative to the parent's workdir for a child) with no approval, a bad one refuses the spawn with an `Error` (seq 0 as above); None inherits — a child its parent's, a compaction successor or fork its source's
         | RevertFiles{session,to_turn?}   // trusted-only, wire-refused: roll back the session (+ spawn sub-tree)'s tracked write/edit/apply_patch edits made after user turn to_turn (1-based; 0 = all); None = undo the latest turn that still has edits. Deferred mid-turn/paused like SetModel; core calls EngineConfig::file_reverter (runtime per-write backups) and answers FilesReverted (#731, ADR-0212)
         | ForkSession{session,at_turn?}   // trusted-only, wire-refused (it mints a session, like Spawn): copy the session's Context verbatim, or only user turns 1..=at_turn, into a fresh root (id minted in the source's lineage, so the tenant prefix is kept) under the same agent/user, inheriting the live model/generation/mode/overlay; the source stays live and untouched. Deferred mid-turn/paused like RevertFiles, so the fork never carries a parked batch and starts idle; no children are cloned; the fork's undo history starts empty (backups are per root). Answered by the fork's SessionStarted (root, no predecessor) + SessionForked (#786, ADR-0223)
         | SetWorkdir{session,dir,approve?}   // trusted-only, wire-refused (approve is an escape-root approval): change the session's working directory — the default cwd for bash/call/rhai exec and the base relative path arguments resolve against. dir is absolute or relative to the current workdir; the supervisor validates it on arrival through EngineConfig::workdir_resolver (canonical, a directory, inside the root or under an exec escape-root grant; approve: session|always records one) and refuses with a supervisor Error carrying a real seq from the session's counter. Deferred mid-turn/paused like SetModel; answered by WorkdirChanged. Existing children keep theirs, later children inherit (#764, ADR-0224)
         | ListSessions{correlation_id}   // supervisor-global query; opaque echo token, not a session (#160, ADR-0072)
         | ListQuestions{correlation_id,session?}   // supervisor-global query; every open ask_user question, or one session's when session is set → QuestionList reply (#515, ADR-0146)
         | ListOperations{correlation_id,session?}   // supervisor-global query; every pending job/script/sub-agent, or one session's when session is set → OperationList reply (#607, ADR-0161 §6); wire-allowed — reads the caller's own outstanding work, mutates nothing
         | ListPending{correlation_id,root?}   // supervisor-global query; every tool call parked awaiting Approve/Reject, or only root's live spawn sub-tree when root is set → PendingList reply (#726, ADR-0217); wire-allowed — read-only
         | McpList{correlation_id}   // supervisor-global query; live MCP servers → McpList reply (#375)
         | McpAdd{name,config:McpServerSpec}   // hot-connect + persist to config.yml → McpChanged (#375); trusted-only, wire-refused (ADR-0124)
         | McpRemove{name}   // hot-disconnect + persist removal → McpChanged (#375); trusted-only, wire-refused (ADR-0124)
         | McpAuth{name,action}   // OAuth Connect|Check|Disconnect for an MCP server → McpAuthChanged (ADR-0153); trusted-only, wire-refused — a forged Connect opens a browser and mints a durable credential
         | ReplayFrom{session,correlation_id,after_seq}   // late-subscriber history fetch → History (#160, ADR-0072)
         | CloseSession{session}   // explicit destroy → SessionEnded, tombstones the id (#21)
         | HibernateSession{session}   // trusted-only: evict memory, NO tombstone → SessionHibernated, resumable (#318, ADR-0077)
         | Resume{session,records}   // internal, not serialized (#[serde(skip)]); replay log → session (§6b)

OutEvent = SessionStarted{session,parent?,predecessor?,agent,model?,root,ts,user?}   // lifecycle, no seq; predecessor = /compact source this session succeeds (ADR-0110); user = owning user in multi-user deployment (#522); no sponsored flag — see Spawn's note
         | SessionEnded{session,ts}           // lifecycle, no seq
         | SessionHibernated{session,ts}      // lifecycle, no seq; memory evicted, id NOT tombstoned (#318, ADR-0077)
         | SessionList{correlation_id,sessions:[SessionInfo]}   // reply to ListSessions, no seq/session (#160, ADR-0072); SessionInfo = {session,parent?,predecessor?,agent,root,user?} — no profile_detail (deleted, ADR-0207: no more permission posture to resolve into it)
         | QuestionList{correlation_id,questions:[PendingQuestion]}   // reply to InMsg::ListQuestions, no seq/session (#515, ADR-0146); PendingQuestion = {session,request_id,questions:[Question]}
         | OperationList{correlation_id,operations:[OperationInfo]}   // reply to InMsg::ListOperations, no seq/session (#607, ADR-0161 §6); OperationInfo = {session,kind:"job"|"agent"|"script",handle,launched_by,elapsed_secs,status:"running"|"complete"}
         | PendingList{correlation_id,approvals:[PendingApproval]}   // reply to InMsg::ListPending, no seq/session (#726, ADR-0217); PendingApproval = {session,request_id,kind:"tool"|"plan"|"mode"|"rhai",tool,input,reason,scopes:[ApprovalScope],since_ms,card_reason?,detached?} — tool/input/scopes/card_reason/detached identical to the ToolRequest that prompted (its reason/scopes); ask_user parks excluded (ListQuestions lists those)
         | McpList{correlation_id,servers:[McpServerStatus]}   // reply to InMsg::McpList, no seq/session (#375); every registered server, connected or not (ADR-0226: registered = available); McpServerStatus.state? retired — serde-defaulted for old logs, always None; McpServerStatus.auth? = OAuth posture (ADR-0153)
         | McpChanged{name,action,error?}   // MCP server hot-added/removed, no seq; reply to McpAdd/McpRemove (#375); `error` set = refused/failed, nothing changed (ADR-0226)
         | McpAuthChanged{status}   // MCP OAuth state change, no seq/session; reply to McpAuth — a Connect emits twice, interim authorize_url then outcome (ADR-0153)
         | Throttle{endpoint,throttled,in_flight,cap,waiters,shared_leases?,retry_in_ms?,pacing_in_ms?}   // LLM endpoint throttle transition, no seq/session — per-endpoint not per-session (#517, ADR-0141); emitted only on enter/exit, not every poll
         | PersistenceDegraded{root,error}   // the persistence tap can't append to root's session log, no seq/session — root is payload, not routing (#767, ADR-0141's model); once per outage, on its first failure
         | PersistenceRecovered{root,lost}   // root's outage ended: its gap tombstone landed recording `lost` records, no seq/session (#767); none if the outage never ends
         | History{correlation_id,session,events:[OutEvent]}   // reply to ReplayFrom; content past the cursor, no seq (#160, ADR-0072)
         | Status{session,state}              // point-in-time, no seq
         | ModeChanged{session,mode}   // point-in-time, no seq; reply to SetMode and, unconditionally, once at session start mirroring AgentChanged — core carries mode as an opaque String and never evaluates it (#560, ADR-0207 §2/§9)
         | WorkdirChanged{session,dir}   // point-in-time, no seq; the validated absolute workdir — reply to SetWorkdir, and at session start/resume whenever the session has one; replay-folded by overwrite. The model learns it from the per-round trailing notice, never Context or the system prompt (#764, ADR-0224)
         | ParentChanged{session,parent}   // point-in-time, no seq; the session was re-linked under a new spawn parent — a live sub-agent moved under its parent's compaction successor instead of dying in the source's close cascade; persisted + replay-folded by overwrite; the only parent change after SessionStarted (#862, ADR-0234)
         | AgentChanged{session,agent}   // point-in-time, no seq; identity/provenance only now — the profile_detail posture field (#189) is deleted (ADR-0207): ModeChanged carries posture instead
         | ModelChanged{session,provider,model,context_window?}   // point-in-time, no seq; reply to SetModel, or a session-start model pin (#218, ADR-0063; #323, ADR-0081, narrowed by ADR-0207 §9 — the pin now applies only at session start, since SetAgent is gone)
         | GenerationChanged{session,generation:GenerationParams}   // point-in-time, no seq; full effective params, reply to SetGeneration (incl. "/show") or a session-start generation overlay (#374/#376, ADR-0094/0095, narrowed by ADR-0207 §9 the same way as ModelChanged's pin)
         | SessionMetaChanged{session,name?,action?}   // point-in-time, no seq; full merged display metadata, reply to SetSessionMeta; persisted + replay-folded by overwrite, head-folded — not mirrored into SessionInfo (ADR-0151)
         | ToolOverlayChanged{session,entries:[ToolOverlayEntry]}   // point-in-time, no seq; full effective overlay, reply to SetToolOverlay; persisted + replay-folded by overwrite (#539, ADR-0149)
         | Plan{session,seq,content,path}          // markdown prose snapshot, runtime-emitted (#231)
         | TextDelta{session,seq,text}
         | ReasoningDelta{session,seq,text}   // reasoning/thinking stream (#54)
         | ToolCallDelta{session,seq,request_id,tool,delta}   // streamed tool-arg fragment; display-only, before the assembled ToolCall (#194)
         | ToolCall{session,seq,request_id,tool,input,provider_meta?,envelope?}      // display-only, every call (before exec); provider_meta = the call's opaque provider metadata (e.g. a Gemini thought signature) so replay rebuilds the signed call; envelope = the emitted mcp call when core unwrapped one (ADR-0226; old logs may carry invoke, ADR-0204)
         | ToolRequest{session,seq,request_id,tool,input,reason?,scopes?,detached?}   // Ask prompt, from runtime (#59); reason = CardReason "root_escape"|"floored_write"|"script_launch"|"inline_launch"|"launch_escape", absent on an ordinary card; scopes = the ApprovalScopes an Approve honors (starts with once), omitted only by a pre-ADR-0230 producer (offer once alone) — heads decide on these, never on input text (ADR-0230); detached = true on a background rhai script's nested card, omitted when false: it outlives the turn's terminal Status, so keep showing it until answered or a Stop unwinds it (ADR-0237)
         | ApprovalResolved{session,seq,request_id,outcome}   // a ToolRequest card left the runtime's PendingDecisions: outcome = approved | rejected (from any head) | expired (question_timeout) | stopped (Stop, its session's end/hibernate for a turn card, a superseding re-offer, any other frame aimed at its id). Emitted on the card's home under the registry lock, always after its ToolRequest; persisted, so a rebuilt view drops it too; ask_user questions never get one (ADR-0240)
         | ToolExec{session,seq,request_id,tool,input,agent,envelope?,workdir?}   // core → runtime: dispatch it (#58/#59); agent = active profile name for authoritative gating (#156); envelope as on ToolCall (ADR-0204); workdir = the session's absolute workdir, absent at the root (#764, ADR-0224)
         | UserQuestion{session,seq,request_id,questions:[Question]}  // ask_user prompt(s), one call → one event (#90, #488); questions flattens onto the wire (Questions newtype); legacy question/options/allow_free_form still deserializes into a one-element vec
         | ToolOutput{session,seq,request_id,tool,output,content?:[ContentPart],is_error=false,duration_ms?,exit_code?,envelope?}   // output = display text; content carries any result output can't rebuild exactly — an image (#221), a tool reference, several parts — for byte-identical replay (ADR-0202); is_error/duration_ms mirror InMsg::ToolResult's structured side channel (#636, ADR-0176) — a head can style a failed call without parsing output's text; exit_code (#681, ADR-0186) mirrors the same field — a wire consumer can branch on a bash/call/polled-job exit status without parsing [exit N]
         | TaskList{session,seq,content}      // full outline snapshot (markdown)
         | Usage{session,seq,input_tokens,output_tokens,cached_input_tokens,cache_write_tokens,cost_usd?,purpose}  // per-round-trip usage + cost (#192); purpose = turn | compaction (default turn)
         | Error{session,seq,message}
         | Done{session,seq}
         | Compacted{session,seq,summary,kept,auto,mode}   // this session was compacted into a successor and is now retired; summary doubles as the successor's seed. auto:false (default) = manual /compact, auto:true = the overflow guard; mode: summary (default) | prune — prune means no LLM summary was available and `summary` carries the pruned transcript. Never a mutation of this session: every path forks (#324, ADR-0082 → ADR-0101/0103 → ADR-0205). The successor's id arrives on its own SessionStarted{predecessor}, not here
         | FileChange{session,seq,path,change_kind,hash}   // file-change audit: runtime executor emits on edit/write/apply_patch; hash = sha256(after) (#202, ADR-0060, #455)
         | PlanChanged{session,seq,path,hash}   // a propose_plan-bound file changed on disk out of band (not through this session's own edit/write/apply_patch); live counterpart to propose_plan's staleness guard, off a dedicated debounced plans-folder watch (#627, ADR-0173). No core replay-fold semantics, like SkillActive
         | FilesReverted{session,seq,to_turn,restored?:[path],skipped?:[{path,reason}]}   // outcome of RevertFiles, emitted by the session itself; a file changed since the agent's edit is skipped, never clobbered. Replay folds it into the same user-role note the live session appended to Context (only when restored is non-empty) and never re-runs the revert (#731, ADR-0212)
         | SessionForked{session,seq,forked_from,at_turn?,context:[Message]}   // emitted once by a ForkSession's fork, before anything else it logs: the verbatim history it starts from. Replay puts it ahead of the fork's own history, so the fork's log is self-contained and replays exactly; a head renders the inherited transcript from it. Distinct from SessionStarted.predecessor, whose source is retired (#786, ADR-0223)
         | SkillActive{session,seq,skill_id?,allowed_tools?}   // wire-facing posture only: the active skill. Core neither interprets nor enforces it. Mirrors FileChange: a fresh per-session seq (#157), no core replay-fold semantics (a head just tracks the latest value). (#400, ADR-0106; the mask is gone — ADR-0194 removed skill-scoped allowed_tools enforcement, so allowed_tools is vestigial: still serialized when present for replay compat, never read or enforced)
         | AmbiguousRetry{session,seq,nudge}   // ambiguous LLM stop → bounded in-place retry: persisted boundary so replay reconstructs the partial round + nudge, not one merged assistant message (ADR-0118)
         | SearchResult{session,seq,part}   // persisted provider-side web-search block (ContentPart::ProviderSearch); replay folds it into the assistant Message's content like TextDelta (#481, ADR-0131)
         | ReasoningBlock{session,seq,part}   // persisted extended-thinking block (ContentPart::Reasoning); same replay fold as SearchResult. The *persistence* rail for reasoning — ReasoningDelta above is the *display* rail and is never folded into Context; both fire for the same thinking. Capture is unconditional; whether the block is sent back is ModelEntry::replay_thinking (ADR-0160)
         | NestedCall{session,parent_request_id,index,tool,args?,duration_ms,is_error?}   // LIVE ONLY, no seq: one binding call of a running rhai script, emitted by the runtime's binding bridge (index 1-based per run; args a bounded, credential-free summary). First 50 calls of a run only; never persisted (OutEvent::is_live_only, the persistence tap skips it), replayed or folded into Context (#821, ADR-0233)
         | NestedCallSummary{session,parent_request_id,suppressed,errors}   // LIVE ONLY, no seq: once at a script's end, only when calls past the 50-call cap were held back — `suppressed` of them, `errors` failed (#821, ADR-0233)
```

**Live-only events — a script's nested calls** ([ADR-0233](../../adr/0233-live-only-nested-call-events-for-rhai-scripts.md), #821).
`NestedCall`/`NestedCallSummary` carry `session` (so every head routes them
like any session event) but no `seq`, and `OutEvent::is_live_only()` is true
for exactly these two: the persistence tap skips them, so they never reach a
session log, `ReplayFrom`'s `History` or `Context`. They are derived display
data — the outer `rhai` `ToolExec`/`ToolOutput` stays the content record — and
the runtime emits them through `Holly::emit_live`, which refuses any other
variant. `parent_request_id` (not `request_id`) names the `rhai` call, so no
head pairs a nested report with an output of its own. A script's first 50
binding calls are each reported; the rest are only counted, into one
`NestedCallSummary` when the script ends — blocking and `background: true`
runs alike.

**`envelope` — an unwrapped `mcp` call** ([ADR-0226](../../adr/0226-one-advertised-surface-mcp-envelope-registered-is-available.md)).
`ToolEnvelope{tool,input}` is the call exactly as the model emitted it: the
outer name (`mcp`) and the raw argument string. It is present on
`ToolCall`/`ToolExec`/`ToolOutput` only when the round advertised the
`mcp {server, tool, args}` spec and core unwrapped the call. `tool`/`input`
then name the inner `mcp__<server>__<tool>` call (non-`[A-Za-z0-9_-]`
characters mapped to `_`, the registered spelling) and its `args`, so heads,
gates, hooks and approvals handle an ordinary tool call and never parse the
envelope. It exists so the model-facing history stays unchanged: `Context`
keeps the emitted call, and replay rebuilds it from `envelope`. A call that is
not unwrapped (malformed, a reserved inner name, or `mcp` not advertised)
keeps its emitted name and has no `envelope`. The type is name-agnostic, so
logs carrying ADR-0204's retired `invoke` envelopes still deserialize and
replay. `skip_serializing_if` omits the field when `None`, so ordinary calls
serialize byte for byte as before.

`AnswerQuestion` mirrors `Approve`/`Reject`: the supervisor drops it off the
inbound fan-out (core never routes it) and the `ask_user` executor consumes it
(§8, [ADR-0027](../../adr/0027-ask-user-interactive-prompt.md)). `Question`
(`{question, options:[QuestionOption], multi_select}`) supersedes the v1
`question`/`options`/`allow_free_form` triple (#488,
[ADR-0127](../../adr/0127-ask-user-v2-multi-question-envelope.md)): `ask_user` can
batch several questions into one call, a typed "Other" answer is unconditional
(no flag to opt into it), and `multi_select` lets a question accept more than
one picked option — `answers` carries one inner vec per question, in call
order.

**List/retract/replace an open question** (#515,
[ADR-0146](../../adr/0146-ask-user-list-retract-replace.md)). `RetractQuestion`/
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

`ListOperations{correlation_id,session?}` (#607, ADR-0161 §6) mirrors
`ListQuestions` exactly — same session-less-query shape, same optional-filter
convention, same wire-allowed rationale (a read of the caller's own
outstanding work, mutating nothing) — but is answered by
`entanglement_runtime::operations::list_operations`, which merges a snapshot
of the job registry (`JobRegistry`, owner-scoped, #605), the background-script
registry (`ScriptRegistry`, owner-scoped, #637/ADR-0185) and the sub-agent
registry (`AgentRegistry`, parent-scoped, #618) rather than one dedicated map.
It is `poll`'s head-facing counterpart: called with no `handle`, `poll` itself
answers the same question for the model (see
[gates & host tools](../gates-and-host-tools.md)), sharing this exact function
rather than duplicating the join logic.

`ListPending{correlation_id,root?}` (#726, [ADR-0217](../../adr/0217-list-pending-query-for-parked-approvals.md))
is the approval counterpart: same session-less-query shape and wire-allowed
read-only rationale, answered by `entanglement_runtime::pending::list_pending`
from the `PendingDecisions` registry every approval parks in. `root` (not
`session`) scopes to a whole live spawn sub-tree, walked over `AgentRegistry`
with the same sub-tree walk the approval parking uses. Every approval path (permission
`Ask`, `propose_plan`, `request_mode`, a rhai binding's `Ask`, a rhai launch
card) parks through `pending::park_approval`, which registers the entry and emits the
`ToolRequest` from one `PendingApproval` value — so a reconnecting head gets
the exact prompt it missed. The decision router handles `ListPending` in
order with `Approve`/`Reject`, so a listing after a decision never shows the
decided call; a timed-out park removes its entry, and one whose task was
aborted is skipped. After a compaction a background script's card moves
to the successor and is listed under it ([ADR-0238](../../adr/0238-compaction-moves-a-background-scripts-card.md)).
Whatever removes a parked card — a decision from any head, a timeout, an
unwind, a superseding re-offer — emits `ApprovalResolved` on the card's home
([ADR-0240](../../adr/0240-approval-resolved-event.md)), so a head drops it by
`request_id` instead of guessing from `Status` or polling `ListPending`. It is
emitted under the registry lock the card's `ToolRequest` was, so it never
precedes the card, and exactly once per removal. A head folds it past its seq
watermark only (a stale end must not drop a newer card reusing the id;
a seq-0 end, from a session with no sequence, is dropped by id
without moving the watermark) and
drops the id along the home's compaction predecessors too, the scope a moved
card uses. A consumer built against an older `OutEvent` cannot parse the new
`kind`: decode the tag first and skip unknown kinds.
`kind: "rhai"` marks both a rhai binding's card (tool
`<name> (rhai)`) and the rhai **launch card** — told apart by `tool == "rhai"`
exactly; only the launch card is answered with `Approve.run_all`
([ADR-0228](../../adr/0228-rhai-path-and-args-skill-tools-frontmatter-removed.md)). Ownership filtering is the head's job, as with
`ListSessions` (a multi-tenant head must scope `root` and route the reply to
its requesting connection only, [embedding](../../embedding/tenants-and-heads.md#1-one-engine-many-tenants) §1); the reply
names no session, so it is never persisted.

**Card reason and scopes** ([ADR-0230](../../adr/0230-structured-card-reason-and-honest-scopes-on-tool-request.md)):
`scopes` lists exactly what an `Approve` on the card honors, so a head offers
those and nothing else. It is computed by `pending::scopes_for(kind, tool, card)`
and is `[once]` for `propose_plan`/`request_mode` (the approver picks a mode),
a **floored write** (it always asks, so no grant is recorded) and an **inline**
rhai launch (no path to key a grant on, likewise a `path` launch whose
script could not be read). An `agent_send` card gets once/session only (its
grant is keyed on a handle minted for this run, [ADR-0235](../../adr/0235-scoped-spawn-rules-grade-the-target.md)). Every other card gets once/session/always, plus
`session_dir` for the read triad — but not on a `root_escape` card, whose
grant is keyed on the exact path. The runtime **enforces** the list:
`PendingDecisions::resolve` clamps an `Approve` whose scope the card did not
offer to `once` and logs a warning, so a wire client cannot earn a wider
grant (for example a durable escape-root grant) than the card advertised.
`reason` (`CardReason`) says why a card is not an ordinary graded `Ask`:

- `root_escape`: an out-of-root host/MCP call (ADR-0109);
- `floored_write`: the write floor carded it. This outranks `root_escape`, and
  the card text keeps the escape warning;
- `script_launch` / `inline_launch` / `launch_escape`: a rhai launch card for
  a `path` inside the root, an inline script, or a `path` outside the root.

A head with nobody to ask starts only `script_launch`/`inline_launch`
(`pending::headless_starts`) and refuses every other card.

---

Next: [§2 — Trust split, session lifecycle & single-shot ops](sessions.md)
