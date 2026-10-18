# Embedding §1–§2 — Many tenants & a custom head

> Part of the [embedding guide](../embedding.md).

## 1. One engine, many tenants

Run **one `Holly`**, not one per tenant. `Holly::spawn` is a supervisor task
plus two broadcast channels ([`entanglement-core/src/holly.rs`](../../entanglement-core/src/holly.rs));
sessions are cheap tasks keyed by `SessionId`
(`entanglement-core/src/holly/supervisor.rs`, the `supervisor` loop), created
on first message per the unknown-session policy (§3). Spinning up a whole
engine per user buys you nothing the session key doesn't already give you, and
it multiplies every piece of shared state (the provider connection pool,
[ADR-0050](../adr/0050-per-endpoint-connection-pool-retry-rate-limit.md); the
definitions watcher, [ADR-0084](../adr/0084-runtime-live-reload-and-managed-file-locking.md))
by tenant count for no isolation benefit — `SessionId` is a plain string
(`pub struct SessionId(pub String)`), so the isolation you actually need is a
**naming convention**, not a process boundary.

**Convention: `{tenant}:{uuid}`.** Mint every root session as
`format!("{tenant}:{}", SessionId::new_uuid())`. The tenant is recovered by
splitting on the first `:`:

```rust
fn tenant_of(session: &SessionId) -> &str {
    session.0.split_once(':').map_or(&session.0[..], |(t, _)| t)
}
```

Two kinds of session are minted by the engine, not by your head: a sub-agent
child (the runtime's `agent` tool) and a **compaction successor** — every
compaction forks one (`predecessor: Some(source)`,
[ADR-0205](../adr/0205-every-compaction-forks-a-successor-session.md)), a root
when its source was one and a child of the source's parent when the source was
a sub-agent ([ADR-0234](../adr/0234-live-sub-agents-survive-compaction.md)). Both go
through `EngineConfig::id_gen` with an `IdContext` naming their lineage, and
the default `DefaultIdGen` copies the parent's — else the predecessor's —
prefix up to and including the first `:` ([ADR-0211](../adr/0211-idgen-context-and-tenant-prefix-inheritance.md)):
the child or successor of `acme:s-6a70…` is `acme:s-6a71…`. So `tenant_of`
holds for the whole lineage **by construction, with the default generator**.
A root with no `:` mints unprefixed descendants, as before.

If you install your own `IdGen` (a DB sequence, UUIDs, a different tenant
encoding), the prefix is yours to keep: `next(kind, ctx)` receives
`ctx.parent`/`ctx.predecessor`/`ctx.user`, and `ctx.inherited_prefix()` gives
the default's answer to reuse. A generator that ignores the context breaks
prefix filtering for every child and successor. When your head mints an id in
a lineage itself, call `Holly::next_id_in(kind, &IdContext::child_of(&parent))`
rather than `next_id`, which mints with an empty context.

**Ownership filtering on `subscribe()`.** `Holly::subscribe()` is one
broadcast fan-out of **every** tenant's events — there is no
`subscribe_session`/`subscribe_tenant` filter built into core, and none is
needed: the filter is a one-line predicate over the plain-string `SessionId`,
and different embedders want different scopes (exact session, a whole root's
subtree, a whole tenant), so a single core helper would either be too narrow or
grow parameters for each shape. Apply the filter once, at the point you relay
an event out to a tenant's transport (a WS socket, an SSE stream) — never
forward an event before checking it:

```rust
let mut sub = holly.subscribe();
loop {
    let ev = match sub.recv().await {
        Ok(ev) => ev,
        Err(RecvError::Lagged(_)) => continue, // §5
        Err(RecvError::Closed) => break,
    };
    if ev.session() != Some(&session) {
        continue; // another tenant's event on the shared fan-out
    }
    relay_to_tenant_socket(ev).await;
}
```

`OutEvent::session()` returns `None` for the supervisor-global query replies
(`SessionList`, `QuestionList`, `OperationList`, `PendingList`, `McpList` —
[ADR-0072](../adr/0072-protocol-warts-settled-before-serve.md)), so the filter
above passes none of them. Several of these replies **do carry tenant data**,
and an unscoped query covers every session in the process:

- `SessionList.sessions`: every live session.
- `QuestionList.questions`: every open `ask_user` question text.
- `OperationList.operations`: every background job's command line.
- `PendingList.approvals`: every parked tool call's full `input`, i.e. plan
  bodies, file contents and commands
  ([ADR-0217](../adr/0217-list-pending-query-for-parked-approvals.md)).

A multi-tenant head that forwards these queries over `send_from_wire` **must**:

1. scope the query itself before sending it: force `ListPending.root` (or
   `ListQuestions`/`ListOperations`' `session`) to a session the connection
   owns, **or** filter the reply's payload by session ownership; and
2. deliver the reply only to the connection that sent its `correlation_id`.
   Replies are broadcast to every subscriber, so relaying one to all
   connections leaks it even when the query was scoped.

## 2. A custom head over the ABI: the trust split

`Holly` exposes two inbound entry points, and picking the wrong one is a
privilege-escalation bug, not a style choice
([ADR-0069](../adr/0069-trusted-untrusted-wire-frame-split.md)):

- **`Holly::send`** — the privileged, in-process path. Use it for anything
  *your embedder itself* decided to send: a `Prompt` you constructed from a
  request your auth layer already validated, a `ToolResult` your own executor
  produced, an internal `Spawn`.
- **`Holly::send_from_wire`** — for anything **deserialized from a client's
  bytes** (a WebSocket frame, an HTTP body). It enforces `InMsg::wire_allowed()`
  and refuses the runtime-authored trio (`ToolResult`, `Spawn`, `Resume`) with a
  `WireError`, because a forged `ToolResult` resolves a parked turn on
  `request_id` alone — bypassing both tool execution and permission dispatch —
  and a forged `Spawn` bypasses the tool path's spawn-refusal gate.

The rule of thumb: **who typed this frame?** If it's a value your server code
built after checking the caller's identity/tenant, `send` it. If it's bytes a
client sent that merely deserialize into an `InMsg`, always go through
`send_from_wire` — even in a trusted-tenant, authenticated-client setup, because
the split isn't about trusting the *user*, it's about not letting a client
socket masquerade as your own executor. `entanglement-runtime`'s own `pipe`
head is the reference: every stdin line that parses as an `InMsg` goes through
`send_from_wire` (`entanglement-runtime/src/pipe.rs`) — only the fallback case,
where `pipe` itself decides to wrap an unparsable line as a `Prompt` it
constructed, uses plain `send`. Same discipline applies even though `pipe` is a
trusted local process; it's what keeps a `serve`-style head safe once its input
actually is a remote client.

Multi-tenant addition on top of the ABI split: your head must also stamp or
validate the tenant prefix on every inbound frame's `SessionId` before handing
it to either `send` method — the trust split stops a client from forging
*privileged variants*, but nothing in core stops an authenticated tenant A from
addressing tenant B's session id if your head doesn't check. That check belongs
to your auth layer, not to `entanglement-core`, for the same reason the tenant
convention above is a naming idiom and not a core type: core has no concept of
"tenant" to enforce. With `UnknownSession::Resume` (§3) this check also guards
**evicted** sessions: an unchecked frame naming another tenant's hibernated id
would revive it — with that tenant's history and `user` (hence provider key) —
so validate ownership before `send_from_wire`, not only for live ids.

---

Next: [§3 — Custom persistence](persistence.md)
