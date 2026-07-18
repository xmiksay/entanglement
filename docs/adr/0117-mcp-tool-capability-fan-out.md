# 0117. Config-side capability hints fan MCP tools into `read`/`write`/`call`

- Status: Accepted
- Date: 2026-07-18

## Context

[ADR-0114](0114-capability-level-permission-keys.md) (#418, part of the #416
epic) let a permission profile write a bare `read`/`write`/`call` capability
key instead of spelling out every member tool. Its membership table,
`tool_names::CAPABILITIES`, is a fixed compile-time list of the built-in host
tools (`read`/`grep`/`glob`, `edit`/`write`, `bash`) plus the two
general-purpose tools (`call`, `rhai`) it grades separately
(`tool_names::MULTI_GROUP`).

External MCP tools (`mcp__<server>__<tool>`, attached per
[ADR-0067](0067-mcp-client-as-runtime-tool-provider.md)/
[ADR-0080](0080-mcp-streamable-http-transport.md)) never joined that table. A
profile author who writes `read: allow` gets every built-in read-only tool
plus `call`/`rhai`'s least-privilege clamp, but an MCP-provided read-only tool
falls straight through as an ungrouped literal name — it only ever grades via
its own `mcp__<server>__<tool>: allow` line. ADR-0114 flagged this as a
deferred gap (tracked in [`../deferred-work-ledger.md`](../deferred-work-ledger.md)
as #426) rather than solving it, because an MCP tool isn't self-describing: its
`tools/list` response carries a name, description, and JSON schema, but no
capability tag, and the MCP spec defines no such annotation to read one from.

## Decision

### A config-side `capabilities:` annotation, not a protocol-level one

Each MCP server block in the user config's `mcp:` section (`McpServerConfig`,
`entanglement-runtime/src/mcp/mod.rs`) gains an optional
`capabilities: HashMap<String, String>` field: raw (un-namespaced) tool name →
capability name (`read`/`write`/`call`, validated against
`tool_names::is_capability_name`). The operator who wires up a server is the
one who actually knows what its tools do, so they annotate it by hand:

```yaml
mcp:
  docs:
    command: docs-server
    capabilities:
      search: read
      fetch: read
```

This is deliberately **not** derived from the server's `tools/list` response —
no such capability field exists in the MCP spec to read, and inventing a
proprietary annotation convention would require every server author to adopt
it before it did anything. A config-side hint works today, for any server,
with no protocol change and no dependency on the server's cooperation.

It's also deliberately **speculative rather than validated against a live
connection**: `entanglement-runtime::mcp::capability_index` builds the
capability → namespaced-tool-name index from configuration alone, before any
server has connected (agent-profile loading and MCP server connection are
separate startup steps, in that order — see "Ordering" below). An annotation
naming a tool the server doesn't actually expose (a typo, or a tool dropped in
a later server version) is simply inert — the resulting permission rule never
matches anything — rather than a startup error, since a `capabilities:` entry
carries no obligation that the server ever registers a matching tool.

### `McpCapabilityIndex` extends the same expansion, additively

`entanglement-runtime::mcp::McpCapabilityIndex` is a plain
`HashMap<String, Vec<String>>` (capability name → sorted namespaced tool
names), built once by `capability_index` from every configured server's
`capabilities` map (namespacing reuses `McpTool`'s own
`namespaced_tool_name(server, tool)` — extracted from `McpTool::new` so the
index can never drift from what a registered tool actually advertises).

`agents::expand_capabilities` (the ADR-0114 chokepoint, shared by agent
frontmatter and the user-config permission ceiling) takes this index as a new
parameter. Only the **bare** capability case (`CapScope::None`) consults it: a
bare `read: allow` now pushes both the static `tool_names::CAPABILITIES`
members and `mcp.get("read")`'s tool names at the same grade. Argument-scoped
(`read(pattern)`) and workdir-scoped (`call{pattern}`) capability keys are
**not** extended — an MCP tool call has no command-line or working-directory
argument for `permission_arg`/`permission_workdir` to extract in the first
place, so a scoped rule naming an MCP tool could never match through
`resolve_scoped` regardless; leaving `mcp` unconsulted there is a no-op by
construction, not a missing feature. `MULTI_GROUP` (`call`/`rhai`) is likewise
untouched — those are general-purpose *host* tools graded by the
least-privileged bare grade, a concept that doesn't extend to an MCP tool
naming its own capability.

### Ordering: MCP fan-out is a startup-time, non-live snapshot

Agent-profile parsing and MCP server *connection* are separate steps in
`main.rs` (profiles load first; `mcp::connect` runs after, inside
`build_config`). Capability fan-out only needs the **declared** `mcp:` config
(available as soon as `Config::load` returns), not a live connection, so
`main.rs` computes `mcp::capability_index(&user_config.mcp)` immediately after
loading the user config and threads it into `agents::load_registry` — no
reordering of the existing connect step was needed.

The index is computed once, matching how the user-config permission ceiling
itself is already handled (captured once at startup, never re-resolved). The
live-reload watcher ([ADR-0084](0084-runtime-live-reload-and-managed-file-locking.md))
re-parses agent/skill definitions on a debounced `config.yml` change but was
already not re-applying ceiling permission edits live; `watch::LiveDefinitions`
carries the same startup-computed `McpCapabilityIndex` into its `reload()` so
re-parsed agent frontmatter still resolves consistently, but an edit to
`mcp.*.capabilities` itself needs a restart — identical to a ceiling edit.

### Debug tooling (`inspect`) is intentionally out of scope

`agents::resolve_registry`/`prompt_report` (`skutter inspect agents`/`inspect
prompt`) call the same `build_profile` chokepoint but keep their existing
signatures, passing an empty `McpCapabilityIndex` — these commands don't load
`Config` today and already don't reflect the ceiling clamp either (they show
each profile's own frontmatter-resolved permission, not the fully effective
grade). Adding MCP fan-out to only one of the two things `inspect` already
omits would not make it more truthful. Real permission resolution
(`agents::load_registry`, which drives the engine) gets the full fan-out;
`built_in_registry()` (the embedded `build`/`plan`/`explore`/`debug` set, used
by dozens of tests and no production code) also keeps its existing signature
and an empty index — none of the built-ins reference an MCP tool.

## Consequences

- Positive: `read: allow` (or any bare capability key) now means what a
  profile author expects even when an MCP server is in play, without waiting
  on a protocol-level capability standard that doesn't exist yet.
- Positive: zero core change — the index is pure runtime data, resolved by the
  same `agents::expand_capabilities` parse-time rewrite ADR-0114 already
  established; `PermissionProfile`/`resolve` are untouched.
- Positive: the annotation is inert-by-default and additive-only — an absent
  or wrong `capabilities:` entry changes nothing (falls back to today's
  ungrouped-literal behavior), so this is a strictly backward-compatible
  extension of #418.
- Neutral: the annotation is manual and can drift from a server's actual tool
  set (a renamed/removed tool leaves a dangling, harmless entry; a new tool
  needs a new entry to be covered) — accepted, since there is no
  machine-readable source of truth to validate against today.
- Deferred (tracked in the ledger): a future MCP protocol revision or
  community convention exposing a per-tool capability hint in `tools/list`
  itself would let `capability_index` (or a sibling) derive this from a live
  connection instead of hand-maintained config — nothing here blocks that;
  it would only add a second index source alongside this one.

## Alternatives considered

- **Derive from the server's `tools/list` response.** Rejected: no such field
  exists in the MCP spec today; every server would need to adopt whatever
  convention this project invented first.
- **Expand capability membership inside `PermissionProfile::resolve` (core).**
  Rejected for the same reason ADR-0114 rejected it: core must stay
  capability-unaware and dependency-free (ADR-0006); the fan-out is pure
  string-key rewriting that belongs in the runtime chokepoint that already
  does it for the built-in set.
- **Thread the MCP capability index through `resolve_registry`/`prompt_report`/
  `built_in_registry` too**, so `inspect` and every test helper reflect it.
  Rejected as disproportionate: those call sites number in the dozens across
  the test suite, `inspect` doesn't load `Config` today, and it would only
  bring one of `inspect`'s two known omissions (this one) in line while
  leaving the other (the ceiling clamp) as-is.
- **Validate `capabilities:` entries against a live connection at startup**
  (bail if a named tool never registers). Rejected: server connection is
  best-effort and independently timed from config parsing (a slow/flaky
  server, or one added before it's implemented the tool yet, shouldn't make an
  otherwise-valid config a startup error) — an inert unmatched entry is the
  safe default the rest of this config layer already uses (e.g. a disabled
  server's block is skipped, not rejected).
