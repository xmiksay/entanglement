# 0106. Skill-scoped `allowed_tools` enforcement — a session-local runtime mask layered after the #116 agent mask

- Status: Accepted
- Date: 2026-07-17

## Context

A `SKILL.md`'s frontmatter has carried an `allowed_tools: [...]` field since
skill discovery landed ([ADR-0036](0036-skill-discovery-and-registry.md)), but
every site that touches it — `SkillMeta::allowed_tools`,
`load_skill`'s doc comment, `system_prompt.rs` — says the same thing: parsed,
never enforced, deferred to skill *provenance* (#116 in ADR-0036/0037's
vocabulary, tracked here as #400). The blocker
([ADR-0037](0037-load-skill-tool-deterministic-resolution.md)) was that
enforcing it needs a notion of "this skill is active in this session right
now", and that notion didn't exist: `load_skill` returns a `skill_id` in its
result (so the audit trail is visible), but nothing recorded it anywhere the
tool executor could read back before dispatching the *next* call.

Meanwhile the **agent-level** mask (#116 in the numeric-issue sense,
[ADR-0038](0038-physical-per-agent-tool-restriction.md)) has been enforced
since `entanglement_runtime::permission::tool_masked` shipped: an
`AgentProfile`'s `tools`/`disallowed_tools` narrows what a session can call at
all, checked structurally before permission and every other route
(`tool_runner.rs`'s `ToolExec` handling, #203). A skill's mask is a *narrower*,
*shorter-lived* restriction layered on top of whatever the agent mask already
allows — conceptually "while this skill is loaded, only these tools", not a
replacement for the profile's own boundary.

## Decision

**Track "this session has an active skill" as runtime-only, in-memory state in
the tool executor** (`entanglement_runtime::tool_runner`), keyed by
`SessionId`, set on a resolved `load_skill` call and cleared at the session's
next `Done` (or when the session ends) — never a core-protocol concept, matching
skills' existing runtime-only status (core has no `skills` module at all).

- **`ActiveSkill { skill_id, allowed_tools }`** and the check
  `skill_masked(active_skill, session, tool) -> Option<String>` live in
  `entanglement_runtime::permission`, next to `tool_masked` — the natural home
  for masking logic, kept separate from `tool_masked` itself (different scope:
  session-local vs. ancestor-clamped, see below).
- **Activation is a result-text parse, not a new tool-execution-record
  field.** `load_skill`'s result already begins `skill_id: <name>\n`
  (ADR-0037); a new `parse_skill_id(&str) -> Option<&str>` in
  `skills::load_skill` reads that header. On a `load_skill` dispatch that
  returns a real skill_id (a failed load — unknown/`user_only` — has no such
  header and is a no-op), `tool_runner::run_and_reply` looks the skill up in
  the same live `Arc<RwLock<Arc<SkillRegistry>>>` `LoadSkillTool` already
  holds, records `(session, ActiveSkill)`, and broadcasts
  `OutEvent::SkillActive`.
- **Enforcement is layered strictly after the #116 agent mask**, in the same
  `ToolExec` handling block, before `Intercept::classify` routes the call: a
  tool surviving `tool_masked` is then checked against `skill_masked`; either
  refusal short-circuits before spawn/permission/execution, mirroring the
  existing masked-tool refusal shape (`tool_runner.rs`'s existing `masked`
  branch). No special-case exemption for `load_skill` itself — if a skill's
  `allowed_tools` omits it, the model cannot switch skills mid-turn; it can
  next turn once the mask clears. This matches the frontmatter's literal
  contract (`allowed_tools` is the *complete* set for the skill's scope) rather
  than inventing an implicit always-on entry.
- **Scope is the session, not the ancestor chain.** Unlike `tool_masked`
  (which clamps down a spawned sub-agent's own mask against every ancestor's),
  `skill_masked` only reads the exact session `load_skill` ran in. A skill's
  scope is "this conversation's current turn", not an inheritable profile
  trait a spawned child should pick up — a child spawned mid-skill starts
  unmasked by the parent's loaded skill (it still inherits the parent's
  *agent* mask via the existing ceiling, unaffected by this change).
- **Cleared on `Done`, not on an explicit unload tool.** The tool executor's
  main loop already handles `OutEvent::Done` for other per-session bookkeeping
  nowhere else; a new arm clears `active_skill` for that session (and emits
  the `SkillActive { skill_id: None, .. }` wire clear) — mirroring how
  `in_flight`/`cancels`/`grants` are cleared on `SessionEnded`. No explicit
  "unload_skill" tool exists or is added: the natural unload point is the
  model finishing its turn, and adding a second unload mechanism before any
  concrete need for one would be speculative.
- **Wire posture: `OutEvent::SkillActive { session, seq, skill_id: Option<String>,
  allowed_tools: Option<Vec<String>> }`**, a new core-protocol variant mirroring
  `FileChange`'s shape exactly — a fresh per-session seq
  ([ADR-0068](0068-shared-per-session-seq-counter.md)), no `Session::replay`
  fold (falls into `replay.rs`'s existing `_ => {}`, since there is no
  session-`Context` state to reconstruct — a resumed session simply starts
  with no active skill, matching the live "cleared every turn" semantics), and
  persisted for free (persistence is variant-agnostic over any event with a
  `session()`). `skill_id: None` is the clear; `Some(id)` with
  `allowed_tools: None` means the skill *is* active but imposed no
  restriction (mirrors `ActiveSkill.allowed_tools: None` ⇒ unrestricted, same
  "absent allowlist ⇒ inherit" convention `AgentProfile::tools` uses). Core
  stays skill-**agnostic**: this is a DTO core carries but never interprets —
  the same posture-only role `ProfileDetail` plays for the agent mask, kept as
  a *separate* variant rather than folded into `ProfileDetail`/`SessionInfo`
  because those are static-per-profile projections computed by core's
  `Holly`/`Session`, while skill activation is dynamic, mid-turn, and
  runtime-owned — reusing them would mean either core learning about skills or
  the runtime reaching into core's session state, both worse than one new
  narrow event. The stdio `run --format text` head and the TUI transcript
  (`tui::session_view::reducer`) both render it as a one-line notice, the same
  treatment `Compacted`/`FileChange` get.

## Consequences

- **Positive.** `allowed_tools` in `SKILL.md` frontmatter now does something:
  a skill author can hand a model a narrowly-scoped capability set (e.g. the
  built-in `commit` skill's `allowed_tools: [bash, read, grep]`) and the
  runtime enforces it physically, the same "does not exist for this call"
  boundary #116 established for the agent mask — not a persona instruction the
  model can ignore.
- **Positive.** Zero core surface beyond the one new `OutEvent` variant: no
  `skill_id` field was added to `ToolCall`/`ToolExec` (the "protocol change
  that would carry no behaviour today" ADR-0037 explicitly avoided) — the
  session-keyed `ActiveSkill` map is the mechanism instead, entirely inside
  `entanglement_runtime::tool_runner`.
- **Positive.** `~30` existing test call sites through `spawn_tool_executor`/
  `spawn_tool_executor_with_hooks` are untouched — both now internally wrap an
  empty `SkillRegistry` via a new `wrap_skills` helper (mirroring
  `wrap_profiles`), so no skill mask ever activates for them, byte-identical
  to their pre-#400 behavior. Only `spawn_tool_executor_with_policy` (three
  call sites: `main.rs`, `examples/embedded.rs`,
  `tests/policy_seam.rs`) gained the new `skills` parameter.
- **Negative / accepted.** `rhai`'s internal bindings (`Intercept::Rhai`)
  resolve their own tool calls against the agent mask/permission chain
  directly and do **not** consult `skill_masked` — a skill loaded, then a
  `rhai` script run in the same turn, is not scoped by the skill's
  `allowed_tools`. Rhai already has its own permission-resolution path
  entirely separate from the generic `Permission` route (it captures a
  `BindingPolicy` snapshot up front); threading skill-mask state through it is
  deferred until a concrete need for skill-scoped script tool calls appears —
  today no built-in or documented skill combines `allowed_tools` with `rhai`
  use.
- **Negative / accepted.** No explicit unload — a skill's mask always lasts
  until the loading turn's `Done`, even if the model's remaining work in that
  turn has nothing to do with the skill. Matches "for the duration" from the
  issue's proposal without adding a tool whose only purpose would be ending a
  restriction early.
- **Neutral.** `skill_masked`'s refusal message ("tool `X` is not available
  while skill `Y` is active…") is distinct from the agent mask's ("…restricted
  by profile") so a model (and a human reading the transcript) can tell which
  boundary fired — useful for the model to reason about switching skills next
  turn rather than retrying the same call.

## Alternatives considered

- **A `skill_id` field on `ToolCall`/`ToolExec`, threaded through core.**
  Rejected: this is exactly what ADR-0037 called "a protocol change that
  would carry no behaviour" without an executor consulting it — and once the
  executor tracks activation locally, core carrying the field adds nothing;
  the session-keyed map is strictly simpler and keeps the change entirely in
  the runtime crate.
- **Fold `SkillActive` into `ProfileDetail`/`SessionInfo`/`AgentChanged`.**
  Rejected: those are computed by core's `Holly`/`Session` from `AgentProfile`
  state that changes only on `SetAgent`/session-start — reusing them for a
  mid-turn, `load_skill`-triggered mutation would mean either core learning
  about skills (violating the crate boundary: skills are runtime-only) or the
  runtime somehow mutating core's posture snapshot out-of-band. A dedicated
  event, mirroring `FileChange`'s already-established "runtime-authored,
  posture-only" pattern, is the smaller, boundary-respecting change.
- **Clamp the skill mask down the ancestor chain like `tool_masked`.**
  Rejected: a skill's scope is explicitly "this session's current turn", set
  by a tool call the *session itself* made — there is no reason a spawned
  child, whose own turn hasn't called `load_skill`, should inherit a
  restriction its parent picked up mid-conversation. The agent mask's
  ancestor clamp exists because a profile's tool set is a standing
  authorization boundary a child must never exceed; a skill's mask is a
  transient, self-chosen narrowing with no such inheritance rationale.
- **Exempt `load_skill` from its own skill's mask** (so a model can always
  switch skills). Rejected for v1: `allowed_tools` is documented as the tool
  set for that skill's scope; carving out an implicit exception the
  frontmatter doesn't declare would surprise an author who wrote
  `allowed_tools: [bash]` expecting exactly that. A skill wanting to allow
  switching lists `load_skill` explicitly.
- **An explicit `unload_skill` tool (or a TTL/round-count based expiry).**
  Rejected: no concrete requirement motivates it yet, and `Done` is already a
  clean, existing boundary the executor observes for other per-session
  cleanup. Speculative expiry policy is exactly the kind of premature
  abstraction the project's engineering standards ask to avoid.
