# 0199. Session tool listing and enablement drive advertisement

- Status: Accepted
- Date: 2026-09-14
- Relates to: [ADR-0149](0149-per-session-tool-overlay.md) (the session tool
  overlay this ADR's part 2 makes a second write path for — enabling a tool
  now also affects advertisement, not just existence/grade),
  [ADR-0163](0163-live-bash-enablement-is-a-tool-overlay-entry.md) (the
  generalized `/enable <name-or-glob> [--allow [<pattern>]]` command shape
  part 4 restores, this time for every tool, not only `bash`),
  [ADR-0196](0196-tool-search-and-lazy-discovery-replace-the-invoke-envelope.md)
  (`explore`'s `kind` filter and section headers, part 1; the lean kernel and
  the `client_side` discovered-set part 2 writes into),
  [ADR-0198](0198-out-of-mask-tool-calls-are-approvable.md) (a `Session`-scope
  approval materializes the same kind of overlay enable entry part 2 reacts
  to — this ADR's advertisement effect applies to it too, for free, since it
  goes through the same `ToolOverlayChanged` fold).

## Context

`explore` (ADR-0196 §4) already lists every discoverable name in one flat,
sorted array. Two gaps showed up once `ToolSearch` mode was the default and
sessions started actually exercising the per-session tool overlay
(ADR-0149):

1. **`explore`'s output doesn't answer "what kind of thing is this."** A
   result mixing a host tool, an MCP server hint, a skill, and an endpoint in
   one alphabetical list makes the MCP server hint's own "enable then call
   it as `mcp__<server>__<tool>`" shape easy to miss — nothing in the output
   distinguishes "this row is a callable tool" from "this row is a server
   you first have to turn on."

2. **Enabling a tool via the overlay never taught the resolver anything.**
   Under `ToolSearch` mode's `client_side` encoding, a tool only enters a
   session's advertised array by going through `describe()` first, which
   both resolves the schema *and* marks the name into the session's
   discovered set (ADR-0196 §3). But `SetToolOverlay` (ADR-0149) — via the
   TUI dialog, a typed `/enable`, or ADR-0198's approval materialization —
   makes a tool *exist* and be *callable* for the session without ever
   touching that set. The model could call an overlay-enabled tool
   correctly (existence and grade are independent of advertisement,
   ADR-0192), but would not see its schema in the advertised array until it
   happened to `describe()` it anyway — a session-scoped grant that leaves
   the advertised surface stale until a redundant discovery round-trip.

Separately, the TUI had a live *editor* for the overlay
(`session_tools_dialog.rs`, a diff-against-profile checklist) but no
*browser*: no single place to see, for the active session, what exists across
every source (host tools, MCP servers + their tools, skills, endpoints),
whether it's masked, and what the model can already see of it without
opening `explore` from inside a running turn. And the generalized
`/enable <name-or-glob> [--allow [<pattern>]]` command ADR-0163 built when it
absorbed live bash enablement had no user-facing report of what a hand-typed
pattern actually matched.

## Decision

### 1. `explore` gains a `kind` filter and sectioned output

`explore`'s input schema gains an optional `kind: "tool" | "mcp" | "skill" |
"endpoint"` alongside the existing `filter` substring param — both apply
together (AND), either or both omitted lists everything. `kind` is derived
from each row's existing `source` label (`"built-in"` → `tool`,
`"mcp:…"`/`"mcp (allowed)"` → `mcp`, `"skill"`/`"skill:…"` → `skill`,
`"endpoint"` → `endpoint`); no new field on the row shape `describe`/
`tool_search` already depend on (`build_index`'s JSON output —
`name`/`description`/`source` — is unchanged, so nothing downstream of it
needed touching).

`explore`'s own reply changes from a flat pretty-printed JSON array to plain,
sectioned text — one header per non-empty kind, in fixed order (tool, mcp,
skill, endpoint), rows underneath as `name — description`. The MCP section
carries one clarifying line every time: "MCP servers — a server bundles
tools; enable the server (`mcp_enable {"server": "<name>"}`), then call its
tools by their `mcp__<server>__<tool>` names:" — the exact gap this ADR's
Context §1 names. `tool_search`'s reuse of the same live index
(`build_index`, unchanged in shape) is unaffected; only `explore`'s own
rendering changed.

### 2. An overlay enable entry joins the `client_side` discovered set

When a session's live tool overlay gains a new **enable** entry — via
`SetToolOverlay`, from any writer (the TUI dialog, a typed `/enable`,
ADR-0198's `Session`-scope approval materialization) — and the session is
`ToolSearch` mode with `client_side` encoding, the pattern is expanded
against the tool registry's **current** name snapshot and every match joins
`AdvertisingState.discovered` — the exact set `describe()` already writes
into (ADR-0196 §3). The resolver's next round therefore advertises the
enabled tool(s) directly, with no extra `describe` round-trip needed just to
catch up to a grant the user already made explicit.

Rules, matching the existing discovered-set contract exactly rather than
inventing a second one:

- **`Full`-mode sessions are a no-op.** Advertisement is already universal
  there (ADR-0192) — there is nothing to add.
- **A deny entry never advertises.** Withdrawing a tool's *existence* has no
  advertisement effect; existence and advertisement are already independent
  questions (ADR-0192), and a deny entry answers only the first one.
- **Only a genuinely new enable entry triggers expansion.** `SetToolOverlay`
  is full-replacement (ADR-0149), so every confirmation re-sends the whole
  list; an entry byte-identical to one already in effect before this change
  contributes nothing new — `DiscoveredSet::mark`'s own idempotence would
  make a re-mark harmless anyway, but skipping it keeps the fold's cost
  proportional to what actually changed.
- **Wildcard expansion is a one-shot snapshot, not a standing subscription.**
  A pattern like `mcp__docs__*` expands against the tools registered *at the
  moment of the overlay change*. A tool matching the pattern that gets
  registered *later* (e.g. the same MCP server reconnects with a new tool)
  is **not** retroactively advertised — it stays reachable via `explore`/
  `describe` like any undiscovered tool, simply without the one-round head
  start this ADR gives already-registered matches. Teaching the discovered
  set to track live pattern subscriptions instead of a fixed name list would
  be real additional machinery for a rare case; the one-shot snapshot is
  the direct generalization of what `describe()` already does for one name
  at a time.
- **Other `ToolSearch` encodings are untouched.** `anthropic_native`/
  `responses_native` have their own `defer_loading`/`tool_reference`
  mechanism for getting a schema in front of the model without a roster
  mutation (ADR-0196 §3); this feature is specific to the `client_side`
  discovered-tail.

Plumbing: the fold lives in the tool executor's existing
`OutEvent::ToolOverlayChanged` handler (`tool_runner.rs`) — the same place
that already mirrors the overlay into its own `overlays` map — reading the
*previous* list (before it's overwritten) to tell a new entry apart from a
re-send, and a fresh `tools.read().names()` snapshot for the wildcard
expansion. The pure logic (`advertise_new_overlay_enables`) lives in a new
`tool_advertising/overlay.rs`, taking `&AdvertisingState` plus the two entry
lists — no engine/session machinery needed to test it.

### 3. TUI `/tools`: a read-heavy browser, separate from the overlay editor

A new slash command, `/tools`, opens a session-scoped browser: every row
`explore`'s own index would list (via `discover::index_rows`, the exact same
live index — never a second, independently-built one), grouped by kind, each
row showing:

- **status** — `kernel` (ADR-0196 §2's fixed `TOOL_SEARCH_KERNEL` list plus
  the discovery pair and the profile-defining specs), `advertised` (`Full`
  mode universally, or already in the session's `client_side` discovered set
  under `ToolSearch` mode), `discoverable` (`ToolSearch` mode, not yet
  discovered), or `n/a` (no live `AdvertisingState` handle available — never
  a guess);
- **masked** — the session's *effective* availability: the active profile's
  mask, overridden by the overlay's disposition (`ToolOverlayEntry::disposition`)
  — the same effective-availability computation `session_tools_dialog`
  already makes for its checklist;
- **grade** — the active profile's own `PermissionProfile::resolve` for the
  name, `None` when no profile is resolvable. Deliberately a *profile-only*
  approximation: no ancestor clamp, no grant-store lookup, no config
  ceiling. This is the "cheaply available" reading of the requirement — a
  full dispatch-accurate resolution would need the tool executor's live
  session state, which the TUI process does not hold and this ADR does not
  thread in.

A free-text filter (typeahead — any plain character extends it, `Backspace`
shortens it) and a `Tab`-cycled category filter narrow the list; `Up`/`Down`
move the highlight; `Enter` enables the highlighted row through the exact
`/enable` path part 4 describes (flat, `Ask`-graded, no `--allow`) and
closes the view; `Esc` closes without acting.

This is a **new dialog** (`tui/tools_view.rs` + `tui/app/tools_view.rs`),
not a generalization of `session_tools_dialog.rs`. The two solve different
problems: `session_tools_dialog` is a checklist whose `Enter` computes and
submits a **diff against the profile** as a single `SetToolOverlay` batch —
its `SessionToolRow` shape (name + three booleans) has no room for a
description/kind/status without reshaping a struct three call sites already
depend on, and `/tools` never submits a batch at all. Reusing
`discover::index_rows` for the row *data* (rather than re-walking the
registry/MCP/skill state a third time) is the actual deduplication this ADR
cares about; the two dialogs' UI shapes staying separate is not a missed
generalization.

Two data-availability trade-offs, both accepted for the same reason (real
wiring cost for a status column, against a browser whose primary value is
"what exists and is it usable," not perfect live state):

- **Skill rows are freshly loaded from disk** (`skills::load_registry`),
  mirroring `skutter inspect`'s own "re-run discovery, no engine" posture —
  not the live, per-turn-activated `SkillRegistry` the tool executor's loop
  privately owns (never threaded out to any head today). A skill row's
  "loaded" reading is therefore "known to exist," not "active this turn."
- **The advertising status column needs the live, shared
  `AdvertisingState`** (ADR-0196 §2-3's session→mode map and discovered
  set), which *is* threaded into the TUI (a new `advertising_state` param
  on `tui::tui`, cloned from the same `Arc` the tool executor and the
  resolver closures already share) — read-only, since `/tools` never writes
  it directly.

### 4. TUI `/enable <name-or-glob> [--allow [<arg-pattern>]]` gains a match-count report

The command shape itself was already fully generalized by ADR-0163 when it
absorbed live bash enablement — `/enable tool <name-or-glob> [--allow
[<pattern>]]` already parses, upserts an overlay enable entry, and lazily
connects an `allowed`-tier MCP server the pattern names. What was missing:
no feedback on *what a hand-typed glob actually matched*. `upsert_enable`
now records a transcript status line — `` `<pattern>` matches N
currently-registered tool(s) `` — counted against the same startup-built
`tool_roster` the `/agent` checklist and the bare-`/enable` session-tools
dialog already read (a live-connected MCP server's tools discovered after
startup share that same known limitation; never a hard error either way).
`/tools`' own `Enter`-to-enable goes through this identical path, so its
rows get the same report.

Bare `/enable` (no arguments) is **unchanged**: it still opens
`session_tools_dialog`'s checklist, per ADR-0149's documented head surface.
`/tools` is a separate command, not an alias — the checklist (bulk diff
submission) and the browser (read-heavy lookup, one-row enable) serve
different moments, and ADR-0149's text describing bare `/enable`'s behavior
is immutable.

## Consequences

### Positive

- `explore`'s output stops burying the one instruction (`mcp_enable` then
  `mcp__<server>__<tool>`) a model most needs to act on an MCP result — the
  clarifying line now appears exactly where that ambiguity lives, every
  time.
- A session-scoped grant (the TUI dialog, a typed `/enable`, or an ADR-0198
  approval) is no longer a "the model can call it, but won't see its schema
  until it happens to `describe()` it anyway" gap under `client_side`
  encoding — the resolver catches up within the same round the overlay
  change lands.
- The TUI gets a genuine "what exists, and can I already use it" surface for
  the first time, without duplicating `explore`'s index-building — the same
  live data reaches the model (`explore`) and the operator (`/tools`)
  through one code path.
- `/enable`'s match-count report closes a real "did that glob actually do
  anything" gap for a hand-typed pattern.

### Negative / accepted trade-offs

- **The wildcard-expansion snapshot is not retroactive** (§2) — a real,
  if narrow, gap: a pattern enabled before the matching tool existed leaves
  that tool merely *discoverable*, not pre-advertised. Accepted as the
  direct generalization of `describe()`'s own one-name-at-a-time behavior,
  not a new kind of staleness.
- **`/tools`' skill rows and permission grades are both approximations**
  (§3) — "known to exist" rather than "active this turn" for skills, and a
  profile-only grade with no ancestor/ceiling/grant awareness. Both are
  documented, neither is a silent inaccuracy: the alternative (threading the
  tool executor's private per-session skill-activation and grant state out
  to every head) is real additional plumbing this feature does not need to
  justify.
- **`/tools` is a new dialog, not a generalization of `session_tools_dialog`**
  — two UI modules to maintain instead of one extended. Accepted per §3's
  reasoning: the row shapes and interaction models genuinely differ, and the
  row-*data* reuse (`discover::index_rows`) is where duplication would
  actually have hurt.
- **Bare `/enable` stays the checklist, not an alias for `/tools`** — a
  reader expecting one command to grow into the other's surface won't find
  it; the two stay reachable as separate, purpose-built commands instead.

## Alternatives considered

- **Add `kind` as a literal field on every `explore` row instead of deriving
  it from `source`.** Rejected: `source` already carries the same
  information in a shape `describe`/`tool_search` depend on byte-for-byte;
  a redundant field invites the two to drift apart on what a given row's
  category "really" is.
- **Keep `explore`'s reply as JSON, add a `kind` key.** Rejected for the
  sectioned rendering (not the filter): the clarifying MCP line and terse
  section headers only make sense as prose a model reads, not as JSON a
  model would have to reconstruct grouping from.
- **Teach the discovered set to track live pattern subscriptions** (retro-
  advertise a tool registered after its enabling pattern). Rejected for this
  pass: real additional state (a set of *patterns*, not just names, checked
  on every future registration) for a narrow case; left as a documented gap
  rather than designed here.
- **Generalize `session_tools_dialog` into `/tools` instead of a new
  module.** Rejected per Decision §3 — the diff-against-profile submit
  shape and the read-heavy browse-and-filter shape don't share enough UI
  logic to be worth forcing into one struct; the actual duplication risk
  (index-building) is avoided by sharing `discover::index_rows`, not by
  sharing the dialog.
- **Thread the tool executor's live per-session skill-activation state and
  full permission-resolution chain into the TUI for `/tools`.** Rejected:
  real plumbing (a new shared handle out of a currently-private executor-
  loop field, plus ancestor/grant-store access this process doesn't have)
  for a status column whose primary value is already delivered by the
  cheaper approximation.
- **Make bare `/enable` an alias for `/tools`.** Rejected: ADR-0149's
  documented bare-`/enable` behavior (the checklist dialog) is immutable
  ADR text describing a real, still-useful UX (bulk diff submission);
  changing what a bare command does is a bigger, separate decision than
  this ADR's scope.

## Files

- `entanglement-runtime/src/discover/explore.rs`: `kind` parsing/filtering,
  `render_sections` (§1), `IndexRow`/`index_rows` (§3's shared row source).
- `entanglement-runtime/src/discover/mod.rs`: `explore_spec`'s `kind`
  property; `IndexRow`/`index_rows` re-export.
- `entanglement-runtime/src/tool_advertising/overlay.rs` (new):
  `advertise_new_overlay_enables` (§2).
- `entanglement-runtime/src/tool_runner.rs`: the `ToolOverlayChanged` fold
  calls into the above before overwriting its `overlays` mirror.
- `entanglement-runtime/src/tui/tools_view.rs` (new): the pure `/tools`
  dialog state (filter, category cycle, row selection).
- `entanglement-runtime/src/tui/app/tools_view.rs` (new): row building
  (`discover::index_rows` plus mask/overlay/advertising status), open/close/
  navigation, `Enter`-to-enable.
- `entanglement-runtime/src/tui/modals/tool_popups.rs`: `draw_tools_view`.
- `entanglement-runtime/src/tui/commands.rs`: `Command::Tools`.
- `entanglement-runtime/src/tui/enable_command.rs`,
  `entanglement-runtime/src/tui/app/enable.rs`: `record_enable_match_count`
  (§4).
- `entanglement-runtime/src/main.rs`, `entanglement-runtime/src/tui/mod.rs`:
  thread the shared `AdvertisingState` handle into the TUI.
