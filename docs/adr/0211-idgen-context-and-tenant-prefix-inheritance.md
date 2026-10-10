# 0211. `IdGen` receives a lineage context; children and successors keep the tenant prefix

- Status: Accepted
- Date: 2026-10-10
- Amends: [ADR-0164](0164-short-sortable-kind-tagged-ids.md) (`IdGen::next`
  gains a context argument; the default prepends an inherited prefix)

## Context

[embedding.md](../embedding.md) §1 tells a multi-tenant embedder to mint root
sessions as `{tenant}:{id}` and recover the tenant by splitting on the first
`:`, and claimed that "filtering by root prefix already covers the whole
subtree". It did not (issue #722). Two kinds of session are minted by the
engine, not by the embedder's head:

- a **sub-agent child**, minted by the runtime's `agent` tool
  (`subagent.rs`) through `Holly::next_id(IdKind::Session)`;
- a **compaction successor**, minted by `session/fork.rs` on every
  compaction ([ADR-0205](0205-every-compaction-forks-a-successor-session.md)),
  a fresh *root* with `parent: None`.

Both came out as bare `s-…` ids. Site's `user_id_from_session` and
chess-base's ownership index both key on the prefix, so a child's tool calls
were denied and a successor's events dropped. Embedders also had no
synchronous way to ask the engine who owns a session: the only source was
folding `SessionStarted` off the broadcast, which races the session's first
tool call.

## Decision

1. **`IdGen::next(&self, kind: IdKind, ctx: &IdContext<'_>) -> String`.**
   `IdContext { parent, predecessor, user }` is a borrowed, `Copy`,
   `#[non_exhaustive]` struct (all `Option<&_>`) so it can grow without
   breaking an implementor; it is built with `IdContext::default()`,
   `child_of(&parent)`, `successor_of(&predecessor)` and `with_user(..)`.
   Jobs, scripts, retained outputs, request ids and fresh roots pass an empty
   context.
2. **The default inherits the prefix.** `DefaultIdGen` prepends
   `ctx.inherited_prefix()`: everything up to and including the first `:` of
   the parent's id, else the predecessor's. The child or successor of
   `t1:s-6a70…` is `t1:s-6a71…`; a lineage without `:` keeps ADR-0164's plain
   15-character form, so every in-tree head is unchanged. `inherited_prefix`
   is public so an overriding generator can reuse it; an override owns the
   whole format otherwise.
3. **Both engine mint sites pass their lineage.** The `agent` tool calls the
   new `Holly::next_id_in(kind, &IdContext::child_of(&parent))`; the fork
   passes `successor_of(source)` with the source session's own `user`.
   `Holly::next_id(kind)` stays and mints with an empty context.
   `next_id_in` fills a missing `user` from the lineage's directory entry
   (parent first, then predecessor), so the runtime — which must not name
   `UserId` ([ADR-0181](0181-userid-leaves-the-runtime-crate.md)) — still
   hands a custom generator the owner.
4. **`Holly::session_info(&SessionId) -> Option<SessionInfo>`.** The
   supervisor's `session_meta` directory moves behind a shared mutex
   (`holly/directory.rs`, the `SeqRegistry` pattern) so `Holly` can read it
   synchronously. `SessionInfo` gains `predecessor: Option<SessionId>`
   (`serde(default, skip_serializing_if)`, so `SessionList` frames and old
   payloads are unchanged when absent), filled on spawn and from the log's
   `SessionStarted` on resume.

**`session_info` semantics.** `Some` from the moment the supervisor registers
the session — before its `SessionStarted` is broadcast — until it processes
the id's `CloseSession` or hibernation; `None` for an unknown, closed or
hibernated id. A resume, explicit or through the unknown-session loader
([ADR-0210](0210-unknown-session-policy.md)), re-registers the same lineage
and `user` from the log. It answers "who owns this live session" for a
resolver handling its tool call. It is not an event-routing key: a session's
trailing `SessionEnded` arrives after its entry is gone, so routing keys on
the inherited prefix.

## Consequences

- With the default generator, `tenant_of(id)` holds for a root's whole
  lineage by construction; embedding.md §1's claim is now true, and says
  why. An embedder with its own `IdGen` must honor the context to keep it.
- **Breaking for embedders (0.7 migration guide, #729):** every `IdGen`
  implementation adds the `ctx` parameter; a direct caller of `IdGen::next`
  passes `&IdContext::default()` for the old behavior. A `SessionInfo` struct
  literal adds `predecessor`.
- A compaction successor's id carries its tenant, so a store keyed by root id
  and a `SessionLoader` validating ownership by prefix see it as the
  tenant's own.
- The directory mutex is never held across an `.await`; a poisoned lock is
  recovered rather than propagated, since the map is a plain insert/remove
  table that cannot be left half-written.

## Alternatives rejected

- **A defaulted second trait method (`next_in`) that wraps `next`.** Non-
  breaking, but a custom generator would silently get the prefix glued onto
  its own format, and the context would be optional to honor rather than part
  of the contract.
- **Inheriting the prefix in the supervisor's `Spawn` handler.** The id is
  minted before `Spawn` is sent (the runtime registers the handle first), so
  rewriting it there would hand the parent a handle that names no session.
- **Retaining `session_info` entries after close/hibernate.** Unbounded growth
  for a long-lived embedder, and a stale answer for a hibernated id that may be
  resumed by another process.
- **Changing `SessionInfo.root` from `bool` to the root's id.** A wire-breaking
  change for a value a caller can compute by walking `parent`.
