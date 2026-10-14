# 0230. `ToolRequest` carries a structured card `reason` and the `scopes` an approval honors

- Status: Accepted
- Date: 2026-10-14
- Issue: #836, #846 (follow-ups to #820, epic #822)
- Amends: [ADR-0228](0228-rhai-path-and-args-skill-tools-frontmatter-removed.md)
  (`skutter run` tells an out-of-root launch card by the card's `reason`, not
  by `rhai_trust::LAUNCH_ESCAPE_NOTE` in its text — the constant leaves the
  public API), [ADR-0217](0217-list-pending-query-for-parked-approvals.md)
  (`PendingApproval.scopes` narrows for floored writes and inline launches;
  `PendingApproval` gains `card_reason`), and
  [ADR-0002](0002-session-multiplexed-protocol.md)'s `OutEvent::ToolRequest`
  (two additive, serde-defaulted fields)

## Context

`OutEvent::ToolRequest` was `{session, seq, request_id, tool, input}`. Two
things a head needs were missing from it.

**Why the card parked.** `skutter run` has nobody to ask. It starts a `rhai`
launch card Ask per command, but it must refuse one whose script lies outside
the project root. It told the two apart by searching the card text for
`rhai_trust::LAUNCH_ESCAPE_NOTE`. That failed safe, because script text can
only add the note and never remove it. Still, it was a contract written in
prose: any rewording of the card would silently change what the head does.

**What an approval honors.** `PendingApproval.scopes` (ADR-0217) promised
"the scopes an `Approve` actually honors". `scopes_for` still returned
Once/Session/Always for every tool or rhai card. Two cases never honor a
wider scope:

- a **floored write** (a trusted `*.rhai` or a skill `SKILL.md`/`lib.rhai`,
  ADR-0228) records no grant, because the floor always asks again;
- an **inline** `rhai` launch has no path to key a grant on.

The TUI footer also hard-coded `[s]`/`[a]`. A user who pressed `a` on a
floored write saw success, and the next identical write asked again.
`serve`/`pipe` clients that built their options from `scopes` offered grants
that did nothing. `ToolRequest` did not carry `scopes` at all. A pure
`OutEvent` consumer, which is what the TUI is, could only get them through a
`ListPending` round trip.

## Decision

1. **`CardReason`** (`entanglement-core`, `protocol/approvals.rs`, snake_case
   on the wire) names the cases the runtime really produces:

   | variant | produced by | an approval honors |
   | --- | --- | --- |
   | `root_escape` | `tool_runner::dispatch::decision` — `Prompt::Escape`, a host/MCP call reaching outside the root (ADR-0109) | once/session/always — no `session_dir`, even for the read triad: the escape grant is keyed on the exact path |
   | `floored_write` | the same park, whenever the write floor carded the call (either prompt) | `once` only |
   | `script_launch` | `script::gate` — a `path` launch inside the root | every scope (the grant is the exact path); `once` only when the script could not be read |
   | `inline_launch` | `script::gate` — an inline launch | `once` only |
   | `launch_escape` | `script::gate` — a `path` launch outside the root | every scope; a head with nobody to ask refuses it |

   An ordinary card carries no reason: a mode's `Ask` grade, a credentialed
   `http` host approval, `propose_plan`, `request_mode`. Each card has one
   value. Where two facts meet, the one that limits what an approval honors
   wins: a floored write outside the root is `floored_write`, and its text
   still carries the escape warning.

2. **`OutEvent::ToolRequest` gains `reason: Option<CardReason>` and
   `scopes: Vec<ApprovalScope>`.** Both use `#[serde(default,
   skip_serializing_if)]`, so a pre-ADR-0230 frame or log record still
   parses and re-serializes byte-identically; a current runtime always sends
   `scopes`. `PendingApproval` gains
   `card_reason` under the same rule. Both events are still built from one
   `PendingApproval` value (`pending::park_approval`), so the listing and
   the prompt cannot drift. The facts are passed into `pending::approval(…,
   card)`. `scopes_for(kind, tool, card)` narrows `floored_write` and
   `inline_launch` to `[once]`.

3. **Heads decide on data.** `pending::headless_starts(reason)` returns true
   only for `script_launch`/`inline_launch`. `skutter run` approves `Once`
   (Ask per command) on that and rejects everything else. The decision is a
   positive signal: a card with no launch reason is refused, so the fail-safe
   direction holds. The TUI footer and its keys offer exactly the card's
   `scopes`. A letter for a scope the card does not carry is swallowed. An
   empty `scopes` (an older producer) offers `Once` alone.

4. **The runtime enforces `scopes`.** `PendingDecisions::resolve` clamps an
   `Approve` whose scope the parked card did not offer to `Once`, with a
   `tracing::warn!`. A head is shown `scopes`, but a wire client can send
   any scope. Unclamped, an `Always` on a floored write outside the root
   would persist a durable extra-roots grant (the escape store records the
   scope as sent), and a `SessionDir` on a launch would degrade to a
   `Session` grant the card never offered.

5. `rhai_trust::LAUNCH_ESCAPE_NOTE` becomes private to `script::gate`. It
   remains part of the card text for the human only. It was added in the
   unreleased 0.7.0 cycle, so no released API breaks.

## Consequences

- A `serve`/`pipe` client can build its approval buttons straight from
  `ToolRequest.scopes`, and can branch on `reason` (for example, warn on
  `root_escape`/`launch_escape`) without parsing text.
- Logs stay compatible both ways. A recorded `ToolRequest` from before this
  change parses, with no reason and empty scopes, and re-serializes
  byte-identically. Replay byte-equality is unaffected, since neither field
  feeds the model's history.
- Adding a variant later is a wire addition, and an older head would fail to
  parse it. A head should keep tolerating an unknown `reason`. In-tree heads
  are built with the runtime, so they never see one.

## Alternatives considered

- **`escape: bool` on `ToolRequest`** (#836's first sketch). This answers
  only the headless question. It does not explain why `scopes` narrowed, and
  it cannot tell an inline launch from a path launch, which the TUI used to
  infer from the `// path:` first line.
- **The TUI derives scopes by calling `scopes_for` itself.** This works only
  in-process: it needs the approval kind, which `ToolRequest` lacks, and it
  leaves every wire client without the information.
- **Only `PendingApproval` carries the reason; heads call `ListPending`.**
  This adds a round trip per card, and a `ToolRequest`-driven head would
  still need the data on the event it reacts to.
- **A reason list instead of one value.** This is more faithful for a
  floored write outside the root. But no head decides differently on the
  second fact, and a single value keeps every match exhaustive.
