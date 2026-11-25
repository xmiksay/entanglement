# 0226. One advertised surface: the `mcp` envelope, list-style discovery, and MCP "registered = available"

- Status: Accepted
- Date: 2026-10-12
- Issue: #818 (part 1 of epic #822; gated by the #826 measurements)
- Supersedes: [ADR-0204](0204-invoke-fallback-for-client-side-discovery.md)
  (whole), [ADR-0196](0196-tool-search-and-lazy-discovery-replace-the-invoke-envelope.md)
  §1–§4 (the `full`/`tool_search` modes, the per-wire encodings, the
  `explore`/`describe` pair), and
  [ADR-0152](0152-provider-bundled-mcp-servers-three-state-enablement.md)'s
  three-state `enabled`/`allowed`/`disabled` model (its catalog-bundled,
  key-gated servers stand)
- Amends: [ADR-0201](0201-unregistered-mcp-tool-dispatch-resolves-truthfully.md)
  (the dispatch-time connect becomes the normal lazy path; there are no tiers
  left to classify against beyond available/disabled/unknown),
  [ADR-0207](0207-permission-modes-replace-agent-borne-authority.md) §12
  (the `explore`/`describe` non-tool kinds become the `list_*` tools),
  [ADR-0199](0199-session-tool-listing-and-enablement-drive-advertisement.md)
  (§1 `explore`'s `kind` filter and §2 the overlay-joins-the-discovered-set
  rule are void — no `explore`, no discovered set),
  [ADR-0202](0202-prompt-cache-discipline-anchors-deferral-replay-compaction-date.md)
  (§1 session-long `defer_loading` is void — nothing is deferred; §2–§7
  stand), [ADR-0149](0149-per-session-tool-overlay.md) (`/enable mcp
  <server>` and the `/mcp` panel's `e` key are removed; the overlay, `/enable
  tool`, `/disable` and the panel's `d` key stand),
  [ADR-0188](0188-session-keyed-per-user-mcp-scopes.md)
  (`McpScopes::overlay_specs` and "a scoped session advertises no MCP tools
  until…" are removed — MCP schemas are never advertised; scopes resolve
  `list_mcp`/`mcp_tools` and dispatch, and a scoped session never reaches a
  global server)
- Supersedes also: [ADR-0169](0169-startup-mcp-connect-is-concurrent-and-fast-fail.md)
  (wholly about the startup connect, which is retired — nothing connects at
  startup)

## Context

By 0.7 a session's tool surface depended on three interacting knobs: the
advertising mode (`full` vs `tool_search`, ADR-0196), the wire encoding
(`client_side` / `anthropic_native` / `responses_native`), and on the client
side the `discovery` strategy (`append` / `native_first` / `invoke`,
ADR-0204). `invoke {name, args}` was a second calling convention next to
native calls; on Gemini and OpenAI Chat its `args: object` carried no schema.
The surface was hard to explain, hard to test, and differed per wire. MCP
added a fourth axis: ADR-0152's three states, per-session enablement via
`mcp_enable` / `/enable mcp`, and startup connects for `enabled` servers.

The prompt-cache goal behind all of it — a `tools` array that never changes
mid-session — is met by **any** stable envelope. What varied was how many
concepts it took. Epic #822 settled the design (decisions 1–5) and #826
measured it on every Coding Plan GLM model (`scratch/tool-use-matrix-2026-10.md`,
2 226 runs, 11 models, thinking on and off):

- the `mcp(server, tool, args)` envelope reached the tool with correct
  arguments in 100 % / 100 % of non-error runs (240 runs, including nested
  objects and a follow-up turn) — on par with native `append` and `invoke`;
- an undeclared call (ADR-0204's `native_first` without fallback) worked only
  on the two flash models: 18 % reached / 14 % args ok overall;
- discovery shape did not change success: `list_mcp`/`mcp_tools` 99 % args ok
  with 1.48 discovery calls and ~7 % fewer completion tokens than
  `explore`/`describe` (99 %, 1.54 calls);
- a stable array kept the first request's cached share at 60–77 %.

## Decision

### 1. One advertised surface, pinned per session, identical on every wire

The advertised array is `tool_names::KERNEL` (renamed from
`TOOL_SEARCH_KERNEL`) picked out of the registered and runtime-owned specs,
deduped and name-sorted:

`read`, `edit`, `write`, `apply_patch`, `bash`, `poll`, `ask_user`,
`update_tasks`, `load_skill`, `propose_plan`, `request_mode`, `agent`,
`agent_send`, `rhai`, `web_fetch`, `list_agents`, `list_models`,
`list_skills`, `list_mcp`, `mcp_tools`, `mcp`.

`tool_advertising::surface` snapshots it at a session's first resolution
(`AdvertisingState::surfaces`) and returns the same bytes every later round;
a resume re-snapshots the same bytes since the kernel is compiled in. There is
no mode, no encoding, no knob. `web_fetch` joins the kernel because with
`describe` gone a non-kernel tool has no model-facing path. `glob`, `grep` and
`call` stay registered but hidden — reachable as `rhai` bindings and still
graded under their own names, like `read_raw`/`glob_json`/`grep_json`.

**Interim:** registered definition-driven `endpoint__*` and `skill__*` tools
stay advertised next to the kernel until #819 (endpoint tools) and #820
(skill `tools:`) remove them; they have no other path.

### 2. `mcp(server, tool, args)` is the only way to call an MCP tool

MCP tool schemas are never advertised. Core's envelope unwrap
(`session/mcp_envelope.rs`, narrowed from ADR-0204's `invoke` unwrap) turns an
`mcp` call into the inner `mcp__<server>__<tool>` — every character outside
`[A-Za-z0-9_-]` mapped to `_`, the spelling the runtime registers — before any
event is emitted, so dispatch, grading, hooks and heads see an ordinary tool
call. `OutEvent.envelope` carries the call as emitted, `Context` keeps it
byte for byte (the cached prefix never moves), and replay rebuilds it from the
envelope. Old logs carrying `invoke` envelopes still deserialize and replay —
`ToolEnvelope` is name-agnostic. A malformed `mcp` call reaches the runtime
under its own name and is declined with the envelope's schema.

`mcp` args are validated pre-dispatch against the tool's real schema
(ADR-0196 §6 unchanged): a violation replies with the schema and what was
wrong — the example shaped as the `mcp` call that reaches the tool — and never
reaches the server. The delivered-schema dedup is keyed by `mcp_tools`
deliveries and earlier declines (`DeliveredSchemas`).

### 3. Discovery is six plain tools

Each takes no argument or one `name`, and all are `Capability::Control` —
read-only catalog lookups, never graded (the `poll` pattern):

| tool | answers |
| --- | --- |
| `list_agents` | registered agents: name, description, model pin |
| `list_models` | the usable catalog's models: provider/id, display name, context window |
| `list_skills` | loadable skills: name + description |
| `load_skill` | one skill's body (unchanged; keeps its `skill_name` parameter) |
| `list_mcp` | registered MCP servers: name, description, state, tool names when known |
| `mcp_tools` | one server's catalog: every tool with its JSON schema and a `call as: mcp {...}` line |

`mcp_tools` is a **read**, the MCP counterpart of `load_skill` — no activation
semantics. MCP has no offline catalog (`tools/list` needs a live session), so
the first `mcp_tools` of a server in the process connects it as an
implementation detail. `list_mcp` never connects: its state is `connected`,
`available`, or `auth required` (with the `/mcp connect <name>` hint), and it
shows tool names live for a connected server, else from the managed last-seen
cache `<state dir>/mcp-tools.yml`, keyed by server name **and** transport
target so a repointed name never shows the old endpoint's list. Every
successful global connect records it.

Per-user scopes ([ADR-0188](0188-session-keyed-per-user-mcp-scopes.md)): a
scoped session's `list_mcp`/`mcp_tools` resolve through its scope
(`McpScopes::rows`, `overlay_registry_for_call`) — it never sees a global
server — and never read the shared last-seen cache, since one target can
expose different tools per credential.

`explore(kind: pending|modes)` are not carried over: a handle-less `poll`
remains the in-flight recovery path for a session's own work, and the system
prompt's static modes preamble already lists the modes. The subtree-wide
pending report (sub-agents, jobs, scripts, retained outputs, open questions,
parked approvals across the whole spawn tree) is deferred to #829, which
folds it into a handle-less `poll`.

**Maskability.** The six discovery tools are `Capability::Control` — the
`poll` pattern (ADR-0207 §3): never graded, and not subject to a mode rule,
the config ceiling or a session overlay deny. They read catalogs and grant
nothing — `mcp_tools` may connect a server, but every call through `mcp` is
still graded under its `mcp__<server>__<tool>` name — while withdrawing one
would strand the model's only view of what exists. A user who wants a server
out of a session denies its `mcp__<server>__*` tools (the `/mcp` panel's `d`
key), which takes effect on the calls themselves.

### 4. MCP: registered = available

Every configured server not `disabled: true` is available to every session;
provider-bundled servers are on while their provider key resolves
(ADR-0152's key-presence rule kept). `disabled: true` is the one explicit
opt-out, for bundled (field-merged override) and configured servers alike.
Nothing connects at startup: the first `mcp_tools` or `mcp(...)` on a server
connects it for the process (ADR-0201's dispatch-time connect, now the normal
path), and each call is graded by the session's mode (default `prompt`, so the
user audits it). `config.yml` `mcp:` entries and bundled catalog entries gain
`description:`. The `/mcp` wire snapshot lists every registered server;
`McpServerStatus.state` stays a serde-defaulted field for old logs and is
always `None`.

### 5. Session-invariant specs and prompt

The `agent` spec carries no roster and no enum — `list_agents` answers "who
can I start", and an unknown name is refused at spawn with the valid ones.
`request_mode` keeps its compiled-in mode enum. The system prompt drops the
tier-1 skill roster (`list_skills` carries it) and gains one static line
(`system_prompt::TOOL_CONVENTIONS`): call the advertised tools directly,
reach every MCP tool through `mcp(server, tool, args)` (`list_mcp`/`mcp_tools`
show what exists), use `rhai` for anything else.

### 6. Removed

`ToolAdvertising` (`full`/`tool_search`, pin/repin, the TUI `/set` Tools
section), the catalog `tool_advertising` and `discovery` knobs, `client_side`
append-on-describe, Anthropic `defer_loading` + `tool_reference` delivery,
Responses `tool_search_call`/`tool_search_output` (the Responses client stays
as a wire; persisted blocks of either kind degrade to text),
`ToolSpec.defer_loading`, `TOOL_SEARCH_CALL_TOOL`, `invoke`,
`explore`/`describe`, `mcp_enable`, `/enable mcp`, `/disable mcp`, the `/mcp`
panel's `e` key, the three-state `state:`, per-session MCP enablement and its
ancestry fold, the startup connect, and `McpScopes::overlay_specs`. The
`/mcp` panel's `d` key stays: a per-session `mcp__<name>__*` deny overlay is
the user's opt-out now that every registered server is on by default.

Retired keys are ignored with one warning each: `config.yml`
`tool_advertising:`, `discovery:` and `mcp.<name>.state:` via
`config/retired.rs`; provider/model `discovery`/`tool_advertising` and a
bundled server's `state` via the catalog merge's `strip_retired_keys`. A
retired `state: disabled` becomes `disabled: true` in both layers, so an old
opt-out never silently re-enables a server.

## Consequences

- One surface to explain and test; the cache-stability invariant becomes
  structural (a compiled-in list) instead of a per-wire discipline.
- An MCP call costs ~15–20 completion tokens over a native one (#826) — the
  price of never advertising MCP schemas.
- A cold `list_mcp` on a fresh install shows no tool names until a server has
  connected once; the cache fills that in thereafter.
- Startup no longer waits on, or logs about, MCP servers; a broken server
  surfaces as a tool error on first use.
- Embedders lose `ToolAdvertising`, `Discovery`, `INVOKE_TOOL`,
  `TOOL_SEARCH_CALL_TOOL`, `ToolSpec.defer_loading`, `McpServerState`,
  `McpEnableTool`, `mcp::connect` and `overlay_specs`; see the migration note
  in [embedding](../embedding.md).
- Deferred: the subtree-wide pending report the retired `explore(kind:
  pending)` gave — #829 folds it into a handle-less `poll`. The retired
  Anthropic deferred-tool probe (`live_anthropic_cache.rs`) is #830.

## Alternatives considered

- **One `explore(kind)` tool instead of six.** Fewer names, but a polymorphic
  schema and result shape; #826 found no success difference, and the plain
  tools need slightly fewer calls and tokens.
- **Keep `invoke {name, args}`.** It measured as well as `mcp`, but it is a
  second universal calling convention for every tool, with no schema on two
  wires; `mcp` scopes the envelope to the one family that needs it.
- **Keep `append` (grow the array on discovery).** Correct but every discovery
  costs a full cache miss on z.ai (ADR-0204's finding), and it keeps the
  per-wire variance this ADR removes.
