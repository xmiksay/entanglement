# 0129. Thread the skill `allowed_tools` mask into `rhai` binding resolution

- Status: Accepted
- Date: 2026-07-22
- Amends: [ADR-0106](0106-skill-scoped-allowed-tools-enforcement.md)

## Context

[ADR-0106](0106-skill-scoped-allowed-tools-enforcement.md) (#400) made a
`SKILL.md`'s `allowed_tools` frontmatter enforce something: while a skill is
active in a session (from a resolved `load_skill` call until that turn's
`Done`), `tool_runner::skill_masked` refuses any tool call outside the
skill's list, checked in `ToolExec` handling strictly after the #116 agent
mask. ADR-0106 explicitly called out the gap this ADR closes, in its
"Negative / accepted" section: `rhai`'s bindings (`Intercept::Rhai`) resolve
their own tool calls against a `BindingPolicy` snapshot — the agent mask plus
the permission chain — captured once per run, entirely bypassing
`skill_masked`. A model that loads a restrictive skill and then runs a
`rhai` script in the same turn could reach a tool the skill's `allowed_tools`
excludes, so long as it went through a script binding instead of a direct
call.

This is the same *shape* of gap [ADR-0119](0119-rhai-bindings-route-through-the-escape-root-gate.md)
(#446) closed for the escape-root gate: an enforcement layer wired into the
generic `Intercept::Permission` route (`tool_runner::dispatch`) but never
threaded into the separate `Intercept::Rhai` path. The 2026-07-21
post-remediation audit (#473) filed it as row 7 of the deferred-work ledger
(#396), tracked as issue [#477](https://github.com/xmiksay/entanglement/issues/477).

Mitigating factor noted in ADR-0106 and still true at the time this landed:
no built-in or documented skill combines `allowed_tools` with `rhai` use, and
a skill that omits `rhai` itself from `allowed_tools` already blocks
launching the script tool mid-turn. This was a real gap, not an exploited
one — closed proactively rather than reactively.

## Decision

**Fold the session's active-skill mask into `BindingPolicy` as a one-time
snapshot, checked after the agent mask, mirroring `tool_runner`'s ordering
for the generic route.**

- `BindingPolicy::capture` gains a new parameter,
  `active_skill: &HashMap<SessionId, ActiveSkill>` — the same map
  `tool_runner`'s main loop already owns and `skill_masked` already reads for
  generic dispatch. For each of the seven `BINDING_TOOLS` that survives the
  existing agent mask, `capture` calls `permission::skill_masked` and records
  a hit into a new `skill_masked: HashMap<&'static str, String>` field
  (tool → the skill id that excluded it, for the refusal message).
- `BindingPolicy::decide` checks this map immediately after the existing
  `masked` (agent-mask) check and before resolving the permission chain — a
  new `Decision::SkillMasked(String)` variant, checked in the same position
  the generic route checks `skill_masked` after `tool_masked`. `read_raw`
  reuses the existing alias-to-`read` substitution, so a skill restricting
  `read` also restricts the unlabeled raw path, exactly as the agent mask
  already does.
- `service_binding` refuses a `SkillMasked` decision with **the identical
  message shape** the generic route's refusal uses: `` `tool `{tool}` is not
  available while skill `{skill_id}` is active (restricted by its
  allowed_tools)` `` — swapped only for the binding's tool name — so a script
  sees a `try`/`catch`-able error it can reason about the same way a direct
  refusal reads in the transcript.
- `tool_runner`'s `Intercept::Rhai` arm locks `active_skill` in the same
  block it already locks `active` (the agent-profile map) to build
  `base_self`/`policy`, and passes it straight into `BindingPolicy::capture` —
  no new lock ordering, no new shared state, the mutex this arm didn't
  previously touch.
- **The mask is a snapshot, not a live read** — captured once at
  `Intercept::Rhai` entry, exactly like the agent mask it sits beside, rather
  than re-checked on every binding call mid-script. This is sound, not a
  shortcut: `load_skill` is not itself a member of `BINDING_TOOLS`, so no
  script binding can activate, switch, or clear a skill mid-run. The
  session's active skill genuinely cannot change between the snapshot and
  the last binding call in the same script — a live read would observe the
  exact same value every time, at the cost of holding (or repeatedly taking)
  the `active_skill` lock from inside the async binding-resolution path.
- **Clears at the same `Done` the generic route's mask clears at** — no new
  clear path was needed, since `Intercept::Rhai`'s snapshot reads the same
  `active_skill` map `tool_runner`'s existing `Done` handling already clears
  per session. A script run in a *later* turn (after the loading turn's
  `Done`) captures an empty mask, same as a direct call would see the tool
  unmasked.

A new integration test in `entanglement-runtime/tests/rhai.rs`
(`skill_mask_refuses_a_binding_then_clears_after_done`) drives the real
engine + tool executor across two turns with a real `SkillRegistry`: turn 1
loads a skill whose `allowed_tools: [read, rhai]` excludes `edit`, then runs
a `rhai` script whose `edit(...)` binding must be refused (caught by the
script, file untouched); turn 2 (after turn 1's `Done` clears the mask) runs
the identical script and the binding succeeds, writing the file. Mirrors
`tests/skill_mask.rs`'s two-turn shape for the generic route.

## Consequences

- **Positive.** Closes the last of the deferred-work ledger's row-7 gap: a
  skill's `allowed_tools` now scopes *every* way a model can reach a tool in
  the session it's active in — direct dispatch and `rhai` bindings alike —
  not just the former. No remaining asymmetry between the two routes for
  this particular mask (the escape-root gate closed the analogous asymmetry
  for containment in ADR-0119; capability/workdir-scoped rule matching for
  the exec bindings remain separately tracked, ledger rows 2 and others,
  unaffected by this change).
- **Positive.** Zero new locks, zero new shared state, zero wire/protocol
  change — `BindingPolicy::capture` already ran inside the block holding
  `active`'s lock; `active_skill` is locked there too, released before the
  policy moves into the detached script task exactly as the agent-mask
  snapshot already was.
- **Neutral.** `BindingPolicy::capture` gained a mandatory sixth parameter,
  so every call site (the one production call in `tool_runner.rs` plus six
  unit tests in `script.rs`) needed updating; all six tests now pass an
  empty `HashMap::new()`, preserving their original agent-mask-only coverage
  byte-for-byte.
- **Neutral / accepted.** Same as the generic route: no exemption for
  `load_skill` inside a script either, but this is moot in practice —
  `load_skill` was never a `rhai` binding to begin with (it's not in
  `BINDING_TOOLS`), so this ADR doesn't change whether a script can switch
  skills; it only changes whether a script's *existing* bindings are scoped
  by whichever skill is already active when the script runs.

## Alternatives considered

- **A live read of `active_skill` on every binding call**, mirroring how the
  permission chain's argument-scoped rules resolve live per call. Rejected:
  there is no state a live read could observe that the snapshot doesn't
  already capture correctly, since nothing reachable from inside a script
  can mutate `active_skill` — the extra lock traffic (once per binding call
  instead of once per run) would buy nothing. If a future change ever makes
  `load_skill` itself a script binding, this would need revisiting; today it
  is out of scope (`BINDING_TOOLS` is a fixed, hand-maintained list with no
  such entry).
- **Give `rhai`'s skill-mask refusal its own message wording**, distinct from
  the generic route's. Rejected: the issue explicitly asked for "the same
  error shape as the generic route's refusal, so a script sees a tool
  failure it can react to" — a model that has already seen the generic
  refusal's wording (e.g. from an earlier turn, or from documentation)
  recognizes the same failure mode inside a script without needing a second
  vocabulary.
- **Clamp the skill mask down the ancestor/spawn chain for bindings**, unlike
  the generic route. Rejected: would create exactly the asymmetry this ADR
  exists to remove, in the opposite direction — the generic route's skill
  mask is deliberately session-local, not ancestor-clamped (ADR-0106's own
  "Alternatives considered"), so a script binding must follow the identical
  scope, not a stricter one.
