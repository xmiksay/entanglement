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
   built-in mode no wider than `build`: `research`, `plan` or `build`.
   Anything else logs a warning at load and is ignored, so a stale value
   never aborts the catalog. That includes `auto`: **a hint can never
   select `auto`.** The shipped `plan` agent sets `default_mode: plan`. The
   value is carried on core's `Agent` as `default_mode: Option<String>` and
   shown by `skutter inspect agents`.
2. **`InMsg::Spawn` gains an optional `mode`** (trusted-only like the rest
   of `Spawn`; serde-defaulted, mirroring #725's `Spawn.model`). A head puts
   its resolved `--mode`/config `mode:` there, so the session starts in its
   final mode and announces **exactly one** starting `ModeChanged`. There is
   no follow-up `SetMode` and so no `[mode: X — changed from Y]` notice for
   the model to read as a real switch. A name not on a non-empty
   `EngineConfig::mode_ladder` refuses the spawn with a seq-0 `Error`, like
   an unresolvable `Spawn.model`. With an empty ladder, core carries the
   name opaquely, as it does for `SetMode`.
3. **Starting-mode precedence for a fresh root:** `--mode` > config `mode:` >
   the agent's `default_mode` > `DEFAULT_MODE` (`build`).
   - Every head (`run`, the implicit one-shot, the TUI) shares one launch
     path, `heads::start_fresh`. It sends one `Spawn` carrying the agent and
     the launch mode.
   - With no launch mode, core starts the root in the agent's hint. As
     defense in depth, core takes the hint only when the ladder shows it is
     no wider than `DEFAULT_MODE`; otherwise the root starts in
     `DEFAULT_MODE`. **A hint never starts a root wider than `build`**, even
     from an agent built outside the runtime's parser.
   - An explicit `Spawn.mode` is a human's choice (`--mode auto` included)
     and is not clamped.
4. **A spawned child never widens.** A child normally inherits its parent's
   live mode (ADR-0207 §6). If its spawn names a `mode`, or else its agent
   declares a `default_mode`, the child starts there only when that mode is
   no wider than the parent's mode. "Wider" means "later on the ladder
   `research` < `plan` < `build` < `auto`". This is the same ordering
   `request_mode` widens along, plus `auto` at the top. The ladder reaches
   core as `EngineConfig::mode_ladder`.
   - A pair not on the ladder cannot be compared, so the child inherits its
     parent's mode. That covers an embedder's own mode names and the empty
     default ladder.
   - A requested mode or hint can therefore only narrow a sub-tree.
   - A compaction successor is the same session continuing, so it keeps its
     source's mode and ignores both.
5. **Resume ignores the hint.** A resumed session takes the mode its own log
   replays (ADR-0215/0216). `default_mode` applies only to fresh sessions
   and spawns. A `plan` session that the user switched to `build` resumes
   in `build`.
6. **The TUI honors `--mode` and config `mode:`** (#752), both for the
   session it starts and for every session it resumes from the resume
   modal. It uses `resume_with_mode` exactly as `run --resume` does, so an
   explicit `--mode` disables ADR-0215's read-only forcing there too.

## Why a starting hint is not authority

ADR-0207's rule is that an agent grants nothing. This decision keeps that
rule:

- The hint only selects one of the existing modes, and never one wider than
  the ordinary default. Every call is still graded by that mode's rule
  table, and the config `permissions:` ceiling still clamps it. An agent
  cannot define rules, widen a mode, or add one.
- It can never pick `auto`. An agent file, including a repo-committed
  project-layer one (trusted per ADR-0047), cannot put a session into the
  unattended posture with nobody choosing it. Everywhere else `auto`
  requires an explicit human choice: `--mode`/config `mode:`, `/mode`, or
  the approver's pick on a plan approval. `request_mode` refuses it outright.
- The user's choice always wins. `--mode` and config `mode:` out-rank it at
  launch, and `/mode` switches it live, as for any session.
- It cannot escalate across a spawn. A child's hint or requested mode is
  clamped to the parent's mode, so a model cannot spawn its way into a
  wider posture. This is the same direction `request_mode` refuses to cross
  without a human.
- It is not sticky. It decides the first mode of a fresh session only and is
  never re-applied on resume, so it cannot override a later user decision.

A starting hint is a convenience default for picking a mode, like config
`mode:`. It is not a permission source.

## Consequences

- `skutter tui --agent plan` lands in `plan` and can call `propose_plan`
  immediately. `--mode build --agent plan` starts in `build`, with one
  announced mode and no transition notice.
- A `general`/`build` parent that spawns `plan` gets a `plan`-mode child.
  A `research` parent that spawns `plan` gets a `research` child.
- A typo'd mode fails closed, before any session exists. `LaunchMode::resolve`
  rejects a name outside the built-in modes from `--mode` or from config
  `mode:`, on every head (`run`, `run --resume`, the TUI). It names the
  source and the valid modes, and `skutter` exits non-zero with no turn run
  and no session log. As a second line, `start_fresh` subscribes before
  sending its `Spawn` and waits for that session's `SessionStarted` or
  supervisor `Error`. A refused `Spawn` fails the launch, so a prompt never
  follows it into ADR-0210's lazy create of a `general`/`build` root. (On
  master before this ADR, a typo went out as an opaque `SetMode`, and the
  runtime's grader refused every call under it.)
- Embedders with their own mode table get inherit-only children, and roots
  that ignore every hint, unless they set `EngineConfig::mode_ladder`
  themselves. That is the safe failure mode.
