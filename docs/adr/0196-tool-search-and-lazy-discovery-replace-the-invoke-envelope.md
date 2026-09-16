# 0196. Tool search and lazy discovery replace the invoke envelope

- Status: Accepted
- Date: 2026-09-14
- Supersedes: [ADR-0193](0193-two-tool-call-modes-and-lazy-tool-discovery.md)
  (**partially** — see "Disposition of ADR-0193" below). The universal
  `invoke(name, args)` envelope and the `tools`/`skills`/`describe` discovery
  trio are rejected outright and replaced by this ADR's design. What survives,
  re-affirmed unchanged: per-session mode resolution (fixed at session start,
  kept across `SetModel`, resolved fresh by a subagent at spawn), the static
  `mcp_enable` schema (`server: string`) in every mode, pull-only discovery
  (no announcements), and MCP management staying maskable + permission-graded.
- Relates to: [ADR-0192](0192-universal-advertisement-enforcement-at-dispatch.md)
  — dispatch-time enforcement is unchanged by this ADR; it already decoupled
  advertisement from enforcement, which is *why* the invoke envelope's central
  justification (permission laundering needs an unwrap step) never applied in
  the first place (see Decision §1). [ADR-0194](0194-skills-are-additive-only.md)
  — skills fold into `explore`'s index (no third discovery tool); its own text's
  references to "invoke mode" and ADR-0193's Phase-8 tool surface now read
  against this ADR. [ADR-0195](0195-bash-is-the-default-exec-and-curated-read-only-rules.md)
  — its "lean kernel" reference (`call` sitting outside invoke mode's advertised
  set) now reads against this ADR's lean kernel (§2). [ADR-0190](0190-poll-is-always-on-non-maskable-internal-tool.md)
  — the always-on, non-maskable, always-`Allow` internal-tool pattern `explore`/
  `describe` extend from one tool (`poll`) to three. [ADR-0188](0188-session-keyed-per-user-mcp-scopes.md)
  — the session-scoped registry view `describe` resolves MCP schemas through.
  [ADR-0176](0176-structured-tool-result-is-error-and-duration-fields.md) — the
  `is_error` channel the error taxonomy (§4) rides.

## Context

ADR-0193 recorded a design for the same underlying problem this ADR solves:
the advertised `tools` array is a prompt prefix, and a rich tool surface (every
built-in, every MCP tool, endpoints, skill tools) mutating that array
mid-session busts the provider's prompt cache. Its answer was a universal
`invoke(name, args)` envelope — an immutable lean kernel plus one router tool
that dispatches everything else by string name, unwrapped to the inner call
at the top of the permission ladder so masks/grades/hooks still see the real
tool.

That design was accepted and partially plumbed (`entanglement-provider/src/catalog/tool_call.rs`,
`entanglement-runtime/src/tool_call_mode.rs`) before any of its dispatch-facing
code shipped. Before the remaining phases landed, a design review
(`tool_search.md`, cross-checked against the actual provider wire contracts in
`scratch/tool-search-wire-reference.md`) found the envelope itself wrong, for
reasons independent of the caching problem it was solving correctly:

| problem | detail |
| --- | --- |
| out of distribution | Models are SFT-trained on `<tool_call>` blocks naming a real tool with a visible schema. A meta-tool forces recall of "what does `invoke` take" instead of pattern-matching against a schema already in view — a tax a small/local model pays far more than a frontier one. |
| constrained decoding loses specificity | Per-tool schemas let a grammar enforce exactly that tool's parameters. A universal envelope's grammar cannot know what is valid until `name` has already been emitted, which needs mid-generation grammar switching — a real constraint for any harness doing constrained decoding, even though this repo does not do so directly. |
| discovery problem is unsolved | The model still needs to learn what exists to route to. Putting a catalog in the prompt to solve that reintroduces exactly the cache invalidation the envelope existed to avoid. |
| JSON-in-JSON nesting risk | A universal envelope's `args` is typically itself a JSON blob nested inside the outer tool-call JSON — string-encoded JSON inside JSON is a live failure mode (escaping, embedded newlines, backslashes) the design invites without needing to. |

A fifth point undercuts the envelope's own justification rather than
critiquing its mechanism: **the envelope was never needed for dispatch.**
ADR-0192 already decoupled advertisement from enforcement — the runtime
executes any registered tool regardless of whether it appeared in the
advertised array, and masks/grades/hooks enforce at the dispatch gate by
real tool name. ADR-0193's unwrap-before-the-gates design was solving a
problem ADR-0192 had already solved a different way. What actually needed
solving was never "how does a masked call get graded through an envelope" —
it was "how does the model learn a tool exists, and call it, without that
knowledge costing a cache-busting roster."

Separately, the major hosted APIs converged on a standardized answer to
exactly that problem in the interval between ADR-0193's acceptance and this
review: server- or client-executed **tool search** with **deferred-loading**
tool definitions (Anthropic Messages API, OpenAI Responses API, and
OpenRouter's cross-provider passthrough of both — verbatim wire facts in
`scratch/tool-search-wire-reference.md`). Building a bespoke envelope when a
standardized, wire-native mechanism already exists for the wires that support
it is the wrong layer to invent at.

## Decision

### 1. Two advertising modes, not two call-encoding modes

```rust
enum ToolAdvertising { Full, ToolSearch }
```

Replaces ADR-0193's `ToolCallMode { Native, Invoke }` — same catalog/config/env
shape, renamed to name what actually varies (the *advertised set*, not how a
call is *encoded*; every mode calls tools by their real name, never through an
envelope):

- `Full`: today's behavior — every registered spec advertised, full-fidelity
  schemas, unchanged from pre-ADR-0193 native mode.
- `ToolSearch`: a lean kernel (§2) plus the `explore`/`describe` discovery pair
  (§3) is advertised; everything else stays registered and reachable through
  discovery. **This is the new default** — the token/cache motivation that
  drove ADR-0193 applies to most sessions, not an opt-in minority.

Precedence: `ENTANGLEMENT_TOOL_ADVERTISING` env > `config.yml`
`tool_advertising` > catalog `ModelEntry.tool_advertising` > default
`ToolSearch` — the same `Option<Enum>` catalog pattern `thinking_style` uses,
renamed from ADR-0193's `tool_call`/`ENTANGLEMENT_TOOL_CALL_MODE`/
`tool_call_mode`.

**Per-session, pinned at session start**, kept across a live `SetModel`
(switching mid-session would bust exactly the cache stability the mode
protects), a subagent resolving its own mode fresh at spawn — unchanged from
ADR-0193 §5, carried forward verbatim. The runtime-side session→mode map and
the resolver/executor plumbing ADR-0193 built for this (`tool_call_mode.rs`)
is renamed and repurposed rather than rebuilt from scratch.

### 2. Lean kernel (the `ToolSearch`-mode advertised set)

`read`, `edit`, `write`, `apply_patch`, `bash`, `poll`, `ask_user`,
`update_tasks`, `load_skill`, `explore`, `describe`, plus the
profile-defining specs (`propose_plan`, the `agent`/`agent_send` spawn enum —
the ADR-0192 carve-out, since those vary legitimately across profiles, never
within a session). Everything else — `call`, `glob`, `grep`, `rhai`, MCP
management (`mcp_enable`/`mcp_add`/`mcp_remove`), all `mcp__*`, endpoints,
skill tools — stays **registered but unadvertised**: reachable only after
`describe`, discoverable via `explore`.

### 3. Per-wire encoding of `ToolSearch` mode

Discovery and direct calls are identical from the model's point of view
across every wire (call tools by their real name, no envelope). How a
discovered tool's schema actually *reaches* the model, and how much of the
advertised array a request re-sends, is a capability derived from the wire —
not a user-visible knob:

- **`client_side`** (OpenAI-compat Chat Completions — including **z.ai, the
  priority target** — Ollama, Gemini): `describe()` appends the described
  tool's full `ToolSpec` into the session's advertised array, **append-only,
  never removed**. This costs one cache invalidation per discovery, not a
  continuous one. Rides the existing `tool_spec_resolver` seam (consulted
  fresh every round) plus a session-keyed discovered-set threaded through it,
  reusing the shape ADR-0193's WIP already built for this purpose.
- **`anthropic_native`**: non-kernel tools are declared with
  `defer_loading: true` and **stay deferred for the whole session**, discovered
  or not — the full tool definition is still sent in `tools` on every request
  (Anthropic's API needs it server-side to run search and expand references),
  but it is stripped from the rendered prompt and the cache-key computation,
  so it never enters the cached prefix. Delivery is the transcript's
  `tool_reference` block: `describe()`'s result is a standard `tool_result`
  carrying one reference per loaded tool plus a one-line list of the names
  loaded (no schema text), and the API keeps each reference expanded into the
  full definition for as long as it stays in history. Un-deferring a
  discovered tool would rewrite the cached `tools` prefix once per discovery
  (amended by [ADR-0202](0202-prompt-cache-discipline-anchors-deferral-replay-compaction-date.md) §1). This is *client-executed* custom search (§1.8 of the wire
  reference), not Anthropic's server-side `tool_search_tool_regex`/`_bm25`
  tools, because the catalog `describe` searches is session/project-state
  dependent in ways a server-side regex/BM25 tool over a static list is not.
  At least one tool must stay non-deferred, a constraint the lean kernel
  satisfies trivially.
- **`responses_native`**: a new OpenAI Responses API client (`{"type":
  "tool_search", "execution": "client"}` plus `defer_loading` on function
  tools). The model emits a `tool_search_call`; the runtime performs the
  lookup and answers with a `tool_search_output` mapped onto the same
  `explore`/`describe` semantics. As on `anthropic_native`, a discovered tool
  stays `defer_loading` for the session: the `tool_search_output` item in
  history is the delivery, and un-deferring would rewrite the cached tools
  prefix once per discovery (ADR-0202 §1).

### 4. Discovery pair — `explore` / `describe`

Two tools, not three — ADR-0193's `tools`/`skills`/`describe` trio collapses
to two, skills folded into `explore`'s index rather than kept as a separate
tool:

- **`explore(filter?)`**: a terse index — name, one-line description, source
  — over built-ins (including the unadvertised `call`, `glob`, `grep`,
  `rhai`, MCP management), MCP servers (three-state; an `allowed`-tier server
  shows the `mcp_enable` hint, never auto-connecting), endpoints, and skills.
- **`describe(names: [string])`**: full JSON schema per name, **byte-identical
  in shape to the native tools-block entry** — same fields, not a friendlier
  or reformatted rendering, so the schema is exactly as in-distribution as a
  schema the model would have seen in `<tools>`. Under `client_side` encoding
  it additionally registers the tool into the session's advertised array
  (§3). MCP schemas resolve through the session-scoped registry view
  (`overlay_registry_for_call`, ADR-0188), so per-user MCP scopes resolve
  correctly for the asking session.

Both are always-on, non-maskable (no profile mask, overlay, or deny entry can
withdraw them), always-`Allow` (read-only introspection), and inert — the
`poll` pattern ADR-0190 established, extended from one tool to two. Present
in **both** advertising modes (redundant but harmless under `Full`, where the
whole surface is already advertised).

### 5. System prompt

`ToolSearch` mode drops the tool/skill rosters for one line naming the
searchable categories ("You can search for tools to interact with X, Y, Z" —
the mitigation `tool_search.md` §3 calls load-bearing: without it, the model
has no signal that anything is searchable at all). `Full` mode keeps today's
sections unchanged.

### 6. Error taxonomy

Three distinct cases, three distinct responses — subsumes ADR-0193's §6
"wrong-args declines," reworked per `tool_search.md` §7:

| case | model should | reply |
| --- | --- | --- |
| schema violation (malformed call, missing/unexpected params) | fix the call shape | schema + exactly what was wrong (`missing required: path`, `unexpected: pathh`) |
| parameter error (valid shape, bad value — file not found, out of range) | change a value, not the structure | why it failed + context (a directory listing, a valid range) |
| command failure (e.g. `make test` exits 1) | read the output | the normal result, verbatim — **not** flagged `is_error`; a non-zero exit is a legitimate result the model must read, not a call to fix (the existing ADR-0176/0186 `is_error`/`exit_code` split already carries this distinction — this ADR reaffirms it, doesn't change it) |

Unknown tool name is a fourth case handled separately: fuzzy catalog matches
(the existing Levenshtein-distance hint), not a schema — the intended tool is
unknown, so there is no schema to show. Mechanism: pre-dispatch validation of
the input against the advertised `ToolSpec` schema, subsuming the MCP
required-param pre-check as a special case rather than a parallel path. User
denial replies with the denial reason only, no schema attached. Two guards
ride alongside: per-session delivered-schema tracking (never re-send a schema
already in context — reply with the diff instead), and a two-identical-
failures loop breaker (a repeated identical failure means the schema was
never the problem; stop retrying the same shape).

### 7. Parallel tool calls are unchanged

`tool_search.md` §8 proposed starting with sequential-only tool calls to
reduce complexity for a small local model. **Not adopted here**: this
repo's batch `ToolExec` parking (ADR-0061) already resolves a batch of
parallel calls in any order and already guarantees every call in a batch gets
a paired result (the "unpaired tool_use_id" trap §8 warns about was already
closed before this design existed). No regression, no change needed.

## Disposition of ADR-0193's shipped artifacts

`entanglement-provider/src/catalog/tool_call.rs` and
`entanglement-runtime/src/tool_call_mode.rs` (the only ADR-0193 code that
landed before this review — catalog field, config/env precedence, and the
per-session pin/resolve/forget lifecycle) are **renamed and repurposed**
rather than deleted and rebuilt: `ToolCallMode` becomes `ToolAdvertising`,
`tool_call.rs` becomes `catalog/tool_advertising.rs`, `tool_call_mode.rs`
becomes `tool_advertising.rs`, the config key becomes `tool_advertising`, the
env var becomes `ENTANGLEMENT_TOOL_ADVERTISING`, and the default flips from
`native`/`Full`-equivalent to `ToolSearch`. The pin-at-session-start,
kept-across-`SetModel`, resolved-fresh-at-subagent-spawn lifecycle this
plumbing already implements is exactly what §1 needs, unchanged in substance.

[ADR-0194](0194-skills-are-additive-only.md) and
[ADR-0195](0195-bash-is-the-default-exec-and-curated-read-only-rules.md)
stand on their own merits — neither depended on the invoke envelope for its
own reasoning, and neither is superseded by this ADR. Their textual
references to "invoke mode" and the "lean kernel" ADR-0193 defined (0194's
Phase-8 additive-surface framing; 0195's "`call` sits outside invoke mode's
lean kernel") now read against **this** ADR's `ToolSearch` mode and lean
kernel (§1–§2) instead — the underlying facts they describe (skills only
ever add capability; `call` is registered but unadvertised outside the full
surface) are unchanged, only the name and shape of the mode itself moved.
The ADR files themselves are not edited (ADRs are immutable); the
architecture docs carry the corrected description going forward.

## Consequences

### Positive

- The advertised array is native-name, native-schema, in-distribution for
  every model that already knows how to call tools — no meta-tool recall
  tax, no grammar-switching requirement if constrained decoding is ever added
  to this repo's own inference path.
- `ToolSearch` mode still gets ADR-0193's core win — a lean, cache-stable
  advertised surface reaching the entire registry — without inventing a
  bespoke protocol where the industry already converged on one: `describe`'s
  byte-identical schema shape plus the `defer_loading`/`tool_reference` and
  `tool_search_call`/`tool_search_output` mechanisms are the same shapes
  Anthropic and OpenAI ship natively, so the anthropic_native and
  responses_native encodings are thin adapters, not new wire formats.
- z.ai (the priority provider, no native tool-search surface documented —
  `scratch/tool-search-wire-reference.md` §3) is fully served by
  `client_side`, the simplest of the three encodings and the one requiring no
  new client.
- The discovery pair drops from three tools to two (no separate `skills`
  tool), and is byte-identical in shape between modes, so a `Full`-mode model
  can also use `explore`/`describe` without any mode-specific behavior to
  reason about.

### Negative / neutral

- Three wire encodings instead of one dispatch mechanism is more surface to
  implement and test than the single invoke-unwrap point ADR-0193 proposed —
  accepted as the cost of using each wire's native mechanism instead of a
  single bespoke one.
- `client_side`'s append-only advertised array still busts the cache once
  per discovery (not continuously, but not zero either) — the same trade-off
  ADR-0193 already accepted for its lean-kernel-plus-invoke design, carried
  forward unchanged.
- A `ToolSearch`-mode model pays a discovery round-trip (`explore` →
  `describe` → the real call) before first use of an undiscovered tool — the
  same explicit rich-surface/cache-stability trade ADR-0193 accepted,
  unchanged in kind.
- OpenRouter's own cross-provider `openrouter:tool_search` surface and a
  Gemini equivalent are both left for follow-up (see Deferred) — a
  `ToolSearch`-mode session on either surface runs `client_side` today, which
  works but doesn't yet use the wire-native mechanism those two might
  eventually offer.

## Alternatives considered

- **The universal `invoke(name, args)` envelope** (ADR-0193's own design).
  Rejected for the four reasons in Context, plus the observation that it was
  never load-bearing for dispatch (ADR-0192 already decoupled advertisement
  from enforcement) — the envelope's central engineering justification never
  applied.
- **Server-executed (hosted) tool search on Anthropic/OpenAI.** Rejected for
  the same reason `tool_search.md` §3 gives: this repo's catalog is
  session/project-state dependent (skills, MCP servers, endpoints all vary
  per session and per project), which is precisely the case both providers'
  own guidance says to use client-executed search for, not their hosted
  regex/BM25/search-managed variants.
- **A single merged discovery tool** (`explore` and `describe` collapsed into
  one call). Considered per `tool_search.md` §3's own "candidates for
  merging" note; not adopted — `explore`'s terse index and `describe`'s full
  schema payload have different result shapes and different call-frequency
  profiles (explore once per session area, describe once per tool), and
  keeping them separate matches every hosted provider's own two-step shape
  (search, then the schema arrives as a `tool_result`/`tool_search_output`).
- **Sequential-only tool calls** (`tool_search.md` §8). Rejected: this repo's
  batch `ToolExec` parking already handles parallel calls correctly and
  predates this design; adopting sequential-start would be a regression, not
  a simplification.
- **Evicting discovered schemas from context after use** (`tool_search.md`
  Q4). Deferred, not adopted: leaving them in context is simpler and keeps
  the cache valid; eviction trades that simplicity for token savings on very
  long sessions with no evidence yet that the trade is worth it.

## Deferred

Filed as GitHub issues at close-out, not designed here:

- **OpenRouter's native `openrouter:tool_search` surface** — today served via
  `client_side`; a native adapter is a follow-up once the priority
  encodings ship.
- **Gemini re-check** — no tool-search or deferred-loading primitive is
  documented today (`scratch/tool-search-wire-reference.md` §5); Gemini stays
  on `client_side` until Google ships one, re-verified periodically.
- **Constrained decoding / forward-masking grammars** — `tool_search.md` §6's
  local-inference-harness work belongs to a different repo (the local
  inference server), not this one.
- **Schema eviction** — left in context indefinitely for now (Alternatives,
  above).

## References

- [ADR-0193](0193-two-tool-call-modes-and-lazy-tool-discovery.md): the design
  this partially supersedes — the invoke envelope and discovery trio rejected,
  the mode-resolution semantics and MCP-management posture carried forward
- [ADR-0192](0192-universal-advertisement-enforcement-at-dispatch.md): the
  dispatch-time enforcement this ADR leaves unchanged, and whose
  advertisement/enforcement split is why the envelope was never load-bearing
- [ADR-0194](0194-skills-are-additive-only.md): the additive-only skill
  surface `explore`'s index now lists, in place of the ADR-0193 `skills` tool
- [ADR-0195](0195-bash-is-the-default-exec-and-curated-read-only-rules.md):
  `call`'s registered-but-unadvertised posture outside the lean kernel — now
  this ADR's lean kernel (§2), not ADR-0193's
- [ADR-0190](0190-poll-is-always-on-non-maskable-internal-tool.md): the
  always-on, non-maskable, always-`Allow` internal-tool pattern `explore`/
  `describe` extend
- [ADR-0188](0188-session-keyed-per-user-mcp-scopes.md): the session-scoped
  registry view `describe` resolves MCP schemas through
- [ADR-0176](0176-structured-tool-result-is-error-and-duration-fields.md)/[ADR-0186](0186-exit-code-joins-the-structured-tool-result-side-channel.md):
  the `is_error`/`exit_code` split the error taxonomy's "command failure"
  case reaffirms rather than changes
- [ADR-0061](0061-parked-turn-state-batch-tool-resolution.md): the batch
  tool-call parking that already covers parallel calls without a sequential-
  start restriction
- `tool_search.md` (design rationale) / `scratch/tool-search-wire-reference.md`
  (provider wire facts): the source documents this ADR records the decisions
  from
