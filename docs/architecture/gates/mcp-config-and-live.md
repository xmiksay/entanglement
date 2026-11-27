# Gates & host tools §10 — MCP client: config, live management & lazy connect

> Part of the [gates & host tools module](../gates-and-host-tools.md) of the [architecture overview](../../architecture.md).

- **Config:** the `mcp:` section of the layered user config (§ADR-0047/#172), a map
  of server name → `McpServerConfig`. A block is one transport XOR the other —
  `{command, args, env}` (stdio) **or** `{url, headers, oauth}` (HTTP; `oauth` is
  the ADR-0153 block in [MCP client](mcp.md)), plus a shared
  `disabled` (the one opt-out) and `description` (shown by `list_mcp`; the
  retired three-state `state:` is an unknown field, a parse error, #920,
  [ADR-0226](../../adr/archive/0226-one-advertised-surface-mcp-envelope-registered-is-available.md)) — resolved by
  `McpServerConfig::transport()`, which rejects both-set or
  neither-set. `deny_unknown_fields`-validated by the same loader as
  `permissions`/`hooks`. Empty ⇒ no servers (the norm). `skutter inspect config`
  lists the configured servers and their resolved transport. An optional
  `capabilities: {tool: read|write|call}` map (raw tool name, #426,
  [ADR-0117](../../adr/archive/0117-mcp-tool-capability-fan-out.md)) hand-annotates a
  server's tools — this predates
  [ADR-0207](../../adr/archive/0207-permission-modes-replace-agent-borne-authority.md)'s
  `Capability` system and is **not** wired into it (`McpTool::capabilities()`
  is hardcoded `Write`, [MCP client](mcp.md)); the one place it still feeds is the
  **`config.yml` `permissions:` ceiling's own capability-key expansion**
  (the pre-ADR-0207 `agents::permission_from_value`/`expand_capabilities`
  pipeline, which the ceiling alone still runs before its result is
  reinterpreted as a `Mode` — `mcp::capability_index` folds this hint into an
  `McpCapabilityIndex` keyed by capability name, resolved from config alone
  (no live connection needed) and consumed by `agents::expand_capabilities`
  so a bare `read: allow` in the ceiling reaches a hinted MCP tool).
- **Wiring:** `build_config` builds the roster (`AvailableMcp::from_config`)
  and an empty `ActiveServers`; **nothing connects at startup** ([ADR-0226](../../adr/archive/0226-one-advertised-surface-mcp-envelope-registered-is-available.md),
  retiring [ADR-0169](../../adr/archive/0169-startup-mcp-connect-is-concurrent-and-fast-fail.md)'s
  concurrent startup connect). The first `mcp_tools` read or `mcp(...)` call
  on a server connects it for the process (`available::ensure_connected`,
  per-server guard, 60 s timeout); a failure is a tool error, never a startup
  failure. The stdio path lives in the **lean library**; the HTTP path rides
  the `mcp-http` feature, so an embedder gets stdio tool servers with no
  CLI/TUI/transport dependency and opts into HTTP by enabling the feature.

### Live add/remove/list — [ADR-0096](../../adr/archive/0096-dynamic-toolregistry-sharedregistry.md) + [ADR-0097](../../adr/archive/0097-live-mcp-server-management.md) (#372, #375)

The tool registry is not frozen:
`entanglement_runtime::SharedRegistry` (`Arc<std::sync::RwLock<ToolRegistry>>`)
replaces the owned `ToolRegistry` as the executor's tool input
(`ToolExecutor::builder(holly, tools)`), and `EngineConfig.tool_spec_resolver` (§ADR-0076) is wired to
snapshot it fresh every turn — so the tools a live add/remove registers reach
both dispatch and model-advertised schemas with no engine restart.

- **`InMsg::McpList { correlation_id }` / `McpAdd { name, config }` /
  `McpRemove { name }`** are engine-global, exactly like `ListSessions`:
  `session()` is `None` and `msg_to_cmd` routes them to no session task. Only
  the read-only `McpList` is wire-allowed; `McpAdd`/`McpRemove` are
  **trusted-only** (#472,
  [ADR-0124](../../adr/archive/0124-wire-refused-mcp-mutation-and-stdio-key-scrub.md),
  reversing #375's wire tier — an unapproved `McpAdd` spawns an arbitrary
  local subprocess, and ADR-0047's "enabling is consent" covers the trusted
  config file, not an unauthenticated wire frame). The TUI `/mcp` command is
  unaffected: it sends over the privileged `Holly::send`.
  `McpAdd`'s `config` is a core-owned `McpServerSpec` DTO — core cannot depend
  on the runtime crate, so it never carries the runtime's `McpServerConfig`
  directly; a `From<McpServerSpec>` conversion happens runtime-side. Answered
  by `OutEvent::McpList { correlation_id, servers: Vec<McpServerStatus> }` /
  `McpChanged { name, action: McpAction }` — no `seq`, point-in-time.
- **`skutter::mcp::spawn_mcp_responder`** subscribes to `Holly::subscribe_inbound()` and
  answers these three (and `McpAuth`), mirroring
  `skutter::history::spawn_history_responder`'s answer to `ReplayFrom`: the
  head, not core, owns the global roster, so a head-side service is the sole
  answerer rather than the core supervisor (unlike `ListSessions`, which the
  supervisor answers directly from its own live-session directory). Two
  `Holly::emit_mcp_list`/`emit_mcp_changed` helpers mirror `emit_history`.
  The library carries none of this (#932): an embedder with per-user servers
  uses `McpScopes` instead.
- **What is connected** is the library's `mcp::active`: `ActiveServers`
  (`name → { client: Arc<McpClient>, tools, transport }`), changed only
  through `install_server`/`drop_server`, which swap exactly the tools a
  connection registered; `mcp_list` snapshots it. **What is configured** is
  `skutter::mcp::live`'s wider `ServerConfigs` (every *configured* server,
  including a `disabled` or failed-to-connect one — the full set a persist
  write must round-trip). `mcp_add` upserts (drops any prior tools/connection under the same name
  first, so re-adding cleanly replaces a broken server) and never holds the
  registry's write lock across the connect/`tools/list` awaits. `mcp_remove`
  drops the tracked `Arc<McpClient>`, which is what actually kills the
  subprocess/closes the HTTP session (`StdioClient`'s `kill_on_drop`) — there
  is no separate teardown call. A live add/remove also updates the roster
  (`AvailableMcp::insert_user`/`remove`), so registered = available holds with
  no restart. The `McpList` snapshot (`mcp::roster`) lists every registered
  server, connected or not, with its OAuth posture (unknown fields from old
  logs are ignored on deserialize).
- **Persistence is the head's** (#966): `mcp_add`/`mcp_remove` change only
  the in-memory `ServerConfigs`. `skutter` writes it back to `config.yml`
  from a subscriber (`state::spawn_mcp_config_writer`) when a successful
  `OutEvent::McpChanged` changed the set — `state::save_mcp`, a **surgical
  `serde_yaml::Value` edit** of just the top-level `mcp` key (not the typed
  `Config`, which would drop unknown keys under `deny_unknown_fields`),
  locked (§ADR-0084) and atomic. A lagged subscriber re-checks the set but
  never rewrites an unchanged one (the round-trip drops comments). A write
  failure follows the success as an `McpChanged` error naming the server:
  the live change stands, for this process only. Unlike the
  managed sibling files (grants/agent-models/agent-generation/the provider-key
  env file), MCP servers stay part of the primary hand-edited `config.yml` —
  the surgical edit exists precisely to avoid disturbing whatever else
  (`permissions`, `hooks`, …) a user set alongside `mcp:`. Does not preserve
  comments (no layer in this config loader does).
- A refused or failed live add/remove is an `McpChanged` carrying `error`
  (engine-global, no session to attach it to), also `tracing::warn!`-logged.
- **Out of scope here:** reconnect-on-external-config-edit (a file watcher) is
  a separate, unscheduled follow-up. The TUI `/mcp` surface landed next —
  §"TUI `/mcp` command" below.

### Registered = available — [ADR-0226](../../adr/archive/0226-one-advertised-surface-mcp-envelope-registered-is-available.md)

The user's `mcp:` map is the only source of MCP servers — the provider
catalog carries none (#915). `AvailableMcp::from_config` builds one roster:
every configured server, minus those with **`disabled: true`** — the single
explicit opt-out, kept in `disabled_names` so a call to one is declined
truthfully rather than as unknown. There is no per-session enablement, no
`mcp_enable` tool, no per-session toggle, no ancestry fold: an
available server is available to every session, and every call to one of
its tools is graded by the session's mode (default `prompt`, so the user
audits it). A retired `state:` key is an unknown-field parse error (#920);
`disabled: true` is the one opt-out.

**`list_mcp`** (`discover::mcp_lists`) never connects: one row per server
(`mcp::roster` — name, description, transport, connected, tools, OAuth
posture) with state `connected` / `available` / `auth required` (an OAuth
server with no stored credential, with the `/mcp connect <name>` hint), and
tool names — live for a connected server, else the **last-seen cache**
(the `mcp::McpToolCache` seam, keyed by server name **and** transport target
(`McpServerConfig::target()`: URL or command line) so a repointed name never
shows the old endpoint's list). The roster's default is in memory
(`InMemoryMcpToolCache`, #966); `skutter` plugs in the managed
`<state dir>/mcp-tools.yml` (`state::FileMcpToolCache`) so the next process
lists it too. Every successful global connect — lazy, `/mcp add`, `/mcp
connect` — records it (`record_last_seen`); a write failure is logged, never
surfaced. The definitions watcher ignores that file.

**`mcp_tools(name)`** is a read of one server's catalog — every tool with its
JSON schema and a `call as: mcp {"server": …, "tool": …, "args": <minimal
example>}` line — the MCP counterpart of `load_skill`. MCP has no offline
catalog, so the first read of a server in the process connects it
(`available::try_lazy_connect`); every listed tool is marked in the session's
`DeliveredSchemas`. Both tools are `Capability::Control`: reading a catalog
grants nothing.

### Dispatch-time lazy connect — [ADR-0201](../../adr/archive/0201-unregistered-mcp-tool-dispatch-resolves-truthfully.md), amended by [ADR-0226](../../adr/archive/0226-one-advertised-surface-mcp-envelope-registered-is-available.md)

An unknown-tool dispatch for an `mcp__<server>__*` name — the normal case for
the first `mcp(...)` on a server, since nothing connects at startup, and for
a resumed session whose registry is process-lifetime — is not a flat
registry-membership test. `AvailableMcp::tier_of` (`mcp/available_tier.rs`)
classifies `server` with **no connect attempt**: `Available` (in the
roster), `Disabled` (configured off), `Unknown`. `server_name_of` parses the
namespaced form; `disabled_decline` renders the one truthful-decline string.

- **`tool_runner::dispatch`'s** unknown-tool check calls
  `mcp::available::try_lazy_connect`: `Available` connects
  (`ensure_connected`, same timeout and per-server guard), re-snapshots the
  live registry and **continues the same function** — grading,
  escape-root, approval all still run unchanged, since a
  connect restores registration only, never permission. A connected server
  without the named tool answers "has no tool named … — `mcp_tools` lists its
  tools". `Disabled` → `disabled_decline`. `Unknown` → the Levenshtein-hinted
  unknown-tool message. A connect failure is a distinguishable error
  (`"...could not be connected: <cause>"`).
- A per-server **failure cooldown** (30 s, global servers only — a scoped
  server's connect has none) guards a broken server's connect timeout
  against a batch of calls each serially retrying it — complementary to the
  per-server connect guard (#556), which serializes *concurrent* attempts.
  It is checked again and recorded *under* that guard, so callers queued
  behind a failing connect return its failure instead of each reconnecting.
- **A script's connect is bounded by its budget** (#831). On the
  `Via::Script` path (`dispatch/resolve.rs`), an `mcp__*` call whose server
  is not connected yet (global, or the session's own scope) waits on that
  server's **one detached connect** (`mcp::InFlight`, keyed by server or
  `(scope key, eviction generation, server)`: started by the first such
  call, joined by every later one) no longer than the remaining-budget-less-grace clock its
  nested calls run under (`Via::budget`). A budget already under 50 ms
  starts no connect and refuses at once; an already-connected tool never
  waits. On expiry the script gets a catchable error naming the server
  ("mcp server `<server>` was still connecting when the script's time
  budget ran out …"), and the connect is **detached, not cancelled**: it
  holds the connect guard, so a model call (or a later script) queued behind
  it gets the finished connection instead of a second attempt, and a global
  failure still sets the cooldown above. Each attempt is capped by the
  connect's own 60 s timeout, and a connect never registers partial state —
  tools are installed only after the handshake and `tools/list` both
  succeed. A detached scoped connect that finishes after `evict_scope` is
  not cached (an eviction generation per scope key), so a logout cannot be
  undone by a connect begun before it; a call after the eviction starts its
  own connect rather than joining the doomed one. A model's call (direct `mcp`
  envelope, `mcp_tools`) waits the connect's timeout as before.

### TUI `/mcp` command — [ADR-0100](../../adr/archive/0100-tui-mcp-command.md) (#373)

`Command::Mcp` (`tui/commands.rs`) joins `all_commands()`; its subcommand
parsing (`McpCommand::List`/`Add`/`Remove`, `parse_mcp_args`) and the async
`send_mcp`/`send_mcp_list` wire-dispatch helpers live in a new sibling
`tui/mcp_command.rs` — `commands.rs` and `event_loop.rs` were already past the
400-line cap, mirroring how `CommandPalette` was split out of `commands.rs`
(§ADR-0095). `/mcp list` (or a bare `/mcp`, or picking `/mcp` from the command
palette, which carries no trailing text) sends `InMsg::McpList` with a fresh
correlation id recorded on `tui::mcp_panel::McpPanel`; the matching
`OutEvent::McpList` opens a read-only popup (`modals::draw_mcp_panel`, `Esc`
closes) listing every registered server's name, transport, state
(`connected`, `available`, or `auth required (/mcp connect <name>)`), and
namespaced tools — a stray reply for a different correlation id (e.g. another
head sharing the engine) is ignored, never opening the panel with the wrong
snapshot. `/mcp add <name> -- <command> [args...]` (stdio) / `/mcp add <name>
--url <url> [--header KEY:VALUE]...` (streamable HTTP) and `/mcp remove
<name>` send `InMsg::McpAdd`/`McpRemove` directly; the confirming
`OutEvent::McpChanged` (or a parse error, caught before the engine is
touched) renders as a transcript status line via `App::handle_mcp_changed`/
`record_mcp_error`, mirroring `/key`'s save notice. The panel has no enable key
any more ([ADR-0226](../../adr/archive/0226-one-advertised-surface-mcp-envelope-registered-is-available.md)); `/mcp connect` (OAuth) stays. No new wire surface — this
is entirely a head-side consumer of the `InMsg`/`OutEvent` pair #375 already
shipped.

[holly]: ../../../entanglement-core/src/holly.rs
[profile]: ../../../entanglement-core/src/protocol/permission.rs
[perm]: ../../../entanglement-core/src/protocol/permission.rs

---

Previous: [§10 — MCP client: transports, OAuth, scopes & proxy](mcp.md)
