# Heads & persistence §6 — The TUI head

> Part of the [heads & persistence module](../heads-and-persistence.md) of the [architecture overview](../../architecture.md).

- **TUI** (`skutter tui`): opencode-style terminal UI over `subscribe()`. Uses
  ratatui + crossterm (ADR-0011), leader-key bindings with which-key popup
  (ADR-0013), rich markdown rendering with pulldown-cmark + syntect
  (ADR-0015), and inline tool approval cards (ADR-0014) with a full-body pager
  (`v` while parked on one, `j`/`k`/`PgUp`/`PgDn` scroll, `Esc` closes back to
  the prompt; the decision keys `y`/`s`/`a`/`d`/`n`/`e` still resolve the
  approval while it's open) reusing the tail's own rendering for a diff/plan
  body too long for the parked tail. Cards queue per session (#273); a
  terminal `Status` (`Idle`/`Done`/`Error`) drops the finished turn's cards
  but keeps a background script's (`ToolRequest.detached`, #872,
  [ADR-0237](../../adr/0237-detached-flag-on-tool-request.md)), still parked
  in the runtime. Every `Stop` (`Esc`, `/stop [--all]`, sessions-modal `s`,
  palette) goes through `stop_command::request_stop`, which clears the
  target's whole queue, since `stop_session` unwinds those cards too. A
  resumed view carries its detached cards over from the view this process
  already had (a hibernated session's are still parked) and drops them for a
  session it never saw. When a terminal `Status` moves the prompt to another
  card, the pager closes and a half-typed reject reason is cleared, so no key
  answers the wrong card (ADR-0237 §6).
  The input box enables crossterm's
  bracketed paste so a multi-line paste lands as one buffer edit (each
  embedded newline starts a new `SimpleInput` row) instead of a flood of key
  events whose embedded newlines each fire the Enter-to-send binding.
  **Clipboard image paste** (#736, [ADR-0220](../../adr/0220-web-fetch-ssrf-guard-and-clipboard-image-paste.md)):
  `Ctrl+V`, or an empty bracketed paste, reads the clipboard image by shelling
  out to `wl-paste` (Wayland), `xclip` (X11) or `osascript` (macOS). There is
  no clipboard crate. A headless session or a missing tool shows a toast. The
  image must be PNG, JPEG, GIF or WebP (checked by magic bytes) and at most
  5 MiB once base64-encoded. It is parked on the input buffer as an
  `[Image #n]` chip. On submit, every image whose chip is still in the text
  is sent as a `ContentPart::Image`. Only a model whose catalog entry sets
  `supports_vision` accepts a paste; any other model is refused at paste time
  and again at submit. Event
  buffering and
  multiplexed-session rendering follow ADR-0012. The transcript body is rendered
  through a **per-block render cache** (#342, `tui::transcript::cache`): a redraw
  fires on every keystroke, scroll, mouse move, and streaming delta, but the
  markdown+syntect+wrap pipeline is the expensive part, so `render_body_lines`
  segments the transcript into content-addressed blocks (coalesced text/reasoning
  runs; each user/tool/error entry its own self-contained block) and re-renders
  only the block whose content hash (`kind + content + expanded/padding flags`)
  changed — an idle redraw re-parses zero markdown and just clones the owned
  `Line<'static>` memo. A `width`/`theme-fingerprint` mismatch (resize or theme
  swap) drops the whole memo and rebuilds once; the approval/question tail stays
  rendered fresh per frame after the cached body. The memo lives on
  `SessionView` beside `expanded_blocks`, so each session keeps its own. Mouse capture is on by default
  (opt out with `config.yml` `tui: {mouse: false}`, which restores native
  terminal text selection; live-reloaded, [ADR-0213](../../adr/0213-prune-env-vars-and-catalog-knobs.md)): the wheel scrolls the chat (or the open modal's selection).
  **Left button drives transcript selection + copy** (`tui::selection`,
  `tui::clipboard`): a drag paints a reversed-video highlight over the selected
  span (`apply_highlight` re-splits the rendered `Line` spans at the selection
  bounds) and, on release, copies the selected text to the system clipboard via
  an **OSC 52** escape (works over SSH/tmux, no clipboard-crate dep) with a
  "Copied N chars" **info-line toast** (`App::set_toast`, ~3s TTL — never a
  transcript entry, which would hard-split a streaming Thinking block at the
  insert point); the write is deferred through a
  `UiEffect::CopyToClipboard` so the event loop (which owns the terminal) emits
  it. A bare **click** (press+release, no drag) instead resolves its target by
  surface: a sidebar session row (or its description line) switches to that
  session, the attention panel jumps to the oldest waiting background session,
  and otherwise the chat area hit-test toggles a transcript block — reasoning
  runs render collapsed as a `▸ Thinking (N lines)` header, and each **tool
  operation** as a single collapsible `▸ {tool}  {primary_arg}  ✓`/`✗` line
  (the badge from `OutEvent::ToolOutput::is_error`, folded through
  `TranscriptEntry`/`Block::ToolCall`, #672 amending ADR-0176 — a failed call
  gets the theme's `error_colors()` red `✗` instead of the green `✓`) with its
  paired output folded in (#340; the `ToolOutput` matches its `ToolCall` by
  `request_id`, so batch results still pair correctly), both expanded on click
  (or via the leader `t` key, which toggles the most recent block of either kind).
  Every call renders by its **real name** with a readable summary
  ([ADR-0204](../../adr/0204-invoke-fallback-for-client-side-discovery.md) §6,
  kept by [ADR-0226](../../adr/0226-one-advertised-surface-mcp-envelope-registered-is-available.md);
  `tui::tool_render`, one renderer per tool family, sharing the ratatui-free
  `run::summary` helpers with the `run` text head's `→`/`=` lines): the header
  is `{tool}  {primary_arg}` (`mcp__server__tool` reads `server › tool`), and the
  expanded body shows arguments as indented `key: value` lines and JSON outputs
  the same way — never escaped JSON — with a failed call's output marked
  `✗ error`. A call made through the `mcp` envelope looks exactly like a direct
  call of the inner `mcp__server__tool` (core unwraps it before any event), and
  the discovery calls are ordinary visible blocks: `list_agents`/`list_models`/
  `list_skills`/`list_mcp` show their rows readably, `mcp_tools` the server name
  and one `- tool: description` line plus its `call as:` line per tool.
  The bottom **info line** (`draw_input_info`) reads
  `zai · GLM-5.2 | ctx 87.3k/200k 44% | $1.2345 | last: 51.2k in (94% cached) · out 1.1k`
  ([ADR-0202](../../adr/0202-prompt-cache-discipline-anchors-deferral-replay-compaction-date.md) §7,
  `tui/input_panel/status_usage.rs`): `ctx` is the last **turn** round's billed
  prompt (uncached + cache-read + cache-write — `OutEvent::Usage.input_tokens`
  is the uncached portion only) against the model's context window, omitted
  until a turn round lands and never moved by a `purpose: compaction` round;
  `$` is the session spend (token counts instead when no round carried
  pricing); `last:` is the most recent round of either purpose with its cache
  share `cached / billed prompt`, rendered in the throttle indicator's red/bold
  style when that round is a full cache miss right after one that hit. A
  `GenerationChanged` that moves `reasoning_effort` or
  `thinking_budget_tokens` against the session's previous one (the first only
  sets the baseline; temperature/max-tokens never count) is allowed but
  re-keys the provider cache, so the view records one `cache` transcript
  notice and the ledger excuses the next round's full miss (replay re-folds
  the same notice, as there is no live/replay split in the fold). All
  three read one per-session `CostLedger` (`tui/session_view/cost.rs`) folded
  from `Usage` + `ModelChanged`, so resume restores it by replay. Cumulative
  in/out totals are not on the line — **`/cost`** (`tui/cost_command.rs`)
  records them as a transcript notice: spend over N rounds with the session
  cache share, the prompt split (billed · uncached · cached · cache-write ·
  output), a `compaction:` line (only with compaction calls), per-model rows
  keyed by the `ModelChanged` in effect when each round landed, and a
  `subtree:` line (own + N child sessions, the `Σ` rollup below; only with
  children), or `cost: no usage recorded yet`. The line then carries one
  transient status slot — a pending two-stage quit hint, else the
  **toast** (`tui/app/toast.rs`, ~3s TTL, expired eagerly by the render loop
  like `quit_pending`): the copy notice and app/config state-change
  confirmations (definitions reload #329, `/key` save, `/model`/generation
  persistence, `/mcp`·`/bash`·`/enable`·`/allow` acks, tools-dialog save,
  modal session deletion) surface here instead of as transcript status lines;
  errors, help text, and approval decisions stay in the transcript — else
  — *only while an endpoint is backing off* — a red throttle indicator
  (`⚠ host throttled · retry Ns · in/cap`, or `⚠ host pacing · next Ns · in/cap`
  while the adaptive gate alone has slowed, #517) polled from
  `HttpClient::throttle_status()`; it no longer duplicates the keybinding hints
  (those live in the input-box placeholder). The same throttle state also
  reaches **stdio/WS heads**, which have no direct `HttpClient` handle: the
  runtime's `throttle::spawn_throttle_responder` polls every endpoint and
  emits a wire-visible `OutEvent::Throttle` on each one's own enter/exit
  transition (#517, [ADR-0141](../../adr/0141-wire-visible-throttle-transitions.md)) —
  engine-global, not per-session, since the resilience pool is per-endpoint
  (ADR-0050). `run` relays it past its own-session filter (#794) — `--format
  text` renders it in full on stdout, `--format json` emits it as an NDJSON
  frame; `serve` relays it like any other frame. **Status line** (#762, `tui::statusline`): the top row
  always shows where the TUI works — default `repo › cwd_rel  ⎇ branch
  [worktree: name]` left, the session short id right, then the old bar's
  `(N sessions)` / `⚠ N` / `⚠ log` (session-log outage, #767) counters. Rendered from `config.yml`
  `tui.statusline.template` (left) and `.right` (right) — placeholders
  `{repo} {cwd} {cwd_rel} {branch} {worktree} {session} {agent} {mode}
  {model} {cost} {context_pct} {action} {tasks} {workdir}` (`{action}` is the
  narrate-driven `Session.action`, `{tasks}` done/total of the last
  `TaskList`), live-reloaded through the definitions-watcher arm that
  re-applies the `tui:` block. An empty value collapses with the literal in
  front of it (the run after the last placeholder is its suffix); an unknown
  `{name}` renders literally and adds one `Resolved::warnings` line. Too
  narrow, the row sheds counters → `{tasks}/{cost}/{context_pct}` →
  `{action}/{model}/{mode}/{agent}/{workdir}` → `{session}` → the worktree name →
  `{worktree}` → `{branch}`, then middle-elides the path (`…/deep/dir`);
  `{repo}` never goes. Git facts come from a **background probe** (the
  `@file` index pattern, posting `Event::StatusLine` only on change) that
  never spawns `git` or walks a tree: an ancestor search for `.git`; a
  `.git` dir is a plain repo; a `.git` file's `gitdir:` with a `commondir`
  is a linked worktree named after the main repo (the common dir's parent),
  without one a submodule; `HEAD` `ref: refs/heads/x` → `x`, an object id →
  short sha, anything else (reftable's `.invalid` stub) → `?`; no dirty flag.
  It re-probes when a `bash`/`call`/`rhai` `ToolOutput` lands and otherwise
  only stats `HEAD`'s mtime every 2 s. Its colours are the
  `status.repo/path/branch/worktree/text/dim/attention` theme slots.
  **Status command** (#762 part B, `tui::statusline::command`):
  `tui.statusline.command` (an `sh -c` string, allowed from the project
  layer per [ADR-0047](../../adr/0047-local-trust-boundary.md) like project
  hooks) replaces the *left* template with its first stdout line; `right`
  and the counters stay template-driven. It reads a versioned JSON payload
  on stdin (`tui::statusline::payload`, `{"v":1, session, cwd, root, repo,
  branch, worktree, agent, mode, model:{id,name}, cost_usd, context_pct, workdir,
  action, tasks:{done,total}}`, unknown facts `null`; fields are only added
  under the same `v`). One background task runs it the hooks' way — own
  process group, SIGKILLed after `timeout_ms` (default 1000) and reaped —
  reading a 4 KiB window of stdout/stderr while draining the rest; ANSI
  escapes and control characters are stripped (the `status.text` slot
  colours it). The App publishes the current job (command, timeout,
  payload) into a `watch` channel on every dirty frame, which wakes the
  runner only on change — git probe, focus switch, mode/model/usage/task/
  action events — and an exec-tool `ToolOutput` forces a re-run; `watch`
  keeps only the latest job and the next run starts no earlier than 1 s
  after the last one started and 0.5 s after it finished, so it runs at
  most once a second, never twice at once, and a slow script never runs
  back to back. Success shows the
  line (kept until the next run reports); a failure (non-zero exit,
  signal, timeout, spawn error) shows the template again with one toast
  per failure streak. A result for a command a reload has since replaced
  is dropped. The child inherits the TUI's environment minus the catalog's
  provider key variables (#164, as for `bash`/`call`/MCP stdio). The task
  stops when the TUI drops its event receiver, killing a run in flight.
  The line is stripped of ANSI escapes, control characters and bidi /
  zero-width format characters (no reordered or hidden text). As with
  `bash` and hooks, the group is killed only on timeout or cancel: a
  grandchild that redirects its output away keeps running after a
  successful run.
  **Themes** (#735, [ADR-0219](../../adr/0219-tui-themes-and-keybindings.md),
  `config::theme` + `tui::theme`): every colour the TUI draws is one named
  `Slot` of a resolved `Theme` palette — no widget names a `Color` itself
  (fenced-code syntax colours are the one exception: they follow the base's
  syntect theme, `base16-ocean.dark`/`.light`). `tui.theme` is `dark` (the
  pre-#735 colours exactly), `light` (all foregrounds ≥ 4.5:1 WCAG contrast on
  its backgrounds, unit-tested), or `{base, slots: {name: colour}}` — named
  ANSI, `#rrggbb` or a 256 index. Lenient: an unknown base/slot or a bad
  colour is one `Resolved::warnings` line and the base's colour stays.
  `NO_COLOR` (set, non-empty) resets every slot and the syntax colours to the
  terminal default; emphasis survives as modifiers (bold/dim, `REVERSED` for
  a list selection and the active tab). A theme change rebuilds the
  `MarkdownRenderer` and the render cache's palette fingerprint drops the
  memo. **Keybindings** (#735, `config::keybindings` + `config::keychord` + `tui::keybindings`):
  `tui.keybindings` maps a `KeyAction` — exactly the pre-#735 configurable
  bindings (plus #736's `paste_image`): `leader submit newline cycle_mode cycle_mode_back paste_image scroll_up
  scroll_down toggle_reasoning jump_attention new_session list_sessions
  pause_toggle pick_agent pick_model inspect toggle_sidebar open_editor export
  command_palette interrupt quit help` — to one spec or a list replacing its
  defaults (`[]` unbinds). A spec is a chord (`ctrl+`/`alt+`/`shift+` on a
  char or named key, normalized so `shift+e` ≡ `E` and `shift+tab` ≡ BackTab)
  or `leader <key>`. Direct chords and leader keys are two contexts; any
  action may live in either, and both dispatch through
  `event_loop::actions::run_action`, so a remapped key behaves exactly like
  the default one. All-or-nothing validation: a key bound to two actions in
  one context, an unknown action, an unparseable key, a bare printable key as
  a direct chord, a dialog navigation key for the pre-dialog `jump_attention`/`list_sessions`, or a reserved key (`ctrl+c` two-stage interrupt/quit,
  `ctrl+q` immediate quit, `esc` cancel — also as `leader esc`) rejects the
  whole override with one warning naming every culprit, and the TUI keeps the
  map it had (the defaults at startup). Dialog-local keys (approval
  `y/s/a/d/n/e/v`, list navigation) and readline editing stay fixed. The help
  dialog lists the effective bindings plus the fixed keys; the input
  placeholder and the attention panel's jump hint read the live map. The
  reject-reason and free-text answer fields submit on `submit` (plain Enter
  too unless `newline` took it); scrolling also fires on a modified
  PageUp/PageDown when that exact chord is unbound; a reload whose config
  no longer parses is reported once the same way. Both
  blocks re-apply on the watcher's reload arm (`App::apply_appearance`), which
  records each *changed* set of theme/keybinding warnings once as a transcript
  notice (startup warnings go to stderr); `skutter inspect config` prints both
  raw blocks with their layer. Approval prompts for path tools
  (`read`/`edit`/`write`/`apply_patch`/`glob`/`grep`) add a `root: <dir>`
  line under the header, since a relative `path:` means nothing without the
  root it resolves against. **Attention signals** (issue #14, `tui::attention`):
  a `Status` transition into `WaitingApproval`, `Done`, or `Error` rings the
  terminal bell — and, opt-in via `config.yml` `tui: {notify: true}`, emits an OSC 9
  desktop notification (iTerm2/kitty/WezTerm; silently dropped elsewhere). Core
  emits `Status` only on a state change, so signalling on those states *is*
  signalling on the transitions; `Done`/`Error` also arrive as their own
  `OutEvent` variants but only `Status` is watched, so a turn end rings once.
  Focus reporting (crossterm `EnableFocusChange`) mutes signals while the
  terminal is focused, but best-effort only — terminals that never report focus
  always signal. **Attention panel + jump**
  ([ADR-0136](../../adr/0136-tui-attention-panel-signpost-and-jump.md),
  `tui::ui::alerts`): a **background** session parked on an approval or
  `ask_user` question surfaces as a one-line panel directly above the input box
  — absent (a `Length(0)` layout row) while nothing waits — naming the oldest
  waiting session (short id + agent) and what it asks (`needs approval: tool
  arg` / `question: text`), plus a count when several wait; the status line
  shows a matching `⚠ N` off the same aggregation (derived per frame from the
  views' pending queues, not the flappy `Status`, #273). `Ctrl+G` — or a click
  on the panel — switches to that session, where the existing
  approval/question UI takes over unchanged (a *signpost + jump*, deliberately
  not a second approval keymap; the active session's own pending request
  already renders as the transcript tail, so the panel covers background
  sessions only). Sessions are identifiable throughout: the sidebar, status
  line, and sessions modal show an **8-char short id** instead of the full
  id (a legacy 36-char UUID or an ADR-0164 15-char `s-…` id — the truncation
  is format-agnostic); the sidebar adds a dim per-session **first-prompt description line**
  (`SessionView::first_prompt`, captured on the first recorded user message
  and rebuilt by resume's Prompt replay) and distinct
  `needs approval`/`question` state words (queue-derived); the sessions modal
  adds a `❓ question` badge beside `⏳ approval`; and sidebar session rows are
  **click-to-select** via a draw-time row map mirroring the chat hit-test
  capture. Both the sidebar and the sessions modal list sessions as a **spawn
  tree** (`tui/session_tree.rs`): `SessionRegistry::all()` re-orders the
  insertion-ordered set depth-first (roots in insertion order, children
  indented under their parent; a corrupt parent cycle appends rather than
  drops), the modal's selection index follows the same ordering, and an ended
  child session renders dim with a `✓`. A session with spawned descendants
  also shows a `Σ` fan-out rollup on its row — own + every descendant's
  token/cost total, recursed over the same parent links
  (`SessionRegistry::usage_rollup`, #560); it's a dollar figure only when
  every contributing session carried catalog pricing, else token counts, and
  it's head-side aggregation of already-broadcast `OutEvent::Usage` totals,
  no protocol change. **Two-stage Ctrl+C** ([ADR-0087](../../adr/0087-two-stage-ctrl-c.md)):
  a first Ctrl+C clears the transient input (text buffer, `@file` popup,
  multiline mode) and arms a pending quit; a second within 3s quits. It is
  intercepted **once** at the top of `handle_event`'s key-press block (before
  any modal/approval routing), so behaviour is identical in every context —
  replacing the eleven duplicate `Char('c')` arms that used to quit on the first
  press. Ctrl+Q remains an unconditional immediate quit (the escape hatch); the
  first press does **not** close modals (`Esc` already owns that). An armed
  state shows a "Press Ctrl+C again to quit" hint in the input info bar. An
  **external** `SIGINT` (`kill -INT`, or a terminal that ignores crossterm's
  keyboard-enhancement flags — in raw mode Ctrl+C arrives as a key event with
  ISIG suppressed) is caught by a `tokio::signal::ctrl_c()` task spawned at TUI
  startup and forwarded as a synthetic `Event::Interrupt` through the same
  `App::handle_quit_key` path, so an out-of-band signal can never leave the
  terminal "half killed" (raw mode / alternate screen / mouse capture left on);
  the panic hook already covered crashes, this covers the signal path.

---

Previous: [§6 — Heads: ABI, stdio & WebSocket](stdio-and-serve.md) · Next: [§6 — The TUI head: lifecycle controls, commands & editor](tui-controls.md)
