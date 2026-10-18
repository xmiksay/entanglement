# entanglement Architecture — Wire protocol & structured outputs

> Part of the [architecture overview](../architecture.md). The *why* behind each choice is in the [decision log](../adr/README.md).

This module is split into the pages below, one per seam; each stays under the 400-line doc cap (`make doc-cap`). Every section heading the single-file version had is kept here, pointing at where that section now lives.

| page | covers |
| --- | --- |
| [messages](protocol/messages.md) | §2 — Message types, questions & queries |
| [sessions](protocol/sessions.md) | §2 — Trust split, session lifecycle & single-shot ops |
| [live-settings](protocol/live-settings.md) | §2 — Live generation, metadata & tool overlay |
| [structured-outputs](protocol/structured-outputs.md) | §4 — Structured outputs |

## 2. Wire protocol (`protocol.rs` + `protocol/`) — [ADR-0002](../adr/0002-session-multiplexed-protocol.md)

Now in [§2 — Message types, questions & queries](protocol/messages.md#2-wire-protocol-protocolrs--protocol--adr-0002).
Continued in [§2 — Trust split, session lifecycle & single-shot ops](protocol/sessions.md).
Continued in [§2 — Live generation, metadata & tool overlay](protocol/live-settings.md).

## 4. Structured outputs (orthogonal to profiles) — [ADR-0004](../adr/0004-structured-plan-and-task-events.md)

Now in [§4 — Structured outputs](protocol/structured-outputs.md#4-structured-outputs-orthogonal-to-profiles--adr-0004).
