# 0048. `serve` head — a local-only WebSocket protocol interface (Vue SPA primary, non-exclusive; browser surface out of scope)

- Status: Accepted
- Date: 2026-07-12

## Context

The architecture posits four interfaces, each a thin adapter over
`holly.send()` / `holly.subscribe()` (ADR-0001): the ABI, the stdio head
(ADR-0005), the TUI (ADR-0011), and **WebSocket `serve`** — planned, not yet
built. The wire protocol (ADR-0002) is a session-multiplexed, JSON-serializable
`InMsg`/`OutEvent` stream.

`serve` is intended as a **local HTTP server** that serves a Vue single-page app
plus a **WebSocket** carrying that protocol — the browser twin of the TUI. A
pre-`serve` audit (epic #153) raised multi-client concerns: forgeable
`ToolResult`/`Approve`/`Spawn` frames, a control plane multiplexed over a lossy
broadcast, and reused per-session `seq`s — all framed as "what a malicious client
could do."

The scoping decision that reframes that epic: `serve` is for a **local single
user**, is **not** designed to be exposed publicly or multi-tenant, and the
WebSocket is a **general protocol interface** — the Vue SPA is its *primary but
not exclusive* client; the user may point their own script, CLI, or editor plugin
at the raw WS, and that is explicitly supported.

## Decision

1. **Local, single-user, loopback-bound.** `serve` binds to loopback
   (`127.0.0.1`) by default and is never publicly exposed. The loopback bind is
   the one **required** control — it is what makes "not public" true at the socket
   level.
2. **The WS is a general protocol interface, not SPA-coupled.** The Vue SPA is the
   primary client; arbitrary user-chosen local clients are first-class. Nothing in
   `serve` may assume the SPA — it stays a thin, equal adapter per ADR-0001.
3. **Browser-page attack surface is out of scope by decision.** A malicious local
   page (including a plain drive-by opening `ws://localhost:<port>`, which does
   *not* require an infected browser or extension), a malicious extension, or a
   compromised browser is the user's responsibility — the same bucket as any local
   dev server. Any `Origin`-check or launch-token is **opt-in, never mandatory**;
   a mandatory browser handshake would break legitimate non-browser clients.
4. **#153 items are robustness, not anti-malicious-client security.** Because the
   clients are one trusted user, the epic's concerns become *robustness under
   multiplexing*: cooperating local clients (e.g. the TUI and the browser on one
   session) must not cross wires. #155 is thus **session ownership**, not
   anti-forgery. Stakes drop; the epic stays **P2** (the TUI already covers the
   local-UI need).
5. **Freeze the wire before the first external client.** Because arbitrary local
   clients *and* the co-developed SPA consume the serialized `InMsg`/`OutEvent`
   JSON, the protocol shape must be stabilized first: the `seq`-uniqueness and
   protocol-wart items (#157, #160) are the **first** work when `serve` begins,
   ahead of building the SPA.

## Consequences

- **Positive.** `serve` stays a thin, equal adapter consistent with ADR-0001; no
  bespoke auth or multi-tenant machinery; the raw WS is usable by any local
  tooling the user writes.
- **Positive.** Clarifies that #153/#155 are robustness/UX (P2), not blocking
  security — consistent with WebSocket's overall deprioritization.
- **Accepted risk.** No defence against a local browser page reaching
  `ws://localhost` — accepted per the trust boundary
  ([ADR-0047](0047-local-trust-boundary.md)); the user's machine and browser are
  trusted.
- **Required before build.** Loopback bind; and the wire-hygiene freeze (#157,
  #160) before the SPA or any client pins the JSON.
- **Coupling.** Depends on ADR-0047's local-single-user trust boundary. A public
  or multi-tenant `serve` would invalidate the out-of-scope browser stance and the
  robustness-not-security framing; both ADRs must then be superseded, and the
  #153 items would be re-elevated to security.

## Alternatives considered

- **Treat `serve` as potentially public / multi-tenant** (client identity,
  enforced per-connection session ownership, forgery-proof frames). Rejected as
  premature and mis-scoped: `serve` is a local UI transport, and building
  multi-tenant auth for a loopback tool is cost spent against a threat we have
  explicitly scoped out.
- **Couple the WS tightly to the Vue SPA** (SPA-specific framing/handshake
  required). Rejected: it violates the "four equal thin adapters" design and
  blocks the explicitly-supported raw-client use.
- **Mandatory `Origin`-check + token for every WS connection.** Rejected: it
  breaks non-browser local clients (no `Origin` header, needless token friction);
  the browser surface is out of scope, so the handshake is opt-in for the SPA case
  only.
- **Bind to `0.0.0.0` / expose on the network with auth.** Rejected: it
  contradicts "local, not public"; the loopback bind is the mechanism that keeps
  the scope honest.
