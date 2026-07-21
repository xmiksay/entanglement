# 0097. Live MCP server management: wire ops, engine-global routing, config persistence

- Status: Amended by [0124](0124-wire-refused-mcp-mutation-and-stdio-key-scrub.md)
- Date: 2026-07-16
- Phase 4 of the MCP umbrella (#377), directly on top of
  [0096](0096-dynamic-toolregistry-sharedregistry.md) (Phase 3 —
  `SharedRegistry`). Issue #375.

## Context

MCP servers were configured statically in `config.yml`'s `mcp:` section and
connected once at startup (`main.rs`, `mcp::connect`) into a `ToolRegistry`
that — before [0096](0096-dynamic-toolregistry-sharedregistry.md) — was
frozen for the process lifetime. There was no way to attach or detach a server
from a running engine, and no way to inspect what was attached from within a
session.

## Decision

### Wire shape: three new global ops, no session

`InMsg::McpList { correlation_id }` / `McpAdd { name, config }` /
`McpRemove { name }`, answered by `OutEvent::McpList { correlation_id,
servers }` / `McpChanged { name, action }`. MCP config is engine-global, not
per-session, so these follow `InMsg::ListSessions`'s precedent exactly:
`session()` returns `None`, `msg_to_cmd` routes them to `None` (never a
session task), and they are `wire_allowed()` (same local-trust tier as every
other head-authored query — ADR-0047/[0080](0080-mcp-streamable-http-transport.md):
enabling a server *is* consent).

`InMsg::McpAdd`'s `config` field is a new core-owned DTO,
`McpServerSpec` — a field-for-field mirror of
`entanglement_runtime::mcp::McpServerConfig` — rather than the runtime type
itself. Core cannot depend on the runtime crate (dependency direction is
provider ← core ← runtime); MCP logic and the `command`/`url` XOR validation
stay entirely runtime-side ([0067](0067-mcp-client-as-runtime-tool-provider.md)),
so the wire DTO is a passive struct core never interprets, converted via a
runtime-side `From<McpServerSpec> for McpServerConfig`.

### Answered by a runtime service, not the core supervisor

Unlike `ListSessions` — which the core supervisor answers directly, because it
already tracks the live-session directory needed to build the reply —
`McpList`/`McpAdd`/`McpRemove` are answered by a **new runtime-side
subscriber**, `mcp::spawn_mcp_responder`, off `Holly::subscribe_inbound()`.
This mirrors `history::spawn_history_responder`'s answer to `ReplayFrom`
exactly, for the identical reason: the runtime, not core, owns the state these
ops read and mutate — here, the `SharedRegistry` + `ActiveServers` + the live
server-config map. Core gained two matching `Holly::emit_mcp_list`/
`emit_mcp_changed` helpers (mirroring `emit_history`) since the outbound
broadcast sender stays private to `holly.rs`.

A failed `McpAdd`/`McpRemove` is logged via `tracing::warn!`, not surfaced as
an `OutEvent` — there is no session to attach an error to, and this matches
the existing MCP philosophy throughout this module (ADR-0067): a server
attach is best-effort, failures are diagnostic, never fatal to a caller's
turn.

### Runtime state: `ActiveServers` + `ServerConfigs`, both live-mutable

`entanglement-runtime/src/mcp/live.rs` adds:

- `ActiveServers = Arc<Mutex<HashMap<String, ActiveServer>>>` —
  `ActiveServer { client: Arc<McpClient>, tools: Vec<String>, transport:
  String }`, tracking exactly what is currently connected. Seeded at startup
  from `mcp::connect`'s (now non-`()`) return value.
- `ServerConfigs = Arc<Mutex<HashMap<String, McpServerConfig>>>` — the wider
  live mirror of every *configured* server, including one that is `disabled`
  or failed to connect. This is deliberately wider than `ActiveServers`: it is
  the set `save_mcp` must round-trip on every write, since `save_mcp` replaces
  the *whole* `mcp:` section — losing track of a never-connected entry here
  would silently drop it from `config.yml` on the next live add/remove.

`mcp_add` upserts: it first `unregister_prefix`es the target name's tools
(cheap no-op if none existed) before registering the new connection, so
re-adding an already-active name — reconfiguring a broken server, or one that
failed at startup — cleanly replaces it rather than leaking the old
`Arc<McpClient>`. The connect + `tools/list` awaits run **before** any lock is
taken ([0096](0096-dynamic-toolregistry-sharedregistry.md): never hold a
lock across `.await`); only the synchronous tool registration runs under the
registry's write lock. `mcp_remove` drops the server from both maps —
releasing the last `Arc<McpClient>` triggers `StdioClient`'s `kill_on_drop`,
so the subprocess dies (or the HTTP session closes) with no separate teardown
step — and succeeds even for a server that was never connected (a `disabled`
one, or one that failed at startup), since removing its leftover config entry
is still a legitimate ask.

### Persistence: surgical edit of `config.yml`'s `mcp:` key, not a sibling managed file

Every other runtime-writable state (grants, agent-model pins, agent-generation
overrides, the provider-key env file) lives in its own **managed** sibling
file under `${config_dir}/entanglement/`, explicitly kept out of the
hand-edited `config.yml` so the runtime can rewrite it freely. MCP servers are
the exception: they are expected to stay part of the primary `config.yml` a
user already edits by hand, so `config::save_mcp` (new,
`entanglement-runtime/src/config/mcp_persist.rs`) loads the file as a
`serde_yaml::Value` — not the typed `Config`, which would drop any key it
doesn't know about — and replaces only the top-level `mcp` mapping before
reserializing. A missing file is created fresh with just that key. Locked
(`config::lock::with_locked_file`) and atomic
(`config::atomic::atomic_write`), matching every other managed write in this
module.

This does **not** preserve comments — no layer in this codebase's config
loader does (`config::merge_value` already operates at the `Value` level with
the same limitation), so `save_mcp` is consistent with, not a regression from,
the existing merge behavior.

## Consequences

- `ENV_LOCK` (guarding the process-global `ENTANGLEMENT_CONFIG_FILE` test env
  var) moved from `config::tests` to `config::mod` (`#[cfg(test)] pub(crate)`)
  so `config::mcp_persist::tests` — a separate test module touching the same
  env var — shares one lock instead of racing a second, independent one under
  parallel `cargo test` execution.
- A live add/remove now touches the same file a user might have open in an
  editor; a concurrent external edit during the lock window is last-write-wins
  on the whole file (the lock only serializes `skutter` writers against each
  other, not against an external editor) — accepted, matching the "out of
  scope: reconnect-on-config-external-edit" note in the parent issue.

## Rejected alternatives

- **A sibling managed `mcp.yml`, mirroring `agent-models.yml`.** Would keep
  `save_mcp` in the same simple read-modify-write shape as every other managed
  file, but splits server config across two files (`config.yml`'s `mcp:` key,
  now dead, plus the new managed one) — confusing for a user who already
  hand-edits `mcp:` in `config.yml` today. Rejected in favor of surgically
  editing the file that already owns the section.
- **Deserializing into typed `Config`, mutating the `mcp` field, and
  re-serializing the whole struct.** Would silently drop `permissions`/
  `hooks`/any future key `Config` doesn't carry a `#[serde(flatten)]` catch-all
  for — `deny_unknown_fields` makes this a data-loss trap, not just a
  cosmetic one. The `Value`-level edit only ever touches the one key it means
  to.
- **Surfacing `McpAdd`/`McpRemove` failures as a new session-scoped
  `OutEvent`.** There is no session to attach one to (these are engine-global
  ops); inventing a session-less error variant purely for this would add wire
  surface for what `tracing::warn!` already covers, consistent with every
  other best-effort MCP failure path (ADR-0067).
