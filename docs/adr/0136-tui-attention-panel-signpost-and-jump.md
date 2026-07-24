# 0136. TUI attention panel: a signpost + jump, not a second approval keymap

- Status: Accepted
- Date: 2026-07-24
- Head-side only (no wire change), on top of
  [0014](0014-tui-inline-tool-approval.md) (inline approval cards),
  [0061](0061-parked-turn-state-batch-tool-resolution.md) (batch-parked
  approvals), and the #488 `ask_user` question flow.

## Context

Every `OutEvent` routes into its own per-session `SessionView`, so a
background (non-active) session's parked `ToolRequest`/`UserQuestion` *is*
tracked — but the approval/question UI reads only
`sessions.active_view()`. A sub-agent or second session parked on an Ask was
effectively invisible: a bell rang and the status bar showed a bare `!`, with
no indication of **which** session waited or **what** it asked. The user had
to guess and cycle sessions manually. For an engine whose core feature is
spawned sub-agents, that is the difference between a parked child resolving in
seconds and hanging until the reoffer timer or the user stumbles onto it.

Two designs were considered for surfacing it:

1. **Act-in-place panel**: a focusable panel above the input box with its own
   approve/reject/answer keys operating on the background session directly.
2. **Signpost + jump**: the panel names the oldest waiting session and what it
   asks; one chord (`Ctrl+G`) or a click switches the active session to it,
   where the existing approval/question UI takes over unchanged.

## Decision

Signpost + jump. The approval scope keys (`y`/`s`/`a`/`d`/`n`) are **plain
characters**, disambiguated today only by the active view's `ApprovalMode`
owning the key loop; a panel acting on a *background* session would need
either global plain-char interception while the user may be typing
(unacceptable) or a panel-focus mode duplicating the entire approval *and*
question keymaps — including the reject-reason and free-text-answer flows,
which borrow the shared input box and would need focus juggling. Jumping
first reuses all of it: after the switch, the requesting session is active,
its transcript tail renders the prompt, and every existing key (and `Esc`'s
interrupt semantics) applies to exactly the session on screen.

Consequences of that choice, and the supporting pieces:

- **The panel covers background sessions only.** The active session's pending
  request already renders as the transcript tail directly above the input;
  including it would show the same ask twice. Invariant: *active pending ⇒
  tail; background pending ⇒ panel* — every parked request is visible
  somewhere.
- **No auto-switch on request arrival.** Yanking the active session while the
  user types (possibly mid reject-reason, which shares the input box) is
  hostile and destructive. `Ctrl+G` is one deliberate keystroke; pressing it
  repeatedly chains through several waiting sessions (it is intercepted ahead
  of the approval/question routing, so it works while the active session is
  itself parked — and before the bare `Char(c)` arms that would otherwise
  type a literal `g` into a reject reason).
- **Aggregation is derived, not stored** (`ui::alerts::background_attention`):
  a per-frame read over the views' pending queues (not the flappy `Status`,
  #273), ordered by registry (creation) order — the same order the jump
  targets, so the panel always describes where `Ctrl+G` goes. The status-bar
  `!` becomes a `⚠ N` count off the same function; one source of truth.
- **Zero-height when idle**: the panel is a `Length(0)` layout row while
  nothing waits — structurally absent, not blanked.
- Alongside (same change, same motivation of making sessions identifiable):
  sidebar/status-bar/sessions-modal show an 8-char short id instead of the
  full UUID, the sidebar gains a dim first-prompt description line
  (`SessionView::first_prompt`, captured on the first recorded user message
  and reconstructed by resume's Prompt replay) and distinct
  `needs approval`/`question` state words, the sessions modal gains a
  `❓ question` badge beside the `⏳ approval` one, and the sidebar's session
  rows + the panel are click-targets (a draw-time row map mirroring the chat
  hit-test capture).

## Rejected

- **Act-in-place panel** — duplicates two keymaps, needs a focus model the
  TUI doesn't have, and still falls back to the input box for free-text.
- **Auto-switch on arrival** — destructive to in-progress input; the panel +
  bell already get attention without stealing focus.
- **A protocol-level aggregate event** — the head owns every view already;
  this is purely a rendering concern, so no wire change is justified.
