# 0100. TUI `/mcp` command — list, add, remove

- Status: Accepted
- Date: 2026-07-16
- Phase 5 (final) of the MCP umbrella, directly on top of
  [0096](0096-dynamic-toolregistry-sharedregistry.md) (the `SharedRegistry`
  live-swap) and [0097](0097-live-mcp-server-management.md) (the
  `InMsg::McpList`/`McpAdd`/`McpRemove` wire + `config::save_mcp` persistence).
  Also mirrors the `/set`/`/show` persist-on-confirmation precedent of
  [0095](0095-tui-set-show-generation-persist-on-confirmation.md). Issue #373.

## Context

0097 landed the full engine-side mechanism — live add/remove/list against the
`SharedRegistry`, best-effort connect, and a surgical `config.yml` persist —
but no head actually drove it interactively: the only way to add or remove an
MCP server mid-session was hand-crafting the wire frame over `pipe`/`serve`.
Without this issue, a TUI user who wants to attach a server has to restart
`skutter` with a hand-edited `config.yml`.

## Decision

**No new wire surface.** Everything here is TUI-side, driving the wire 0097
already shipped.

### `/mcp list|add|remove` (`tui/commands.rs`, new `tui/mcp_command.rs`)

`Command::Mcp` joins `all_commands()` (palette discoverability). Its
subcommand grammar — `list` (or nothing: a bare `/mcp` defaults to `list`),
`add <name> -- <command> [args...]` (stdio), `add <name> --url <url>
[--header KEY:VALUE]...` (streamable HTTP), `remove <name>` — is re-parsed
from the raw input text (`parse_mcp_args`), the same raw-text re-parse
pattern `/compact`/`/set` already established, since `parse_command` only
matches the command name and drops everything after it. Parsing is
whitespace-split only, no quoting, matching every other `/`-command's re-parse
in the TUI.

Unlike `/set`/`/show`/`/compact`, which stayed in `event_loop.rs`,
`/mcp`'s parsing (`McpCommand`, `parse_mcp_args`) **and** its async wire
senders (`send_mcp`, `send_mcp_list`) move into a new sibling module,
`tui/mcp_command.rs`. Both `tui/commands.rs` and `tui/event_loop.rs` were
already past the 400-line cap before this issue — the exact scenario that
moved `CommandPalette` out of `commands.rs` in 0095 — so growing either
further with `/mcp`'s ~150 lines of parsing plus senders was rejected;
`event_loop.rs`'s Enter-arm and the command palette handler each keep only a
few lines calling into the new module, the same footprint every past command
addition left at those two central dispatch points.

- `send_mcp_list` sends `InMsg::McpList` with a fresh correlation id
  (`SessionId::new_uuid().0` — reusing the existing `uuid` dependency rather
  than adding a new one to the runtime crate) recorded on
  `App`/`McpPanel::request`. It is the shared entry point for three callers:
  the typed `/mcp list`, a bare `/mcp`, and the command-palette pick of `Mcp`
  (which carries no trailing text, so — like `/show` — it always runs
  `list`).
- `send_mcp` (add/remove/list dispatch) sends `InMsg::McpAdd`/`McpRemove`
  directly for those subcommands; a parse error (unknown subcommand,
  malformed add/remove) is rendered as a transcript status line via the new
  `App::record_mcp_error`, never reaching the engine — mirroring `/set`'s
  friendly-`Err` path.

### `/mcp list` panel (new `tui/mcp_panel.rs`, `modals::draw_mcp_panel`)

A **correlation-id-gated** popup, not a generic "last snapshot" cache:
`McpPanel::request(id)` records the outstanding query; `McpPanel::apply_list`
only opens the panel (and only overwrites the stored snapshot) when the
incoming `OutEvent::McpList`'s `correlation_id` matches — a stray reply (e.g.
another head sharing the same engine over `serve`, or a slow reply racing a
second `/mcp list`) is silently dropped rather than popping the panel open
with an unrelated snapshot. This mirrors 0095's "reflects the pending
overrides" guard, adapted from a value-equality check to an id match since
`McpList` carries no other correlating data.

The panel itself (`modals::draw_mcp_panel`) is read-only — `Esc` is its only
key, so it needs no `ListState`/highlight, reusing the static-list shape of
`draw_help_dialog` rather than the interactive-selection shape of
`draw_sessions_modal`/`draw_tools_dialog`. Each server renders its name,
transport (`stdio`/`http`), connected/disconnected status, and either its
connect error or its namespaced tool list (`mcp__<server>__<tool>`, the exact
names 0067 registers into the `ToolRegistry`).

### `/mcp add`/`remove` confirmations (`app/mcp.rs`, new)

`OutEvent::McpChanged { name, action }` folds into the **active session's**
transcript as a status line (`App::handle_mcp_changed`, reusing
`SessionView::record_status`, the same sink `/key`'s save notice and `/set`'s
parse-error hint use) rather than a dedicated confirmation UI — MCP config is
engine-global (0097), so there is no "the session this belongs to" the way a
`ModelChanged`/`GenerationChanged` has; the active session's transcript is
simply where the user is looking. A failed add/remove is logged runtime-side
only (0097's existing best-effort philosophy — no `OutEvent` for a failure),
so the TUI shows nothing when a connect attempt fails; the next `/mcp list`
reveals the true state.

`App::handle_out_event` folds both `McpList`/`McpChanged` unconditionally
(they carry no `session` to filter on — `OutEvent::session()` is `None` for
both, per 0097), the same way it already intercepts session-less
`ModelChanged`/`GenerationChanged` before falling through to
`sessions.handle_out_event`, which returns `false` for them without ever
reaching a per-session view.

## Consequences

- The correlation-id guard means a `/mcp list` sent from one TUI instance
  never opens another instance's panel, even though `McpList`/`McpChanged`
  broadcast to every subscriber of the shared engine (relevant once `serve`
  and a TUI share a process, or two TUI processes attach to the same `serve`
  socket in a future embedding). The cost is one extra `String` compare and a
  dropped-not-queued stray reply — acceptable, since a dropped reply is
  recoverable by re-issuing `/mcp list`.
- `add`/`remove` failures are invisible until the next `list` — accepted as
  consistent with 0097's existing best-effort MCP philosophy rather than a
  TUI-specific carve-out (adding a synthetic failure event here would diverge
  from every other head).
- Splitting `/mcp`'s TUI glue into its own module, rather than accepting the
  cap violation `commands.rs`/`event_loop.rs` were already carrying, keeps the
  workspace rule ("never grow a file already over the cap") intact for this
  change, at the cost of one more file to navigate when reading `/`-command
  wiring end-to-end.

## Rejected alternatives

- **Folding `/mcp`'s parsing into `commands.rs` and its senders into
  `event_loop.rs`**, matching `/set`/`/show`/`/compact`'s placement exactly.
  Rejected only because both files had already crossed the 400-line cap
  before this issue — the same reasoning 0095 used to carve `CommandPalette`
  out, applied here to a newer addition instead of an existing one.
- **A dedicated `OutEvent::McpAddFailed`/`McpRemoveFailed`.** Would let the
  TUI show an inline failure without a follow-up `/mcp list`, but diverges
  from 0097's explicit "no session to attach an error to" design for every
  other head — adding it only for the TUI's benefit would special-case one
  consumer of an otherwise head-agnostic wire.
- **An always-open "live" MCP sidebar section** (mirroring the Plan/Tasks
  sidebar) instead of an on-demand popup. Rejected as scope creep: the issue
  asks for a `list` command with a panel, and MCP server churn is rare enough
  mid-session that polling via `/mcp list` is sufficient — a persistent
  sidebar section would need its own live-refresh story `McpChanged` doesn't
  yet drive.
