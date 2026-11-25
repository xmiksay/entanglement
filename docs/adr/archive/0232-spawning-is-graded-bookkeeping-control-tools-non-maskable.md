# 0232. Spawning is graded; the bookkeeping Control tools are non-maskable

- Status: Accepted
- Date: 2026-10-14
- Issue: #849 (found while implementing #843)
- Amends: [ADR-0207](0207-permission-modes-replace-agent-borne-authority.md)
  §3 ("`Control` … is never graded") and §6 ("Spawning is never graded —
  `agent`/`agent_send` are `Control`"); extends
  [ADR-0190](0190-poll-is-always-on-non-maskable-internal-tool.md)'s
  non-maskable posture from `poll` to the other bookkeeping tools, and
  [ADR-0226](0226-one-advertised-surface-mcp-envelope-registered-is-available.md)'s
  Maskability to the full `tool_names::NON_MASKABLE_TOOLS` set

## Context

ADR-0207 §3 made every `Capability::Control`-only tool ungraded: the
dispatch ladder skips the overlay and the mode for it. The runtime-owned
routes (`agent`, `agent_send`, `poll`, `ask_user`, `propose_plan`) bypass
the ladder entirely in the interception table. So an overlay entry or a
mode rule naming any of them was read by nothing. `/disable agent` toasted
"off" and spawning went on. `modes: build: deny: [agent]` loaded silently
and denied nothing.

#843 added `NON_MASKABLE_TOOLS` for names no entry can match (the discovery
tools, the `mcp` envelope, the script-only read variants). The TUI refuses
them, and a config rule or a wire overlay entry naming one warns. The
Control tools above were left out: each needed a decision.

Two findings shaped this decision:

- `propose_plan` was never ungraded. It is `Capability::Plan` and grades
  itself through the pluggable resolver (`resolve_effective`) before
  anything else runs. The built-in `deny: [plan]` class rule in
  `research`/`build`/`auto` is what keeps it out of those modes. A rule
  naming it did take effect: `plan: deny: [propose_plan]` denied it, while
  a widening named rule (`build: allow: [propose_plan]`) is refused at load
  by the tuning guard (ADR-0207 §5). The overlay was never consulted.
- `request_mode` is ungraded: the dispatch `Control` bypass, then its own
  checks (refused in `auto`, never targets `auto`, widening pairs only)
  and an unconditional approval park.

## Decision

1. **`agent`/`agent_send` are graded like any tool.** Both stay
   `Capability::Control`, so a `control` class rule reaches them. They are
   the only graded Control tools (`capability::bypasses_grading`).
   Before launching, each climbs the same dispatch ladder as a host tool
   (`Dispatcher::gate`): overlay deny, mode grade + grants, the
   `pre_tool_use` hook, the mode deny, the unattended collapse, the
   approval park. A refusal is an ordinary `is_error` result. A `prompt:`
   rule parks an approval card. The spawn-guard slot is reserved first,
   because the guard is loop state and the ladder may park in the detached
   task. A refusal releases it.
2. **The built-in modes allow the `control` class.** `research`/`plan`/
   `build` default to `prompt`, and spawning ran unprompted before this
   change. So each mode lists `control` under `allow` (`auto` too, for the
   record). A user rule naming `agent` outranks that class rule (ADR-0207
   §4 as amended by ADR-0228), so `deny: [agent]` and `prompt: [agent]`
   work as written.
3. **The bookkeeping Control tools join `NON_MASKABLE_TOOLS`:** `poll`,
   `ask_user`, `update_tasks`, `request_mode`, `propose_plan`. `poll`
   collects work already authorized (ADR-0190). `ask_user` and
   `update_tasks` only report. `request_mode` only asks the user for more
   authority. Withdrawing any of them strands work or hides a question.
   It never reduces capability. Through #843's machinery, the TUI refuses
   them with the reason, and config rules and wire overlay entries naming
   them warn.
4. **A rule naming a non-maskable tool never matches.** The rule matcher
   (`mode::rules::best_match`) skips a `Named` rule when the call's tool
   is non-maskable. This one matcher serves both modes and the
   `permissions:` ceiling. For every other member this changes nothing,
   since those rules were inert already. For `propose_plan` it makes the
   config warning true. Its `plan` class, or a `*` rule, is now its only
   gate. Plan authorship stays allowed in `plan` mode only, and the
   built-in class rules never warn (only rules naming the tool do).
   `request_mode` keeps its own checks.

## Consequences

- `/disable agent` and a wire `SetToolOverlay` deny on `agent` really stop
  spawning, and `modes: x: deny: [agent]` refuses with a catchable error.
- A `pre_tool_use` hook now sees `agent`/`agent_send` calls, and an
  embedder's `PermissionResolver` is asked about them. A resolver that
  denies unknown names now refuses spawning, so it has to grade `agent`.
- A user-layer `permissions:` ceiling with `default: prompt` now asks for
  each spawn, like every other tool it doesn't list.
- `plan: deny: [propose_plan]` stops working and warns that it has no
  effect. The equivalent is `plan: deny: [plan]`: a tie with the built-in
  `allow: [plan]` goes to the stricter grade.
- A `permissions: deny: [propose_plan]` ceiling is inert too, and warns.
  Its replacement is `deny: [plan]`.
- An embedder's hand-built `Mode` or `PermissionProfile` that names
  `propose_plan` silently loses that rule: the warning is config-only.
- Within the class kind the longest key wins, and `control` is the longest
  class name. So the `control` class matches only a tool whose capabilities
  are exactly `[Control]`. Otherwise the built-ins' `control` allow would
  widen a mixed `[Write, Control]` tool past `research`'s `deny: [write]`.
  A mixed tool follows its other classes.
- When a session ends or hibernates, the cards of its turn are unwound
  (`PendingDecisions::stop_attached`): tool and spawn cards, `ask_user`
  questions, plan and mode cards, a blocking script's nested cards. So a
  spawn card can't pin its `max_agents` slot forever. A resume re-offers a
  parked `ToolExec`, which parks a fresh card.
- A background (`detached`) script's nested card is exempt on session end
  and hibernate. It is no `ToolExec`, so nothing would ever re-offer it, and
  the script outlives the session: it keeps running after a hibernate, and
  a compaction successor inherits it (ADR-0231). So its card stays
  answerable.
- A session `Stop` is not exempt: it still unwinds every card of the
  session, background ones included (`stop_session`). `Stop` is the user's
  explicit escape hatch, and the TUI clears its card view on it. An attended
  park has no timeout, so a card left parked there would hang the script
  until the process exits. ADR-0185's "survives a session Stop" means the
  script is not aborted. Its nested call fails with "rhai run stopped" and
  the run ends normally.
- `skutter inspect modes` shows `agent`/`agent_send` with their grade, no
  longer "control (never graded)".

## Alternatives considered

- **Make every Control tool graded.** Rejected: denying `poll` or
  `ask_user` strands work the mode already allowed (ADR-0190's argument),
  and grading them adds an approval round-trip to bookkeeping.
- **Give `agent` a new capability (or `Exec`).** Rejected: spawning runs
  nothing on the host itself, since the child's own calls are graded.
  `Control` plus a named exception keeps the vocabulary at five classes.
- **Keep named `propose_plan` rules effective and warn anyway.** Rejected:
  a warning that says "no effect" about a rule that does take effect is
  worse than no warning.
- **Strip non-maskable rules at config load.** Rejected: that needs two
  load sites (`modes:` tuning and the ceiling), and it misses an embedder's
  hand-built `Mode`/`PermissionProfile`. The matcher is the one place every
  built-in grade passes through: modes, the ceiling and embedder-built
  rule tables alike. An embedder's own `PermissionResolver` can still grade
  `propose_plan` by name. That is the embedder's policy, outside this
  rule language.
