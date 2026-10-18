# Heads & persistence §6 — The TUI head: lifecycle controls, commands & editor

> Part of the [heads & persistence module](../heads-and-persistence.md) of the [architecture overview](../../architecture.md).

  **Turn/session lifecycle controls** (#6, #516): bare `Esc` in Normal mode
  sends `InMsg::Stop` for the active session — **a deliberate behavior
  change** (ee85bc5): `Esc` used to quit the app; interrupting the in-flight
  turn is what it means now (matching the `Esc` an approval/`ask_user` park
  already sent), and quitting is `/exit`, two-stage `Ctrl+C`, or `Ctrl+Q`.
  `/stop [--all]`, `/pause [--all]`, `/continue [--all]` send the matching
  `Stop`/`PauseSession`/`ResumeSession` frame to the active session (or fan
  out to every live one), and `Ctrl+Space` toggles pause/resume on the active
  session (safe because the engine treats `PauseSession` on an idle session
  and `ResumeSession` on a non-paused one as idempotent no-ops,
  [ADR-0208](../../adr/0208-pause-resume-a-hold-between-cancel-and-hibernate.md)).
  The sessions modal adds lifecycle **quick keys** on the highlighted row
  (`tui/modal_events.rs`) — `s` stops its turn, `p` pauses, `r` resumes — the
  modal staying open so several sessions can be acted on in a row, plus
  `d`/`Delete` to delete the highlighted session's log (§6b, [persistence](persistence.md); a live
  session is refused with a status line). **Every single-target `Stop` site**
  (bare `Esc`, `/stop`'s bare form, the sessions modal's `s`, the command
  palette's `/stop` pick) routes through `tui/stop_command.rs::request_stop`
(originally #626,
  [ADR-0172](../../adr/0172-tui-stop-cascade-vs-detach-confirm-modal.md)): it
  used to arm a cascade-vs-detach confirm modal (`c` cascades, `Enter`/`y`/`d`
  detaches, `Esc`/`n` cancels) when the target was `WaitingAgent` with a live
  **sponsored** `propose_plan` build child
  (flagged by the since-deleted `SessionStarted.sponsored`). [ADR-0207](../../adr/0207-permission-modes-replace-agent-borne-authority.md)
  §7 retires that handoff entirely — plan approval switches the session's own
  mode instead of spawning a child, and the `sponsored` field left the wire
  with it — and with the confirm's
  premise gone, `request_stop` now **always** sends `Stop` immediately,
  unconditionally, for every target. `/stop --all` keeps its raw fan-out
  semantics unchanged. `/name <text>` sets the session's
  display name via `InMsg::SetSessionMeta`
  ([ADR-0151](../../adr/0151-settable-session-metadata.md)); the sidebar and
  sessions modal prefer the name over the short id and the live `action` over
  the first-prompt description line. `/undo` and `/rewind <N>`
  (`tui/undo_command.rs`, [ADR-0212](../../adr/0212-undo-rewind-via-per-write-file-backups.md))
  send `InMsg::RevertFiles` for the active session (`to_turn: None` / `Some(N)`);
  the engine applies it once the session is idle and the reducer renders the
  `FilesReverted` reply as an `undo` notice (`file_backup::summary`: what was
  restored, what was skipped and why, and that `bash`/`call`/`rhai` exec
  changes are not tracked). Bare `/rewind` is the picker: it lists the
  turns that still have revertable edits (turn, file count, prompt snippet)
  read straight from the backup store — no engine traffic.
  `/cd [--session|--always] [<dir>]` (`tui/cd_command.rs`, #764,
  [ADR-0224](../../adr/0224-session-working-directory.md)) sends
  `InMsg::SetWorkdir` for the active session (bare = the project root, `~`
  expanded); the flag is the escape-root approval for a directory outside the
  root, so a plain `/cd` there is refused with that hint. The reducer folds
  `WorkdirChanged` into the view; the status line shows `{workdir}` (relative
  to the TUI root, absolute outside it) and a fixed `⚠ cd <dir>` attention
  counter whenever the focused session's workdir differs from the TUI root,
  compared against the root canonicalized once at startup. `!bash` runs in the
  focused session's workdir. `/fork [N]`
  (`tui/fork_command.rs`, #786,
  [ADR-0223](../../adr/0223-non-compacting-session-fork.md)) sends
  `InMsg::ForkSession` for the active session. A `SessionForked` whose source
  is the active session moves the user into the fork (`app/out_event.rs`). The
  reducer (`session_view/reducer/fork.rs`) opens the fork's view on a lineage
  notice plus the inherited transcript rebuilt from the event's `context`, the
  same path a restored fork log takes. The sessions modal tags the row
  "⑂ forked from …". The persistence tap logs no record for an empty `Spawn`
  prompt, since the supervisor queues no turn for one. That covers the fork's
  spawn and `heads::start_fresh`'s (#752: every `run`/TUI launch with an
  agent, `--mode` or a config `mode:`), whose log used to start with a blank
  `Prompt`. Before this change, a resume replayed that record as a phantom
  blank user message, and `/rewind` (which numbers turns by `Prompt` records)
  counted it as turn 1. New logs number from the first real prompt. An old log
  keeps its blank record and its offset, consistently on both sides: replay
  counts the blank message as turn 1 too. `/init [--update]
  [instructions…]` (`tui/init_command.rs`, ✅ #732,
  [ADR-0222](../../adr/0222-no-silent-echo-first-run-onboarding-and-init.md)) sends
  a built-in prompt plus a bounded repo snapshot (the `@file` index — the capped
  `glob` walk, built once if not yet landed — summarized as notable files,
  top-level entries, a type histogram and ≤ 200 shallowest paths) as an ordinary
  `Prompt`; the model writes `AGENTS.md` with its own `write`/`edit`, graded by
  the session's mode. An existing `AGENTS.md` refuses a bare `/init`;
  `--update` asks for an in-place improvement. **User slash
  commands** (#733, [ADR-0221](../../adr/0221-user-slash-commands-are-head-side-prompt-templates.md)): the slash popup, `Ctrl+P` palette,
  input whisper and `/help <cmd>` list the built-ins plus the live
  `slash_commands` registry (description + `argument-hint`; `SlashPopup` holds
  the live handle). Enter on `/name args` that is not a built-in expands the
  template and sends it as an ordinary `Prompt`; the transcript shows a
  one-line notice naming the command's file, then the **expanded** prompt —
  the text the model gets and the log replays. Picking a user command from the
  palette stages `/name ` in the input for its arguments. An unknown `/word`
  still goes to the model as typed. **Modals and popups are mouse-clickable**
  (f48cec5, `tui/modal_events/click.rs::click_modal`): with a modal or the
  slash/`@file` popup open, a left click on a row selects it and fires the
  same action `Enter` would, a click outside the open modal's rect closes it
  (matching `Esc`), and the wheel moves the modal's selection — the
  transcript-selection path runs only when no modal/popup is open.
  **External editor + export** (✅ #13,
  [ADR-0029](../../adr/0029-external-editor-and-markdown-export.md), `tui::editor` +
  `tui::export`): `<leader>e` / `/editor` suspends the TUI and opens the resolved
  editor (`pick_editor`: the persisted `config.yml` `editor:` **wins over**
  `$VISUAL`→`$EDITOR`→`vi`, each source skipped when blank) on the input draft,
  reading the result back into the
  input box; `<leader>E` / `/export` writes the transcript to
  `<session>-<unix_secs>.md` and opens it. Both defer through a `UiEffect` on
  `App` that the event loop (terminal owner) runs, restoring the alternate screen
  symmetrically; an editor failure is logged, not fatal. **`@file` mentions +
  `!bash` passthrough** (✅ #15,
  [ADR-0030](../../adr/0030-tui-file-mentions-and-bash-passthrough.md), `tui::mention`):
  typing `@` opens a fuzzy file-completion popup over a startup snapshot of the
  working dir (`host::list_files`, minus `target`/`node_modules`/… trees);
  Tab/Enter inserts the pick as `@path` prompt text (the model reads it via the
  `read` tool — no content pre-expansion). An input starting with `!` is a
  head-side shell escape: the command runs through the existing `BashTool` and its
  output is injected into the transcript as a `!bash` tool call/output pair, local
  only (never sent to the engine). Always available now that `bash` is a
  default-registered built-in (ADR-0195 superseded ADR-0010's opt-in; the
  typed command itself is the consent this gate was for). **In-session inspection
  overlay** (✅ #214, `tui::modals::inspect` + `tui::app::inspect`): `<leader>i` /
  `/inspect` opens a read-only four-tab overlay (Prompt / Agents / Skills / Commands) over
  the **active session's** resolved state — the same views the CLI's `skutter
  inspect prompt|agents|skills` print (the CLI also has `inspect config` for the
  resolved user config, #172), so you can debug a misbehaving session
  without leaving the TUI. It reuses the identical engine-free renderers
  (`inspect::tui_reports` → the shared `render_*` helpers): the Prompt tab is the
  active agent's `--parts` breakdown; the Agents tab is the registry table plus
  the active agent's full detail — **identity and provenance only** since
  [ADR-0207](../../adr/0207-permission-modes-replace-agent-borne-authority.md)
  (name, description, model/provider pin, assembled-prompt length,
  overridden lower layers; no more permission/mask/spawn/plan-authorship
  columns to show — that posture is `skutter inspect modes`' job now, not
  yet surfaced as a fourth tab here); the
  Skills tab is the exact `disclosures()` block the model sees plus the full table
  of every resolved skill. Views resolve on open from the cwd + live agent, so
  they stay fresh across mid-session definition edits. The Agents and Skills tabs
  are **two-level** (✅ #331): a selectable list (name + summary + winning layer)
  where `j`/`k`/arrows move the highlight and `Enter` opens the per-item detail
  pane rendered by the same per-name code path the CLI uses (`inspect agents
  <name>` / `inspect skills <name>`); `Esc`/`Backspace` returns to the list, `Esc`
  again closes. The Prompt and Commands (the `inspect commands` table, #733)
  tabs stay single scroll-only documents. `Tab`/`←`/`→`
  switch tabs from either level, arrows/`j`/`k`/`PgUp`/`PgDn` (or the wheel)
  scroll the document panes, `Esc` closes. **`/key`
  dialog** (✅ #304, [ADR-0073](../../adr/0073-managed-env-file-writer-and-key-surfaces.md),
  `tui::key_dialog`): a two-stage modal after the `/model` pattern — a keyed-provider
  list, then a masked input (`masked()` renders bullets only, the key is never
  shown). On submit it drives the shared `config::env_key::set_key` writer and
  `std::env::set_var`, so the live model resolver (ADR-0063) binds the new key on
  the next `/model` switch — no restart (startup auto-detect still needs one). A
  status line (never the key) is recorded into the transcript; `Esc` wipes the
  buffer. The CLI twin is `skutter config set-key <provider> [--key V]`
  (`config::keys`, a pre-engine fast path like `inspect`): it resolves the catalog
  `key_env` (keyless Ollama → clean error), sources the value from `--key`, a
  hidden `rpassword` prompt, or piped stdin — never echoed — and warns when the
  process env already carries a *different* value (env > file). Its OAuth
  sibling is `skutter config connect|disconnect <provider>` (✅ #684,
  [ADR-0189](../../adr/0189-oauth-for-llm-provider-endpoints.md),
  `config::llm_connect`): for a catalog entry carrying an `oauth:` block, it
  runs the browser-loopback (or `--device-code`) authorization against the
  entry's resolved base URL and persists the credential to the managed
  `llm-tokens.yml` (in the managed-state dir, `ENTANGLEMENT_STATE_DIR`) — the same file
  format, locking, and quarantine as `mcp-tokens.yml` (one shared
  implementation, `McpTokenStore::load_llm()`), keyed by provider name and a
  deliberately *separate* file so neither surface's writes contend with the
  other's credentials. `disconnect` revokes best-effort (RFC 7009) then
  deletes locally. **Session settings dialog** (`tui::settings_dialog`,
  `tui::app::settings`, `tui::modals::settings`): bare `/set` (or `/set` from
  the palette) opens a tabbed modal; `/set <key> <value>` stays the ADR-0095
  one-shot. `Tab`/`Shift+Tab` or a click on a title switch tabs, `↑↓` move,
  `←→`/`Space` change the focused row, `Enter` confirms, `Esc` cancels
  everything; a footer lists the pending changes. Tabs: **Session** — agent
  and provider/model over the `/agent`/`/model` data (the agent switch is
  session-only); **Generation** — only the knobs the *selected* model
  accepts, re-derived live when the Session tab's model changes: temperature
  unless `supports_temperature: false`, reasoning effort over the catalog
  `effort_tiers` (all five when unset, hidden when empty), a thinking budget
  only for fixed-budget (`thinking_style: budget`) models on a budget-reading
  wire, max output tokens; every field offers "model default", sent as the
  catalog's concrete default because `SetGeneration` merges and cannot clear
  (a field with no catalog default is reported, not sent), with an inline
  note that effort/thinking changes rebuild the prompt cache; **Tools** —
  the per-session overlay rows bare `/enable` uses (the advertising-mode and
  discovery rows and whole-server MCP enable rows are gone with
  [ADR-0226](../../adr/0226-one-advertised-surface-mcp-envelope-registered-is-available.md):
  one surface, MCP registered = available); **Aux** — the per-purpose pins. Each tab has its own "save as
  default": Session → the agent-models pin (ADR-0081), Generation → the
  agent-generation store once `GenerationChanged` confirms (ADR-0095), Tools →
  the overlay materialized as the agent's user-layer allowlist (ADR-0083,
  next restart), and Aux pins are process-wide and always persisted (checkbox
  always on). Confirming runs one plan in a fixed order — agent → model →
  generation (validated against the final model, incl. an agent pin the
  switch rebinds to) → overlay → aux — each step through its single-purpose path, stopping at the
  first failure; one `settings` transcript line names what applied, what
  failed and what didn't run (the effort cache warning stays the reducer's
  own).

---

Previous: [§6 — The TUI head](tui.md) · Next: [§6c–§6d — Managed key file, onboarding & auxiliary models](keys-and-aux-models.md)
