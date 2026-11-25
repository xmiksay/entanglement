# 0001. Actor model is the integration ABI

- Status: Accepted
- Date: 2026-07-04

## Context

The Phase-1 bootstrap modeled the engine as `Holly<T: Transport>` calling a
**pull-based** `Transport` trait (`transport.next_action()` blocks; the engine
calls *into* the harness). That fits exactly one blocking caller.

It fails the moment we look at the real heads:

- **Web/serve** multiplexes many conversations over one WebSocket and fans one
  agent's events out to N attached clients. A pull trait can't multiplex or
  fan-out.
- **ABI / direct embedding** wants to push messages and read events without
  serialization. A trait the engine *calls into* isn't a handle a host can drive.
- Both reference projects (`agent`, `design`) instead use a **push-based actor
  mailbox**: an inbox the harness writes, an outbox (broadcast) the harness
  reads.

## Decision

The engine is an **actor**. `Holly` owns a process-wide inbox
(`mpsc::Sender<InMsg>`) and outbox (`broadcast::Sender<OutEvent>`):

```rust
impl Holly {
    pub fn spawn(cfg: EngineConfig) -> Holly;
    pub async fn send(&self, msg: InMsg);          // push a typed message in
    pub fn subscribe(&self) -> broadcast::Receiver<OutEvent>;  // fan-out events
}
```

Every head (ABI, stdio, WebSocket, TUI) is a thin adapter over these two methods.
The `Transport` trait is removed.

This **is** the ABI: an embedder holds a cheaply-cloned `Holly`, calls `send`
with typed `InMsg`s and drains `subscribe` for `OutEvent`s — zero serialization.

## Consequences

- **(+)** Adding a head never touches the engine — it's protocol translation only.
- **(+)** N subscribers can read the same event stream (web fan-out falls out).
- **(+)** Truly embeddable, in-process, no I/O.
- **(−)** Approval semantics flip from "pause the call" to "emit `ToolRequest`,
  park the turn on the inbox until an `Approve` arrives." There is no call to
  block.

## Alternatives considered

- **Keep the pull-based `Transport` trait.** Rejected: cannot multiplex sessions
  or fan out to many clients; and an embedder can't drive it without a transport
  shim.
- **JSON-RPC with request/response id correlation.** Rejected: the `agent`
  reference showed fire-and-forget event push (with a monotonic `seq` for
  ordering, not correlation) is sufficient and simpler. Approval correlation
  rides on the model's own `request_id`, not a transport-level id.
