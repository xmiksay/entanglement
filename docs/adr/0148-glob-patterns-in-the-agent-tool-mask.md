# 0148. Glob patterns in the agent tool mask

- Status: Accepted (amends [0038](0038-physical-per-agent-tool-restriction.md) —
  supersedes its "name-based, no globbing" consequence; composes with
  [0117](0117-mcp-tool-capability-fan-out.md))
- Date: 2026-08-01

## Context

The #116 agent tool mask (`AgentProfile.tools` allowlist /
`disallowed_tools` denylist, ADR-0038) matches tool names by exact string
equality. ADR-0038 accepted that deliberately: *"the mask is name-based (exact
tool-name match) … sufficient for the fixed host-tool roster."* External MCP
tools (#198/ADR-0067) broke that assumption without anyone noticing:

- An MCP tool's registered name is `mcp__<server>__<tool>` — unknowable when a
  profile is authored, and not even known at profile-*parse* time:
  `agents::load_registry` runs before `mcp::connect` in `main.rs`, and live
  `McpAdd` (#375/ADR-0097) can register new names mid-process.
- So a profile with **no** `tools:` key (built-in `build`/`debug`, every
  lenient foreign agent) inherits all MCP tools, while a profile with **any**
  `tools:` allowlist (built-in `plan`/`explore`, any hand-authored restricted
  agent) silently loses every MCP tool with no way to opt back in short of
  hand-listing namespaced names after every server change (#537).
- The asymmetry with the permission side made this sharper: #426/ADR-0117
  taught bare capability keys (`read: allow`) to *grade* annotated MCP tools —
  but grading is moot for a tool the mask says doesn't exist. #426 solved the
  grading half and explicitly could not solve the existence half.

## Decision

`tools:` and `disallowed_tools:` entries become **`*`/`?` wildcard patterns**,
matched by the same private `glob_match` the #173 argument-scoped and #425
workdir-scoped permission rules already use (`*` = any run including empty,
`?` = exactly one char, everything else literal; separator-agnostic, no
`**`/classes, dependency-free per ADR-0006). The change is confined to
`AgentProfile::advertises_tool`, now delegating to a new public associated
`AgentProfile::mask_allows(tools, disallowed, tool)` so heads holding only a
mask projection (the TUI's profile info) reuse the predicate instead of
re-implementing it.

Semantics:

- A pattern with no wildcard degenerates to exact equality — every existing
  profile behaves bit-for-bit as before.
- `tools: [read, "mcp__*"]` admits every MCP tool; `"mcp__docs__*"` one
  server's; `disallowed_tools: ["mcp__*"]` strips MCP from an inherit-all
  profile. (YAML requires quoting `*` entries.)
- Deny-wins ordering is unchanged and applies pattern-to-pattern: a matching
  deny entry beats any allow entry.
- `tools: []` still advertises nothing; `tools: ["*"]` ≡ inherit-all
  (`tools: None`) — with one carve-out below.
- Matching is evaluated **dynamically at advertisement/dispatch time**, never
  expanded at parse time — which is precisely what makes names that don't
  exist yet (MCP connect races, live adds) coverable, and keeps core
  MCP-unaware (no `mcp__` special case anywhere).

Because both enforcement points funnel through `advertises_tool`, one change
covers the whole surface: core's per-turn spec filter (`session/turn.rs`), the
runtime's dispatch-side `tool_masked` **including the ancestor-chain clamp**
(a parent's `"mcp__*"` admits a child's MCP call; evaluated per profile per
link, ADR-0138 sponsorship untouched), and the rhai `BindingPolicy` mirror.

Carve-outs and non-goals:

- **Plan authority stays literal-exact.** `plan_tasks::explicitly_allowlists`
  (the #231/ADR-0049, #513/ADR-0145 default-closed gate for
  `propose_plan`) does not glob: `tools: ["*"]` widens the mask without
  granting plan authorship, exactly as `tools: None` already doesn't. This
  yields the same benign advertisement/dispatch asymmetry `tools: None` has
  today (dispatch would accept a call the model is never advertised).
- **Skill `allowed_tools` (#400/ADR-0106), permission rule tool-name keys,
  and `spawnable_agents` stay exact** — separate mechanisms, each with its own
  ordering/authority concerns; none is needed for the MCP gap.
- **Built-in `plan.md`/`explore.md` are unchanged.** Auto-granting MCP tools
  would break their read-only posture (an MCP tool can be arbitrarily
  write-capable). The recipe is a user/project-layer override adding
  `"mcp__*"` (or a server-scoped pattern) to `tools:`.
- **The #330 tools-dialog seeds through `mask_allows`** (deleting its
  duplicated exact-match resolution), so a glob entry shows its matches
  checked; **saving still emits the concrete checked set** — a glob does not
  survive the checklist round-trip (the module's documented
  resolve-to-final-set philosophy). Keeping a live pattern means hand-editing
  the frontmatter.

## Consequences

- Positive: a restricted profile can finally hold MCP tools — per server or
  wholesale — and an unrestricted one can subtract them; the mask now covers
  names minted after profile load; zero migration (literals unchanged); no
  new dependency or wire change.
- Negative: a broad allow pattern is a standing wider grant — every tool a
  later-added MCP server registers under a matching name is auto-admitted;
  the permission ladder (profiles, ceiling, approval) remains the graded
  backstop. Mask entries and permission keys now glob with the same matcher
  but permission *tool-name* keys still don't — a documented asymmetry left
  for a future need.
- The dialog's glob-expansion-on-save is mildly lossy by design; noted in the
  UI docs.

## Rejected alternatives

- **A dedicated `mcp:` frontmatter key** (`mcp: all | [server, …]`, the #479
  `sandbox:` template): a second mechanism beside `tools:` for the same
  concept, needing its own precedence story against the mask; globs subsume
  it (`"mcp__<server>__*"`) with one generic rule.
- **Parse-time capability-style fan-out** (extend #426's `McpCapabilityIndex`
  expansion to `tools:`): only covers servers configured at startup — misses
  live `McpAdd`, and turns the mask from data into something recomputed on
  registry change.
- **A reserved `mcp` pseudo-capability in `tools:`**: MCP-aware core, and no
  per-server granularity without growing a syntax anyway.
