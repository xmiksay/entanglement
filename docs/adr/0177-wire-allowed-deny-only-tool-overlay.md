# 0177. Wire-allowed deny-only tool overlay

- Status: Accepted
- Date: 2026-08-06
- Amends: [ADR-0149](0149-per-session-tool-overlay.md) (wire posture: a
  deny-only `SetToolOverlay` is now wire-allowed)

## Context

ADR-0149 made `InMsg::SetToolOverlay` trusted-only — refused by
`Holly::send_from_wire` — because an **enable** entry can inject a tool past
the agent mask, optionally graded `allow` with no approval prompt. That is
exactly the `McpAdd`/`McpRemove` rationale (ADR-0124): an unauthenticated
local `serve` connection, or a `pipe` head relaying arbitrary bytes, must
never be able to hand the model an un-prompted capability.

The consequence, tracked as deferred-work-ledger row 14 (#634, orig. #539):
only an in-process head (the TUI's `/enable`/`/disable`, or an embedder over
`Holly::send`) has any overlay surface at all. A `serve`/`pipe` client that
wants to *narrow* a session's tools — withdraw `bash`, mask out an MCP
server for one session — has no way to do it; the only wire-visible lever is
the whole-profile `SetAgent` switch, which is much coarser and also changes
everything else the profile carries (permission rules, system prompt, model
pin).

The ledger's own note named the fix: restrict wire opt-in to **deny-only**
entries, since a deny entry can only remove something the profile already
advertises — it has no path to widen the model's tool surface, so it carries
none of ADR-0149's or ADR-0124's risk. The other option the ledger named —
riding an authenticated head (ADR-0174) — isn't available yet: ADR-0174 is a
design only, no authenticated `serve` mode is built. Deny-only doesn't need
authentication to be safe, so it isn't gated on that follow-up.

## Decision

`InMsg::wire_allowed()` becomes **content-aware for `SetToolOverlay` only**
(every other variant stays a pure per-variant allow/refuse, preserving the
exhaustive-match fail-closed guarantee ADR-0124 established): a
`SetToolOverlay` frame is wire-allowed iff every entry in `entries` has
`deny: true` — `entries.iter().all(|e| e.deny)`. The empty list (clearing an
overlay back to the profile default) is vacuously deny-only and stays
wire-allowed, matching its existing semantics. A frame carrying **any**
enable entry is refused in full — since `SetToolOverlay` is a full
replacement, not a merge, there is no partial-apply to fall back to; a mixed
list is refused exactly like an all-enable one.

`Holly::send_from_wire` maps this specific refusal to a new
`WireError::OverlayEnable`, distinct from the blanket
`WireError::Privileged(variant_name)` every other refused variant gets — the
message names the actual constraint ("deny-only allowed") instead of
implying the whole frame type is off-limits, which would now be misleading.
Both `serve` and `pipe` add a match arm for the new variant (their
`Result<(), WireError>` handling is an exhaustive match with no wildcard, by
the same fail-closed convention) — logged and skipped, same as
`Privileged`.

No change to `ToolOverlayEntry`, `SessionCmd::SetToolOverlay`, or the
session-side apply logic (`s.tool_overlay = entries.clone()`,
`OutEvent::ToolOverlayChanged`) — the gate lives entirely at the wire
boundary, and everything downstream already treats a deny entry as pure
withdrawal.

## Consequences

- Positive: a `serve`/`pipe` client can now self-restrict a session's tool
  surface (withdraw `bash`, mask out one MCP server, etc.) without an
  in-process head — closing ledger row 14 for the deny direction. Zero new
  risk: a deny-only overlay cannot grant anything an enable entry or a
  compromised profile switch couldn't already deny more coarsely via
  `SetAgent`.
- Negative / deferred: **enable** entries (and hence `bash` live-registration
  via an overlay, ADR-0163) are still trusted-only from the wire — riding an
  authenticated head (ADR-0174) remains the identified path once that ships.
  This ADR does not change that; only the deny direction opens up.
- The `wire_allowed()` doc comment's "explicit exhaustive allowlist match"
  claim still holds structurally (every variant appears in exactly one arm,
  a new variant is a compile error to skip) — it now additionally evaluates
  content for the one arm that isn't a flat bool, which the updated doc
  comment calls out explicitly so a future reader doesn't assume every arm
  is variant-only.

## Alternatives considered

- **Leave `SetToolOverlay` fully trusted-only until ADR-0174 ships**: the
  ledger's own status quo. Correct but leaves the safe deny-only case
  blocked on an unrelated, unbuilt feature (an authenticated wire head) for
  no security reason — deny-only needs no authentication to be safe.
- **A separate wire-allowed message for deny-only overlays** (e.g.
  `InMsg::DenyTool { session, patterns }`): keeps `wire_allowed()` a pure
  per-variant match, but duplicates `ToolOverlayEntry`'s pattern semantics
  and `SessionCmd::SetToolOverlay`'s full-replacement/stash-gate machinery
  behind a second frame shape and a second `OutEvent` reply — real
  complexity for a distinction (deny-only vs. mixed) `wire_allowed()` can
  already express with one predicate over the existing shape.
- **Partial-apply a mixed frame's deny entries, refuse only the enable
  ones**: would make a wire head's `SetToolOverlay` sometimes-succeed
  sometimes-partial, breaking the "full replacement, the head computes the
  next list from the last confirmation" invariant ADR-0149 established —
  the head would have to reconcile which of its intended entries actually
  landed. Refusing the whole frame keeps it all-or-nothing, matching every
  other `SetToolOverlay` caller's expectations.
