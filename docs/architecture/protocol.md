# entanglement Architecture — Wire protocol & structured outputs

> Part of the [architecture overview](../architecture.md). The *why* behind each choice is in the [decision log](../adr/README.md).

## 2. Wire protocol (`protocol.rs`) — [ADR-0002](../adr/0002-session-multiplexed-protocol.md)

One set of serde-tagged types crosses every transport:

```
#[serde(tag = "kind", rename_all = "snake_case")]
InMsg    = Prompt{session,text} | Approve{session,request_id}   // approval →
         | Reject{session,request_id,reason?}                   // runtime, not core (#59)
         | ToolResult{session,request_id,output}   // runtime → core: tool ran (#58)
         | AnswerQuestion{session,request_id,answer}  // ask_user answer → runtime (#90)
         | Stop{session}
         | SetTasks{session,content} | SetPlan{session,content} | SetAgent{session,agent}
         | Spawn{session,parent,agent,prompt}   // start a child session (sub-agent) (#60)
         | ListSessions{session}   // supervisor-global query; session = correlation id (#21)
         | CloseSession{session}   // explicit destroy → SessionEnded (#21)
         | Resume{session,records}   // internal, not serialized (#[serde(skip)]); replay log → session (§6b)

OutEvent = SessionStarted{session,parent?,profile,model?,root,ts}   // lifecycle, no seq
         | SessionEnded{session,ts}           // lifecycle, no seq
         | SessionList{session,sessions:[SessionInfo]}   // reply to ListSessions, no seq (#21)
         | Status{session,state}              // point-in-time, no seq
         | AgentChanged{session,agent}        // point-in-time, no seq
         | Plan{session,seq,content}          // markdown prose snapshot
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
`SessionInfo{session,parent?,profile,root}` set — a reconnecting head enumerates
in one round-trip instead of folding the whole broadcast; its `session` field is
a correlation id the reply echoes. `CloseSession` drops the session's command
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
  [ADR-0040](../adr/0039-markdown-task-list.md), supersedes ADR-0004's structured
  `Vec<TaskItem>`): the outline is **user-facing progress info** — the engine
  never consumed the item structure and the list is not fed back to the model,
  so the per-item id/status JSON envelope was pure model overhead.

Both are written two ways:
1. A **built-in engine tool** the model calls — `update_plan { content }`
   and `update_tasks { content }` (both markdown). These bypass permissions
   (they only mutate session state) and never need approval. `update_plan` is
   authority-gated: advertised and accepted only under a profile that `owns_plan`
   (default-closed, ✅ #140, [ADR-0041](../adr/0041-update-plan-ownership-default-closed.md));
   `update_tasks` is unconditional.
2. A **harness message** — `InMsg::SetPlan` / `InMsg::SetTasks` (user edits).

This is why `entanglement` has *both* the opencode agent-profile axis *and* structured
events: profiles control **what the agent is instructed/permitted to do**;
structured events give every head a native plan/task panel to render.
