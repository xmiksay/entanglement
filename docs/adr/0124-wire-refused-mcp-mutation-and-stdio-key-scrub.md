# 0124. MCP mutation is trusted-only, the wire gate fails closed, and stdio servers lose the provider keys

- Status: Accepted
- Date: 2026-07-21
- Amends: [ADR-0069](0069-trusted-untrusted-wire-frame-split.md) (widens the
  wire-refused set and re-shapes `wire_allowed` fail-closed) and
  [ADR-0097](0097-live-mcp-server-management.md) (reverses `McpAdd`/`McpRemove`'s
  wire-allowed tier). Issue #472.

## Context

The 2026-07-21 post-remediation security audit found three related gaps around
the MCP surface, none tracked anywhere:

1. **`StdioClient::spawn` inherited the engine's full environment.** The
   subprocess `Command` applied the per-server `env:` map on top of the
   *inherited* parent env — so every MCP server received the provider API keys
   (`ZAI_API_KEY`, `ANTHROPIC_API_KEY`, …). #164 deliberately scrubs exactly
   these vars (`catalog.key_envs()`) from `bash`/`call` children because an
   exec child is an arbitrary external process; an MCP server is the same
   class of process, but the scrub never reached it. The deferred-work
   ledger's row 6 tracks only the narrower MCP-**HTTP** `${VAR}` header
   expansion — the stdio whole-env inheritance was a distinct, broader leak.

2. **`InMsg::McpAdd` was wire-allowed and unapproved.**
   [ADR-0097](0097-live-mcp-server-management.md) placed the MCP trio on the
   same wire tier as `ListSessions`, reasoning "enabling a server *is*
   consent" ([ADR-0047](0047-local-trust-boundary.md)). That consent argument
   is about the **config file** — a user hand-editing `mcp:` into trusted
   config. A wire frame is not that: the `serve` head's origin check is
   opt-in-off by design ([ADR-0048](0048-serve-head-local-trust-model.md)),
   and browsers permit cross-origin WebSocket connects, so a hostile web page
   the local user merely *visits* could open `ws://127.0.0.1:<port>/ws` and
   send `McpAdd { command: … }` — which `mcp::live::mcp_add` executes
   immediately, with no approval prompt, spawning an arbitrary local
   subprocess (which, per gap 1, also inherited the keys). ADR-0048 scoped
   the unauthenticated local page surface out as "prompt injection at worst";
   #375 later widened that same surface to subprocess execution without
   revisiting the decision.

3. **`wire_allowed` was a fail-open blocklist.** It was written as
   `!matches!(self, ToolResult | Spawn | Resume | HibernateSession)`, so any
   *future* `InMsg` variant lands wire-allowed by default. Its siblings
   (`session()`, `variant_name()`) are exhaustive `match`es precisely so a new
   variant forces a compile-time decision; the security-relevant classifier
   was the one place that didn't.

## Decision

Three moves, one PR:

- **Scrub `secret_env` from MCP stdio children.** `StdioClient::spawn` gains a
  `secret_env: &[String]` parameter (threaded from `catalog.key_envs()`
  through `mcp::connect` / `mcp_add` / `spawn_mcp_responder`, mirroring how
  `register_default_tools` already feeds `bash`/`call`). `env_remove` runs
  **before** `.envs(per_server_env)`, so an explicit per-server `env:` entry
  that names a secret var still wins — the user writing a key into a specific
  server's own config block is deliberate consent; silent inheritance is not.
  The command assembly is split into a `build_command` helper so the resulting
  env shape is unit-testable without spawning a process.

- **`McpAdd`/`McpRemove` become trusted-only.** They join
  `ToolResult`/`Spawn`/`Resume`/`HibernateSession` in the wire-refused set.
  The read-only `McpList` stays wire-allowed. Trusted heads are unaffected:
  the TUI `/mcp add`/`remove` path sends over the privileged in-process
  `Holly::send`, and an embedder holding a `Holly` was never gated. A WS
  client that wants MCP management is asking for local-subprocess execution;
  that belongs to the trusted tier until someone designs an approval-gated
  wire flow (a `ToolRequest`-style prompt for `McpAdd` — deferred, no current
  consumer).

- **`wire_allowed` becomes an explicit fail-closed allowlist `match`.** Every
  variant is now listed on one side or the other; adding a variant without
  classifying it is a compile error, and the safe default for an unclassified
  frame is *refused* (you must opt a new variant **in** to the wire, not
  remember to opt it out).

## Consequences

- **Positive:** a hostile local web page driving the origin-unchecked `serve`
  WS can no longer spawn subprocesses or mutate engine-global MCP config; MCP
  servers no longer see provider credentials they were never granted; the
  next protocol variant cannot silently join the wire surface.
- **Negative / accepted:** a remote WS client loses live MCP management
  (`McpList` still works). No known consumer existed; the TUI path is
  unchanged. Re-enabling it safely needs an approval-gated flow — tracked as
  deferred work in the ledger, to be designed only if a concrete consumer
  appears.
- The per-server `env:` override means a user can still hand a *specific*
  server a *specific* key — the scrub removes ambient inheritance, not
  explicit configuration.

## Alternatives considered

- **Approval-gate `McpAdd` instead of refusing it from the wire** — keeps the
  remote-management feature, but invents a `ToolRequest`-shaped approval flow
  for a frame that is not a tool call (new wire semantics, new head UX) for a
  capability with no known wire consumer. Refusal is the smallest sound fix;
  the gate can supersede it later if the need materializes.
- **Default the `serve` origin check on** — mitigates the browser vector but
  not the surface itself (any local process could still drive it; ADR-0048
  deliberately treats the WS as a general local protocol interface, and a
  mandatory origin gate breaks the raw-script clients it exists for). Also
  does nothing for gaps 1 and 3.
- **`env_clear()` + explicit allowlist for the stdio child env** — strictly
  stronger than scrubbing, but breaks real servers that legitimately need
  `PATH`/`HOME`/locale and would force every user to enumerate a base env.
  `bash`/`call` set the scrub precedent (#164); matching it keeps one mental
  model: *children inherit everything except provider secrets*.
- **Leave `wire_allowed` a blocklist and rely on review** — the audit itself
  is the counterexample: `McpAdd` slipped into the wire tier in #375 without
  the trust question being re-asked. Structure beats vigilance.
