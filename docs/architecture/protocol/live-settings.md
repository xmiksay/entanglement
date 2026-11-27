# Wire protocol §2 — Live generation & metadata

> Part of the [wire protocol module](../protocol.md) of the [architecture overview](../../architecture.md).

**Live generation-parameter changes — `InMsg::SetGeneration`** (#374,
[ADR-0094](../../adr/archive/0094-reasoning-effort-and-per-profile-generation-persistence.md)).
`SetGeneration{session,overrides:GenerationParams}` merges a **partial**
`GenerationParams` (temperature / max-output / thinking-budget / the new
`reasoning_effort`) onto the session's current one via
`GenerationParams::apply_overrides` — a `None` field in `overrides` leaves the
corresponding field untouched, so `/set temperature 0.7` only touches
`temperature`. Unlike `SetModel` there is no resolver to fail against (a pure
local merge, no network/catalog lookup), so it always succeeds and **always**
emits `OutEvent::GenerationChanged{session,generation}` with the full merged
result — even when nothing actually changed — so a head can rely on the reply
alone to confirm the write landed. The merged result is also recorded into
`Session.profile_generation` keyed by the active profile (the
generation-parameter analogue of `Session.profile_models`, #323/ADR-0081), so a
later session spawned under that same profile re-applies it at start —
`SetAgent` is gone
([ADR-0207](../../adr/archive/0207-permission-modes-replace-agent-borne-authority.md)
§9), so session start is the only re-application locus left. Deferred
(stashed) while a turn is live, like `SetModel` (unlike `SetMode`, which
applies immediately, #560). See the engine doc
for the session-start precedence and the runtime doc for the
per-profile persisted store.

**Settable session display metadata — `InMsg::SetSessionMeta`**
([ADR-0151](../../adr/archive/0151-settable-session-metadata.md)).
`SetSessionMeta{session,name?,action?,if_unset=false}` merges a
human-readable display `name` (a session title, e.g. derived from the first
prompt) and/or the current `action` ("what the agent is doing now") onto
`Session.name`/`Session.action` — a `None` field leaves the stored value
untouched, `Some("")` clears it. Pure metadata: nothing in the engine reads
it. Unlike `SetGeneration` it is applied **immediately, never stashed** (the
`ChildSpawned` pattern) — `action`'s whole purpose is to change mid-turn, so
a set while the turn is parked on tool calls acks right away; a mid-*stream*
arrival still rides the generic stash like every command, since the session
task is single-threaded. Always acks with
`OutEvent::SessionMetaChanged{session,name?,action?}` carrying the **full
merged** values (both fields, not just the touched ones) — persisted and
replay-folded by overwrite (last write wins), like `GenerationChanged`.
Wire-allowed (cosmetic, session-scoped, no privilege). Heads fold it into
their session views (the `AgentChanged` pattern); it is deliberately **not**
mirrored into `SessionInfo`/`SessionList`, whose supervisor directory records
lineage facts only. The TUI's `/name <text>` sets `name` for the active
session (the sidebar title updating is the confirmation) and prefers `name`
over the short id and `action` over the first-prompt snippet in the sidebar
and sessions modal; `skutter sessions` and the resume modal recover the name
from the log's last `SessionMetaChanged` record. `action` has no in-tree
producer yet — the intended consumer is an external namer/status writer
sending the wire message. `if_unset` (#553) is the auto session-title
generator's guard: it always sets it `true`, so the fold applies `name` only
when `Session.name` is still `None` — a late generated title silently no-ops
(still acking, with the unchanged state) against a name set by `/name` (in
either arrival order) or restored on `Resume` from a prior process's log; the
generator itself also folds a `Resume`'s replayed `SessionMetaChanged`
history off the inbound fan-out to seed its per-process "already titled" set,
so an already-named resumed session skips the aux call on its next prompt too
(see [heads & persistence §6d](../heads/keys-and-aux-models.md#6d-per-purpose-auxiliary-models--auto-session-titles--adr-0154adr-0183-aux_llm--configaux_models--session_title--narrate)).

---

Previous: [§2 — Trust split, session lifecycle & single-shot ops](sessions.md) · Next: [§4 — Structured outputs](structured-outputs.md)
