# 0152. Provider-bundled MCP servers and the three-state enablement model

- Status: Accepted
- Date: 2026-08-01
- Issue: #542

## Context

z.ai ships first-party MCP servers (`web_search_prime`, `web_reader`,
`zread`) reachable with the same `ZAI_API_KEY` every Coding-Plan user already
holds — but using them required hand-authoring `mcp:` blocks in `config.yml`.
The native provider-side web search (ADR-0075/#481) covers only search, is
provider-executed outside the permission ladder, and has no reader/repo-doc
counterpart. Every MCP server was also binary: `disabled: bool` — either
connected at startup and advertised everywhere (profile mask permitting), or
invisible. There was no middle tier for "exists, but only when someone asks
for it", and no way for the *agent* to pull a tool server into its own
session the way `load_skill` pulls in a skill.

## Decision

**Bundled servers are catalog data** (the #118 rule): `ProviderEntry` gains
`mcp_servers: HashMap<name, ProviderMcpServer>` — transport surface mirroring
the runtime's `McpServerConfig` (`command` XOR `url`, `headers` with `${VAR}`
expansion), the #426 capability hint, and a default `state`. The embedded
`defaults.yml` bundles z.ai's three servers over streamable HTTP (ADR-0080),
auth header `Bearer ${ZAI_API_KEY}`, every tool hinted `read`. Mapping-valued,
so the existing catalog deep-merge overrides field-wise per server for free.

**Three-state activation** (`McpServerState`, declared in the provider crate
as catalog data, interpreted only by the runtime) applies to *every* MCP
server, bundled or user-declared:

- `enabled` — connects at startup, tools advertised (mask/overlay still
  apply). Legacy `disabled: false` maps here; user entries default here.
- `allowed` — *available*: not connected, tools nonexistent, but visible to
  `/enable mcp <name>` and enumerated in the `mcp_enable` tool's schema.
  Bundled entries default here.
- `disabled` — invisible everywhere. Legacy `disabled: true` maps here.

`McpServerConfig` gains an optional `state` overriding the legacy bool
(`effective_state()`); a user `config.yml` `mcp:` entry field-merges over a
same-name bundled definition (`merge_user_over_bundled` — a set field wins,
setting one transport clears the other; a user cannot *unset* a bundled field,
accepted). `AvailableMcp::partition` splits the merged universe into the
startup-connect set and the available roster at startup.

**Availability is key presence, read live.** A bundled server exists only
while its provider's `key_env` resolves (checked against the process env on
every availability query, so a TUI `/key` save — which `set_var`s — unlocks
the bundle with no restart). Keyless ⇒ silently absent: not in `/mcp list`,
not in the `mcp_enable` enum, indistinguishable from nonexistent by design.

**Enablement is session-ephemeral and lazy.** Enabling an `allowed` server
(TUI `/enable mcp <name>`, the `/mcp` panel's `e` key, or the agent's new
`mcp_enable` tool) connects it on first use (`enable_for_session` — the
`mcp_add` machinery *minus* persistence: `ServerConfigs`/`save_mcp` never see
it) and marks the calling session in `AvailableMcp`. Visibility is enforced in
the runtime's `tool_spec_resolver` (ADR-0076/ADR-0096): a lazily-connected
server's specs are filtered to its enabling sessions — no core change, no
overlay surgery. `/disable mcp <name>` unmarks the session (the connection
stays up for others); nothing is ever persisted — durable state changes are
config edits. A startup-`enabled` server passed to `enable_for_session` is
a no-op returning its tools: it must never become lazy-scoped, or enabling
it would *hide* it from every other session.

**`mcp_enable` is an ordinary host tool**, profile-graded like `load_skill`,
no executor interception. The live-available roster rides its JSON schema as
a dynamic `enum` (recomputed per spec snapshot), so the model only ever sees
currently-unlocked names. Newly registered tools reach the model on its next
round (the per-round spec snapshot); the tool's reply says so.

**Wire**: `McpServerStatus` gains an optional `state` (`"enabled"`/
`"allowed"`, serde-defaulted for old logs). The MCP responder's `McpList`
snapshot appends available-unconnected servers (`connected: false`), and
`McpRemove` of a bundled/lazy server routes to a plain disconnect (it stays
available) instead of failing halfway through the config-map removal.

## Alternatives rejected

- **Hardcoded runtime table / embedded `mcp:` config layer** — violates the
  catalog-data principle; config.yml has no embedded-server layer today and
  provider-conditional logic doesn't belong in config land.
- **Auto-connect on key presence** (the original framing) — changes the tool
  surface of every session the moment a key exists; the user chose explicit
  per-session activation with an agent-visible `allowed` tier instead.
- **Persist enablement (`state: enabled` flip on `/enable`)** — rejected as
  scope: enablement is a session decision; promoting a server durably is a
  deliberate config edit.
- **Session scoping via the #539 overlay** — an overlay *admits past the
  mask*; it cannot hide a registered tool from every *other* session under an
  inherit-all profile. The spec-resolver filter can, with zero core change.

## Consequences

- z.ai users get web search/reader/zread as first-class, permission-governed
  tools the moment their key exists, opt-in per session, zero config.
- The `mcp_enable` → `SharedRegistry` handle forms a deliberate `Arc` cycle
  with the registry that owns the tool (both process-lifetime singletons).
- Deferred (ledger): spawned children don't inherit a parent's lazy-server
  visibility (the spec filter is exact-session); enablement marks are not
  cleared on `SessionEnded` (bounded, memory-only); `skutter inspect` doesn't
  surface bundled servers (matches the #426 precedent); the bare-`/enable`
  session-tools dialog submit path doesn't lazy-connect (typed `/enable mcp`
  and the panel's `e` key do).
