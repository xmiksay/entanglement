# 0186. `exit_code` joins the structured tool-result side channel

- Status: Accepted
- Date: 2026-08-12
- Amends: [ADR-0176](0176-structured-tool-result-is-error-and-duration-fields.md)
  (fires its explicit revisit trigger; closes #681, the narrowed remainder of
  #672, part of #624)

## Context

ADR-0176 gave `InMsg::ToolResult`/`OutEvent::ToolOutput` a structured side
channel (`is_error`, `duration_ms`) but deliberately stopped short of
`exit_code`: `bash`/`call` know the numeric status at the point they format
`[exit N]` (`format_bash_output`/`format_call_output`) and then discard it
into text. The deferral carried a named revisit trigger — *a concrete consumer
that needs the numeric code rather than the `[exit N]` text*.

That consumer now exists: an external wire consumer over `serve`/`pipe` that
wants to branch on a specific exit status without pattern-matching `output`'s
text. The trigger also aged in our favor — ADR-0176 costed the change as "the
`Tool` trait itself must return structured metadata", assuming a richer return
type on every impl; a *defaulted* trait method makes the blast radius two
overrides, not sixteen.

Two facts shape the design:

- The `[exit N]` text is **lossy**: a signal-killed child (`code() == None`)
  prints the `-1` sentinel, and a timeout prints no code at all.
- Background jobs report exit status through a second path entirely — `poll`
  (`j-` handles), which is a runtime-owned orchestration route that never
  passes through `ToolRegistry::execute`.

## Decision

**Wire.** `InMsg::ToolResult` and `OutEvent::ToolOutput` gain
`exit_code: Option<i32>`, additive and serde-skipped when `None` — the exact
ADR-0176 pattern, so pre-existing logs replay unchanged and `serve`/`pipe`
relay the field with zero head changes. It rides
`Holly::submit_tool_result` → `SessionCmd::ToolResult` → `emit_tool_output`
verbatim; like `is_error`, it is display/wire-only and never feeds `Context`.

**Tool half.** The `Tool` trait gains one **defaulted** method,
`run_with_meta(session, request_id, input) -> ToolRun { content, exit_code }`,
which `ToolRegistry::execute` now calls; the default delegates to
`run_for_session` with `exit_code: None`, so every existing impl is untouched.
Only the process runners override it: `bash`/`call`'s `run_impl` return the
code alongside the formatted text (their `run_for_session` overrides delegate
to `run_with_meta` and drop the code, preserving behavior for direct callers).
`ToolExecution` carries the field to `run_and_reply`, which passes it to
`seam::reply_content` (now taking `exit_code` like it takes `duration_ms`).

**Semantics.** `Some(code)` only when the call ran a process to completion
with a real status. A background launch, a timeout kill, a signal-killed
child, an errored dispatch, and every non-process tool are `None` — the
`[exit -1]` text keeps its sentinel; the field does not repeat the lie.
`exit_code` is **orthogonal to `is_error`**: a command exiting nonzero still
executed (`is_error: false` stays the dispatch-level classification ADR-0176
defined). The `[exit N]` text is byte-identical — ADR-0176's no-text-regression
contract holds.

**`poll`.** A job (`j-`) poll that observes `JobStatus::Exited(Some(code))`
carries that code on its own `ToolResult` — the background half of the same
fact. A running/list/script/agent/retained poll, and a killed job, stay
`None` (a background `rhai` script is an in-process computation with no exit
status). `poll`'s `is_error` classification stays deferred exactly as
ADR-0176 left it (#695).

**Hooks.** The `post_tool_use` payload gains `"exit_code"` (JSON `null` when
`None`) — observational only, unchanged contract; a hook can branch on a
specific status without string-matching `[exit N]`.

## Consequences

- The named wire consumer reads the code off `OutEvent::ToolOutput`; every
  in-tree head ignores it (the TUI keeps rendering the `[exit N]` text and the
  `is_error` badge — no rendering change in this pass).
- `ToolExecution` gains a public field and `seam::reply_content`/
  `Holly::submit_tool_result`/`emit_tool_output` each gain a parameter —
  the same mechanical break ADR-0176's `is_error` made, at fewer sites.
- A hook still cannot *act* on the code beyond observation — `post_tool_use`
  returns `()`. A retry-on-exit-status hook contract would be its own ADR;
  nothing here precludes it, and the field it would need now exists.
- `ContentPart::Status` (a metadata part in the model-facing array) stays
  rejected for the reason ADR-0176 gave: it would leak to the model or force
  every `content` consumer to filter it.
