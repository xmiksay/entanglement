# 0201. Unregistered-but-configured MCP tools resolve truthfully: enable or attributed decline

- Status: Accepted
- Date: 2026-09-14
- Issue: #560 (pre-release audit umbrella)
- Relates to: [ADR-0152](0152-provider-bundled-mcp-servers-three-state-enablement.md)
  (the three-state `enabled`/`allowed`/`disabled` tier this ADR classifies
  against, unchanged — no new state invented), [ADR-0198](0198-out-of-mask-tool-calls-are-approvable.md)
  (the out-of-mask hard-limit check this ADR also makes tier-aware, and whose
  post-approval `dispatch` replay is where a mask-miss `mcp__*` call's real
  connect actually happens), [ADR-0176](0176-structured-tool-result-is-error-and-duration-fields.md)
  (the `is_error` side channel every truthful decline here rides, unchanged).

## Context

`ToolRegistry` is process-lifetime, in-memory, never persisted. A session's
replay (resume after a process restart, or a hibernate/resume cycle) rebuilds
its tool-overlay and permission state from the logged protocol events, but
has no way to re-run whatever live `McpAdd`/`mcp_enable`/`/enable mcp` calls
put a bundled or lazily-connected server's tools into the registry in the
first place — nothing in the log says "and also, re-connect `web_search_prime`
into the registry." So a resumed session's conversation history (and its
restored tool overlay) can legitimately remember a server as enabled while
the fresh process has never registered its tools at all.

Before this change, `tool_runner::dispatch`'s unknown-tool check was a flat
registry membership test: `tools.contains(&tool)` or it's `unknown_tool_message`
— a Levenshtein-hinted "did you mean `bash`?" string, `is_error: true`. That
message is correct for a genuinely hallucinated name. It is **actively wrong**
for `mcp__web_search_prime__web_search_prime` on a resumed session: the
server is real, configured, and (per ADR-0152) `allowed` — the model
retrying the exact same call it made successfully before restart, in good
faith, gets told the tool doesn't exist. There is a second, structurally
identical call site: the out-of-mask hard-limit check ADR-0198 runs *before*
even offering a mask-miss approval prompt refuses the same way on the same
flat membership test.

A first draft of this fix special-cased "lazily re-enable on unknown
`mcp__<server>__*`, else report unknown" — but that conflated two questions
that the three-state model already answers separately: *is this server
tier-eligible for a zero-approval connect* (the `mcp_enable`/`/enable mcp`
question) and *is this name unknown at all*. A `disabled`-tier server run
through that draft's logic still fell through to "unknown tool" — false: the
operator (or the bundled catalog) explicitly named and turned this server
off. The model, and whoever reads the transcript, deserve the true reason,
not a report that a configured thing doesn't exist.

## Decision

### 1. `McpTier`: a pure, connect-free classification (`mcp/available_tier.rs`)

`AvailableMcp::tier_of(server) -> McpTier` reads the live roster with **no
connect attempt** and answers exactly one of:

- **`Eligible`** — `allowed` tier, or already lazily-connected (by this or
  another session). Covers the resume gap directly: an `allowed` server's
  registration went missing on restart, but its tier says a zero-approval
  connect is still the agent's own prerogative, exactly as if it had called
  `mcp_enable` fresh.
- **`Disabled`** — explicitly configured `disabled`. A **known** name;
  consent was withheld by configuration, not merely absent. `partition`
  (`mcp/available.rs`) now retains disabled names in a new `disabled_names`
  set specifically so this is distinguishable at dispatch time — previously
  a `disabled` entry was discarded entirely, indistinguishable from "never
  configured." A keyless bundled `enabled` server is **not** added here: it
  must keep looking silently absent (ADR-0152's "a keyless bundle must look
  absent, not locked"), never "disabled," which would leak that a key is
  required.
- **`Unknown`** — no registered tool, no configured/bundled server under
  this name in any tier. Reserved strictly for this case.

No fourth "ask to enable" tier exists or is introduced. ADR-0152's model is
exactly three states; the "ask" middle the original task framing
hypothesized maps onto the **`allowed` tier's own pre-existing consent
design** — `mcp_enable` and `/enable mcp` already run zero-approval once a
server is `allowed` (the tier itself is the consent boundary, established by
ADR-0152 and reaffirmed by the `explore`/`mcp_enable`-under-`explore` test in
`permission_dispatch.rs`). A dispatch-time lazy re-enable of an `allowed`
server rides that exact same boundary; it does not invent a new approval
gate, and it does not skip the tool's own subsequent `Allow`/`Ask`/`Deny`
grading — see §3.

### 2. Two call sites, one classification, two different self-heal depths

**`tool_runner::dispatch`** (the real, in-mask, or post-mask-approval
execution path) gets the full treatment via
`mcp::available::try_lazy_reenable`:

- `Unknown` → the unchanged `unknown_tool_message` (Levenshtein hint,
  `is_error: true`).
- `Disabled` → `mcp::available::disabled_decline(server)`: `"mcp server
  \`<server>\` is disabled by configuration — its tools cannot be enabled
  this session"`, `is_error: true`. Shared as one function so both call
  sites phrase it identically.
- `Eligible` → attempts the *same* connect `enable_for_session` performs for
  `mcp_enable`/`/enable mcp` (same `CONNECT_TIMEOUT`, same per-server
  `connect_guard` TOCTOU serialization). On success, `dispatch` re-fetches a
  fresh registry snapshot (the enable registered into the *live*
  `SharedRegistry`, invisible to the already-cloned snapshot `dispatch`
  received before spawning) and **continues the exact same function** against
  it — alias rewrite, grading, escape-root, hooks, the approval round-trip
  all still run, unchanged, exactly as if the tool had been registered all
  along. On failure, a distinguishable error: `"mcp server \`<server>\` could
  not be re-enabled: <cause>"`, never confused with "unknown."

**The out-of-mask hard-limit check** (`tool_runner`'s `ToolExec` arm, before
`mask_request::handle` ever parks an approval) needs only the pure
`McpTier` read, no connect: `Eligible` is treated as "exists" and falls
through to the ordinary mask-miss approval flow (the real connect happens
later, inside `dispatch`, once/if the human approves — this check must not
itself dial an external server before permission is even asked); `Disabled`
hard-refuses immediately with the same truthful decline; `Unknown` keeps the
unchanged hard-refuse. `mask_request::handle` forwards the registry/`AvailableMcp`/
`ActiveServers`/`HttpClient` handles through unchanged into its own `dispatch`
call, so an approved out-of-mask `mcp__<server>__*` call self-heals exactly
like the ordinary in-mask route.

### 3. The tier is not a permission grade — grading still runs

A dispatch-time lazy re-enable restores **registration**, nothing else. It
does not widen, bypass, or pre-answer the tool's own `Allow`/`Ask`/`Deny`
grade: `dispatch`'s re-snapshot happens *before* the grading section runs (the
same code, unmoved), so an `Ask`-graded profile still prompts for the
now-registered tool, and a granted session/`Always` scope still upgrades it
exactly as it would for any tool that had been registered from the start.
Verified end-to-end in `tests/it/mcp_lazy_reenable.rs`.

### 4. Stampede guard: per-server failure cooldown, not per-call retries

A broken/unreachable `allowed`-tier server must not have every unknown-tool
dispatch against it (a batch can contain several) each serially eat the full
`CONNECT_TIMEOUT` (60s) before answering. `AvailableMcp` gains
`recent_enable_failures: Mutex<HashMap<String, Instant>>` (keyed by server
name only — a broken server is broken for every session, not per-caller):
`try_lazy_reenable` checks it before attempting a connect and, within a
30-second cooldown, short-circuits to a distinguishable message (`"...
recent connect failure, retry shortly"`) instead of retrying. A success
clears the server's recorded failure so a later transient failure doesn't
inherit a stale cooldown start.

The pre-existing per-server `connect_guard` (`Arc<AsyncMutex<()>>`, #556)
already serializes *concurrent* connect attempts against the same server —
this cooldown is the complementary guard against *sequential* repeat
attempts once one has already failed; the two compose (the guard is checked
first, cheaply, before ever touching the `AsyncMutex`).

### 5. Observability

`enable_for_session`'s own `tracing::info!` (unchanged — the same log line a
manual `mcp_enable` produces) fires on a successful dispatch-time re-enable
too, since it rides the identical function; `try_lazy_reenable` adds one more
`tracing::info!` naming it a "dispatch-time lazy re-enable (resume
self-heal)" so an operator can distinguish an agent-initiated `mcp_enable`
from this self-heal in the logs. The enablement lands in `AvailableMcp`'s
session-scoped `enabled` map exactly as a manual enable would (`mark_enabled`,
called from inside `enable_for_session` either way) — session-scoped, dies
with the session, matching `mcp/enable_tool.rs`'s documented intent
unchanged. No `OutEvent::McpChanged` is emitted (it isn't emitted for a
manual `mcp_enable` tool call either, only for `McpAdd`/`McpRemove` — this
self-heal stays consistent with that, not a new wire event).

### Rejected alternatives

- **Eager re-enable on resume** (re-run every server the session's history
  shows as enabled, as part of the resume/replay path itself) — rejected:
  couples core's resume path to runtime MCP state (core holds no executable
  tools and makes no policy calls, a hard rule this codebase enforces
  elsewhere), and only heals the resume case specifically. The registry can
  desync from a session's expectation for other reasons too (a future
  registry restart mid-process, a manual `/mcp remove` racing a call already
  in flight) — dispatch-time self-heal fixes all of them uniformly, for free.
- **A fourth "ask to enable" tier** — rejected per the analysis in §1: the
  three-state model already has a consent boundary at `allowed`; adding a
  state would duplicate it and fork `mcp_enable`'s own established
  zero-approval behavior from this self-heal path's behavior for the same
  tier.
- **Treating a `disabled` tier the same as unknown** (the original,
  corrected draft) — rejected: actively misleading. A configured-off server
  is known; saying otherwise hides the operator's own decision from the
  model and from anyone reading the transcript.

## Consequences

- A resumed session's next call to a previously-enabled `allowed`-tier
  bundled/user MCP server works transparently, with no visible error and no
  model-side confusion — the bug this ADR fixes.
- Any other registry/`AvailableMcp` desync (not just resume) self-heals the
  same way, for free — the fix is general, not resume-specific.
- A `disabled`-tier MCP tool call now gets a distinct, truthful,
  attributed decline instead of being misreported as unknown — a small
  behavior change to the *wording* of an already-refused call, not a new
  refusal.
- A connect failure during a dispatch-time self-heal is now visibly
  different from "tool doesn't exist," including in a batch of several calls
  against the same broken server (the cooldown guard), at the cost of one
  more `HashMap<String, Instant>` per `AvailableMcp` and two new fields on
  `mcp/available.rs`'s struct — both process-lifetime, unbounded only in the
  sense the existing `enabled`/`connecting` maps already are (per-server, not
  per-call).
