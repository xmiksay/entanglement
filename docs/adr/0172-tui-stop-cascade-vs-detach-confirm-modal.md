# 0172. TUI stop cascade-vs-detach confirm modal

- Status: Accepted
- Date: 2026-08-06
- Amends: [0145](0145-one-plan-tool-file-backed-plans-and-blocking-review-loop.md)
  "Consequences" (closes the "No TUI stop-cascade-vs-detach modal" item).
  Issue [#626](https://github.com/xmiksay/entanglement/issues/626) (orig.
  [#513](https://github.com/xmiksay/entanglement/issues/513), tracked by the
  #396 ledger epic via [#624](https://github.com/xmiksay/entanglement/issues/624)).

## Context

[0145](0145-one-plan-tool-file-backed-plans-and-blocking-review-loop.md)
shipped the backend half of this choice: a `Stop` targeting the plan session
alone detaches (the sponsored `propose_plan` build child keeps running,
untouched — nothing new, just what an unregistered future being dropped
already does); a head that wants the child stopped too sends it a second,
ordinary `Stop`. Two integration tests
(`stop_on_the_plan_session_detaches_the_build_child_which_keeps_running`,
`stop_on_both_sessions_cascades_and_stops_the_build_child_too`) pinned both
paths. What 0145 explicitly deferred was the *interactive* half: a TUI prompt
letting the user pick, instead of a `Stop` on the plan session always
detaching by default.

0145's own "Consequences" named the prerequisite this was blocked on: the TUI
had no general "interrupt the in-flight turn" keybinding at all outside the
approval/`ask_user`-parked `Esc` paths. That prerequisite shipped separately
(#6): bare `Esc` in Normal mode now sends `InMsg::Stop`, alongside `/stop
[--all]` and the sessions-modal `s` quick key.

What remained, per the deferred-work-ledger row 6 entry, was the confirm
modal itself — **including disambiguating `AgentState::WaitingAgent`'s two
callers** first. The state is a bare unit variant emitted from three call
sites that collapse into two families: a plain blocking sub-agent wait (the
`agent` tool, `subagent.rs`; the `agent_send` tool's blocking follow-up,
`agent_send.rs`) and a sponsored `propose_plan` build handoff
(`propose_plan.rs`'s `launch_sponsored_build`). Both park the parent on
`WaitingAgent` and spawn a child with an identical `parent: Some(_)` shape on
the wire — a head receiving `OutEvent::Status { state: WaitingAgent, .. }`
could not tell which kind of wait it was looking at, so it could not safely
decide whether to *offer* the cascade choice at all. Only the sponsored-build
kind has a meaningful cascade target; offering the choice for a plain
blocking sub-agent wait would be nonsensical (there is no plan-approval
handoff to cascade into).

The runtime already tracked this distinction internally: `SpawnGuard`
(`subagent.rs`) records which spawned children are sponsored, via
`record_sponsored_start`/`is_sponsored`. That bookkeeping is
tool-executor-internal and single-threaded by design (the sponsor check must
be race-free) — it was never on the wire, so no head could read it.

## Decision

### Disambiguate `WaitingAgent` on the wire: `sponsored: bool`

`InMsg::Spawn` gains a `sponsored: bool` field (`#[serde(default)]`, so every
pre-existing non-sponsored spawn construction site — the overwhelming
majority — stays terse and every persisted pre-#626 log still deserializes).
`propose_plan.rs`'s sponsored build spawn sets it `true`; every other `Spawn`
site (`subagent.rs`'s blocking `agent` tool, the `/compact` successor fork,
lazy-`Prompt` auto-created roots) sets or defaults it `false`. Core threads it
straight through — no new engine-side sponsorship bookkeeping, no policy
change — onto `OutEvent::SessionStarted` and `SessionInfo` (same default,
same terseness), so it round-trips over `ListSessions` and survives a
hibernate/resume cycle: `Session` gained a matching field, populated on
replay from the target's own `SessionStarted` record exactly like `parent`,
and `spawn_resumed`/`session_loop` pass it through the same shape already
used for `parent`.

This is a plain data field, not a new protocol message or a policy hook —
consistent with 0145's own "zero new protocol messages" framing for the
cascade/detach backend primitive itself.

### The TUI confirm

A new `App`-level state (`tui/app/stop_confirm.rs`, mirroring the two-stage
Ctrl+C quit's `quit.rs` shape) holds an optional `StopConfirm { target,
build_child }`. `App::stop_needs_confirm(target)` returns `Some(child)` only
when `target` is `WaitingAgent` *and* has a live (not yet ended) child whose
`SessionView::sponsored()` is true — both conditions, so a plain blocking
sub-agent wait is never offered the choice, only a genuine `propose_plan`
handoff is.

Every interactive single-target `Stop` site — bare `Esc` in Normal mode,
`/stop` (bare form), the sessions-modal `s` quick key, and the command
palette's `/stop` pick — now routes through one function,
`stop_command::request_stop`, instead of sending `InMsg::Stop` directly: it
arms the confirm when `stop_needs_confirm` says so, otherwise sends
immediately (byte-identical to pre-#626 behavior for every other case —
idle, `WaitingApproval`, a plain blocking sub-agent wait). While armed, the
confirm is a blocking modal checked ahead of every other key-dispatch site:
`c` cascades (sends `Stop` to the plan session, then the build child), `Enter`
/`y`/`d` detaches (sends `Stop` to the plan session only — the pre-#626
default, so an unfamiliar user who just presses Enter gets the same behavior
they always did), `Esc`/`n` cancels and sends nothing.

`/stop --all` deliberately bypasses the confirm and keeps its raw fan-out
semantics: it targets every live session at once, and re-confirming per
sponsored session in that loop would defeat the point of a bulk action.

## Consequences

- `AgentState::WaitingAgent`'s two callers are now distinguishable by every
  head, not just the TUI — `sponsored` is plain wire data.
- Zero new protocol *messages*: the confirm resolves into ordinary `Stop`
  sends (one for detach, two for cascade) over the already-existing wire
  contract 0145 shipped.
- The TUI gained one new small module family (`app/stop_confirm.rs`,
  `stop_command.rs`, `modals/stop_confirm.rs`) rather than growing any of the
  already-over-cap `event_loop.rs`/`modal_events.rs` — those two only gained
  the minimal call-site routing.
- [docs/deferred-work-ledger.md](../deferred-work-ledger.md) row 6 moves to
  Resolved.

## Rejected alternatives

- **Infer sponsorship from the `propose_plan` tool call already in the plan
  session's own transcript.** The child's id only reaches the plan session's
  transcript as unstructured text in the eventual tool *result*
  (`propose_plan.rs`: `"build \`{child}\` completed in …"`), not as
  structured data available *while* the child is still running — exactly the
  window the confirm modal needs to act in. A wire field is the only way to
  have the id and the sponsorship fact before the child finishes.
- **A popup-style picker modal** (the `modals/` `Clear`+`List` pattern used
  for the sessions modal, profile picker, etc.). Rejected: this is a binary
  decision, not a pick-from-a-list — the inline state-enum pattern
  `ApprovalMode` already established for the approval prompt is the closer
  structural precedent. A small dedicated popup (this ADR's choice) still
  fits that shape without forcing the decision into `ApprovalMode` itself,
  which is keyed to the *active* session's view while the sessions-modal
  `s` key can target a session that isn't active.
- **Offering the confirm on `/stop --all` too.** Rejected as user-hostile: a
  bulk action that stops to ask per matching session isn't bulk anymore.
