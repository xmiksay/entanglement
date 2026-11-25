# 0219. TUI themes and keybindings are named slots and validated action maps

- Status: Accepted
- Date: 2026-10-11
- Extends: [ADR-0213](0213-prune-env-vars-and-catalog-knobs.md) (the shared
  `config.yml` `tui:` block, live-reloaded)

## Context

The TUI had one hardcoded dark palette. About 170 `Color::…` literals were
spread over ~25 render files, so the TUI was unreadable on a light terminal.
Its keys were hardcoded too: the `Ctrl+X` leader chords in a `KeyMap`, plus
direct keys (`Enter`, `Tab`, `Ctrl+G`, `Ctrl+L`, …) as `match` arms in the
event loop. Issue #735 asks for `tui.theme` (built-in dark/light plus user
palettes) and `tui.keybindings`, both live-reloaded, with a clear error on
conflicts.

## Decision

1. **Every colour is a named slot.** `config::theme::Slot` is the one list of
   names (`message.bg`, `accent`, `success`, `warning`, `error`, `muted`,
   `selection.bg`, `approval`, `diff.add`, `status.branch`, …). Widgets ask
   the resolved `Theme` for `theme.c(slot)` or `theme.fg(slot)` and never
   name a `Color`. The per-base tables are exhaustive `match`es, so a new
   slot fails to compile until `dark` and `light` both define it. `dark` is
   the pre-#735 colours exactly; a TestBackend test checks the rendered cells.
   `light` keeps every foreground at WCAG ≥ 4.5:1 on its backgrounds, and a
   unit test checks that too. The one exception is fenced-code syntax
   colours: a syntect theme has dozens of scopes, so it follows the base
   (`base16-ocean.dark`/`.light`) instead of being a slot.
2. **A theme is lenient.** `tui.theme` is a base name or
   `{base, slots: {name: colour}}`. A colour is a named ANSI colour,
   `#rrggbb` or a 256-colour index. An unknown base, an unknown slot or a bad
   colour each add one config warning and keep the base's value. A typo in a
   colour must never stop the TUI from starting. Unknown keys *inside* the
   object (`bsae:`) stay a hard error, like everywhere else in `config.yml`.
3. **`NO_COLOR` wins.** When it is set and non-empty, every slot and the
   syntax colours fall back to the terminal default. Emphasis stays as
   modifiers: bold, dim, and `REVERSED` for list selection and the active tab.
4. **A key map is all-or-nothing.** `tui.keybindings` maps an action to one
   spec or a list. The list replaces that action's defaults, and `[]` unbinds
   the action. The action set is exactly the pre-#735 configurable bindings,
   with no new actions. A spec is a chord or `leader <key>`. Chords are
   normalized so `shift+e` ≡ `E` and `shift+tab` ≡ BackTab, the same way
   events are normalized. The override is refused whole, with one warning
   naming every problem, if any of these holds:
   - two actions share a key in one context (direct, or after the leader);
   - it names an unknown action;
   - a key does not parse;
   - a bare printable key is bound directly (it would stop typing);
   - it binds a reserved key.

   The TUI then keeps the map it had (the defaults at startup). A partial
   apply could leave e.g. `submit` and `newline` on one key.
5. **Reserved keys:** `ctrl+c` (two-stage interrupt/quit, ADR-0087, also
   where an external SIGINT lands), `ctrl+q` (immediate quit, the escape
   hatch in every dialog) and `esc` (cancel/close, and the leader-chord
   cancel). A remap must never be able to trap the user. Dialog-local keys
   (approval `y/s/a/d/n/e/v`, list navigation) and readline editing also
   stay fixed. They are context-bound, and remapping them would multiply the
   conflict contexts for little gain. `jump_attention` and `list_sessions`
   are checked ahead of every open dialog. As direct chords, they therefore
   may not take a dialog navigation key (enter, tab, arrows, page up/down,
   home/end, backspace/delete, under any modifier).
6. **One dispatch path.** Leader chords and direct chords both resolve to a
   `KeyAction` and run through `event_loop::actions::run_action`. Any action
   may be bound in either context, and a remapped key behaves exactly like
   the default one.
7. **Live reload and reporting.** Startup warnings go to stderr, as for
   every config warning. On a reload, each *changed* set of theme/keybinding
   warnings is recorded once as a transcript notice. The watcher fires on any
   config-dir change, so an unchanged problem must not repeat. A reload
   whose config no longer parses at all (a shape error such as
   `theme: {bsae: …}`) keeps every previous TUI setting. The error is
   reported the same way, once.
   `skutter inspect config` prints both raw blocks with the layer that won
   them.

## Consequences

- Every render helper now takes the `Theme` (or reads `app.theme()`). This
  touches many signatures, but it is a plain value (`Copy`), so the threading
  is mechanical. A global "current theme" was rejected: it would race in
  parallel tests, and the codebase already threaded `Theme` by value.
- The input placeholder, the attention panel's jump hint and the help dialog
  read the live key map, so a remap is reflected everywhere it is named.
- Small behaviour change: direct keys now match their modifiers exactly,
  except that `scroll_up`/`scroll_down` also fire on any modified form of
  their key (`Shift+PageUp` still scrolls) unless that exact chord is bound
  to something else. Enter stays exact on purpose: `Ctrl+Shift+Enter` no
  longer inserts a newline unless bound, and on kitty-protocol terminals,
  which report `Ctrl+Enter` with its modifier, `Ctrl+Enter` is now a dead key
  (it used to submit). Bind it (`submit: [enter, ctrl+enter]`) to restore
  that. A guessed modifier fallback for Enter could silently swallow a key
  the user meant for `newline`.
- The one-line fields (reject reason, free-text answer) submit on the
  `submit` action, and on plain Enter unless `newline` was moved onto Enter.
  Every valid remap therefore keeps them submittable.
- Light-theme hashed profile colours use HSV value 40, so every hue the hash
  can produce keeps ≥ 4.5:1 on the light backgrounds (tested over all hues).

## Alternatives considered

- **A free-form palette with no slot list:** rejected. It could not warn on
  a typo, and it could not guarantee that `light` covers everything.
- **Per-entry keybinding leniency** (apply the valid entries, drop the bad
  ones): rejected. A half-applied remap is worse than none (point 4).
- **Rebindable `ctrl+c`:** rejected. SIGINT already routes to it, and an
  unbound interrupt is a trap.
