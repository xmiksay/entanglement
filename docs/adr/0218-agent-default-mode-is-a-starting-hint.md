# 0218. An agent's `default_mode` is a starting hint, not authority

- Status: Accepted
- Date: 2026-10-10
- Amends: [ADR-0207](0207-permission-modes-replace-agent-borne-authority.md)
  (§4/§6/§12: an agent may suggest the session's *starting* mode),
  [ADR-0216](0216-resume-modes-ride-the-replay.md) (§4: the TUI now passes
  its launch mode)

## Context

ADR-0207 made `Agent` identity-only. The mode is a separate axis, chosen by
`--mode` or config `mode:`. That left the shipped `plan` agent in an awkward
spot. Its persona expects the `plan` mode, but `skutter tui --agent plan`
started in `build`. `build` denies `propose_plan`, so the agent's only exit
was refused until the user also typed `--mode plan` or `/mode plan` (#763).

A second gap made this worse: the TUI ignored `--mode` and config `mode:`
entirely (#752). Only `run` honored them, although the brief said "`--mode`
on `run`/`tui`".

## Decision

1. **Agent frontmatter gains an optional `default_mode:`.** It must name a
   built-in mode (`research`/`plan`/`build`/`auto`). An unknown name logs a
   warning at load and is ignored, so a stale value never aborts the catalog.
   The shipped `plan` agent sets `default_mode: plan`. The value is carried
   on core's `Agent` as `default_mode: Option<String>` and shown by
   `skutter inspect agents`.
2. **Starting-mode precedence for a fresh root:** `--mode` > config `mode:` >
   the agent's `default_mode` > `DEFAULT_MODE` (`build`). Core seeds the
   root's mode from the agent at spawn. The head then sends its `--mode`
   or config `mode:` as an ordinary `SetMode`, which out-ranks the hint.
   Every head (`run`, the implicit one-shot, the TUI) shares one launch
   path for this (`heads::start_fresh`).
3. **A spawned child never widens.** A child normally inherits its parent's
   live mode (ADR-0207 §6). If its agent declares a `default_mode`, the child
   starts there only when that mode is no wider than the parent's mode.
   "Wider" means "later on the ladder `research` < `plan` < `build` < `auto`".
   This is the same ordering `request_mode` widens along, plus `auto` at the
   top. The ladder reaches core as `EngineConfig::mode_ladder`. A pair that
   is not on the ladder (for example an embedder's own mode names, or the
   empty default ladder) cannot be compared, so the child inherits its
   parent's mode. A hint can therefore only narrow a sub-tree. A compaction
   successor is the same session continuing, so it keeps its source's mode
   and ignores the hint.
4. **Resume ignores the hint.** A resumed session takes the mode its own log
   replays (ADR-0215/0216). `default_mode` applies only to fresh sessions
   and spawns. A `plan` session that the user switched to `build` resumes
   in `build`.
5. **The TUI honors `--mode` and config `mode:`** (#752), both for the
   session it starts and for every session it resumes from the resume
   modal. It uses `resume_with_mode` exactly as `run --resume` does, so an
   explicit `--mode` disables ADR-0215's read-only forcing there too.

## Why a starting hint is not authority

ADR-0207's rule is that an agent grants nothing. This decision keeps that
rule:

- The hint only selects one of the existing modes. Every call is still
  graded by that mode's rule table, and the config `permissions:` ceiling
  still clamps it. An agent cannot define rules, widen a mode, or add one.
- The user's choice always wins. `--mode` and config `mode:` out-rank it at
  launch, and `/mode` switches it live, as for any session.
- It cannot escalate across a spawn. A child's hint is clamped to the
  parent's mode, so a model cannot spawn its way into a wider posture.
  This is the same direction `request_mode` refuses to cross without a human.
- It is not sticky. It decides the first mode of a fresh session only and is
  never re-applied on resume, so it cannot override a later user decision.

A starting hint is a convenience default for picking a mode, like config
`mode:`. It is not a permission source.

## Consequences

- `skutter tui --agent plan` lands in `plan` and can call `propose_plan`
  immediately. `--mode build --agent plan` still starts in `build`.
- A `general`/`build` parent that spawns `plan` gets a `plan`-mode child.
  A `research` parent that spawns `plan` gets a `research` child.
- The root may announce two modes at start (the hint, then the head's
  `SetMode`) when both are set. The second is an ordinary, logged transition.
- Embedders with their own mode table get inherit-only children unless they
  set `EngineConfig::mode_ladder` themselves. That is the safe failure mode.
