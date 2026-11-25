# 0107. WS per-connection approval ownership — session-scoped, first-writer-wins

- Status: Accepted
- Date: 2026-07-17

## Context

[ADR-0069](0069-trusted-untrusted-wire-frame-split.md) split the inbox into a
privileged in-process path (`Holly::send`) and an untrusted wire path
(`Holly::send_from_wire`), gating the runtime-authored trio
(`ToolResult`/`Spawn`/`Resume`) so a wire head deserializing attacker-adjacent
bytes can't forge them. Its Consequences section named the residual gap
explicitly: "per-connection **session ownership** for `Approve`
(first-writer-wins among cooperating local clients) — a connection-scoped
concern with no inbox today." `serve` (#153) landed
`send_from_wire`-routing but deferred that residual to this issue (#402).

[ADR-0048](0048-serve-head-local-trust-model.md) frames the whole `serve`
head as **local, single-user, robustness/UX** — the browser-page attack
surface is explicitly out of scope, and #153's concerns are "cooperating
local clients (e.g. the TUI and the browser on one session) must not cross
wires," not defence against a hostile client. This ADR's decision inherits
that framing: the goal is that two cooperating local WS clients (two browser
tabs, a tab and a raw script) don't race to resolve the same parked approval,
not to authenticate who is allowed to approve what.

The concrete gap before this change: any connected WS connection could send
`Approve`/`Reject`/`AnswerQuestion` for **any** session's parked
`ToolRequest`/`UserQuestion`, including one it never initiated and has no
context for — a second tab opened on the same `serve` instance could resolve
a decision the first tab's user was about to make, with no ownership check at
all (`send_from_wire` only ever checked *frame kind* against the
ADR-0069 allowlist, never *which connection* sent it).

There is no connection-identity concept anywhere in `entanglement-core` or
`entanglement-runtime` prior to this change — it has to be invented in
`serve.rs`, since request-emission call sites (`tool_runner.rs`/
`ask_user.rs`/`propose_plan.rs`) have zero wire-connection context and adding
it would be a core protocol change out of scope here.

## Decision

**Session-scoped, first-writer-wins ownership, tracked entirely inside
`entanglement-runtime::serve`:**

- Each WS connection is minted a process-lifetime `ConnId` (`u64`, an
  `AtomicU64` counter on `ServeState`) when `handle_socket` starts.
- A new `SessionOwners` (`Mutex<HashMap<SessionId, ConnId>>` on `ServeState`)
  tracks, per session, which connection owns it. `SessionOwners::touch(session,
  conn)` claims the session for `conn` if unowned (`HashMap::entry().or_insert`),
  else reports whether `conn` is the existing owner — **the first connection to
  send any frame referencing a session claims it**, typically the initiating
  `Prompt`.
- `touch` runs for **every** inbound frame that carries a `SessionId`
  (`InMsg::session()`), so ownership is claimed as early as possible — but the
  **gate** (refusing the frame) only fires for the three decision variants:
  `Approve`/`Reject`/`AnswerQuestion`. Every other variant
  (`Prompt`/`Stop`/`SetAgent`/etc.) passes through to `send_from_wire`
  regardless of `touch`'s result, so ownership never blocks anything but a
  decision.
- A gated frame from a non-owning connection is **refused**: logged
  (`tracing::warn!`) and dropped — the connection is not closed, mirroring
  exactly how ADR-0069's `WireError::Privileged` refusal already treats a
  forged `ToolResult`. No new `OutEvent` is emitted (see Consequences).
- **Release-on-disconnect**: when `handle_socket`'s inbound loop ends for any
  reason (clean close, error, client disconnect), `SessionOwners::release(conn)`
  drops every session that connection owned. A still-parked
  `ToolRequest`/`UserQuestion` becomes claimable by whichever connection next
  sends a session-scoped frame for it — e.g. another connected client's
  `Approve`.

This is entirely orthogonal to `PendingDecisions`
(`entanglement-runtime::pending`) and the parked-turn/reoffer machinery
([ADR-0061](0061-parked-turn-state-batch-tool-resolution.md),
[ADR-0071](0071-parked-turn-reoffer-timer.md)): both keep working exactly as
before. Ownership only decides *which connection's* `Approve`/`Reject`/
`AnswerQuestion` frame is allowed to reach `send_from_wire` in the first
place; once a frame passes the gate, resolution proceeds through the existing
executor path unchanged.

## Consequences

- **Positive.** Two cooperating local clients no longer race to answer the
  same parked approval — the connection that started the conversation (or
  otherwise first referenced the session) is the one whose decision wins,
  matching the UX a single-user TUI-equivalent would have.
- **Positive.** The refusal is silent server-side-logged only, exactly
  mirroring the existing forged-`ToolResult` precedent — no new wire surface,
  no new `OutEvent` variant, no protocol change. A refused frame's connection
  is otherwise fully functional (proven by the integration test: the
  non-owning connection keeps receiving broadcast events and can still be
  used).
- **Negative / accepted.** No "owned by another client" hint is sent back to
  the refused connection today — a user driving a raw script or a second tab
  gets no feedback that their `Approve` was silently dropped, only server
  logs. A future issue can add a targeted local-only hint event if this proves
  to matter in practice; not adding it now avoids the complexity of a second
  per-connection channel merged into the outbound pump's `select!` (an
  `Option<mpsc::Receiver>`/sender-drop edge case) for what the originating
  issue itself phrased as optional.
- **Negative / accepted.** Release-on-disconnect means a still-parked approval
  is claimable by a different connection the moment the owner disconnects —
  intentional (it prevents a permanent deadlock if the owning tab/client goes
  away mid-approval), but it opens a small race window if a disconnect
  coincides exactly with another client's frame for the same session.
  Acceptable per ADR-0048's "robustness among cooperating local clients," not
  a security boundary a hostile client could reliably exploit for gain (it
  would just get to answer a question it can already observe on the shared
  broadcast).
- **Neutral.** Ownership state lives entirely in `entanglement-runtime::serve`
  (`ServeState`), scoped to one `serve` process's lifetime — it is not
  persisted, not replayed, and carries no core-protocol change. A restarted
  `serve` process starts with no ownership recorded, which is correct: no
  connections exist yet to have claimed anything.

## Alternatives considered

- **Request-id-scoped ownership**, captured at the moment a `ToolRequest`/
  `UserQuestion` is emitted (`tool_runner.rs`/`ask_user.rs`/`propose_plan.rs`).
  Rejected: those call sites run inside the runtime's tool executor, with zero
  visibility into which WS connection (if any) is even attached — threading a
  connection identity that deep would mean either a new core-protocol field or
  a parallel out-of-band registration path, both larger changes than the
  problem warrants. Session-scoped ownership, claimed at the `serve` layer
  alone, needs no core change at all.
- **A targeted "owned by another client" hint `OutEvent`** delivered only to
  the refused connection over a side channel. Rejected for now: it requires a
  second per-connection `mpsc` merged into the outbound pump's `tokio::select!`
  (handling the sender-drop/no-receiver-yet edge case cleanly), meaningful
  complexity for a UX nicety the originating issue phrased as optional. Can be
  added later without touching the ownership mechanism itself.
- **Keep a session parked-forever until its owning connection reconnects**
  (no release-on-disconnect). Rejected: risks a permanent deadlock if the
  owning tab/client never comes back — strictly worse than the small race
  window release-on-disconnect accepts, and inconsistent with `serve`'s
  robustness-over-strictness framing (ADR-0048).
