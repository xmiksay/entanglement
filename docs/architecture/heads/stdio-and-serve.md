# Heads & persistence §6 — Heads: ABI, stdio & WebSocket

> Part of the [heads & persistence module](../heads-and-persistence.md) of the [architecture overview](../../architecture.md).

## 6. Heads — ADRs [0005](../../adr/0005-ndjson-stdio-head.md) (stdio), 0001 (ABI), [0010](../../adr/0010-single-head-crate-and-bash-opt-in.md) (packaging), [0011](../../adr/0011-tui-head-ratatui-crossterm.md)–[0015](../../adr/0015-rich-text-pipeline-syntect.md) (TUI)

All heads live in one crate, **`entanglement-runtime`** (✅ #56; binary
`skutter`), as subcommands. The "four interfaces"
(in-process ABI + three transports) are a design concept, not a packaging
boundary — the real seam is `entanglement-core` ↔ everything else (ADR-0006,
ADR-0010).

The heads (and the `skutter` binary that carries them) need the crate's
**default features** — `default = ["tui", "serve", "mcp-http", "rhai", "web-fetch"]` pulls
clap + the providers + the render stack + the axum WS server + the
streamable-HTTP MCP transport (ADR-0080) + the sandboxed script tool
([ADR-0135](../../adr/0135-deferred-build-speed-trims-tokio-rhai-syntect.md))
+ the `web_fetch` tool ([ADR-0220](../../adr/0220-web-fetch-ssrf-guard-and-clipboard-image-paste.md)),
and `[[bin]] skutter` declares
`required-features = ["cli","provider","tui"]` (the `provider` feature was split
out of `cli` in #208 so the `ws` `serve` head — added in #153 behind its own
`serve` feature, `serve = ["cli","provider","dep:axum","dep:futures"]` — pulls
providers without dragging in clap or the TUI stack).
Building the crate with `default-features = false` yields an **embeddable
library** — the tool-execution loop, permission dispatch, sub-agent spawn, and
persistence machinery with none of the CLI/TUI/transport weight
([ADR-0025](../../adr/0025-runtime-cargo-feature-gates.md), §7). Wiring a custom,
multi-tenant head on top of this library (session namespacing, the trust
split, pluggable persistence/policy, approval-across-restart) is covered in
[`../embedding.md`](../../embedding.md), backed by a compiling
[`examples/embedded.rs`](../../../entanglement-runtime/examples/embedded.rs).

- **ABI** — `holly.send()` / `holly.subscribe()`. Done.
- **stdio** (`skutter run` / `skutter pipe`): one-shot `run [--format text|json]
  [--agent <name>] [--session <id> | --resume <id>] [--mode <mode>]`; bidirectional
  `pipe` NDJSON (`InMsg` in, `OutEvent` out). `skutter sessions` lists past root
  sessions for the cwd (see §6b). `run` has no interactive user to answer a
  `ToolRequest` (most commonly the escape-root gate forcing a prompt for an
  out-of-root path even under a profile's `Allow`, ADR-0109) or a `propose_plan`
  approval, so both are settled immediately instead of parking until the relay
  loop's 60s `recv` timeout kills the whole run (#554): `propose_plan` always
  auto-rejects (accepting hands off to a `build` child with its own review loop,
  which a headless run can't drive either way). A `rhai` launch card whose
  `reason` is `script_launch`/`inline_launch` starts Ask per command (a `Once`
  approval). Every other `ToolRequest` auto-rejects with a reason, including an
  out-of-root `launch_escape` and every nested card. The decision is read off
  the structured `reason`, never the card text (`pending::headless_starts`,
  [ADR-0230](../../adr/0230-structured-card-reason-and-honest-scopes-on-tool-request.md)).
  `--mode auto` is the unattended posture; `--yes` is retired. `run` and
  `pipe` never register as watchers, so they stay unattended
  ([ADR-0247](../../adr/0247-attended-sessions-wait-for-their-approver.md)). `ask_user` questions keep
  auto-answering their first option (ADR-0027). `skutter inspect prompt --agent <name> [--parts]` prints an
  agent's **assembled** system prompt (#184) — it re-runs the load-time discovery
  (`PromptContext::load` + skill/agent registries) with no engine, so a wrong
  brief pick, an empty preamble override, or a subagent losing the skill index is
  visible before model behaviour degrades; `--parts` breaks the prompt into its
  component slices, each tagged with the source it came from (built-in default,
  brief path, generated env, …). A load-time `debug!` (`agent=… prompt_len=…
  brief=<path|none> skills=…`) surfaces the same facts during any run.
  `skutter inspect agents [name]` (#185) surfaces the **layer-collision winner**
  the silent later-wins `insert` used to swallow: with no `name`, a table
  (name, model, winning layer, source path) of every resolved agent; with a
  `name`, the full resolved profile — **identity and provenance only**
  now — name, description, model/provider pin, assembled-prompt length,
  plus which lower-layer definitions it overrode.
  [ADR-0207](../../adr/0207-permission-modes-replace-agent-borne-authority.md)
  moved every permission fact (the tool mask, the permission rules, plan
  authorship, spawn control, sandbox confinement) off `Agent` and
  onto the session's independent permission mode, so this view lost the
  columns those facts used to fill — a profile has no posture left to
  render here at all. Same engine-free discovery as `inspect prompt`, via a
  `(layer, source)` provenance sidecar (`agents::resolve_registry`); `load_registry`
  also emits a `replaces=<prior layer>` `debug!` at each overriding insert.
  What replaces the old mask/dispatch-state columns is `skutter inspect modes
  [name]` (§permission modes in [agents & permissions](../agents/modes.md)):
  with no `name`, the four built-in modes at a glance (default grade,
  sandbox posture); with a `name`, its **resolved** rule table (built-in
  shape plus any `config.yml` `modes:` tuning) and the outcome for a known
  tool roster — `allowed` / `asks` / `declines`, the same three-state
  vocabulary the old `inspect agents` view used, now attached to the mode
  rather than the agent. Engine-free means no registry there either: the
  roster is the compile-time built-in vocabulary plus the runtime-owned
  pseudo-tools (`capability::static_capability_of`), and MCP tools
  are necessarily absent — a dynamic, environment-dependent surface out of
  scope for a static report.
  `skutter inspect commands [name]` (#733, [ADR-0221](../../adr/0221-user-slash-commands-are-head-side-prompt-templates.md)) lists the
  user slash commands — usage, winning layer, source file, description, and a
  marker on any file a built-in name hides — or one command's provenance and
  raw template.
  `skutter inspect skills [name] [--disclosures]` (#186) does the same for the
  **skill** registry — the authoring loop was "start a session and ask the model":
  no `name` prints a table (name, winning layer, `root_dir`,
  description); `--disclosures` prints the **exact** roster the model is handed
  — every skill ([ADR-0229](../../adr/0229-every-skill-is-model-visible-user-only-removed.md)),
  the same `discover::skills_list` text the `list_skills` discovery tool
  replies with (`inspect/skills.rs::render_disclosures`; the roster is no
  longer in the system prompt, [ADR-0226](../../adr/0226-one-advertised-surface-mcp-envelope-registered-is-available.md));
  a `name` **dry-runs the `load_skill` path
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
  (issue #187, `runtime::logging`). Cold-start attribution (#703):
  `RUST_LOG=skutter::startup=debug` logs the wall-clock since process start at
  each phase boundary (`config+prune+logging`, `skills+agents`,
  `tools`, `engine+responders ready`, `tui first frame`); the TUI's
  syntect syntax/theme sets load on the first fenced code block, not before
  the first frame. `inspect config` (#172) prints the resolved
  user config with per-field provenance — every fallback setting in
  [`config::Config`](../../../entanglement-runtime/src/config/mod.rs), including
  `max_turns`/`idle_ttl_secs`/`auto_compact`/`editor`/`session_retention_days`
  and the `tui:` block (#558, [ADR-0213](../../adr/0213-prune-env-vars-and-catalog-knobs.md)), any retired key a layer still carries
  (as a `warnings:` section), and an **advanced catalog** section listing each
  provider/model's non-default wire facts (`thinking_*`, `effort_tiers`,
  rate limits, … — [provider](../provider/catalog.md#advanced-catalog-wire-facts) §advanced catalog). Four more
  engine-free CLI-only views round out the managed-file/live-state blind spots
  (#558): `inspect aux-models` prints the
  persisted `/aux-model` pins (`aux-models.yml`); `inspect mcp-tokens` prints
  which MCP servers hold a stored OAuth credential (`mcp-tokens.yml`,
  ADR-0153), redacted — never a token value; `inspect mcp` prints the resolved
  MCP server roster (provider-bundled servers folded with the user `mcp:` map,
  #542) — every registered server with its provider, description, OAuth posture
  and last-seen tool names (registered = available, nothing connects at startup,
  [ADR-0226](../../adr/0226-one-advertised-surface-mcp-envelope-registered-is-available.md));
  `inspect session <id>` folds one **root** session's persisted log into its
  resolved agent/model/name plus its live tool overlay (ADR-0149) — the one
  piece of session state with no managed file of its own, so it is read
  straight from the `.jsonl` log rather than re-run load-time discovery.
- **WebSocket** (`skutter serve`, ✅ #153, `runtime::serve` behind the `serve`
  cargo feature): an axum HTTP server exposing `GET /ws` (plus a `GET /healthz`
  liveness probe), one `subscribe()` fan-out per socket relayed out as JSON text
  frames (every event, so a client sees `ApprovalResolved` for a card another
  socket or the TUI answered, [ADR-0240](../../adr/0240-approval-resolved-event.md)), each inbound text frame parsed into an `InMsg` and routed through the
  **untrusted** `send_from_wire` (#155) so a forged `ToolResult`/`Spawn`/`Resume`
  is refused per-frame (a non-JSON line falls back to a `Prompt` on the socket's
  own default session, `pipe` parity); a 30s ping keeps an idle socket alive and
  a `broadcast::Lagged` is a dropped-events gap → `continue`, never a silent
  relay death (#158). Graceful shutdown on Ctrl-C **or SIGTERM**; a live
  socket gets a `Close` frame and ends when the engine's outbox closes
  (`Holly::shutdown`), so an open browser tab never holds shutdown open (#699). Scoped **local, single-user, loopback-bound**: reached via
  `--port <N>` and **always** bound to `127.0.0.1` (no non-loopback bind is
  offered — the loopback bind is the one required non-public control). The WS is
  a general protocol interface (the future Vue SPA is the primary but not
  exclusive client — raw local scripts/CLIs/plugins are supported), so the
  `--allow-origin <ORIGIN>` check is **opt-in, never mandatory** (unset ⇒ every
  origin, including a raw client that sends none, is accepted) and the
  browser-page surface is out of scope. Accepted risk (2026-07-23 audit): with
  it unset, a malicious web page can open `ws://127.0.0.1:<port>/ws`, create
  its *own* session and self-approve its tool calls — ADR-0107's
  per-connection approval ownership defends only *existing* sessions. In
  scope of ADR-0048's local single-user trust model; revisit if `serve` ever
  grows beyond loopback. The wire hygiene it consumes (`seq`
  uniqueness #157, protocol warts #160) was frozen first, per the pre-`serve`
  hardening epic ([ADR-0048](../../adr/0048-serve-head-local-trust-model.md)). Lives
  behind the `serve` feature (implies `cli` + `provider`) so axum stays out of
  the lean library / `--no-default-features` build (ADR-0025). **Per-connection
  approval ownership** (#402, [ADR-0107](../../adr/0107-ws-per-connection-approval-ownership.md)):
  a `SessionOwners` map on `ServeState` claims a session for the first
  connection to send any frame referencing it (first-writer-wins); a later
  connection's `Approve`/`Reject`/`AnswerQuestion` for that session is refused
  (logged, dropped, connection unaffected) — every other `InMsg` variant is
  unaffected by ownership. Released on disconnect, so a still-parked approval
  is reclaimable rather than deadlocked behind a client that went away.
  **A connection attends the sessions it owns**
  ([ADR-0247](../../adr/0247-attended-sessions-wait-for-their-approver.md)):
  owning one registers it with the runtime's `attendance::Attendance` (the
  same registry the executor's gate reads, passed to `serve::router`), so
  that session and every sub-agent under it park their `Ask`s instead of
  collapsing them, with no deadline. Claiming frames: every session-bearing
  frame, plus `ListPending{root: Some(s)}` and `ListQuestions{session:
  Some(s)}` (they name no routing session, but they are what a
  reconnecting client sends), and a bare-text prompt for the connection's
  default session. The watch ends on disconnect; a card already waiting
  keeps waiting, and a reconnecting client lists it with a filtered
  `ListPending`/`ListQuestions` (which re-attends the session, stopping a
  running deadline) and answers it. A filtered listing for a session this
  process hibernated wakes it first (`serve::Rejoin`, which folds
  `SessionHibernated` off the engine): serve resumes it from the session
  log as the TUI does on its own task (the read loop keeps serving), plus
  each sub-agent the root's own hibernate put down (`session_store::revived_with`,
  as every resume path does; one
  hibernated on its own, or closed, stays down), waits — on the re-offered
  `ToolRequest`/`UserQuestion`/`ToolOutput` of each call core re-offers in
  that tree (each replayed turn's unresolved batch, from `Session::replay`;
  none after a `Stop` or a finished turn; a re-offered spawn is not
  awaited), at most 5s — and only then forwards the listing, so the cards
  are on that one reply. Only roots are woken (`serve::sleepers`): a
  listing naming a sub-agent wakes its root, which brings the child back.
  One wake per root: a later listing waits for it, and the connection's
  frames for any session in the woken tree queue behind the listing in order, bounded
  (`serve::rejoin::Relays`). A failed wake leaves the root asleep for the
  next listing to retry; past 4096 tracked roots the oldest is forgotten
  (warned once). A live, unknown or closed session, or a non-owner's
  listing, is listed as is.
  `serve` stays exactly local/single-user/unauthenticated as scoped above —
  no in-tree head ships multi-user. The authenticated multi-user mode
  designed by [ADR-0174](../../adr/0174-authenticated-multi-user-wire-head.md)
  (#674, `serve/auth.rs`) was built, then removed (#686): `UserId` must not
  appear anywhere in `entanglement-runtime`
  ([ADR-0181](../../adr/0181-userid-leaves-the-runtime-crate.md)). An
  authenticated wire head binding credentials to `UserId` is an embedder's
  own build, out of tree, on the same `Holly`/`send_from_wire` seams a
  single-user head uses.

---

Next: [§6 — The TUI head](tui.md)
