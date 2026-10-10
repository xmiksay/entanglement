# 0217. `ListPending`: a correlation-id query for parked approvals

- Status: Accepted
- Date: 2026-10-10
- Relates to: [ADR-0146](0146-ask-user-list-retract-replace.md) (the
  `ListQuestions` pattern this mirrors),
  [ADR-0072](0072-protocol-warts-settled-before-serve.md) (correlation-id
  queries), [ADR-0069](0069-trusted-untrusted-wire-frame-split.md) (wire
  allowlist)

## Context

A head that reconnects (a browser tab reloaded against an embedder) cannot ask
which tool calls are parked awaiting `Approve`/`Reject`. A `ToolRequest` it
missed leaves no trace outside the session logs, so embedders rebuilt the set
by scanning their own log store (#726). `ask_user` questions already have
`ListQuestions` (ADR-0146) and background work has `ListOperations`
(ADR-0161 §6); approvals were the remaining gap.

## Decision

1. **`InMsg::ListPending { correlation_id, root? }` →
   `OutEvent::PendingList { correlation_id, approvals: [PendingApproval] }`**,
   the same session-less, `seq`-less snapshot shape as `ListQuestions`.
   `root: Some` limits the reply to that session's live spawn sub-tree
   (itself included); `None` lists every session. `root`, not `session`,
   because the unit a head renders is one conversation, children included.
2. **`PendingApproval = {session, request_id, kind, tool, input, reason,
   scopes, since_ms}`.** `kind` is `tool | plan | mode | rhai` (which path
   parked it); `tool`/`input` are byte-identical to the `ToolRequest` that
   prompted (no redaction: the head already received that text once);
   `reason` is a human string (``mode `build` grades `bash` Ask``, an
   out-of-root access, …); `scopes` lists the `ApprovalScope`s an `Approve`
   on it honors (`Once` only for `plan`/`mode`; `SessionDir` only for the
   read triad); `since_ms` is the park time, Unix epoch ms. `ask_user` parks
   are excluded: they take `AnswerQuestion` and `ListQuestions` lists them.
3. **Answered by the runtime**, from `PendingDecisions` — the registry every
   approval already parks in — on the executor's single inbound decision
   router. Every approval path now parks through one helper
   (`pending::park_approval`) that builds the registry entry and the
   `ToolRequest` from the same value, so the listing can't drift from the
   prompt. Because `ListPending` and `Approve`/`Reject` are handled in order
   on that one loop, a listing sent after a decision never shows the decided
   call. The sub-tree walk is the one `explore(kind: "pending")` already uses
   (the runtime `AgentRegistry` parent links).
4. **Wire-allowed**, like `ListQuestions`: a read-only snapshot of prompts a
   head may already answer over the wire. Ownership filtering is the head's
   job, as for `ListSessions`; `serve` stays single-user (ADR-0048). The
   reply carries every listed call's full `input`, so a multi-tenant head
   must force `root` to a session it owns (or filter `approvals` by
   ownership) and deliver the reply only to the connection that sent its
   `correlation_id` ([embedding](../embedding.md) §1).
5. **Not persisted.** The reply names no session, so the persistence layer
   skips it; old logs are unaffected.

A timed-out park used to drop its receiver but leave its entry in the
registry forever; in an unattended mode expiry is the normal path, so the map
grew until `Stop`. Now the timeout branch removes the entry
(`PendingDecisions::await_timed`), and any entry whose waiter is gone for
another reason (an aborted task) is skipped by both this listing and
`explore(kind: "pending")`, so neither reports a call nobody can still
approve.

## Consequences

- Embedders drop their log-scan reconstruction and ask the engine.
- Adding a new approval path means parking through `park_approval`, or it
  won't be listed.

Rejected: a `session` filter only (the head would issue one query per child
it has to discover first); adding scope options to `ToolRequest` itself (a
change to a persisted, replayed event for a need only the snapshot has).
