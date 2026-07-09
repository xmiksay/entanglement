# 0029. External `$EDITOR` compose + Markdown transcript export

- Status: Accepted
- Date: 2026-07-09

## Context

The TUI's `<leader>e` / `/editor` and `<leader>E` / `/export` entry points were
scaffolded (command-palette entries, leader keybindings, `Command`/`Action`
variants) but their handlers were no-ops returning `false`. Two features remain:

1. **Compose in `$EDITOR`** — for long or structured prompts the single-line-ish
   input box is cramped; users want to drop into their real editor.
2. **Export the session transcript to Markdown** — a readable, shareable record
   of the conversation reconstructed from the accumulated `OutEvent` stream.

Both require *suspending* the TUI: leaving the alternate screen + raw mode,
running a blocking editor process with inherited stdio, then re-entering.

## Decision

### Where the effect runs

The editor launch needs the `Terminal`, which the event loop owns — `App` (and
therefore the command/action handlers) does not. So a handler records a deferred
**`UiEffect`** (`OpenEditor` | `Export`) on `App`; the event loop drains it once
per iteration (after the terminal/engine event drains) and runs it through
`tui::editor::run_effect(&mut terminal, &mut app, effect)`. This keeps the
existing `execute_command` / `dispatch_action` → `bool` (quit?) contract intact
— they set the effect and return `false` — and localizes all terminal-mode
juggling to one module.

Effect failures are **logged, not fatal**: a missing/erroring `$EDITOR` must not
kill a live session. The suspend/resume is symmetric with `tui::tui` setup and
`restore_terminal` (disable raw + leave alt-screen + pop keyboard flags +
disable mouse on the way out; the reverse on the way back), and always re-enters
+ `clear()`s regardless of the editor's exit, so the terminal is never left in a
half-suspended state.

### `<leader>e` — compose (round-trip, not submit)

Seeds a temp file (`$TMPDIR/skutter-input-<pid>.md`) with the current input
draft, opens it in `$EDITOR`, and **reads the result back into the input box**
(trailing newlines trimmed). Chosen over submitting directly: it composes with
the existing send/edit flow (the user still reviews and presses Enter), matches
the issue's "reads the content into the input (#4)" path, and is non-destructive
if the user quits the editor.

### `<leader>E` — export

Reconstructs Markdown from `App`'s visible transcript (`tui::export`, pure over
its inputs so it unit-tests without a terminal): coalesces consecutive
`TextDelta` / `ReasoningDelta` runs the same way the renderer does (preserving
text-vs-thinking arrival order), and maps each entry to a section — `## User`,
`## Assistant`, `### Reasoning` (blockquoted), `### Tool call: \`name\`` (fenced,
pretty-printed JSON input), `**Output**` (fenced), `### Error` (blockquoted),
`---` between turns on `Done`. Plan + task snapshots render at the top when
present. Fenced blocks pick a backtick-fence longer than any run inside the
content so tool output containing ``` can't break out. Writes
`<session>-<unix_secs>.md` (session id sanitized to `[A-Za-z0-9_-]`) in the cwd,
then opens it in `$EDITOR`.

### Editor resolution

`$VISUAL`, then `$EDITOR`, then `vi`. The env value is word-split so
`EDITOR="code --wait"` works; the process is run with `.status()` (blocking),
honoring the `--wait` convention by construction. Configurable-via-`tui.json` is
explicitly **out of scope** (backlog) — env only.

## Consequences

- **Positive:** long-form prompt composition in the user's real editor; a
  shareable Markdown record of any session; all terminal-suspend logic isolated
  in one module; export logic is pure and unit-tested.
- **Negative / neutral:** a new deferred-effect indirection on `App`; GUI editors
  need their own `--wait` flag in `$EDITOR` (documented, not enforced); export
  filename timestamp is Unix seconds (no date lib pulled in for one label).

## Alternatives considered

- **Submit the edited buffer directly** instead of reading it back. Fewer
  keystrokes, but destructive on accidental-quit and can't be reviewed before
  send; the round-trip is safer and reuses the normal send flow.
- **Run the editor inline in the handler** (thread the `Terminal` into
  `execute_command`/`dispatch_action`). Rejected: it would force `App` to hold or
  borrow the terminal and change the clean `-> bool` handler contract; the
  deferred-effect drain keeps that boundary.
- **A new protocol event for export.** Unnecessary — export is a pure head-side
  view over already-accumulated transcript state; the engine has no stake in it.

## References

- Issue #13: External editor (`$EDITOR`) + export session to Markdown
- [ADR-0011](0011-tui-head-ratatui-crossterm.md): TUI head (terminal setup/teardown this mirrors)
- [ADR-0013](0013-keybinding-leader-which-key.md): leader-key scheme (the entry points)
