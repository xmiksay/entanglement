# 0013. Keybinding scheme: leader key + which-key

- Status: Accepted
- Date: 2026-07-05

## Context

The TUI has many keybindings (quit, new session, switch agent, model picker, sidebar, editor, export, etc.). Direct single-key bindings risk colliding with the user's terminal or editor preferences. opencode's TUI solves this with a **leader key** (default `Ctrl+x`) and a **which-key popup** that shows the available next keys after the leader.

Why a leader key?

- **Avoids collisions:** Only a few single-key shortcuts are truly safe (`q` to quit is the only one we use directly). Everything else uses the leader prefix.
- **Discoverability:** The which-key popup lists all options, so users don't have to memorize them.
- **Terminal neutral:** Works even in terminals with intercepting keymaps.

Rejected options:

- **Single-key bindings everywhere:** Too collision-prone. `Ctrl+q`, `Ctrl+n`, `Ctrl+l`, etc. are often already bound.
- **Vim-style modal states:** Powerful but higher learning curve. Our user base is broader.

## Decision

Adopt a **leader-key + which-key** scheme:

- Leader key: `Ctrl+x` (configurable via env or, later, `tui.json`).
- Timeout: 2000 ms (configurable). If the next key doesn't arrive in that window, the pending state cancels.
- Which-key popup: When the user presses the leader, a modal shows a table of `{ key → action }`. Pressing a key dispatches the action.
- Help dialog: `Ctrl+x ?` or `Ctrl+x h` shows the full keybind list (same content as the popup, but viewable anytime).

All bindings route through a `KeyMap` table:

```rust
enum Action { Quit, NewSession, PickAgent, CycleAgent, ... }
struct KeyBinding { keys: Vec<KeySequence>, action: Action }
```

The which-key popup filters to show only the keys that start with the user's pending prefix (usually just the leader at first). For nested prefixes (not planned for MVP), it would drill down.

## Consequences

- **(+)** Safe against terminal/editor collisions.
- **(+)** Discoverable via which-key.
- **(+)** Configurable (future via `tui.json`).
- **(−)** Requires two key presses for most actions (leader + key).
- **(−)** Timeout state is a new failure mode (user can be too slow). A visual timer is recommended (future).

## Alternatives considered

- **Single-key bindings:** Rejected (collision risk).
- **Vim modal states (normal/insert):** Rejected (higher learning curve; our target audience includes non-vim users).
- **Emacs-style chords only (no popup):** Rejected because chords are harder to discover. The popup is a UX improvement over raw chords.