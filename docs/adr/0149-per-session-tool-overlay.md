# 0149. Per-session tool overlay — live injection past the agent mask

- Status: Accepted (builds on [0148](0148-glob-patterns-in-the-agent-tool-mask.md);
  wire posture per [0124](0124-wire-refused-mcp-mutation-and-stdio-key-scrub.md)'s
  fail-closed rule and [0133](0133-live-bash-enablement-graded-by-permission.md)'s
  trusted-only rationale)
- Date: 2026-08-01

## Context

ADR-0148 made the #116 agent tool mask glob-capable, so a *profile* can opt
into MCP tools (`tools: [..., "mcp__*"]`). But the mask is still static
per agent definition: there was no way to hand **one running session** an MCP
server or tool the active profile masks out — "use the chessbase server in
this session, now" meant editing an agent file (restart or profile round-trip),
or running an unmasked profile everywhere. The existing per-session seam,
`tool_spec_resolver` (ADR-0076), deliberately *widens discovery but never
bypasses masking*, so it cannot express this either. What was missing is a
sanctioned, explicit, session-scoped mask override — set live by the user (or
an embedder) rather than authored into a profile.

## Decision

A **session-scoped tool overlay**, replacing wholesale via a new trusted-only
frame and confirmed/persisted via a new lifecycle event:

- `ToolOverlayEntry { pattern: String, allow: bool }` — `pattern` uses the
  ADR-0148 `*`/`?` mask semantics (`mcp__chessbase__*` = one server,
  `mcp__chessbase__evaluate` or `bash` = one tool, `mcp__*` = all MCP).
  `allow: false` (the serde default, mirroring `BashGrade::Ask`) still routes
  every matching call through the approval prompt; `allow: true` grants
  outright.
- `InMsg::SetToolOverlay { session, entries }` — **full replacement**, not a
  merge (an empty list clears; the head computes the new list from the last
  confirmation it holds). Always succeeds, always confirms — the
  `SetGeneration` shape — and is deferred during a live turn via the same
  stash gate, so the advertised surface never changes under a round in
  flight. **Trusted-only** (wire-refused): with `allow: true` it hands the
  model tools with no approval prompt, exactly `BashEnable`'s rationale.
- `OutEvent::ToolOverlayChanged { session, entries }` — point-in-time, no
  `seq`, carrying the full effective list; persisted by the variant-agnostic
  tap and folded by `Session::replay` by overwriting (the `GenerationChanged`
  pattern), so a resumed session keeps its overlay.
- `Session.tool_overlay: Vec<ToolOverlayEntry>` — session-scoped state that
  **survives `SetAgent`** by design (overriding the profile is its point) and
  dies with the session.

Semantics — "exists, and how it's graded":

1. **Existence (mask):** a matching entry beats the active profile's
   allowlist *and* denylist. Core's advertisement filter ORs the overlay with
   `advertises_tool`; the runtime's `tool_masked` applies the identical
   predicate **per link** of the ancestor walk — so a parent's overlay also
   admits the tool for its spawn sub-tree (each descendant's own profile
   permitting), and a masked child stays masked unless its own link has an
   overlay. The rhai `BindingPolicy` inherits the mask half through
   `tool_masked` unchanged.
2. **Grade:** on the generic dispatch route, a matching overlay entry
   *replaces* the profile chain's resolved grade with `Ask`/`Allow` — applied
   in the executor ladder itself (after the pluggable `PermissionResolver`,
   which stays untouched for embedders), then clamped against the config
   permission ceiling (#172), so `deny` in the ceiling still wins. Grants
   (#174) compose as usual (an approved `Ask` upgrades on the next identical
   call). The escape-root gate (ADR-0109) is unaffected — an out-of-root
   access still forces its prompt.
3. The overlay does **not** touch: plan authority (`explicitly_allowlists`
   stays literal on the profile allowlist), the skill mask (#400, still
   layered after), spawn gating, or the sandbox clamp.

Head surface (TUI): `/enable mcp <server>` (→ `mcp__<server>__*`),
`/enable tool <name-or-pattern>`, both `--allow`-capable; bare `/enable`
renders the session's overlay + the advertised roster (discovery detail lives
in `/mcp`'s panel and the `/agent` tools dialog); `/disable mcp|tool <x>`
retracts one entry, bare `/disable` clears. Confirmations render as
transcript status lines off `ToolOverlayChanged`; the head mirrors the
per-session lists to compute each full-replacement update.

## Consequences

- Positive: per-session MCP/tool enablement with no agent-file edit and no
  restart, honest wire visibility (event carries the full state), replay/
  resume fidelity for free, and safe defaults (Ask + ceiling clamp +
  trusted-only ingress). Embedders get the same seam programmatically via
  `holly.send`.
- Negative / deferred: rhai binding *grades* don't consult the overlay (the
  mask half does) — a script's `bash()` under an overlay still grades through
  the profile chain; acceptable since the overlay's primary target (MCP
  tools) has no binding surface. Grade override applies to the overlay
  session only — a child's grade still resolves through its chain even where
  the parent's overlay admits existence. Both noted for a follow-up if real
  use demands them (deferred-work ledger).
- The `serve`/`pipe` heads cannot set overlays (wire-refused); a future
  authenticated head would revisit that allowlist deliberately.

## Rejected alternatives

- **Per-session `tool_spec_resolver` composition** (ADR-0076): resolver
  output is still mask-filtered by design; using it would invert that
  deliberate invariant implicitly instead of adding an explicit, auditable
  override.
- **Mutating the session's `AgentProfile` in place**: the executor's #156
  self-heal and `AgentChanged` overwrite the folded profile from the
  registry, so an injected mutation silently reverts; a separate overlay
  keyed by session is race-free and survives profile switches by
  construction.
- **Merge semantics on the wire** (`add`/`remove` ops): full replacement
  keeps the event a complete snapshot (replay = overwrite, heads stateless-
  recoverable), mirroring `GenerationChanged`; the head-side list arithmetic
  is trivial.
- **Wire-allowing the frame** for `serve`: fails ADR-0124's bar — an
  unauthenticated local WebSocket could grant the model no-prompt tools.
