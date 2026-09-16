# 0198. Out-of-mask tool calls are approvable

- Status: Accepted
- Date: 2026-09-14
- Amends: [ADR-0192](0192-universal-advertisement-enforcement-at-dispatch.md) (the
  mask stays enforced entirely at dispatch, but is no longer a hard boundary —
  most mask misses now park an approval instead of an attributed flat decline)
  and [ADR-0196](0196-tool-search-and-lazy-discovery-replace-the-invoke-envelope.md)
  (unaffected mechanically: the lean kernel, `explore`/`describe`, and the
  per-wire encodings all still work exactly as before — a discovered,
  out-of-mask tool now offers an approval on its first real call instead of
  an attributed decline).
- Relates to: [ADR-0149](0149-per-session-tool-overlay.md) (the session tool
  overlay — this ADR's `Session` scope is *another writer* of the same
  overlay state, not a new mechanism), [ADR-0052](0052-approval-scope-and-persisted-grants.md)
  (the `ApprovalScope` enum and its `Session`-upgrades-`Ask`-to-`Allow`
  precedent this ADR extends to mask existence, not just grade),
  [ADR-0138](0138-sponsored-build-child-and-propose-plan-cycle.md) (spawn
  sponsorship, the reason `agent`/`agent_send` stay hard-excluded),
  [ADR-0197](0197-compound-bash-commands-grade-per-segment.md) (an approved
  out-of-mask `bash` call still grades per-segment on replay — untouched).

## Context

ADR-0192 made advertisement universal: the model sees every tool's schema
regardless of the active profile's mask, and a call to a masked-out tool dies
with an attributed flat decline (`Declined by agent profile …`) on the
`is_error` channel. That was the right call when the alternative was a
cache-busting mid-session advertisement change — but it also means a
legitimate "let me use this once" request from the user has no path: the
mask was authored once, ahead of time, and the only way past it is editing
the agent file or the session's overlay out of band, before the model ever
tries the call.

The user wants the mask to behave like an ordinary `Ask` permission grade
instead: the model calls the tool, the user sees an approval prompt naming
which authority withheld it, and approving unlocks the call — once, or for
the rest of the session. This is a deliberate widening of what a mask miss
*means*, so it needs its own ADR rather than folding into ADR-0192's text
(which is immutable).

## Decision

### 1. Two scopes: `Once` and `Session` — no `Always`

The wire's `ApprovalScope` enum (ADR-0052) is unchanged (no protocol
surface added), but this feature only honors two of its four variants:

- `Once` — this single call proceeds; nothing persists.
- `Session`/`SessionDir` — materializes a session tool-overlay **enable**
  entry (ADR-0149) for the exact tool name, with `allow: true` (a new
  `ToolOverlayEntry::allow` constructor) — the tool both exists for the rest
  of the session *and* skips the permission prompt on every later call,
  mirroring the "stop asking me" semantics an ordinary Session grant already
  gives an in-mask `Ask` tool (ADR-0052). `SessionDir` has no narrower
  meaning for a whole-tool mask widening, so it degrades to `Session`.
- `Always` is **not offered** as a durable mask-widening scope — durable
  widening stays an explicit agent-file/allowlist edit (ADR-0083), so a
  model that got used to a tool via approval doesn't silently rewrite the
  profile's authored intent. The protocol's approval-scope set is fixed per
  ADR-0052, not configurable per request, so an `Always` sent anyway is not
  rejected — it **degrades to `Session`** with a `tracing::warn!` notice.
  This is a deliberate compromise: changing the protocol to advertise which
  scopes a given `ToolRequest` accepts was rejected as far more surface than
  this feature needs (see Alternatives).

### 2. Three hard limits still flat-decline, no prompt ever offered

- **(a) An explicit, bare-name `Deny` rule.** If the profile chain or the
  config ceiling carries a literal rule keyed to exactly this tool's name
  with value `Deny` (`PermissionProfile::explicit_bare_deny`), the call
  flat-declines with the ordinary permission-deny wording
  (`tool \`x\` denied by permission profile`) — the author's deliberate
  floor for *this* tool, not the mask.

  Critically, this is narrower than "the tool's grade would resolve to
  `Deny`": the **ambient `default`** a profile falls through to for every
  tool it never mentions does **not** count, nor does a bare `*` catch-all,
  nor an argument-/workdir-scoped rule. `explore`'s embedded definition is
  `default: deny` with no rule ever naming `edit` or `write` — under the
  old flat-decline behavior this was indistinguishable from an explicit
  floor, but it is not one: the profile author excluded `edit` from the
  mask (deliberate), and separately never wrote an opinion about its grade
  (not deliberate, just unreached). Treating the ambient default as a hard
  floor would mean *no* out-of-mask tool under any `default: deny` profile
  (the common shape — most masked profiles rely on the mask, not per-tool
  `deny` rules) is ever approvable, defeating the feature for its most
  common case. So `explore`+`edit` **parks an approval**; a profile that
  additionally writes `edit: deny` explicitly still flat-declines.

  This check reads the local `AgentProfile`/config-ceiling rule sets
  directly (already how the mask check itself works, `tool_mask_source`),
  not through the pluggable `PermissionResolver` seam (#311): a custom
  resolver's own `Deny` answer is opaque (can't be told apart from a
  default), so it is never treated as this hard floor for an embedder's
  resolver. An embedder wanting its own resolver's `Deny` to also block the
  approval offer expresses it as an explicit bare rule on the session's
  `AgentProfile` instead.

- **(b) `agent`/`agent_send`.** Spawn control is profile-defining (the
  ADR-0192 carve-out: these specs vary legitimately across profiles, never
  within a session) and sponsorship (ADR-0138) depends on spawn staying a
  hard per-profile boundary — an out-of-mask spawn call keeps the pre-ADR-0198
  flat decline unconditionally, independent of any grade.

- **(c) An unknown tool name.** A hallucinated name that matches no
  registered tool (and isn't the runtime-owned `update_tasks` state tool)
  keeps its existing fuzzy-match unknown-tool hint — offering an approval
  for a call that can only fail afterward would be pointless and confusing.

Everything else — including a mask miss caused by a session overlay **deny**
entry (the user's own earlier `/disable tool x`), not just a profile mask —
is approvable. This is a deliberate reading of "everything else": an
overlay deny is not listed as a fourth hard limit, so it converts too.

### 3. UX: the existing approval flow, no protocol change

The parked request is an ordinary `ToolRequest`/`Approve`/`Reject`
round-trip — the same TUI/`serve` approval surface, `InMsg`/`OutEvent` DTOs
completely untouched. There is no separate "reason" field on `ToolRequest`
to carry the mask attribution, so it rides the same mechanism an
escape-root-forced approval already uses: appended to the `input` text a
head renders, e.g.:

```
{original input}

⚠ tool `edit` is outside agent profile `explore`'s tool mask — approve to run it
```

The attribution wording (`decline::mask_request_attribution`) reuses the
same `MaskSource`/`MaskAuthority` pair `mask_decline` already computes for
the flat-decline case — own profile, ancestor profile, own overlay, ancestor
overlay, or (rarely) an unseen fail-closed session — so the two code paths
can never drift apart on *who* is named.

### 4. Ladder shape: one function call for the "rest of the ladder"

The dispatch loop's mask check (`tool_runner`'s main loop, before
`Intercept::classify`) now branches instead of unconditionally declining:

```
masked?
  ├─ is_spawn_tool(tool)              → flat decline (b), unchanged wording
  ├─ !registry.contains(tool)         → flat decline (c), unknown-tool hint
  ├─ explicit_deny_floor(...)         → flat decline (a), "denied by permission profile"
  └─ else                             → mask_request::handle (park + approve/reject)
```

`mask_request::handle` parks exactly one `ToolRequest` (mask-attributed).
On `Reject`, it declines like any other rejected approval. On `Approve`, it
computes a `forced_entry: Option<ToolOverlayEntry>` from the scope and then
calls `tool_runner::dispatch` — **the same function** an in-mask call's
`Ask` grade already goes through — passing `forced_entry` as its
`overlay_entry` parameter:

- `Once` → `forced_entry` is whatever overlay grade entry the ordinary chain
  walk already found for this tool (typically `None`), **unchanged** from
  what an in-mask call would have received. This is deliberately the *same*
  input `dispatch` would see if the tool had never been masked at all: the
  call now proceeds through the real permission ladder exactly as written.
  If the underlying grade is itself `Ask` (not just the mask), `dispatch`
  legitimately parks a **second**, ordinary `ToolRequest` — the mask was not
  the only reason to prompt, so the user is asked again, this time by the
  permission layer, not the mask. If the underlying grade is `Allow`, this
  was the only prompt — a `Once` approval on a mask-miss whose grade would
  have been `Allow` is, and stays, a single round-trip.
- `Session`/`SessionDir` → `forced_entry` is `Some(ToolOverlayEntry::allow(tool))`,
  the *exact* entry just persisted via `SetToolOverlay` (not re-read back
  from the folded overlay state, closing the race a re-read would invite).
  `dispatch` therefore resolves `Allow` deterministically — no second
  prompt, ever, for this call or the next.

This was chosen over two alternatives (see below) as the cleanest available
shape: it adds no new permission-resolution logic beyond the hard-limit (a)
precheck (which duplicates roughly ten lines of `dispatch`'s own opening —
necessary, since peeking at what `dispatch` *would* decide without running
it requires computing it once outside), and every downstream ladder step —
alias rewrite, hooks, escape-root, the actual host-tool execution — runs
completely unchanged because it *is* the same function.

One consequence worth naming plainly: hard limit (a) is deliberately
narrower than "grade would resolve to Deny" (§2). This means a mask offer
can be approved and the *replayed* ladder can still refuse the call on the
ambient-default grade — e.g. `explore`+`write` parks, and an `Approve`
still ends in "tool `write` denied by permission profile" because
`explore`'s `default: deny` is real, just not an *explicit* floor. This
costs one wasted round-trip in that specific combination, accepted as the
price of not making every `default: deny` profile's entire out-of-mask
surface permanently unapprovable (the much more common and much worse
failure mode).

### 5. Uniform across advertising modes, wires, and MCP

Nothing in this ADR is advertising-mode-specific: `ToolSearch` mode's
`explore`/`describe` pair is unaffected (still always-on, non-maskable,
per ADR-0196 §4), and a tool discovered via `describe` under `client_side`
encoding behaves identically to a tool that was in the `Full`-mode roster
all along once the model actually calls it — the mask check runs at
dispatch either way. `mcp__*` tools go through the identical path: an
out-of-mask MCP tool name parks the same mask-attributed approval as any
built-in.

## Files

- `entanglement-core/src/protocol.rs`: `ToolOverlayEntry::allow` (flat-grant
  constructor), `PermissionProfile::explicit_bare_deny` (the narrow
  bare-name-rule check hard limit (a) needs).
- `entanglement-runtime/src/decline.rs`: `mask_request_attribution` — the
  approval-offer counterpart to `mask_decline`, same wording table.
- `entanglement-runtime/src/mask_request.rs` (new): `is_spawn_tool`,
  `explicit_deny_floor`, `handle` (the park/approve/reject orchestration),
  `materialize_session_grant`.
- `entanglement-runtime/src/tool_runner.rs`: the mask-miss branch now
  routes through the above instead of an unconditional decline; `dispatch`
  and `set_thinking` are `pub(crate)` so `mask_request` can call them.

## Consequences

### Positive

- The mask stops being a dead end reachable only by editing files out of
  band — a user can grant a one-off or session-wide exception from the
  approval prompt itself, exactly like any other `Ask`.
- No protocol change: every existing head (TUI, `serve`, an embedder) gets
  this for free — the approval UI it already renders for `Ask` renders this
  too, with the attribution folded into the same text field.
- The single-prompt story is honest, not hand-waved: it is a direct
  consequence of replaying the real `dispatch` function, not a separate
  "did we already ask" bookkeeping layer that could drift from it.

### Negative / accepted trade-offs

- **`Always` is unavailable and silently degrades to `Session`** rather than
  the protocol expressing "this request doesn't support that scope." A head
  that lets a user pick `Always` from a generic menu gets a `Session` grant
  instead, logged but not surfaced back over the wire (no field to carry it
  in without a protocol change — the explicit trade this ADR makes, see
  Alternatives).
- **Hard limit (a) can still let a mask offer end in a decline anyway**
  (§4's `explore`+`write` example) — a wasted round-trip in the specific
  combination of "masked out" + "no explicit rule" + "ambient default
  denies." Accepted because the alternative (treating the ambient default
  as a floor) defeats the feature for the common case.
- **A custom `PermissionResolver`'s own `Deny` is never a hard floor** for
  this feature — only the local `AgentProfile`/ceiling rule set is
  introspected. An embedder wanting parity must express the floor as an
  explicit bare rule on the profile, not solely in resolver logic.
- **The `Session`-scope overlay write races against a concurrent overlay
  change** (another approval, a `/disable`, a TUI edit) between when this
  call snapshotted the session's overlay entries and when it sends the
  full-replacement `SetToolOverlay` — the same inherent race ADR-0149 already
  accepts for any two concurrent overlay writers ("the head computes the new
  list from the last confirmation it holds"). Not new to this ADR, just
  another writer of the same state.

## Alternatives considered

- **Force `Allow` for every approved out-of-mask call, mask or grade.**
  Rejected: a profile's own `Ask` grade for a tool is a deliberate signal
  ("run this, but confirm each time") independent of the mask; silently
  upgrading it to `Allow` on the strength of a mask approval would erase
  that signal. Replaying the ladder unchanged preserves it, at the cost of
  a possible second prompt — judged the more honest trade.
- **Add an `available_scopes` field to `ToolRequest`.** Would let the
  protocol tell a head exactly which scopes a given request supports (so
  `Always` could be genuinely absent from the UI instead of silently
  degrading). Rejected for this change: it is real protocol surface for a
  narrow need, and every existing head would need updating to read and
  honor it; degrading `Always` to `Session` with a logged notice solves the
  immediate problem without it. Left as a natural follow-up if more approval
  kinds want to restrict their offered scopes.
- **Treat any mask-miss whose grade would resolve to `Deny` (ambient default
  included) as the hard floor.** Rejected (§2/§4): defeats the feature for
  every `default: deny` profile's entire out-of-mask surface — the common
  shape, not the exception.
- **A brand-new grant mechanism instead of reusing the ADR-0149 overlay.**
  Rejected: the overlay already expresses exactly "this tool exists for
  this session, with this grade" — inventing a parallel mechanism would
  duplicate `tool_mask_source`/`overlay_grade_entry`'s walk for no benefit.
- **Recompute permission independently instead of calling `dispatch`.**
  Rejected: would duplicate escape-root handling, alias rewriting, hooks,
  and the schema-validation pre-check — four more places to keep in sync
  with the real ladder. Reusing `dispatch` verbatim is the "single prompt
  where possible" mechanism *and* the correctness argument in one.

## Deferred

- Surfacing which scopes a `ToolRequest` supports over the wire (see
  Alternatives) — filed as a follow-up if a future approval kind needs it
  too, rather than one-off for this feature.
