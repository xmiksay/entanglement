# entanglement Architecture — Heads & session persistence

> Part of the [architecture overview](../architecture.md). The *why* behind each choice is in the [decision log](../adr/README.md).

## 6. Heads — ADRs [0005](../adr/0005-ndjson-stdio-head.md) (stdio), 0001 (ABI), [0010](../adr/0010-single-head-crate-and-bash-opt-in.md) (packaging), [0011](../adr/0011-tui-head-ratatui-crossterm.md)–[0015](../adr/0015-rich-text-pipeline-syntect.md) (TUI)

All heads live in one crate, **`entanglement-runtime`** (✅ #56; binary
`skutter`), as subcommands. The "four interfaces"
(in-process ABI + three transports) are a design concept, not a packaging
boundary — the real seam is `entanglement-core` ↔ everything else (ADR-0006,
ADR-0010).

The heads (and the `skutter` binary that carries them) need the crate's
**default features** — `default = ["tui", "serve", "mcp-http"]` pulls clap +
the providers + the render stack + the axum WS server + the streamable-HTTP
MCP transport (ADR-0080), and `[[bin]] skutter` declares
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
  [--agent <name>] [--session <id> | --resume <id>]`; bidirectional `pipe` NDJSON
  (`InMsg` in, `OutEvent` out). `skutter sessions` lists past root sessions for
  the cwd (see §6b). `skutter inspect prompt --agent <name> [--parts]` prints an
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
  (issue #187, `runtime::logging`).
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
  the lean library / `--no-default-features` build (ADR-0025).
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
  (opt out with `ENTANGLEMENT_TUI_NO_MOUSE=1`, which restores native text
  selection): the wheel scrolls the chat (or the open modal's selection), and a
  left click hit-tests the chat area to toggle a transcript block — reasoning
  runs render collapsed as a `▸ Thinking (N lines)` header, and each **tool
  operation** as a single collapsible `▸ {tool}  {primary_arg}  ✓` line with its
  paired output folded in (#340; the `ToolOutput` matches its `ToolCall` by
  `request_id`, so batch results still pair correctly), both expanded on click
  (or via the leader `t` key, which toggles the most recent block of either kind). **Attention signals** (issue #14, `tui::attention`):
  a `Status` transition into `WaitingApproval`, `Done`, or `Error` rings the
  terminal bell — and, opt-in via `ENTANGLEMENT_TUI_NOTIFY=1`, emits an OSC 9
  desktop notification (iTerm2/kitty/WezTerm; silently dropped elsewhere). Core
  emits `Status` only on a state change, so signalling on those states *is*
  signalling on the transitions; `Done`/`Error` also arrive as their own
  `OutEvent` variants but only `Status` is watched, so a turn end rings once.
  Focus reporting (crossterm `EnableFocusChange`) mutes signals while the
  terminal is focused, but best-effort only — terminals that never report focus
  always signal. **Two-stage Ctrl+C** ([ADR-0087](../adr/0087-two-stage-ctrl-c.md)):
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
  **External editor + export** (✅ #13,
  [ADR-0029](../adr/0029-external-editor-and-markdown-export.md), `tui::editor` +
  `tui::export`): `<leader>e` / `/editor` suspends the TUI and opens `$EDITOR`
  (`$VISUAL`→`$EDITOR`→`vi`) on the input draft, reading the result back into the
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
  `InMsg::Resume` is skipped (it carries the whole prior log → recursion/bloat)
  and `InMsg::Spawn` is skipped (a child's turns are already captured in the
  root's file via out events; logging the spawn would create a stray child root).
- **Spawned children fold into the root file** via a `roots` map built from
  `SessionStarted { root, parent }`, so each root file is a self-contained,
  replayable record of the whole session tree.
- **Resume** reads the file, `pair_records` builds the `(Option<InMsg>, OutEvent)`
  stream, and `Holly::resume` seeds a session from `Session::replay`. The CLI
  exposes `skutter run --resume <id>` and `skutter sessions` (lists past root
  sessions for the cwd); the TUI `/resume` modal restores the full visible
  transcript (`restore_from_records`) *and* reseeds engine context. Both
  listings carry a **first-prompt snippet** (#327): `list_sessions` captures the
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
- **Pluggable append target — `RecordSink`** (#313). The tap's *what to persist*
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
