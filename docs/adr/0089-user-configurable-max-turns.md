# 0089. User-configurable `max_turns`

- Status: Accepted
- Date: 2026-07-16

## Context

The inner LLM→tool loop is capped so a model wedged in a tool loop can't run
forever (#177). The bound lived as a hard-coded `const MAX_TURNS: usize` inside
`run_round` (`session/turn.rs`): 50 rounds originally, raised to 200 in this
change. The counter resets per prompt, so a legitimate long session (many
prompts) is never capped — only a single runaway turn.

Two forces made the hard code unsatisfying:

1. **Workload variance.** A short scripted turn finishes in a few rounds; a
   long autonomous build/plan turn can legitimately run dozens. One compile-time
   constant can't fit both. A user running an agent on a bounded budget wants to
   *lower* it; one trusting an autonomous run wants to *raise* it — both without
   recompiling.
2. **The pattern already exists.** Every other tunable engine knob
   (`context_window`, `reoffer_interval`, `generation`) is a field on
   `EngineConfig`, fed by the layered user config (`config.yml`) with the
   project's standard precedence (**env > user config > embedded default**).
   `max_turns` was the lone hold-out: a fixed constant with no config path.

## Decision

Make `max_turns` a user-configurable field threaded through the existing
resolution path, matching the pattern of every other tunable.

### `EngineConfig::max_turns: usize`

New field on `EngineConfig` (`holly/config.rs`), default `200`. `run_round`
reads `cfg.max_turns` (already available — it receives `&EngineConfig`) instead
of the local `const`, guarded with `.max(1)` so a misconfigured `0` can't
disable the cap entirely. The trip path is otherwise unchanged: it emits the
`OutEvent::Error` ("exceeded maximum turn limit (N)"), and the known missing-
`Done` robustness gap (#177) is untouched by this change.

### User config surface

`max_turns` added to the layered user config alongside `agent`/`provider`/
`model`/`hooks`/`web_search`:

- `RawConfig` / `Config` (`config/mod.rs`) carry `max_turns: Option<usize>`.
- Documented in `defaults.yml` (`max_turns: 200` — visible + editable) and
  `template.yml` (commented-out starter), and added to the provenance keys list
  so `/inspect` reports which layer won.

### Startup wiring

`build_config` (the runtime head) threads `user_config.max_turns` into
`EngineConfig` when set, otherwise the `EngineConfig::default()` (200) stands.
No env var is added — the turn cap is a structural bound, not a per-invocation
tuning knob, and the config file is the single source of truth (an env layer
would be a third precedence tier with no demonstrated need).

## Consequences

- **(+)** A user can loosen the cap for a long autonomous run or tighten it for
  a budgeted one without a recompile, matching the precedent of every other
  tunable.
- **(+)** The cap value now surfaces in the `Error` message and `/inspect`, so a
  tripped limit is self-documenting instead of "is it 50 or 200?".
- **(+)** One fewer compile-time constant pretending to be a policy — the policy
  lives in config where the other knobs are.
- **(−)** New config surface to document and keep stable. Mitigated by reusing
  the existing layered-config plumbing (no new mechanism, one new key).
- **(−)** A user can set a very large value and run up provider cost before the
  guard trips. This is a local-trust-boundary call (ADR-0047): the repo/user is
  trusted, and the guard exists for runaway loops, not cost control.

## Alternatives considered

- **Env var (`ENTANGLEMENT_MAX_TURNS`).** Rejected: it would add a third
  precedence tier (env > config > default) for a knob with no per-invocation
  need. The config file already wins for `provider`/`model`; a dedicated env var
  would fragment the "where do I set this?" story for no gain.

- **Per-profile `max_turns`.** Rejected as over-scoped: the cap bounds a *single
  runaway turn*, which is a property of the loop, not the agent's role. A
  profile field would imply agent-specific loop budgets and complicate the
  profile schema for a marginal benefit. A global cap with the standard
  per-prompt reset is sufficient.

- **Keep the hard-coded constant, just raise it to 200.** Rejected: it leaves
  the workload-variance problem in place and continues the "lone constant
  pretending to be a policy" smell. The config path is cheap and removes the
  question permanently.

- **Make `max_turns: Option<usize>` on `EngineConfig` (None = default).**
  Rejected: a plain `usize` with a default is simpler, and the config layer
  already carries the `Option` (unset ⇒ default) so the engine field doesn't
  need to. The `.max(1)` guard covers the degenerate value at the consumption
  site.
