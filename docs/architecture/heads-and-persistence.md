# entanglement Architecture — Heads & session persistence

> Part of the [architecture overview](../architecture.md). The *why* behind each choice is in the [decision log](../adr/README.md).

## 6. Heads — ADRs [0005](../adr/0005-ndjson-stdio-head.md) (stdio), 0001 (ABI), [0010](../adr/0010-single-head-crate-and-bash-opt-in.md) (packaging), [0011](../adr/0011-tui-head-ratatui-crossterm.md)–[0015](../adr/0015-rich-text-pipeline-syntect.md) (TUI)

All heads live in one crate, **`entanglement-runtime`** (✅ #56; binary
`skutter`), as subcommands. The "four interfaces"
(in-process ABI + three transports) are a design concept, not a packaging
boundary — the real seam is `entanglement-core` ↔ everything else (ADR-0006,
ADR-0010).

The heads (and the `skutter` binary that carries them) need the crate's
**default features** — `default = ["tui", "serve", "mcp-http", "rhai"]` pulls
clap + the providers + the render stack + the axum WS server + the
streamable-HTTP MCP transport (ADR-0080) + the sandboxed script tool
([ADR-0135](../adr/0135-deferred-build-speed-trims-tokio-rhai-syntect.md)),
and `[[bin]] skutter` declares
`required-features = ["cli","provider","tui"]` (the `provider` feature was split
out of `cli` in #208 so the `ws` `serve` head — added in #153 behind its own
`serve` feature, `serve = ["cli","provider","dep:axum","dep:futures"]` — pulls
providers without dragging in clap or the TUI stack).
Building the crate with `default-features = false` yields an **embeddable
library** — the tool-execution loop, permission dispatch, sub-agent spawn, and
persistence machinery with none of the CLI/TUI/transport weight
([ADR-0025](../adr/0025-runtime-cargo-feature-gates.md), §7). Wiring a custom,
multi-tenant head on top of this library (session namespacing, the trust
split, pluggable persistence/policy, approval-across-restart) is covered in
[`../embedding.md`](../embedding.md), backed by a compiling
[`examples/embedded.rs`](../../entanglement-runtime/examples/embedded.rs).

- **ABI** — `holly.send()` / `holly.subscribe()`. Done.
- **stdio** (`skutter run` / `skutter pipe`): one-shot `run [--format text|json]
  [--agent <name>] [--session <id> | --resume <id>] [--yes]`; bidirectional
  `pipe` NDJSON (`InMsg` in, `OutEvent` out). `skutter sessions` lists past root
  sessions for the cwd (see §6b). `run` has no interactive user to answer a
  `ToolRequest` (most commonly the escape-root gate forcing a prompt for an
  out-of-root path even under a profile's `Allow`, ADR-0109) or a `propose_plan`
  approval, so both are settled immediately instead of parking until the relay
  loop's 60s `recv` timeout kills the whole run (#554): `propose_plan` always
  auto-rejects (accepting hands off to a `build` child with its own review loop,
  which a headless run can't drive either way); every other `ToolRequest`
  auto-rejects with a reason by default, or auto-approves (`Once` scope) when
  `--yes`/the global `skutter … --yes` flag is set. `ask_user` questions keep
  auto-answering their first option (ADR-0027). `skutter inspect prompt --agent <name> [--parts]` prints an
  agent's **assembled** system prompt (#184) — it re-runs the load-time discovery
  (`PromptContext::load` + skill/agent registries) with no engine, so a wrong
  brief pick, an empty preamble override, or a subagent losing the skill index is
  visible before model behaviour degrades; `--parts` breaks the prompt into its
  component slices, each tagged with the source it came from (built-in default,
  brief path, generated env, …). A load-time `debug!` (`agent=… prompt_len=…
  brief=<path|none> skills=…`) surfaces the same facts during any run.
  `skutter inspect agents [name]` (#185) surfaces the **layer-collision winner**
  the silent later-wins `insert` used to swallow: with no `name`, a table (name,
  mode, model, winning layer, source path, tool-mask summary) of every resolved
  agent; with a `name`, the full resolved profile (permission rules, tool mask,
  spawn control, plan authority, assembled-prompt length) **plus** which
  lower-layer definitions it overrode — the exact fields #116/#119/#140
  enforcement hinges on. Same engine-free discovery as `inspect prompt`, via a
  `(layer, source)` provenance sidecar (`agents::resolve_registry`); `load_registry`
  also emits a `replaces=<prior layer>` `debug!` at each overriding insert.
  `skutter inspect skills [name] [--disclosures]` (#186) does the same for the
  **skill** registry — the authoring loop was "start a session and ask the model":
  no `name` prints a table (name, user_only, winning layer, `root_dir`,
  description); `--disclosures` prints the **exact** tier-1 block the model
  receives (the same `system_prompt::render_skills` output the assembled prompt
  embeds, `user_only` skills withheld); a `name` **dry-runs the `load_skill` path
  substitution** (`${SKILL_DIR}` + relative-ref resolution) plus layer provenance,
  so a wrong payload path surfaces without a model. Engine-free via
  `skills::resolve_registry` (the `(layer, source, shadowed)` sidecar mirroring
  agents); `load_registry` emits a `replaces=<prior layer>` `debug!` at each
  overriding insert, and a broken symlink under a skills dir is now a `warn!`
  (was a silent skip). Logs go to **stderr**, keeping stdout clean for the
  prompt / disclosures / NDJSON frames — **except under the TUI**, whose raw mode
  owns the screen, so there logs are appended to
  `<data_dir>/entanglement/logs/skutter.log` (path echoed to stderr at startup).
  The filter honors `RUST_LOG` first (`EnvFilter::try_from_default_env`, so
  per-target directives and `trace` are reachable — e.g.
  `RUST_LOG=entanglement_core::host=trace`); absent it, `--verbose` (a **global**
  flag, so it may follow the subcommand) selects `debug`, otherwise `warn`
  (issue #187, `runtime::logging`). `inspect config` (#172) prints the resolved
  user config with per-field provenance — every fallback setting in
  [`config::Config`](../../entanglement-runtime/src/config/mod.rs), including
  `max_turns`/`idle_ttl_secs`/`auto_compact`/`editor`/`session_retention_days`
  (#558; `session_retention_days` reports `env (…)` as its source instead of a
  file layer when `ENTANGLEMENT_SESSION_RETENTION_DAYS` actually won). Four more
  engine-free CLI-only views round out the managed-file/live-state blind spots
  (#558, deferred-work-ledger row 10c): `inspect aux-models` prints the
  persisted `/aux-model` pins (`aux-models.yml`); `inspect mcp-tokens` prints
  which MCP servers hold a stored OAuth credential (`mcp-tokens.yml`,
  ADR-0153), redacted — never a token value; `inspect mcp` prints the resolved
  bundled/available MCP server roster (provider-bundled servers folded with the
  user `mcp:` map, #542) — which servers auto-connect vs. wait for `/enable`;
  `inspect session <id>` folds one **root** session's persisted log into its
  resolved agent/model/name plus its live tool overlay (ADR-0149) — the one
  piece of session state with no managed file of its own, so it is read
  straight from the `.jsonl` log rather than re-run load-time discovery.
- **WebSocket** (`skutter serve`, ✅ #153, `runtime::serve` behind the `serve`
  cargo feature): an axum HTTP server exposing `GET /ws` (plus a `GET /healthz`
  liveness probe), one `subscribe()` fan-out per socket relayed out as JSON text
  frames, each inbound text frame parsed into an `InMsg` and routed through the
  **untrusted** `send_from_wire` (#155) so a forged `ToolResult`/`Spawn`/`Resume`
  is refused per-frame (a non-JSON line falls back to a `Prompt` on the socket's
  own default session, `pipe` parity); a 30s ping keeps an idle socket alive and
  a `broadcast::Lagged` is a dropped-events gap → `continue`, never a silent
  relay death (#158). Scoped **local, single-user, loopback-bound**: reached via
  `--port <N>` and **always** bound to `127.0.0.1` (no non-loopback bind is
  offered — the loopback bind is the one required non-public control). The WS is
  a general protocol interface (the future Vue SPA is the primary but not
  exclusive client — raw local scripts/CLIs/plugins are supported), so the
  `--allow-origin <ORIGIN>` check is **opt-in, never mandatory** (unset ⇒ every
  origin, including a raw client that sends none, is accepted) and the
  browser-page surface is out of scope. The wire hygiene it consumes (`seq`
  uniqueness #157, protocol warts #160) was frozen first, per the pre-`serve`
  hardening epic ([ADR-0048](../adr/0048-serve-head-local-trust-model.md)). Lives
  behind the `serve` feature (implies `cli` + `provider`) so axum stays out of
  the lean library / `--no-default-features` build (ADR-0025). **Per-connection
  approval ownership** (#402, [ADR-0107](../adr/0107-ws-per-connection-approval-ownership.md)):
  a `SessionOwners` map on `ServeState` claims a session for the first
  connection to send any frame referencing it (first-writer-wins); a later
  connection's `Approve`/`Reject`/`AnswerQuestion` for that session is refused
  (logged, dropped, connection unaffected) — every other `InMsg` variant is
  unaffected by ownership. Released on disconnect, so a still-parked approval
  is reclaimable rather than deadlocked behind a client that went away.
- **TUI** (`skutter tui`): opencode-style terminal UI over `subscribe()`. Uses
  ratatui + crossterm (ADR-0011), leader-key bindings with which-key popup
  (ADR-0013), inline tool approval cards (ADR-0014), and rich markdown
  rendering with pulldown-cmark + syntect (ADR-0015). Event buffering and
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
  (opt out with `ENTANGLEMENT_TUI_NO_MOUSE=1`, which restores native terminal
  text selection): the wheel scrolls the chat (or the open modal's selection).
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
  operation** as a single collapsible `▸ {tool}  {primary_arg}  ✓` line with its
  paired output folded in (#340; the `ToolOutput` matches its `ToolCall` by
  `request_id`, so batch results still pair correctly), both expanded on click
  (or via the leader `t` key, which toggles the most recent block of either kind).
  The bottom **info line** (`draw_input_info`) shows `provider · model | tokens`
  plus one transient status slot — a pending two-stage quit hint, else the
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
  transition (#517, [ADR-0141](../adr/0141-wire-visible-throttle-transitions.md)) —
  engine-global, not per-session, since the resilience pool is per-endpoint
  (ADR-0050). `run --format text` renders it in full; `serve` relays it like
  any other frame. **Attention signals** (issue #14, `tui::attention`):
  a `Status` transition into `WaitingApproval`, `Done`, or `Error` rings the
  terminal bell — and, opt-in via `ENTANGLEMENT_TUI_NOTIFY=1`, emits an OSC 9
  desktop notification (iTerm2/kitty/WezTerm; silently dropped elsewhere). Core
  emits `Status` only on a state change, so signalling on those states *is*
  signalling on the transitions; `Done`/`Error` also arrive as their own
  `OutEvent` variants but only `Status` is watched, so a turn end rings once.
  Focus reporting (crossterm `EnableFocusChange`) mutes signals while the
  terminal is focused, but best-effort only — terminals that never report focus
  always signal. **Attention panel + jump**
  ([ADR-0136](../adr/0136-tui-attention-panel-signpost-and-jump.md),
  `tui::ui::alerts`): a **background** session parked on an approval or
  `ask_user` question surfaces as a one-line panel directly above the input box
  — absent (a `Length(0)` layout row) while nothing waits — naming the oldest
  waiting session (short id + agent) and what it asks (`needs approval: tool
  arg` / `question: text`), plus a count when several wait; the status bar
  shows a matching `⚠ N` off the same aggregation (derived per frame from the
  views' pending queues, not the flappy `Status`, #273). `Ctrl+G` — or a click
  on the panel — switches to that session, where the existing
  approval/question UI takes over unchanged (a *signpost + jump*, deliberately
  not a second approval keymap; the active session's own pending request
  already renders as the transcript tail, so the panel covers background
  sessions only). Sessions are identifiable throughout: the sidebar, status
  bar, and sessions modal show an **8-char short id** instead of the full
  UUID; the sidebar adds a dim per-session **first-prompt description line**
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
  child session renders dim with a `✓`. **Two-stage Ctrl+C** ([ADR-0087](../adr/0087-two-stage-ctrl-c.md)):
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
  [ADR-0144](../adr/0144-pause-resume-a-hold-between-cancel-and-hibernate.md)).
  The sessions modal adds lifecycle **quick keys** on the highlighted row
  (`tui/modal_events.rs`) — `s` stops its turn, `p` pauses, `r` resumes — the
  modal staying open so several sessions can be acted on in a row, plus
  `d`/`Delete` to delete the highlighted session's log (§6b below; a live
  session is refused with a status line). `/name <text>` sets the session's
  display name via `InMsg::SetSessionMeta`
  ([ADR-0151](../adr/0151-settable-session-metadata.md)); the sidebar and
  sessions modal prefer the name over the short id and the live `action` over
  the first-prompt description line. **Modals and popups are mouse-clickable**
  (f48cec5, `tui/modal_events.rs::click_modal`): with a modal or the
  slash/`@file` popup open, a left click on a row selects it and fires the
  same action `Enter` would, a click outside the open modal's rect closes it
  (matching `Esc`), and the wheel moves the modal's selection — the
  transcript-selection path runs only when no modal/popup is open.
  **External editor + export** (✅ #13,
  [ADR-0029](../adr/0029-external-editor-and-markdown-export.md), `tui::editor` +
  `tui::export`): `<leader>e` / `/editor` suspends the TUI and opens the resolved
  editor (`pick_editor`: the persisted `config.yml` `editor:` **wins over**
  `$VISUAL`→`$EDITOR`→`vi`, each source skipped when blank) on the input draft,
  reading the result back into the
  input box; `<leader>E` / `/export` writes the transcript to
  `<session>-<unix_secs>.md` and opens it. Both defer through a `UiEffect` on
  `App` that the event loop (terminal owner) runs, restoring the alternate screen
  symmetrically; an editor failure is logged, not fatal. **`@file` mentions +
  `!bash` passthrough** (✅ #15,
  [ADR-0030](../adr/0030-tui-file-mentions-and-bash-passthrough.md), `tui::mention`):
  typing `@` opens a fuzzy file-completion popup over a startup snapshot of the
  working dir (`host::list_files`, minus `target`/`node_modules`/… trees);
  Tab/Enter inserts the pick as `@path` prompt text (the model reads it via the
  `read` tool — no content pre-expansion). An input starting with `!` is a
  head-side shell escape: the command runs through the existing `BashTool` and its
  output is injected into the transcript as a `!bash` tool call/output pair, local
  only (never sent to the engine). Gated on `ENTANGLEMENT_ENABLE_BASH=1`, the same
  opt-in as the model-facing `bash` tool (ADR-0010). **In-session inspection
  overlay** (✅ #214, `tui::modals::inspect` + `tui::app::inspect`): `<leader>i` /
  `/inspect` opens a read-only three-tab overlay (Prompt / Agents / Skills) over
  the **active session's** resolved state — the same views the CLI's `skutter
  inspect prompt|agents|skills` print (the CLI also has `inspect config` for the
  resolved user config, #172), so you can debug a misbehaving session
  without leaving the TUI. It reuses the identical engine-free renderers
  (`inspect::tui_reports` → the shared `render_*` helpers): the Prompt tab is the
  active agent's `--parts` breakdown; the Agents tab is the registry table plus
  the active agent's full detail (permission / mask / spawn / plan authorship); the
  Skills tab is the exact `disclosures()` block the model sees plus the full table
  (including `user_only`). Views resolve on open from the cwd + live agent, so
  they stay fresh across mid-session definition edits. The Agents and Skills tabs
  are **two-level** (✅ #331): a selectable list (name + summary + winning layer)
  where `j`/`k`/arrows move the highlight and `Enter` opens the per-item detail
  pane rendered by the same per-name code path the CLI uses (`inspect agents
  <name>` / `inspect skills <name>`); `Esc`/`Backspace` returns to the list, `Esc`
  again closes. The Prompt tab stays a single scroll-only document. `Tab`/`←`/`→`
  switch tabs from either level, arrows/`j`/`k`/`PgUp`/`PgDn` (or the wheel)
  scroll the document panes, `Esc` closes. **`/key`
  dialog** (✅ #304, [ADR-0073](../adr/0073-managed-env-file-writer-and-key-surfaces.md),
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
  process env already carries a *different* value (env > file).

## 6c. Managed provider-key env file — [ADR-0073](../adr/0073-managed-env-file-writer-and-key-surfaces.md) (`config::env_file` + `config::env_key`)

Provider API keys live in `${config_dir}/entanglement/.env` (override
`ENTANGLEMENT_ENV_FILE`, #220), a sibling of `config.yml`. `env_file` scaffolds a
commented `#KEY=` template on first run and `load()`s `KEY=VALUE` lines into the
process env for any var the real environment left unset (env > file). `env_key`
(✅ #304) is the **writer** both key surfaces above share: a pure `upsert(text,
key, value)` (replace the first *live* `KEY=` line — first-occurrence-wins,
matching `load()` — else the `#KEY=`/`# KEY=` placeholder, else append; other
lines byte-for-byte preserved; idempotent) plus `set_key(key, value) ->
Result<PathBuf>` (loud error with no managed path; create from `template` when
missing; atomic temp-file-in-dir + rename; `0o600` on unix; reject empty/`\n`
values). `env_key` is pure std + `anyhow` (lean/gate-clean); only the `keys`
handler (rpassword + catalog) is feature-gated behind `cli`+`provider`.

## 6d. Per-purpose auxiliary models & auto session titles — [ADR-0154](../adr/0154-per-purpose-auxiliary-models.md) (`aux_llm` + `config::aux_models` + `session_title`)

Side transformations (a compaction summary, an auto session title) can run on
a cheaper/faster model than the session's own. The pin store is a managed
`${config_dir}/entanglement/aux-models.yml` (override
`ENTANGLEMENT_AUX_MODELS_FILE`, sibling of `agent-models.yml`, same
`with_locked_file`+`atomic_write` discipline) mapping a closed `Purpose` enum
(`summarize` | `session_title`) to a `{provider, model}` pin.
`aux_llm::AuxLlmRegistry` resolves a `Purpose` → fresh `Box<dyn Llm>` by
reusing the catalog `ModelResolver` the runtime already builds at startup (the
same closure `SetModel` calls, capturing the catalog + the warm per-endpoint
client, so an aux client rides the shared pool — and a second provider is just
a second endpoint pool/key, already supported), **falling back to the primary
model's `LlmFactory`** when a purpose is unset or its pin no longer resolves
(unknown to the catalog, missing key — logged at debug, never wedging the
transformation). Two consumers reach it by deliberately different routes:

- **The session-title generator** (`session_title.rs`, behind the `provider`
  feature): a background task off `holly.subscribe_inbound()` that, on the
  **first `Prompt` of an unnamed session**, spawns a detached one-shot call to
  the `session_title` aux LLM (prompt capped ~2k chars, output capped 80) and
  sets the result via an `InMsg::SetSessionMeta` with **`if_unset: true`**
  ([ADR-0151](../adr/0151-settable-session-metadata.md)) — best-effort
  throughout (a failed/empty generation is logged and dropped), idempotent per
  session (a titled-set guards late second prompts), and never clobbering an
  already-named session (#553): `if_unset` makes the engine's fold a no-op
  once `Session.name` is already `Some(_)`, so a late generated title can
  never win a race against — or overwrite — a `/name` set (in either arrival
  order) or a name a resumed process's replay already restored. The generator
  also folds a `Resume`'s replayed `SessionMetaChanged` history (per session
  id, last-write-wins) off the same inbound fan-out to pre-seed its
  in-process titled-set, so an already-named resumed session skips the aux
  call on its next prompt too, not just the write. It has no session backend
  to fall back to, so it calls `AuxLlmRegistry::resolve` directly and an unset
  pin yields the primary model. It fires **concurrently** with the main turn
  by default — except when `AuxLlmRegistry::concurrency_cap(SessionTitle)`
  reports the resolved model's effective per-model cap
  ([ADR-0140](../adr/0140-per-model-concurrency-cap-layered-on-endpoint-cap.md))
  at 1 or below ([ADR-0158](../adr/0158-defer-session-title-aux-call-under-contended-primary-concurrency.md),
  #589): with only one permit, the main turn's own request is guaranteed to
  hold it first, so the generator instead waits for that session's first
  `Done`/`Error` (bounded, 300s safety net) before making its aux call,
  sequencing the two instead of racing for a permit neither can win early.
- **Session compaction** runs *inside core*, which reaches the pin through the
  `EngineConfig::aux_llm_resolver` seam built by `AuxLlmRegistry::resolver`
  (see the engine doc's *Auxiliary models* section): there `None` means "use
  the session's own backend" — strictly better than a fixed primary, since a
  live `/model` switch keeps applying to compaction.

The TUI surface is `/aux-model <purpose> <provider>/<model>`
(`parse_aux_model_args` — the raw-text re-parse pattern; `title` is accepted
as an alias for `session_title`; bare `/aux-model` or `/aux-model list`
renders the current pins), writing the pin through the shared store handle so
the live registry sees it with no restart.

## 6b. Session persistence & resume — [ADR-0020](../adr/0020-event-sourced-session-persistence.md) (`persistence` + `session_store`)

Sessions are event-sourced to disk, one JSONL file per **root** session under
`<data_dir>/entanglement/sessions/<safe-cwd>/<root_id>.jsonl` (`session_store`).
`spawn_persistence_subscriber` (`persistence`) taps **both** directions of the
ABI — `holly.subscribe()` for `OutEvent`s and `holly.subscribe_inbound()` for
`InMsg`s — and appends each frame as a `LogRecord { ts, session, payload }` where
`payload` is `LogPayload::In(InMsg) | Out(OutEvent) | Gap { dropped }` (the last
is a tombstone, below). Logging inbound messages is
what makes a session resumable: `Session::replay` reconstructs user turns from
the logged `InMsg::Prompt` records, so without them a resumed context holds only
assistant/tool messages and the model appears to forget the conversation.

- **Inbound is biased ahead of outbound** so a prompt lands on disk before the
  events it produces (`pair_records` pairs each `Out` with the preceding `In`).
  `InMsg::Resume` is skipped (it carries the whole prior log → recursion/bloat).
  `InMsg::Spawn` is still never persisted verbatim (`roots` can't resolve the
  child to its parent's root file until the child's own `SessionStarted`
  arrives — logging it earlier would create a stray child root). Its `prompt`
  **is** captured, though (#421,
  [ADR-0113](../adr/0113-persistence-synthesizes-a-spawned-childs-initiating-prompt.md)):
  the tap caches it (`pending_spawn_prompts: HashMap<SessionId, String>`, keyed
  by child id) and, once that `SessionStarted` resolves `roots`, synthesizes an
  `InMsg::Prompt { session: child, .. }` record right after it — so replay
  reconstructs the task instruction that framed the child's first turn, not
  just its eventual reply. The cache entry is consumed on first use, so a
  resumed child's re-announced `SessionStarted` (resume never re-sends `Spawn`)
  never re-synthesizes or duplicates the record.
- **Spawned children fold into the root file** via a `roots` map built from
  `SessionStarted { root, parent }`, so each root file is a self-contained,
  replayable record of the whole session tree.
- **Resume** reads the file, `pair_records` builds the `(Option<InMsg>, OutEvent)`
  stream, and `Holly::resume` seeds a session from `Session::replay`. The CLI
  exposes `skutter run --resume <id>` and `skutter sessions` (lists past root
  sessions for the cwd); the TUI `/resume` modal restores the full visible
  transcript (`restore_from_records`) *and* reseeds engine context. **Resume
  cascades over the spawn sub-tree** (#415,
  [ADR-0112](../adr/0112-resume-cascades-over-the-spawn-subtree.md)): since a
  root file already carries every spawned child's interleaved records
  (previous bullet), the supervisor doesn't stop at re-materializing the
  requested id — it walks that session's replay-reconstructed `children` and
  recursively `Session::replay`s + re-spawns each one still "live" in the log
  (a `SessionStarted` with no matching `SessionEnded`/`SessionHibernated`),
  mirroring `CloseSession`/`HibernateSession`'s teardown cascade in reverse. A
  parent resumed after a crash/hibernation can still reach and continue its
  sub-agents instead of them silently vanishing (a lazy-respawn under an
  untouched child id would otherwise come back blank, with no prior history).
  Both listings carry a **first-prompt snippet** (#327): `list_sessions` captures the
  first `InMsg::Prompt` in the same pass that finds `SessionStarted` (no extra
  I/O), truncates it to ~60 chars on a word boundary with `…`
  (`SessionMeta::first_prompt`), and both `skutter sessions` (DESCRIPTION column)
  and the `/resume` rows render it beside the bare UUID. The in-memory
  `ListSessions`/`SessionList` supervisor query is unaffected (no capture-at-spawn).
- **Mid-turn tails are resumable** (#271/#272,
  [ADR-0061](../adr/0061-parked-turn-state-batch-tool-resolution.md)). A log
  ending after `ToolCall`/`ToolExec` with no matching `ToolOutput` replays into
  a **parked `TurnState`**: the completed assistant message commits, logged
  outputs fold, and the unanswered calls become `Session.turn.pending`. On
  resume the session **re-offers** each pending call as a fresh `ToolExec`
  (same `request_id`, fresh `seq`) — the tool executor, or any external
  resolver holding a `Holly` handle, answers it like a first offer;
  **at-least-once**, so a tool that ran before the crash but whose result never
  reached disk runs again. A drained tail (all results logged, next round never
  streamed) continues the turn directly; a text-only tail (mid-stream crash)
  stays dropped. This event-log + `Holly::resume` path is also the persistence
  seam for embedders of `entanglement-core`: records are serde values storable
  anywhere (a DB, a queue); the JSONL store here is the reference
  implementation.
- **Compaction is copy-on-write — it forks, never mutates** (#324,
  [ADR-0082](../adr/0082-single-shot-session-ops-and-persisted-compaction.md) →
  [ADR-0101](../adr/0101-compaction-forks-into-a-new-session-copy-on-write.md)).
  `InMsg::Oneshot`'s `"compact"` op emits `OutEvent::Compacted{summary,kept}` —
  an ordinary seq-bearing content event, so it needed **zero** persistence-tap
  code: the tap already appends every `OutEvent` with `session().is_some()`
  regardless of variant, and the `ReplayFrom` history responder (§6, below)
  already includes every event with `seq().is_some()`. **But the source session
  is never mutated** (ADR-0101): the summary rides only in the event, and the
  head forks it into a new session via `InMsg::Spawn`. `Session::replay`'s
  `Compacted` fold is a **no-op** — a resumed source recovers its full
  pre-compaction history (the implicit undo), and a truncated summary is refused
  outright (`StopReason::MaxTokens` → `Error`) so it never forks either.
  **Keep-tail** (#397,
  [ADR-0102](../adr/0102-compact-keep-tail-verbatim-in-the-fork-prompt.md)):
  `args.kept: u64` (optional, default `0`) requests that the last `kept`
  messages ride into the fork **verbatim** instead of being paraphrased into
  the summary. `Context::safe_kept` clamps the request to the nearest safe
  turn boundary — the tail must start at a `User` message, or a `Tool` reply
  could replay without its paired `Assistant` tool-call half, breaking
  providers' `tool_use`/`tool_result` pairing (ADR-0082's deferred-to-v1
  blocker). `compact_op` summarizes only the *head*, then appends the tail's
  rendered transcript after the summary — the composed text ships inside the
  same `summary` field, so **no wire change** was needed and the TUI's
  existing fork path (`wrap_compaction_summary`/`InMsg::Spawn { prompt, .. }`)
  carries it unmodified. The TUI's `/compact [--keep N] [instructions]`
  (`tui::commands::parse_compact_args`) is the head-side entry point; the
  command-palette pick still defaults to `kept: 0`.
- **Auto-compaction is the in-place exception to copy-on-write** (#398,
  [ADR-0103](../adr/0103-auto-summarize-on-context-overflow.md)). A turn
  overflowing its context window mid-flight has no head to fork into, so
  `session/turn.rs` mutates `Context` directly via `apply_compaction` and
  marks the same wire event `OutEvent::Compacted { auto: true, .. }` (`false`
  is the default, matching every pre-#398 — i.e. manual, copy-on-write —
  record). `Session::replay`'s `Compacted` fold branches on it: `auto: false`
  stays the no-op described above; `auto: true` flushes whatever pending
  assistant/tool state has accumulated (mirroring the `Done` fold) and then
  calls the same `apply_compaction(summary, kept)` the live engine ran, so a
  resumed session's history matches the live one instead of recovering a
  pre-compaction tail that was never actually live. The summary here carries
  *no* rendered tail text (unlike the copy-on-write report above) — `kept`
  alone is enough for both the live mutation and its replay to re-derive the
  same tail structurally from `Context::messages()`.
- **Pluggable append target — `RecordSink`** (#313,
  [ADR-0086](../adr/0086-recordsink-pluggable-persistence-append-target.md)).
  The tap's *what to persist*
  (route each record to its root, tombstone lag gaps) is split from its *where to
  persist*: it appends every finished `LogRecord` through a
  `RecordSink { fn append(&self, root: &SessionId, record: &LogRecord) }`, and an
  embedder swaps in any target without forking the subscriber (and so tracks
  upstream gap/lag fixes for free). The default `FileSink` is the JSONL store
  above; `spawn_persistence_subscriber(holly, cwd)` is just
  `spawn_persistence_subscriber_with_sink(holly, Arc::new(FileSink::new(cwd)))`.
  `append` is **synchronous** — the file sink is one `writeln!`. A sink whose
  store can block (DB, network) must **not** block the tap: that starves the
  broadcast receiver and manufactures the very `Gap` tombstones the tap exists to
  avoid. Such a sink puts a bounded channel + writer task behind `append` and
  returns immediately, surfacing back-pressure as an `Err` (dropped past the
  bound) rather than awaiting. `session_store::read`/`pair_records` stay the
  file-side read helpers; resume already accepts records from anywhere
  (`Holly::resume(root, records)`), so no read-side trait is needed.
- **One-shot flush**: a `run` invocation ends the moment the turn does, so `main`
  aborts the tool executor and drops its `Holly` handle to close the broadcast
  channels, then awaits the persistence task so buffered events reach disk before
  the process exits.
- **Log integrity — never resume a hole** (#104). The persistence tap reads
  Holly's *lossy* broadcast, so a fast turn that outruns disk appends can drop a
  contiguous run of events (`RecvError::Lagged`) — a well-formed file whose
  history is silently incomplete. On lag the tap writes a `Gap { dropped }`
  tombstone into every known root file (a lag can't say *which* session lost
  records, so all are marked); `integrity_gap` detects it and both resume paths
  (`skutter run --resume`, the TUI modal) **refuse** rather than fold an
  incomplete context. `session_store::read` likewise distinguishes a
  crash-truncated *tail* line (tolerated with a warning) from *interior*
  corruption (a hole → hard error), and `list_sessions` skips-and-warns per bad
  file instead of aborting the whole enumeration.
- **Deletion + startup auto-prune** (Issue 4, 15175e0). Session logs can be
  deleted explicitly — `d`/`Delete` on the TUI sessions modal removes the
  highlighted session's `.jsonl` (a **live** session is refused with a status
  line: the modal lists the live set, and deleting under one would orphan its
  view) — and age out automatically: `session_store::prune(cwd,
  retention_days)` runs **best-effort at startup** (`main.rs`, before any head
  spawns, so the resume modal and `skutter sessions` both reflect the pruned
  set), removing root files older by mtime than the retention window from
  **the current project's session dir only**. The window resolves env >
  config > default: `ENTANGLEMENT_SESSION_RETENTION_DAYS` >
  `config.yml`'s `session_retention_days` > the embedded default `30`; an
  unreadable dir/entry is a warn-and-skip, never fatal.
