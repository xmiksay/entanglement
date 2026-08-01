# 0146. `ask_user` gains list/retract/replace of open questions

- Status: Accepted
- Date: 2026-08-01
- Amends: [ADR-0027](0027-ask-user-interactive-prompt.md) (the original
  `ask_user`/`UserQuestion` round-trip), [ADR-0127](0127-ask-user-v2-multi-question-envelope.md)
  (#488's multi-question envelope)

## Context

[#515](https://github.com/xmiksay/entanglement/issues/515): the `ask_user`/
`UserQuestion` flow is write-once and fire-and-forget, with no way to inspect
or revise an open question:

- **No list.** `PendingDecisions` (`entanglement-runtime/src/pending.rs`) is a
  bare `Mutex<HashMap<(SessionId, String), oneshot::Sender<Decision>>>` —
  engine-internal, not queryable over the wire, and it carries no question
  *content* even if it were. There is no `InMsg` that enumerates open
  questions, unlike `ListSessions`/`McpList`.
- **No edit.** Once `OutEvent::UserQuestion` is emitted, the only ways to
  clear the waiter are the matching `AnswerQuestion` or a session-scoped
  `Stop`, which cancels the *whole turn* — collateral damage for a head that
  just wants to correct or withdraw one question.

A head must currently track open questions locally from the `OutEvent`s it
happened to receive — there is no authoritative source of truth it can query,
which matters most for a reconnecting WS head or a multi-pane UI.

## Decision

**Add four wire-allowed variants and one runtime-owned registry**, all scoped
to the `ask_user` flow specifically (not the generic `PendingDecisions` map,
which stays as-is for permission `Ask`/`propose_plan`/`rhai` bindings).

### `InMsg::ListQuestions { correlation_id, session: Option<SessionId> }` → `OutEvent::QuestionList`

Mirrors `ListSessions`/`McpList`: a session-less snapshot query
(`InMsg::session()` returns `None` for it — the optional `session` field is a
result *filter*, not a routing target, exactly like `ReplayFrom`'s query
carries a session yet is answered off-core). `None` lists every open question
across every session (the common case); `Some(id)` narrows to one session's.
Answered by a new runtime-owned `OpenQuestions` registry
(`entanglement-runtime/src/questions.rs`), not core — `PendingDecisions` alone
can't answer this query since a bare `oneshot::Sender` carries no question
text.

### `InMsg::RetractQuestion { session, request_id }`

Withdraws a specific open question *without* cancelling the rest of the turn.
The proposal in the issue considered relying on `PendingDecisions::register`'s
existing side effect (a second `register` for the same key drops the first
waiter, resolving it to `Decision::Stop`) — but that resolves to *silence*: the
`ask_user` orchestrator's `Stop` arm never replies, on the theory that core is
about to cancel the whole turn anyway so no `ToolResult` is owed. A *targeted*
retract has no such cover: the turn keeps running, so the model's tool call
must still resolve or the turn hangs forever waiting on a `ToolResult` that
never comes. This is why retract is a **first-class decision**
(`seam::Decision::Retract`), not a re-registration: it resolves the *same*
waiter the orchestrator is already parked on, and the orchestrator replies
with an explicit withdrawal note ("The user withdrew this question without
answering.") instead of unwinding silently.

### `InMsg::ReplaceQuestion { session, request_id, questions }`

Swaps a parked question's content in place. Unlike retract, this **is not
terminal** — `run_ask_user` is restructured from a single park-then-reply into
a loop: a `Decision::Replace { questions }` updates the loop's local
`questions` and continues around, re-emitting `OutEvent::UserQuestion` under
the **same** `request_id` and re-registering a fresh waiter, without ever
replying to the model. The original tool call stays open across any number of
replaces, resolved exactly once by the eventual `Answer` or `Retract`.

### `OpenQuestions` — a second, content-bearing registry

`entanglement-runtime/src/questions.rs` adds `OpenQuestions`
(`Mutex<HashMap<(SessionId, String), Vec<Question>>>`, cloneable `Arc`
wrapper, mirroring `PendingDecisions`'s shape). `run_ask_user` inserts before
every emit (mirroring the `pending.register`-before-emit discipline that
closes the #156 lag race) and removes on every terminal outcome — `Answer`,
`Retract`, or `Stop`. It is deliberately **not** folded into
`PendingDecisions` itself: that map is generic across four different parked
round-trips (permission `Ask`, `ask_user`, `propose_plan`, `rhai` bindings)
and stores only a `oneshot::Sender<Decision>`; teaching it to also carry
`ask_user`-specific question text would leak a leaf concern into a shared
primitive for no benefit to the other three consumers.

### Wire trust: all four are wire-allowed

`ListQuestions` is read-only, exactly like `ListSessions`/`McpList`.
`RetractQuestion`/`ReplaceQuestion` withdraw or revise a question the *same*
head already saw surfaced — no more privileged than answering it, which
(`AnswerQuestion`) is already wire-allowed. The `serve` head's per-connection
approval-ownership gate (#402, ADR-0107) is extended to cover both alongside
`AnswerQuestion`: all three resolve the same class of parked `ask_user`
waiter, so a non-owning WS connection must not be able to retract/replace one
either.

### Routing: resolved by the runtime, never a session task

`RetractQuestion`/`ReplaceQuestion` carry a mandatory `session` but are
core-oblivious exactly like `AnswerQuestion` — added to the supervisor's
top-of-loop bypass list (`Holly`'s inbound loop) and to `msg_to_cmd`'s
never-routed set, so they reach the runtime's single inbound router (the same
one that already resolves `Approve`/`Reject`/`AnswerQuestion` via
`seam::Decision::from_inmsg`) instead of a session task, which has no notion
of `ask_user` at all. `ListQuestions` needs no such bypass — its `session()`
is already `None`, so it falls through the generic "no session ⇒ answered
off-core" path the MCP ops already use.

## Consequences

- **(+)** A reconnecting WS head or a multi-pane UI has an authoritative
  source of truth for open questions, closing the gap `OutEvent` replay alone
  can't (a `UserQuestion` missed under `broadcast` lag leaves no trace
  elsewhere for the head to reconstruct from).
- **(+)** A head can correct a question it phrased poorly, or narrow its
  options, without burning the model's whole turn via `Stop`.
- **(+)** `run_ask_user`'s loop restructuring is the only behavior change to
  the existing `Answer` path — the terminal `Answer`/`Stop` arms are
  byte-identical to before, just moved inside a `loop`.
- **(−)** A second Mutex-guarded registry (`OpenQuestions`) alongside
  `PendingDecisions`, kept in sync by hand (`run_ask_user` must insert/remove
  at the same points it registers/resolves `pending`) rather than derived from
  one source of truth — accepted because `PendingDecisions` is intentionally
  generic and content-free; teaching it to carry `ask_user` question text
  would be a worse coupling than a second small registry.
- **(−)** `seam::Decision` gains two variants (`Retract`, `Replace`), which
  every existing exhaustive match over it (`propose_plan.rs`, `tool_runner.rs`'s
  permission-ask arm, `script.rs`'s `rhai` approval) must now cover — each
  folds them into its existing "not a valid decision for this route, unwind"
  arm, so no route's behavior actually changes, just its match's arm count.

## Alternatives considered

- **Formalize only the re-register side effect** (the issue's own suggested
  alternative for retract: a second `pending.register` for the same key
  already drops the prior waiter to `Decision::Stop`). Rejected: that
  resolves to *silence* — sound for `Stop` because core is about to cancel
  the whole turn regardless, unsound for a targeted retract where the turn
  keeps running and the tool call's `ToolResult` is still owed. A first-class
  `Retract` decision that the orchestrator replies to is the only way to keep
  the round-trip contract (#58) intact.
- **`ReplaceQuestion` mints a new `request_id`** instead of reusing the
  original. Rejected: the model's tool call already has one `id` (the
  `ToolCall.id` core is parked on) — a new `request_id` would need a second
  `ToolExec`/`ToolResult` round-trip core has no reason to run, since nothing
  about the *tool call* changed, only the *question* the runtime is asking
  the human on its behalf. Reusing `request_id` keeps the whole replace
  invisible to core.
- **Fold `OpenQuestions` into `PendingDecisions`** (store `(Vec<Question>,
  oneshot::Sender<Decision>)` instead of a bare sender). Rejected: three of
  `PendingDecisions`'s four consumers (permission `Ask`, `propose_plan`,
  `rhai` bindings) have no `Vec<Question>` to store, so every non-`ask_user`
  call site would carry a meaningless empty vec — worse coupling than a
  second small registry scoped to the one consumer that needs it.

## References

- Issue #515: `UserQuestion` is write-once — no list/retract/replace
- [ADR-0027](0027-ask-user-interactive-prompt.md): the original `ask_user`
  round-trip this extends
- [ADR-0127](0127-ask-user-v2-multi-question-envelope.md): #488's
  multi-question `Questions` envelope, reused verbatim for `ReplaceQuestion`'s
  payload
- [ADR-0072](0072-protocol-warts-settled-before-serve.md): the
  `correlation_id` query/reply pattern `ListQuestions`/`QuestionList` follows
- [ADR-0107](0107-ws-per-connection-approval-ownership.md): the per-connection
  ownership gate `RetractQuestion`/`ReplaceQuestion` join alongside
  `AnswerQuestion`
- Refs #512 (the related TUI draft-until-submit follow-up, #518/ADR-0143,
  tracked separately)
