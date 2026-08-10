# 0151. Settable session display metadata (name + action)

- Status: Amended by [0154](0154-per-purpose-auxiliary-models.md) (the in-tree auto session-title generator 0151 deferred shipped, driving `SetSessionMeta` off the aux `session_title` model), [0181](0181-narrate-purpose-and-per-user-aux-pins.md) (`action`'s first in-tree producer: the live action narrator drives `SetSessionMeta` off the aux `narrate` model on every tool call)
- Date: 2026-08-01

## Context

A session is identified to the user by its UUID (shortened to 8 chars in the
TUI) plus a derived first-prompt snippet. Neither is a *title*: the snippet is
whatever the user happened to type first, and engine-spawned children have no
head-side prompt at all. There is also no way to surface *what a session is
doing right now* beyond the coarse `AgentState` enum. The intended consumer is
an external namer — a head or embedder service that watches a session's
context and, via a common model/provider, derives a human title ("fix login
bug") and a live activity line ("running tests") — but no wire surface existed
to *set* such metadata. This ADR adds the settable surface only; no LLM-based
auto-naming ships with it.

## Decision

A new session-scoped frame pair, modeled on `SetGeneration`/`GenerationChanged`
with one deliberate divergence (immediacy):

- `InMsg::SetSessionMeta { session, name: Option<String>, action:
  Option<String> }` — a **merge**: a `None` field leaves the stored value
  untouched; `Some("")` **clears** it (no separate clear op — KISS). Stored on
  `Session.name`/`Session.action`; pure metadata, nothing in the engine reads
  it.
- **Immediate apply, never stashed.** `SetGeneration` defers behind a live
  turn (`s.turn.is_some() || s.paused` → stash). `action` exists precisely to
  change mid-turn, so `SetSessionMeta` takes the `ChildSpawned` path: applied
  the moment the session task dequeues it, even while the turn is parked on
  tool calls or the session is paused. (A mid-*stream* arrival is still
  deferred by the generic stash-at-next-safe-point mechanism every command
  rides — the session task is single-threaded — same documented scope
  boundary as ADR-0144's `Pause`.)
- `OutEvent::SessionMetaChanged { session, name, action }` — always emitted,
  carrying the **full merged** state (both fields as stored, not the delta),
  so a head folds by overwrite. Seq-less lifecycle event like
  `GenerationChanged`; persisted automatically (the tap persists any
  session-bearing event) and folded by `Session::replay` last-write-wins.
- **Wire-allowed.** Cosmetic, session-scoped, no privilege — same tier as
  `SetGeneration`. It cannot change what the model can do or see.
- **Not mirrored into `SessionInfo`/`SessionList`.** The supervisor's
  `session_meta` directory records creation-time facts only (documented on
  `SessionInfo`); adding live-updated fields would require a supervisor-side
  intercept for every metadata set. Heads fold `SessionMetaChanged` exactly
  like `AgentChanged`; offline listings (`skutter sessions`, the resume
  modal) recover the name from the log's last `SessionMetaChanged` record in
  the existing single-pass scan.

Head surfaces in this change: the TUI `/name <text>` command (raw-text
re-parse pattern) sets `name` on the active session — the sidebar title
updating is the confirmation; the sidebar/sessions modal prefer `name` over
the short id and `action` over the first-prompt description line. `action`
had **no in-tree producer at the time of this ADR** — the field was not dead
code; the external namer was the intended writer, until [0181](0181-narrate-purpose-and-per-user-aux-pins.md)
shipped the in-tree live action narrator.

## Consequences

- A future auto-namer needs zero protocol work: it subscribes, derives, and
  sends `SetSessionMeta` over any wire head.
- The full-state ack makes the event self-contained: replay, the store scan,
  and head folds never need to reconstruct a merge.
- `Some("")`-clears means the wire cannot express "set the empty string as a
  name" — accepted; an empty title is indistinguishable from unset anyway.
- Immediate apply means a `SessionMetaChanged` can interleave mid-turn between
  content events; it carries no `seq`, so transcript ordering is unaffected.

## Rejected alternatives

- **Extending `SessionInfo`** — turns the creation-only supervisor directory
  into a live-updated one for a purely cosmetic field.
- **Stash-deferred apply (the `SetGeneration` template verbatim)** — would
  make `action` useless: it could only ever change between turns.
- **A dedicated clear flag or separate `ClearSessionMeta`** — more surface for
  no expressiveness gain over `Some("")`.
