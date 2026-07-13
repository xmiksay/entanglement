# entanglement Architecture — Wire protocol & structured outputs

> Part of the [architecture overview](../architecture.md). The *why* behind each choice is in the [decision log](../adr/README.md).

## 2. Wire protocol (`protocol.rs`) — [ADR-0002](../adr/0002-session-multiplexed-protocol.md)

One set of serde-tagged types crosses every transport:

```
#[serde(tag = "kind", rename_all = "snake_case")]
InMsg    = Prompt{session,text} | Approve{session,request_id,scope?}  // approval →
         | Reject{session,request_id,reason?}                         // runtime, not core (#59)
         //   scope: once (default) | session | always  — persisted grants (#174, ADR-0052)
         | ToolResult{session,request_id,output}   // runtime → core: tool ran (#58)
         | AnswerQuestion{session,request_id,answer}  // ask_user answer → runtime (#90)
         | Stop{session}
         | SetAgent{session,agent}   // switch profile (plan/task state is a runtime tool now, #231)
         | Spawn{session,parent,agent,prompt}   // start a child session (sub-agent) (#60)
         | ListSessions{session}   // supervisor-global query; session = correlation id (#21)
         | CloseSession{session}   // explicit destroy → SessionEnded (#21)
         | Resume{session,records}   // internal, not serialized (#[serde(skip)]); replay log → session (§6b)

OutEvent = SessionStarted{session,parent?,profile,model?,root,ts}   // lifecycle, no seq
         | SessionEnded{session,ts}           // lifecycle, no seq
         | SessionList{session,sessions:[SessionInfo]}   // reply to ListSessions, no seq (#21)
         | Status{session,state}              // point-in-time, no seq
         | AgentChanged{session,agent,profile_detail?}   // point-in-time, no seq; detail = posture (#189)
         | Plan{session,seq,content}          // markdown prose snapshot, runtime-emitted (#231)
         | TextDelta{session,seq,text}
         | ReasoningDelta{session,seq,text}   // reasoning/thinking stream (#54)
         | ToolCall{session,seq,request_id,tool,input}      // display-only, every call (before exec)
         | ToolRequest{session,seq,request_id,tool,input}   // Ask prompt, from runtime (#59)
         | ToolExec{session,seq,request_id,tool,input}      // core → runtime: dispatch it (#58/#59)
         | UserQuestion{session,seq,request_id,question,options,allow_free_form}  // ask_user prompt (#90)
         | ToolOutput{session,seq,request_id,tool,output}
         | TaskList{session,seq,content}      // full outline snapshot (markdown)
         | Error{session,seq,message}
         | Done{session,seq}
         | FileChange{session,seq,path,before?,after?,change_kind}   // file-change audit record (#41)
```

`AnswerQuestion` mirrors `Approve`/`Reject`: the supervisor drops it off the
inbound fan-out (core never routes it) and the `ask_user` executor consumes it
(§8, [ADR-0027](../adr/0027-ask-user-interactive-prompt.md)).

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
(cancel-semantics, ADR-0017) does not perform. Session ids are single-use: after
`SessionEnded`, mint a fresh `SessionId::new_uuid()` rather than reuse a closed
id (which would restart `seq` at 0). The supervisor routes to sessions with a
non-blocking `try_send` + bounded retry, shedding to a saturated session rather
than parking its single loop and stalling every other session.

- **Session-multiplexed** like the `agent` reference's `task_id`: one connection
  routes many sessions by `SessionId`.
- **Monotonic `seq`** on content events so a head can dedupe against replayed
  history (`agent`'s pattern); lifecycle frames (`Status`, `AgentChanged`)
  carry no `seq`.

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
