# 0105. Expose `EngineConfig.idle_ttl` via `config.yml` — an engine-global setting, not a `serve`-only flag

- Status: Accepted
- Date: 2026-07-17

## Context

[ADR-0090](0090-idle-ttl-auto-hibernation.md) (#363) added
`EngineConfig.idle_ttl: Option<Duration>` and the supervisor sweep that drives
it, but left it a core-only knob — nothing in `entanglement-runtime` ever set
it, so it stayed permanently `None` in every shipped binary. ADR-0090's own
"Consequences" section named this explicitly: *"`skutter serve` can wire this
once configuration exposes it (deferred; this ADR is core-only, matching how
`reoffer_interval` shipped before any runtime config surface existed for it)."*
[ADR-0077](0077-session-hibernation-evictable-resumable.md) (#318), the manual
`HibernateSession` this builds on, made the same deferral for the same reason.

A long-lived `serve` embedding one `Holly` across many users/sessions grows
memory monotonically without this: `HibernateSession` gives an embedder manual
eviction, but nothing evicts on its own. Issue #401 is that missing wiring.

## Decision

**Add `idle_ttl_secs: Option<u64>` to the layered user config (`config.yml`,
[ADR-0047](0047-local-trust-boundary.md)) and copy it onto
`EngineConfig.idle_ttl` (as `Duration::from_secs`) in `build_config`,
alongside the existing `max_turns` config→engine wiring it mirrors exactly.**

- **Whole seconds, not a duration string.** No `humantime` (or equivalent)
  dependency exists anywhere in this workspace — every `Duration` in the tree
  is a hardcoded constant. `max_turns: Option<usize>` is the only existing
  precedent for a scalar `Option<T>` engine setting flowing config → engine.
  `idle_ttl_secs` follows that exact shape rather than introducing a new
  dependency and a parser for `"30m"`/`"1h"` strings to save one unit
  conversion at the call site.
- **One engine-global setting, not a `serve`-specific CLI flag.** Every head
  (`run`/`pipe`/`tui`/`serve`) shares a single `EngineConfig` built once in
  `main()` before the subcommand is matched (`Holly::spawn` runs before the
  `match cli.cmd` block) — there is no per-head `EngineConfig` to special-case
  a `--idle-ttl` flag onto. Setting it in `config.yml` (or the project-layer
  `.entanglement/config.yml`) reaches every head uniformly, same as
  `max_turns`/`web_search`/`hooks` already do.
- **Applying it to the CLI/TUI is harmless by construction, not by
  exclusion.** The sweep (ADR-0090) only ever evicts a session it can prove is
  **settled** — `Session::turn.is_none()` across the whole spawn sub-tree —
  and hibernation is resumable, not destructive (#318): a single interactive
  TUI session left idle past the TTL gets evicted and would need a `resume` to
  come back (out of scope here — the TUI does not currently drive `resume` on
  a `SessionHibernated` it receives, so in practice an operator wanting this
  only sets it for `serve`). Nothing about the config surface forces a head
  distinction; the safety property is the sweep's settledness check, not which
  head asked for the TTL.

## Consequences

- `skutter serve --port … ` run against a `config.yml` carrying
  `idle_ttl_secs: 1800` now auto-hibernates a settled root (and its spawn
  sub-tree) after 30 minutes of inactivity, freeing its supervisor-side
  bookkeeping; a subsequent message against the same session id rebuilds it
  losslessly via `Holly::resume` — closing the memory-growth gap #401 was
  filed against.
- `idle_ttl_secs` unset (the default, in both `defaults.yml` and every
  scaffolded `template.yml`) leaves `EngineConfig.idle_ttl` at `None` —
  byte-identical to every release before this change, for every head.
- A policy-owning embedder (the case ADR-0077 named,
  [xmiksay/site#13](https://github.com/xmiksay/site/issues/13)) keeps driving
  `Holly::hibernate` directly from its own idle-timeout logic and simply never
  sets this key — `config.yml` is `entanglement-runtime`'s own CLI-binary
  configuration surface, not something an embedder linking `entanglement-core`
  directly has to touch at all.
- The CLI/TUI can technically opt into the same sweep by setting the same
  global key, with no dedicated support for resuming a hibernated interactive
  session today — an acceptable, narrow gap since the issue's stated scope is
  `serve`, and nothing stops a future TUI change from handling
  `OutEvent::SessionHibernated` by auto-resuming if that becomes desirable.

## Alternatives considered

- **A `serve`-only `--idle-ttl-secs` CLI flag on the `Serve` subcommand.**
  Rejected: would require special-casing one head's slice of `EngineConfig`
  construction inside the otherwise head-agnostic `build_config`/`main()`
  startup sequence, for a setting that is genuinely orthogonal to which head
  is running — the sweep's own settledness guard is what makes this safe for
  any head, not a flag scoped to one.
- **A `humantime`-style duration string (`"30m"`, `"1h"`).** Rejected for v1:
  no such parsing dependency exists anywhere in the codebase yet, and whole
  seconds is precision enough for an eviction TTL measured in minutes-to-hours.
  Nothing here blocks adding duration-string parsing later as a shared utility
  if a second Duration-typed setting (e.g. `reoffer_interval`) needs the same
  treatment.
- **Leaving it core-only and expecting embedders to construct `EngineConfig`
  by hand.** Rejected: `skutter serve` is itself exactly the "long-lived,
  multi-session, no bespoke persistence layer" embedder ADR-0090 described as
  needing this, and it is not a hand-assembled `EngineConfig` — it goes through
  `entanglement-runtime`'s config-file-driven startup like every other head.
