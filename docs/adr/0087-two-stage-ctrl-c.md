# 0087. Two-stage Ctrl+C: clear inputs first, then quit

- Status: Accepted
- Date: 2026-07-15

## Context

Before this change, **a single Ctrl+C killed the TUI immediately** from every
input context — normal mode and all eleven modal/picker/approval handlers. Each
of those handlers carried an identical match arm:

```rust
KeyCode::Char('q') | KeyCode::Char('c') if key.modifiers == KeyModifiers::CONTROL =>
    return Ok(true),   // immediate quit
```

Two problems flowed from that:

1. **No intermediate "clear" step.** Typed text, an open `@file` mention popup,
   and multiline mode were all discarded on the first press. A user who hit
   Ctrl+C intending to clear the line (the readline convention, and what most
   terminal apps do) instead lost their draft and the session view. The only
   finer-grained escape was `Esc`, which is overloaded (close modal / exit
   multiline / hide mention / quit-on-empty) and so not a reliable "clear input"
   gesture either.

2. **The "half killed" state.** Only the keyboard path ran `restore_terminal`.
   An **external** `SIGINT` (`kill -INT <pid>`, or a Ctrl+C from a terminal that
   doesn't honor crossterm's `KeyboardEnhancementFlags`) bypassed the key handler
   entirely and aborted the process with the terminal still in raw mode —
   alternate screen left up, mouse capture still on, no visible cursor. The
   `serve` head had a `tokio::signal::ctrl_c()` handler; the TUI head did not.

## Decision

Make Ctrl+C a **two-stage** gesture, and route every quit-adjacent signal
through one path.

### Centralized intercept (replaces the eleven arms)

Ctrl+C is intercepted **once**, at the top of `handle_event`'s
`Event::Key` → `KeyEventKind::Press` block, before any modal or approval-mode
routing:

```rust
if key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL {
    return Ok(app.handle_quit_key());   // false = cleared + armed, true = quit
}
app.clear_quit_pending();               // any other key disarms
```

This guarantees identical behaviour in every context and deletes the eleven
duplicate `Char('c')` arms (each modal handler keeps a `Char('q')`-only arm).
**Ctrl+Q stays an unconditional immediate quit** — the escape hatch for users
who want to quit on the first press.

### `handle_quit_key` semantics

```rust
pub fn handle_quit_key(&mut self) -> bool {
    if self.quit_pending && !self.quit_pending_expired() {
        return true;                    // 2nd press within window → quit
    }
    self.input = SimpleInput::default();
    self.input_multiline = false;
    self.mention.hide();
    self.quit_pending = true;
    self.quit_pending_at = Some(Instant::now());
    false
}
```

- **First press** (or a press after the prior arming expired): clears the text
  buffer, the `@file` popup, and multiline mode; arms a pending quit.
- **Second press within the window**: quits from any context.
- **Does not close modals.** `Esc` already closes modals everywhere; the first
  Ctrl+C is "clear my input," not "discard the dialog I'm in." A second press
  still quits, so a user in a modal is never trapped.

Two new `App` fields carry the state: `quit_pending: bool` and
`quit_pending_at: Option<Instant>` (in `tui::app::quit`).

### Timeout — `QUIT_TIMEOUT = 3s`

Checked **lazily** in `handle_quit_key` (so correctness needs no polling) **and
eagerly** once per render-loop iteration (so the "press again" hint disappears
promptly after expiry, not only on the next keystroke).

### Visual feedback

When `quit_pending` is true, the input info bar (`draw_input_info`) shows
**"Press Ctrl+C again to quit"** (yellow, bold) instead of the normal help text.

### SIGINT safety net

A `tokio::signal::ctrl_c()` task is spawned at TUI startup alongside
`spawn_crossterm_task`. In raw mode Ctrl+C is delivered as a key event (ISIG is
suppressed), so this handler only fires for an **external** SIGINT. It sends a
synthetic `Event::Interrupt` through the existing event channel, which routes
through the same `handle_quit_key` path → graceful `restore_terminal`. An
out-of-band signal can no longer leave the terminal in a broken state.

## Consequences

- **(+)** A reflexive Ctrl+C no longer destroys a half-typed prompt or a
  mention popup. The cost of a misfire is one extra keypress, not a lost draft.
- **(+)** Identical behaviour in every context — one intercept replaced eleven
  divergent copies that could only drift.
- **(+)** External `SIGINT` exits gracefully with the terminal restored. The
  panic hook already covered crashes; this covers the signal path.
- **(+)** Ctrl+Q remains a one-press quit for users who want it, so the
  two-stage gesture adds no friction for the "I meant to quit" case.
- **(−)** Quitting now takes two presses (within 3s) for anyone who was used to
  the single-press Ctrl+C. Mitigated by Ctrl+Q and by the visible hint.
- **(−)** The synthetic `Event::Interrupt` is a new event variant; it is handled
  only by the TUI's `handle_event` and carries no protocol meaning (never
  crosses the ABI).

## Alternatives considered

- **Keep single-press Ctrl+C, add a separate "clear" key.** Rejected: Ctrl+C is
  the muscle-memory "clear/abort" key in virtually every terminal app and
  shell; repurposing it to quit-only was the original surprise. Adding a new
  key for clear would still leave the surprise in place.
- **Two-stage Ctrl+C but closing modals on the first press too.** Rejected: the
  modal's `Esc` is already the close gesture, and folding it into Ctrl+C would
  make "I wanted to clear my text" accidentally dismiss the dialog. Keeping the
  modal open on the first press matches "clear input, don't discard context."
- **Process-level SIGINT handler that force-quits (not two-stage).** Rejected:
  it would fix the half-killed terminal but reintroduce the single-press-quit
  surprise. Routing the signal through `handle_quit_key` gives both the graceful
  restore *and* the two-stage UX from a single code path.
- **Poll the timeout with a timer event instead of checking lazily.** Rejected:
  the lazy check in `handle_quit_key` is correct without any timer, and the
  eager check in the render loop (which already runs ~30 fps) clears the hint
  promptly enough. A dedicated timer would add event-channel traffic for no
  behavioral gain.
