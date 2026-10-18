# Wire protocol §2 — Live generation, metadata & tool overlay

> Part of the [wire protocol module](../protocol.md) of the [architecture overview](../../architecture.md).

**Live generation-parameter changes — `InMsg::SetGeneration`** (#374,
[ADR-0094](../../adr/0094-reasoning-effort-and-per-profile-generation-persistence.md)).
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
([ADR-0207](../../adr/0207-permission-modes-replace-agent-borne-authority.md)
§9), so session start is the only re-application locus left. Deferred
(stashed) while a turn is live, like `SetModel` (unlike `SetMode`, which
applies immediately, #560). See the engine doc
for the session-start overlay precedence and the runtime doc for the
per-profile persisted store.

**Settable session display metadata — `InMsg::SetSessionMeta`**
([ADR-0151](../../adr/0151-settable-session-metadata.md)).
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

**Live tool overlay — `InMsg::SetToolOverlay`** (#539,
[ADR-0149](../../adr/0149-per-session-tool-overlay.md); live bash enablement
grades folded in, #611, [ADR-0163](../../adr/0163-live-bash-enablement-is-a-tool-overlay-entry.md)
— its registration half retired by
[ADR-0195](../../adr/0195-bash-is-the-default-exec-and-curated-read-only-rules.md)).
`SetToolOverlay{session,entries:[ToolOverlayEntry{pattern,allow,deny,arg_pattern?}]}`
**replaces** the session's live tool overlay: `*`/`?` patterns (unchanged
ADR-0148 glob semantics) that override the session's own **permission mode**
grade in both directions — an enable entry makes matching tools grade
`Allow` (or, `arg_pattern`-narrowed, an argument-scoped
`tool(arg_pattern): allow` rule) even past a mode `Deny` (`mcp__chessbase__*`
for a server, a literal name for one tool), a `deny: true` entry *withdraws*
them even when the mode would otherwise allow them (deny > enable > mode,
`ToolOverlayEntry::disposition`). Full replacement, not a merge (an empty
list clears); like
`SetGeneration` there is nothing to fail against, so it always succeeds and
always emits `OutEvent::ToolOverlayChanged` with the full effective list —
which is also what persistence logs and `Session::replay` folds back (by
overwrite), so a resumed session keeps its overlay. Session-scoped, but now
**mode-scoped too**
([ADR-0207](../../adr/0207-permission-modes-replace-agent-borne-authority.md)
§8): it is dropped wholesale on any live `SetMode` — an entry enabled under
one mode carries no meaning in another — where it used to simply survive
`SetAgent` unconditionally. `allow: false` (default) grades matching calls
`Ask`; `allow: true` grades them `Allow`, optionally narrowed by
`arg_pattern` (ADR-0163) to an argument-scoped `tool(arg_pattern): allow`
rule instead of a blanket grant — the grade override replaces the mode's own
resolution on the runtime's generic dispatch route, still clamped by the
config permission ceiling (a deny entry has no grade; it removes the tool
ahead of any permission decision). Don't confuse `pattern` (a tool-name
glob) with `arg_pattern` (a command-argument glob) — the two mean different
things despite the near-identical name. Disposition is resolved per
ancestor-chain link (`permission::ancestor_chain`), so a parent's overlay
also covers its spawn sub-tree — even though every session in that sub-tree
already shares the identical mode (§6), this walk still matters because the
overlay itself is per-session state, not part of the mode.
`bash` registers at startup like every other built-in (ADR-0195), so an
enable entry naming it is a pure grade override; there is no more closed
lazily-registrable-built-in table to describe.
An entry naming a tool dispatch never grades by name — `mcp` (unwrapped to
`mcp__<server>__<tool>` first), a discovery tool, `read_raw`/`glob_json`/
`grep_json` — is still accepted and echoed (core holds no tool policy), but
matches nothing; the in-tree runtime logs a no-effect warning when it folds
the reply, and the TUI refuses to send one (#843,
[gates §one surface](../gates/one-surface.md)).
Trusted-only for an **enable** entry
(wire-refused, the `McpAdd` rationale); a **deny-only** overlay (every entry
`deny: true`, including the empty list) is wire-allowed instead (#634,
[ADR-0177](../../adr/0177-wire-allowed-deny-only-tool-overlay.md) amending this
ADR) — it can only withdraw tools from dispatch, so a `serve`/`pipe` client
can self-restrict a session without an in-process head, though it still
cannot grant anything. The TUI drives the full surface via
`/enable`/`/disable` (a bare `/enable` being the session-tools checklist
dialog — the overlay as a diff against the mode's own grade) and the `/mcp`
panel's `d` key, which denies a server's `mcp__<name>__*` tools for the
session: with every registered server on by default
([ADR-0226](../../adr/0226-one-advertised-surface-mcp-envelope-registered-is-available.md)),
that per-session opt-out is the user control left (there is no enable key).
The overlay never touches the advertised array (ADR-0226 pins it per
session); it is still stash-deferred while a turn is live, like `SetModel`,
so every call of a round is graded under the one overlay the round started
with rather than a mid-round mix.

---

Previous: [§2 — Trust split, session lifecycle & single-shot ops](sessions.md) · Next: [§4 — Structured outputs](structured-outputs.md)
