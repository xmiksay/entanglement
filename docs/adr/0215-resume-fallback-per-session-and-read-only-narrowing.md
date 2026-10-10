# 0215. Resume fallback per session, with read-only narrowing

- Status: Accepted
- Date: 2026-10-10
- Amends: [ADR-0214](0214-retired-agent-logs-resume-as-general.md)
  (which agent a retired-agent log resumes as, in which mode, and the note)

## Context

ADR-0214 made a log naming a retired agent (`build`/`explore`/`research`)
resume under `general` instead of being refused. Review turned up three
problems.

- **Wrong note.** It reported the first retired name anywhere in the root
  file, children included. That disagreed with what replay actually
  rebuilds. A root that switched from `build` to `plan` resumes as `plan`,
  and a fixture whose root is `site-editor` got a note about its `explore`
  child as if the root had fallen back.
- **Silent widening of authority.** `explore` and `research` were read-only.
  Resuming them as `general` in the default `build` mode widened what the
  session may do, with nothing but a note.
- **Mid-turn race.** `Holly::resume` only enqueues the resume. The rebuilt
  session announces its replayed mode and re-offers a parked tool batch
  before it reads its inbox. A narrowing `SetMode` sent afterwards would
  let a re-offered call be graded under the wider mode first.

## Decision

1. **Per session, from its last effective agent.** Every session the resume
   rebuilds is judged on the last agent its log named (`SessionStarted`,
   then any legacy `AgentChanged`). That means the root, plus each child
   still live in the log (ADR-0112); a closed or hibernated child is not
   rebuilt, so it gets neither a note nor a mode switch. A session falls
   back only if that name is retired **and** the agent catalog doesn't
   carry it, so a user agent still named `build` is a real agent. It
   resumes as what replay folds: the last agent it named that the catalog
   carries, else `general`.
2. **Read-only stays read-only.** A session whose retired agent was
   `explore` or `research` resumes in the `research` mode. Only an explicit
   `--mode` overrides this; a configured `mode:` does not. A retired `build`
   resumes in the normal default (`--mode` > `config.yml` `mode:` >
   `DEFAULT_MODE`).
3. **The narrowing rides the replay.** `ResumeCheck::paired_for_resume`
   appends a synthetic `OutEvent::ModeChanged { session, mode: "research" }`
   to the paired records, one per forced session. Replay folds it
   last-write-wins, so the session *starts* narrowed: its announced mode and
   any re-offered call are already under `research`. The CLI also re-sends
   the switch per session, after its own `--mode`/config `apply_mode`,
   because that switch cascades over the whole tree and would otherwise
   widen the child again.
4. **One note per fallback**, naming the session when it isn't the root:

   ```
   note: session `c1` used retired agent `explore`; resuming as `general` in `research` mode (ADR-0215)
   ```

   `run --resume` prints it to stderr; the TUI shows it as a transcript
   status line.

The logic is the pure `session_store::check_for_resume(records, known)`,
which also performs the gap refusal (#104). `load_for_resume` is a thin read
plus that check. The CLI and the TUI (both entry points share one function)
use it identically.

## Consequences

- A resumed session never gains authority it didn't have. A read-only
  posture that used to belong to the agent now carries over as a mode,
  which is where ADR-0207 put it.
- The TUI has no resume-time `--mode`, and today it ignores `--mode`
  altogether (#752). So on the TUI the read-only narrowing always applies.
  That is the safe direction until #752 lands.
- The synthetic `ModeChanged` is never logged. The resumed session's own
  start-up `ModeChanged` announcement is, as usual, so a later resume sees
  `research` in the log itself.
