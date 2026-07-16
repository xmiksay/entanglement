# 0090. Optional `idle_ttl` — supervisor auto-hibernates settled sessions

- Status: Accepted
- Date: 2026-07-16
- Follow-up to [0077](0077-session-hibernation-evictable-resumable.md) (#318),
  which shipped `HibernateSession` but deliberately left auto-hibernation on an
  idle TTL to the embedder. Builds on the parked-turn state of
  [0061](0061-parked-turn-state-batch-tool-resolution.md) and reuses the
  sub-tree cascade `HibernateSession` introduced. Issue #363.

## Context

`HibernateSession` (#318) gives an embedder manual eviction, which is enough
for a policy-owning embedder that already drives idle timeouts from its own
persistence layer. But every other long-lived embedder — and `skutter serve`
itself — has to reimplement the same boilerplate to avoid leaking sessions
until restart: track last-activity per session, run a sweep timer, skip
sessions that are mid-turn, send `HibernateSession`. A forgetful embedder never
does this, and memory grows monotonically — the exact failure #318 was filed
against, just pushed one layer up instead of solved.

ADR-0077 named the reason it didn't take this on directly: *"it needs the
supervisor to observe outbound activity (a long streaming turn has no inbound
traffic) to avoid evicting a busy session, which the supervisor does not
subscribe to."* That framing assumed settledness had to be inferred from
traffic. It doesn't: core already has an exact, cheap settledness signal —
`Session::turn.is_none()`. A turn is `Some` for the *entire* span core needs to
protect — mid-stream, and parked on a tool/approval/`ask_user` result (both
runtime-side waits ride the same pending `ToolExec` batch, so
`WaitingApproval`/`WaitingAnswer` never exist without `turn.is_some()`). So
"never touch a session core can't prove is idle" reduces to "never touch a
session with `turn.is_some()` anywhere in its spawn sub-tree" — no runtime
`AgentState` needed, no outbound-broadcast subscription needed.

## Decision

**Add `EngineConfig.idle_ttl: Option<Duration>` (default `None`, byte-identical
to today). When set, the supervisor runs a coarse periodic sweep that
auto-hibernates a settled root — and its whole spawn sub-tree — once idle past
the TTL, driving the existing `HibernateSession` mechanism rather than adding a
second eviction path.**

### Settledness: a shared `ActivityRegistry`, not supervisor traffic-watching

Each session task already sits inside `holly.rs`'s per-session task model; it
alone knows its own `Session::turn`. Add a new shared map, `ActivityRegistry`
(`Arc<Mutex<HashMap<SessionId, Option<tokio::time::Instant>>>>`), populated the
same way `SeqRegistry` already is (a session task publishes to it, the
supervisor only reads):

- `None` — the session is mid-turn *or* parked on a tool/approval/question
  result (`Session::turn.is_some()`).
- `Some(instant)` — the `tokio::time::Instant` the session last became settled
  (`Session::turn` went back to `None`).
- **Absent** — not yet reached its first idle point, or already gone. Treated
  as unsettled: the sweep only ever evicts a session it can positively prove is
  idle, never one it merely lacks data on.

The session loop publishes this at the top of every iteration (covers the
common transitions — a round ending in `Done`, a park, a `Stop`-cleared turn)
**and** the instant a `Prompt` flips `turn` from `None` to `Some`, *before*
`drive_turn` runs — the top-of-loop write from the previous (idle) iteration
would otherwise sit stale in the map for the entire streaming round, which is
exactly the failure mode ADR-0077 was worried about. `tokio::time::Instant`,
not `std::time::Instant`: the sweep timer and the settle timestamps must share
one clock so a paused/advanced test runtime (`start_paused = true` +
auto-advance) drives both consistently.

### Supervisor sweep: coarse, and armed only when configured

`holly::supervisor` replaces its bare `rx.recv().await` with a `tokio::select!`
over `rx.recv()` and, only when `idle_ttl` is `Some`, a `tokio::time::interval`
at `max(idle_ttl / 4, 30s)` — an eviction poll, not a scheduler; a session can
sit idle up to one extra sweep period past `idle_ttl` before it's noticed, which
is fine for a memory-capping policy. When `idle_ttl` is `None` the interval
branch doesn't exist at all, so the `None` path is the exact same code as
before this issue — the acceptance criterion "`idle_ttl: None` → behavior
byte-identical" falls out of the `Option` shape, not a runtime check.

Each tick: for every **root** session (`SessionInfo::root`), walk its spawn
sub-tree (`collect_subtree`, the same helper `CloseSession`/`HibernateSession`
already use) and require every member to have a `Some(instant)` entry — one
`None` or missing entry anywhere in the sub-tree skips the whole root, no
matter how long its other members have been idle. When every member is
settled, the sub-tree's idle-since point is the **latest** of their individual
settle instants (a recently-active child resets the whole tree's window, not
just its own) — then `now - idle_since >= idle_ttl` decides eviction.

Qualifying roots hibernate through a `hibernate_subtree` helper extracted from
the existing `InMsg::HibernateSession` handler (now shared by both call
sites) — same cascade, same **no tombstone**, same `OutEvent::SessionHibernated`,
same resumability via `Holly::resume` as a manual eviction. No new lifecycle
state, no new outbound event.

### Deliberately stricter than manual `HibernateSession`

Manual hibernation is stop-then-hibernate: it will cancel a mid-stream turn on
request, because an embedder invoking it has made a deliberate policy call. A
*timer* firing in the background has made no such call, so the sweep never
touches a session with `turn.is_some()` anywhere in its sub-tree — it only ever
evicts a session already at rest. This is stricter, on purpose: a background
sweep must never be indistinguishable from "the engine randomly cancelled my
in-flight request."

## Consequences

- An embedder gets memory-capping for free by setting one `Duration` — no
  per-session bookkeeping, no sweep timer, no turn-state awareness of its own.
  `skutter serve` can wire this once configuration exposes it (deferred; this
  ADR is core-only, matching how `reoffer_interval` shipped before any runtime
  config surface existed for it).
- `idle_ttl: None` remains the only supported "eviction is my policy, not the
  engine's" mode — a multi-tenant embedder with its own idle-timeout logic
  (the motivating case ADR-0077 named, xmiksay/site#13) keeps driving
  `Holly::hibernate` directly and simply never sets `idle_ttl`.
- Settledness piggybacks on `Session::turn` alone; no `AgentState` plumbing,
  no supervisor subscription to the outbound broadcast — narrower than the gap
  ADR-0077 originally described, because that gap was framed in terms of
  traffic-watching rather than the turn-state signal core already had.

## Alternatives rejected

- **Per-session self-timer** (mirroring the ADR-0071 re-offer timer). Wrong
  owner: cascade-safety ("a parked child pins the whole tree live") needs
  sub-tree visibility that only the supervisor's `parent_links` has: a session
  task can't see its children's state.
- **Infer settledness from outbound traffic** (the shape ADR-0077 assumed
  necessary). Would require the supervisor to subscribe to its own broadcast
  outbox and re-derive turn state from event types — strictly more machinery
  than reading `Session::turn.is_none()` off a registry the session already
  updates, for the same answer.
- **A dedicated `Clock` trait for test injection.** No such abstraction exists
  anywhere in this codebase; every timer uses tokio's directly. Reusing
  `tokio::time::Instant` + `tokio::time::pause()`/`start_paused` gives
  deterministic, wall-clock-free tests (the runtime auto-advances a paused
  clock once every task is otherwise idle) without introducing a new seam.
