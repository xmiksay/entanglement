# 0069. Trusted/untrusted wire-frame split: privileged in-process `ToolResult`/`Spawn`

- Status: Accepted
- Date: 2026-07-15

## Context

`InMsg` is one flat inbox: an embedder — a head relaying wire bytes *or* the
runtime's in-process tool executor — pushes any variant through
`Holly::send`. Two variants are only ever legitimately authored **in process**
by the runtime, never by a head reading attacker-adjacent bytes:

- `ToolResult` resolves a parked turn matched on `request_id` **alone**
  (`session.rs`, the parked `TurnState`), folding straight into `Context` and
  driving the next round — bypassing both tool execution and the runtime's
  permission dispatch (#59). `request_id`s are broadcast to every `subscribe`r,
  so any listener learns them.
- `Spawn` mints a child session in the supervisor; the tool path's
  `spawn_refusal` gate (`may_spawn` + target allowlist, #119) guards only the
  `agent_spawn`/`agent` tool, not a raw `Spawn` frame.

Unlike `Approve`/`Reject`/`AnswerQuestion` — already filtered from session
routing and consumed by the runtime off the inbound fan-out (#59) — `ToolResult`
was routed to the session with no origin check, and `Spawn` was handled by the
supervisor unconditionally. A forged frame from *any* head therefore reached the
engine as if the executor had produced it.

`serve` (#153) is scoped to a **local single-user browser client** (browser-page
threats out of scope, [ADR-0048](0048-serve-head-local-trust-model.md)), so this
is **robustness/UX**, not defence against a remote attacker: which cooperating
local client may author which frame. It stays P2. But the seam it needs — a
trusted/untrusted split at the inbox — is worth building now, on the one wire
head that exists (`pipe`), so `serve` inherits it rather than re-deriving it.

## Decision

Split the inbox entry into a **privileged in-process** path and an **untrusted
wire** path, with a single allowlist as the source of truth:

- **`InMsg::wire_allowed()`** — `false` for the runtime-authored trio
  `ToolResult`/`Spawn`/`Resume`, `true` for everything a head legitimately
  authors (`Prompt`/`Approve`/`Reject`/`AnswerQuestion`/`Stop`/`SetAgent`/
  `SetModel`/`ListSessions`/`CloseSession`). `InMsg::variant_name()` names the
  `kind` tag for diagnostics.
- **`Holly::send`** stays the privileged entry: an in-process embedder holds a
  `Holly` and is trusted to author any frame.
- **`Holly::send_from_wire(msg) -> Result<(), WireError>`** is the untrusted
  entry a wire head calls after deserializing a line. A non-`wire_allowed`
  variant is **refused** (`WireError::Privileged`, logged), never routed; an
  allowed frame passes through to `send`.
- **`Holly::submit_tool_result(session, request_id, content)`** is the executor's
  named privileged handle — a thin wrapper over `send` that documents the trust
  boundary. The runtime's shared fold-back (`seam::reply_content`, the single
  site every runtime-owned tool ends at) uses it.
- The `pipe` head calls `send_from_wire`; a refused frame is a logged note, not
  fatal to the relay.

`Resume` is already `#[serde(skip)]` (never on the wire); listing it in the trio
keeps the predicate honest against a future serializer.

## Consequences

- A forged `ToolResult`/`Spawn` deserialized by a wire head is dropped at the
  inbox, so a parked turn can only be resolved by the executor's privileged
  handle and a child session can only be spawned by the guarded tool path.
- The split is one predicate (`wire_allowed`) plus two typed entry points; the
  supervisor's routing is unchanged, so parked-turn semantics (#270,
  [ADR-0061](0061-parked-turn-state-batch-tool-resolution.md)) are untouched.
- Deferred to the `serve` build (#153): the WS head must call `send_from_wire`,
  and per-connection **session ownership** for `Approve` (first-writer-wins among
  cooperating local clients) — a connection-scoped concern with no inbox today.

## Alternatives considered

- **A separate privileged mpsc inbox for `ToolResult`/`Spawn`.** The supervisor
  would `select!` over two channels; the executor sends on the privileged one.
  Rejected: it re-plumbs routing for no gain over an entry-point check — the
  trust boundary is *who calls the API*, and only a wire head deserializes
  untrusted bytes, so guarding the wire entry is sufficient and far smaller.
- **A per-frame origin token / capability.** Stamp each frame with a sender
  identity and check it in the supervisor. Rejected as over-scoped for a
  local single-user client (ADR-0048): there is no multi-principal boundary to
  authenticate, only a trusted-vs-wire distinction a boolean allowlist captures.
- **Making `ToolResult`/`Spawn` non-deserializable (`#[serde(skip)]`).** Kills
  the legitimate use: heads *do* serialize `ToolResult` for log replay, and an
  external resolver (ADR-0061) answers over the wire in embeddings that choose to
  trust it. The allowlist is a per-head policy, not a type-level ban.
- **Filtering inside the supervisor instead of at `Holly`.** Loses the caller's
  ability to distinguish trusted from untrusted origin — by the time a frame
  reaches the supervisor both paths look identical. The split has to live at the
  entry point where the caller's trust is known.
