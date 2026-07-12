# entanglement Architecture — Heads & session persistence

> Part of the [architecture overview](../architecture.md). The *why* behind each choice is in the [decision log](../adr/README.md).

## 6. Heads — ADRs [0005](../adr/0005-ndjson-stdio-head.md) (stdio), 0001 (ABI), [0010](../adr/0010-single-head-crate-and-bash-opt-in.md) (packaging), [0011](../adr/0011-tui-head-ratatui-crossterm.md)–[0015](../adr/0015-rich-text-pipeline-syntect.md) (TUI)

All heads live in one crate, **`entanglement-runtime`** (✅ #56; binary
`skutter`), as subcommands. The "four interfaces"
(in-process ABI + three transports) are a design concept, not a packaging
boundary — the real seam is `entanglement-core` ↔ everything else (ADR-0006,
ADR-0010).

The heads (and the `skutter` binary that carries them) need the crate's
**default features** — `default = ["tui"]` pulls clap + the providers + the
render stack, and `[[bin]] skutter` declares `required-features = ["cli","tui"]`.
Building the crate with `default-features = false` yields an **embeddable
library** — the tool-execution loop, permission dispatch, sub-agent spawn, and
persistence machinery with none of the CLI/TUI/transport weight
([ADR-0025](../adr/0025-runtime-cargo-feature-gates.md), §7).

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
- **WebSocket** (`skutter serve`, _next_): axum HTTP server for a local Vue SPA
  plus `GET /ws`, one `subscribe()` per socket, inbound frame → `InMsg` →
  `send()`, 30s ping, `continue` on `broadcast::Lagged`. Scoped **local,
  single-user, loopback-bound**; the WS is a general protocol interface (the SPA
  is the primary but not exclusive client — raw local clients are supported), so
  any `Origin`-check / launch-token is **opt-in, never mandatory** and the
  browser-page surface is out of scope. Freeze the wire hygiene (`seq`
  uniqueness, protocol warts) before a client pins the JSON
  ([ADR-0048](../adr/0048-serve-head-local-trust-model.md)).
- **TUI** (`skutter tui`): opencode-style terminal UI over `subscribe()`. Uses
  ratatui + crossterm (ADR-0011), leader-key bindings with which-key popup
  (ADR-0013), inline tool approval cards (ADR-0014), and rich markdown
  rendering with pulldown-cmark + syntect (ADR-0015). Event buffering and
  multiplexed-session rendering follow ADR-0012. Mouse capture is on by default
  (opt out with `ENTANGLEMENT_TUI_NO_MOUSE=1`, which restores native text
  selection): the wheel scrolls the chat (or the open modal's selection), and a
  left click hit-tests the chat area to toggle a transcript block — reasoning
  runs render collapsed as a `▸ Thinking (N lines)` header, expanded on click
  (or via the leader `t` key). **Attention signals** (issue #14, `tui::attention`):
  a `Status` transition into `WaitingApproval`, `Done`, or `Error` rings the
  terminal bell — and, opt-in via `ENTANGLEMENT_TUI_NOTIFY=1`, emits an OSC 9
  desktop notification (iTerm2/kitty/WezTerm; silently dropped elsewhere). Core
  emits `Status` only on a state change, so signalling on those states *is*
  signalling on the transitions; `Done`/`Error` also arrive as their own
  `OutEvent` variants but only `Status` is watched, so a turn end rings once.
  Focus reporting (crossterm `EnableFocusChange`) mutes signals while the
  terminal is focused, but best-effort only — terminals that never report focus
  always signal. **External editor + export** (✅ #13,
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
  inspect prompt|agents|skills` print, so you can debug a misbehaving session
  without leaving the TUI. It reuses the identical engine-free renderers
  (`inspect::tui_reports` → the shared `render_*` helpers): the Prompt tab is the
  active agent's `--parts` breakdown; the Agents tab is the registry table plus
  the active agent's full detail (permission / mask / spawn / `owns_plan`); the
  Skills tab is the exact `disclosures()` block the model sees plus the full table
  (including `user_only`). Views resolve on open from the cwd + live agent, so
  they stay fresh across mid-session definition edits. `Tab`/`←`/`→` switch tabs,
  arrows/`j`/`k`/`PgUp`/`PgDn` (or the wheel) scroll, `Esc` closes.

## 6b. Session persistence & resume (`persistence` + `session_store`)

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
  transcript (`restore_from_records`) *and* reseeds engine context.
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
