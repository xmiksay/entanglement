# 0176. Structured tool-result channel: `is_error`/`duration_ms` as fields

- Status: Accepted
- Date: 2026-08-06
- Amends: [ADR-0006](0006-core-dependency-hygiene-gate.md)/[ADR-0010](0010-single-head-crate-and-bash-opt-in.md) (cited elsewhere in this codebase, e.g. `tool_runner.rs`, for "core holds no executable tools, the runtime owns the round-trip" — the invariant this ADR's fields ride on), extends the "it's all just text" observation [ADR-0161](0161-unified-async-work-background-flag-and-one-poll.md) named but explicitly left unscoped (deferred-work ledger row 16, #636, part of #624)

## Context

Every tool result is `anyhow::Result<String>` → `text_parts` → `Vec<ContentPart>`
(`entanglement-runtime/src/tools.rs`). Exit codes, truncation notices, denials
and failures are all **textual markers inside the string**:

- `[exit N]` (`host/bash.rs`, `host/call/format.rs`)
- `... [truncated: N bytes total]` (`host/mod.rs`)
- `` tool `X` failed: … `` (`ToolRegistry::execute`'s `Err` fold)
- `` tool `X` denied by permission profile `` (three independent `format!`
  sites: `tool_runner.rs`'s generic dispatch, `script.rs`'s `rhai` self-gate,
  `script.rs`'s binding gate)
- `` tool `X` is not available to this agent … `` / `… while skill … is active
  …`` (the #116/#400 mask refusals)
- `rhai error: …` (a thrown/uncaught script exception or a panicked
  `spawn_blocking` join)

So a failing tool call is **indistinguishable from a succeeding one at the
type level**. `ToolRegistry::execute` returns a bare `Vec<ContentPart>` either
way; `OutEvent::ToolOutput` carries only `output: String`; a head, a hook, or
an external wire consumer that wants to know "did this actually run" has no
choice but to pattern-match a specific English sentence, which is fragile
(wording drift breaks every consumer silently) and already inconsistent (four
independent call sites hand-format the same "denied" sentence).

The sharpest instance is `run_and_reply` (`tool_runner.rs`): it derives
`output_text` from the result purely to hand it to the `post_tool_use` hook —
and the hook is **explicitly documented as unable to rewrite `content`**
(`docs/architecture/gates-and-host-tools.md`'s hook table: "observational —
exit code logged, never fed to the model"). A hook that wants to alert on
failure (post a Slack message, touch a marker file, whatever) has always had
to grep `output` for `denied`/`failed`/`error`, one string match per hook
author, with no shared vocabulary.

ADR-0161 named the same underlying fact — "all four launchers return the same
thing: `anyhow::Result<String>` → `text_parts` → `Vec<ContentPart>`… what
actually differs between them is *when* the text is ready, not its shape" —
and spent its whole decision on *timing* (block vs. background) and
*retention* (truncation/paging via `poll`). It never asked whether *success
vs. failure* should be a field rather than a substring; the deferred-work
ledger (row 16, #636) named that gap explicitly and asked for its own ADR plus
its own audit of what each head would do with the fields, rather than folding
it into ADR-0161's already-large scope.

**Scope of the audit.** Adding `is_error`/exit code/duration as fields
potentially touches the `Tool` trait (every host tool's return shape),
`ToolRegistry::execute`, `InMsg::ToolResult`, `OutEvent::ToolOutput`, and every
head's rendering — the audit below classifies each one instead of assuming
the whole surface changes uniformly.

## Decision

### `is_error: bool` and `duration_ms: Option<u64>` as wire fields

`InMsg::ToolResult` and `OutEvent::ToolOutput` each gain `is_error: bool`
(`#[serde(default, skip_serializing_if = "std::ops::Not::not")]`) and
`duration_ms: Option<u64>` (`#[serde(default, skip_serializing_if =
"Option::is_none")]`). Both default off, so a pre-#636 persisted log or a
hand-built `InMsg::tool_result(...)` still deserializes/round-trips unchanged
— purely additive, matching every prior protocol field addition in this
codebase (ADR-0141's `Throttle`, ADR-0151's `SetSessionMeta`).

**`content`/`output`'s text is unchanged.** The structured fields are a
**side channel**, not a replacement — the markers listed in Context still
exist verbatim, because the model still needs the text (it has no separate
"error" input) and a human reading a transcript still needs it. `is_error` is
for anything that wants to branch on outcome *without* parsing that text:
hooks, a head's rendering, an external wire consumer.

### Where `is_error` originates: the `Tool` trait is untouched

The `Tool` trait (`run`/`run_content`/`run_for_session`, all returning
`anyhow::Result<Vec<ContentPart>>` or `anyhow::Result<String>`) is **not
changed**. `ToolRegistry::execute` already sees the one bit that matters at
its own boundary — `Ok` vs. `Err`, found vs. unknown — so `is_error` is
classified *there*, in the one place, not threaded through every host tool's
implementation:

```rust
pub struct ToolExecution {
    pub content: Vec<ContentPart>,
    pub is_error: bool,
}
```

`execute()` now returns `ToolExecution` instead of a bare `Vec<ContentPart>`:
`Ok(content)` → `is_error: false`; a tool's own `Err(e)` or an unregistered
name → `is_error: true`. This is the only signature change inside
`entanglement-runtime::tools` — every existing `Tool` impl (`read`/`edit`/
`write`/`bash`/`call`/MCP tools/…) is untouched.

This deliberately **does not** attempt to classify a *within-band* failure —
`bash`/`call` returning a nonzero `[exit N]` inside an otherwise-successful
`Ok(...)` result. The tool ran; the *command* it ran happened to fail. That is
a different, tool-specific signal (see Alternatives) and is left as `[exit N]`
text, exactly as today.

### Where `is_error` originates: every other reply site, audited by hand

Not every `ToolResult` reply goes through `ToolRegistry::execute` —
`ask_user`, `propose_plan`, `agent`/`agent_send`, `rhai`'s own gate, and the
generic dispatch's mask/spawn/permission refusals all fold their own text
back via `crate::seam::reply`/`reply_content`, independent of the registry.
`reply`/`reply_content` gained a mandatory `is_error: bool` parameter (and
`reply_content` a `duration_ms: Option<u64>`), forcing every one of the ~25
call sites across `tool_runner.rs`, `script.rs`, `ask_user.rs`,
`propose_plan.rs`, `agent_send.rs`, `subagent.rs`, and `poll/mod.rs` to state
its classification explicitly rather than defaulting silently. The audit:

| site | classification | why |
| --- | --- | --- |
| `tool_runner::dispatch` — mask/skill-mask refusal, spawn refusal, sponsor-spawn refusal, `Permission::Deny`, `Decision::Reject`, unknown tool, `pre_tool_use` block | `true` | the call structurally never ran |
| `tool_runner::run_and_reply` — `update_tasks` ack | `false` | not a host-tool call; always succeeds |
| `tool_runner::run_and_reply` — the generic host-tool result | dynamic, from `ToolExecution.is_error` | the one path `ToolRegistry::execute` classifies |
| `script.rs` — invalid input, `rhai`'s own `Deny`/`Reject` | `true` | the script never ran |
| `script.rs` — `execute_script`'s final result | dynamic, from whether the Rhai `eval` itself errored (`format_output` now returns `(String, bool)`) | distinct from an individual **binding**'s denial, which the script sees as a catchable exception it may recover from — only an uncaught script-level error is structural |
| `ask_user` — folded answer, withdrawal note | `false` | both are valid, deliberate terminal outcomes of a call that ran to completion |
| `propose_plan` — parse/resolve error, rejection, spawn-inbox-closed | `true` | structural failure |
| `propose_plan` — the sponsored build's folded answer | `false` | the call collected an answer; a *bad* build outcome is content, not a call failure (mirrors `agent`/`agent_send` below) |
| `agent_send`/`subagent` — parse error, registry refusal, spawn-inbox-closed | `true` | structural failure |
| `agent_send`/`subagent` — background/detached launch ack, the collected answer | `false` | the call ran to completion; an errored **child turn** is still a delivered answer, not a launcher failure |
| `poll` | `false`, uniformly | `poll`'s result folds several outcomes (running/complete/list/unknown handle) into one string; splitting those is left to a follow-up (see Consequences) — `poll` is a runtime-owned orchestration route this pass's audit didn't extend to |

### `duration_ms`: measured once, generically

Rather than threading a timer through every `Tool` impl, `duration_ms` is
measured in exactly one place — around the whole `tools.execute(...)` call in
`run_and_reply` (`std::time::Instant`) — and is `None` everywhere else (every
non-execute `reply`/`reply_content` site above). This covers the case that
actually matters (how long did the tool take) without asking every host tool
to report its own timing, and without pretending a denial/refusal/ack has a
meaningful "duration" at all.

### `post_tool_use` observes `is_error` too

`Hooks::run_post_tool_use` gained an `is_error: bool` parameter, folded into
the hook's JSON stdin payload (`{event, session, tool, input, output,
is_error}`). This does not change the hook contract stated in
`docs/architecture/gates-and-host-tools.md` — it is still purely observational
and still cannot rewrite the result — it only gives the hook a field to branch
on instead of grepping `output`.

### Heads, audited

- **`serve`/`pipe`** (WS/NDJSON): relay the raw `OutEvent` with no per-variant
  match — the new fields are transparent, exactly as ADR-0141's `Throttle`
  was. Nothing to change.
- **stdio `run.rs`**: the `OutEvent::ToolOutput` arm now picks a sigil from
  `is_error` (`✗` vs. `=`) instead of always `=` — the one-line, low-risk
  change this pass makes to a head's rendering.
- **TUI**: **not changed in this pass.** `TranscriptEntry::ToolCall`/
  `ToolOutput` and the `Block`/render-cache pipeline built on top of them
  (`segment.rs`, `block.rs`, `render_run.rs`, `export.rs`, `event_loop.rs`,
  plus their tests) have ~15-20 construction/destructuring sites, several of
  them exhaustive-field `matches!` patterns rather than `..`-tolerant ones.
  Fully threading `is_error` through to `render_run.rs`'s existing (and
  currently indiscriminate — it shows a green `✓` for *any* completed call,
  failure or not) status-suffix badge is real, valuable, and independently
  scoped work; bundling it here would let the highest-risk piece gate the
  rest of this change the way ADR-0161 warned against for `rhai`
  backgrounding. Flagged below as a concrete follow-up.

## Consequences

### Positive

- **A denied/masked/refused/unknown-tool/errored call is now a real `bool`**,
  not a string a consumer has to recognize by wording. `content`/`output`'s
  text is unchanged, so nothing that already worked by reading text
  regresses.
- **`post_tool_use` hooks can finally branch on outcome** without grepping
  `output` — the exact gap the issue's `run_and_reply` comment ("it cannot
  rewrite content") pointed at.
- **`duration_ms` is free telemetry** for any hook, log processor, or future
  head that wants it — measured once, not per-tool.
- **Every wire consumer (`serve`/`pipe`) gets both fields for free** — no head
  code changed for them, only the protocol grew two optional fields.
- **The `Tool` trait, `ToolRegistry` public API shape (`execute`'s
  *signature*, not return type), and every existing `Tool` implementation are
  untouched** — the blast radius stayed at the boundary that actually needed
  to classify success/failure, not the ~30 host tools behind it.

### Negative / neutral

- **`exit_code` is *not* added as a field in this pass.** `bash`/`call` already
  know the numeric exit code at the point they format `[exit N]`
  (`format_bash_output`/`format_call_output`), but surfacing it as a real
  field requires the `Tool` trait itself to return structured metadata (or an
  equivalent side channel per tool), which is the one piece of the original
  audit that genuinely touches every host tool. Deferred with an explicit
  revisit trigger: a concrete consumer that needs the numeric code rather than
  the `[exit N]` text (e.g. a hook wanting to retry only on a specific exit
  status).
- **`poll`'s own result classification stays uniformly `false`.** It folds
  running/complete/list/unknown-handle outcomes into one string and is a
  runtime-owned orchestration route (like `agent`/`ask_user`), not the generic
  host-tool dispatch this audit covers end-to-end. A future pass can split its
  branches the same way this one split `tool_runner`'s.
- **The TUI's own rendering gains nothing yet** — a failed tool call still
  shows the same "▸ tool ✓" header a successful one does
  (`render_run.rs`'s `status_suffix`), because `output.is_some()` is the only
  signal it currently reads. This is a known, load-bearing gap this ADR
  accepts rather than papers over with a partial TUI change; a follow-up
  threads `is_error` through `TranscriptEntry`/`Block` and swaps the badge to
  `✗`/red via the theme's existing `error_colors()`.
- **`reply`/`reply_content` gaining a mandatory `is_error` parameter touched
  ~25 call sites across seven files.** Mechanical, but real — a reviewer
  should treat the classification table above as the actual content of this
  change, not the plumbing.
- **`ToolRegistry::execute`'s return type changed** from `Vec<ContentPart>` to
  `ToolExecution` — the one public-API break, contained to
  `entanglement-runtime::tools` and its single production caller
  (`run_and_reply`).

## Alternatives considered

- **Add `exit_code`/`duration`/`is_error` all as fields in one pass,
  threading structured metadata through the `Tool` trait.** This is the
  audit's original framing and the ledger's literal wording. Rejected for
  this pass: it requires every host tool (`bash`, `call`, `read`, `edit`,
  `write`, MCP tools, …) to return a richer type, which is exactly the
  "touches the `Tool` trait … and every head's rendering" scope the ledger
  flagged as wanting its own audit — this ADR *is* that audit, and it found
  that `is_error`/`duration_ms` deliver the load-bearing value (a hook or head
  can finally tell success from failure) at a fraction of the blast radius,
  while `exit_code` genuinely needs the larger change and has no concrete
  consumer yet.
- **Encode outcome as a new `ContentPart` variant** (e.g. `ContentPart::Status
  { is_error, exit_code, duration_ms }`) instead of sibling fields on
  `ToolResult`/`ToolOutput`. Rejected: `ContentPart` is the model-facing
  multimodal content array (`content_text` folds only `Text` parts today), and
  a status variant would either need every consumer of `content` to filter it
  out or would leak into what the model sees. Sibling fields keep the
  model-facing shape (`content`) and the side channel (`is_error`/
  `duration_ms`) cleanly separated.
- **Classify `is_error` from the existing text markers** (regex/prefix match
  on `output` inside `emit_tool_output`) instead of threading a real bool from
  each call site. Rejected: this is the exact fragility the issue names —
  four independent hand-formatted "denied" sentences already exist, and a
  central classifier would have to enumerate every current and future marker
  string, silently breaking whenever wording drifts. Classifying at the
  source, where the Rust code already knows the outcome as a `Result`/`match`
  arm, is strictly more robust and was already available for free at every
  site.
- **Thread the whole TUI rendering change through in this pass.** Rejected
  per the Context/Consequences above — real, valuable, and independently
  scoped; bundling it would risk the mechanical, low-risk protocol change on
  the highest-blast-radius piece.

## References

- [ADR-0006](0006-core-dependency-hygiene-gate.md)/[ADR-0010](0010-single-head-crate-and-bash-opt-in.md):
  cited in this codebase for "core holds no executable tools, the runtime owns
  the round-trip" — the invariant this ADR's fields ride on
- [ADR-0161](0161-unified-async-work-background-flag-and-one-poll.md): named
  "all just text" for the four launchers; this ADR is the deferred structured
  side channel it explicitly left out of scope
- [ADR-0141](0141-wire-visible-throttle-transitions.md): the template for a
  purely-additive `OutEvent` field addition that every head picks up
  automatically except the one that needs an explicit render change
- `docs/deferred-work-ledger.md` row 16 (#636, part of #624): the deferral
  this ADR narrows — `is_error`/`duration_ms` ship here, `exit_code` and TUI
  rendering stay open with the revisit triggers stated above
