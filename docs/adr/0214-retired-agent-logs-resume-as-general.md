# 0214. Logs naming a retired agent resume as `general`

- Status: Accepted
- Date: 2026-10-10
- Amends: [ADR-0207](0207-permission-modes-replace-agent-borne-authority.md)
  stage 6a ("Sessions logged under a retired name do not resume")

## Context

ADR-0207 stage 6a retired the built-in agents `build` (renamed `general`),
`explore` and `research` (both became the `research` permission mode), and
made every in-tree resume path (`skutter run --resume`, both TUI resume entry
points) refuse a log naming one. It argued that replaying a conversation under
a different persona misrepresents it.

`build` was 0.6.0's default agent, so the refusal stranded nearly every log
0.6.0 wrote — the opposite of #720's guarantee that logs from 0.6.0 onward
load. It was also inconsistent: `Holly::resume`, the path embedders use,
already keeps the base `general` seed for any agent name its catalog lacks,
so the same log resumed fine for an embedder and was refused by `skutter`.

## Decision

A log naming a retired agent resumes on every path instead of being refused.

1. **Per session, from its last effective agent.** Every session the resume
   rebuilds (the root, plus each still-live child, ADR-0112) is judged on
   the last agent its log named (`SessionStarted`, then any legacy
   `AgentChanged`). If that name is retired and the catalog doesn't carry
   it, the session falls back. It resumes as what replay actually folds:
   the last agent it named that the catalog still carries, else `general`.
   A `build` root that later switched to `plan` resumes as `plan` with no
   note. A user agent that still carries a retired name is a real agent,
   not a fallback.
2. **No silent authority widening.** If the retired agent was read-only
   (`explore`, `research`), the session resumes in the `research` mode. An
   explicit `--mode` wins; a configured `mode:` does not. A retired `build`
   resumes in the normal default (`--mode` > `config.yml` `mode:` >
   `DEFAULT_MODE`). The mode is sent per session after the head's own mode
   switch, so it narrows only that session's sub-tree.
3. **One line per fallback**, naming the session when it isn't the root:

   ```
   note: session used retired agent `build`; resuming as `general` (ADR-0214)
   note: session `c1` used retired agent `explore`; resuming as `general` in `research` mode (ADR-0214)
   ```

   `run --resume` writes it to `stderr`; the TUI shows it as a transcript
   status line. The TUI has no resume-time `--mode`, so the read-only
   narrowing always applies there. The TUI ignores `--mode` altogether
   today; that is tracked in #752.

The logic is pure, `session_store::check_for_resume(records, known)`, which
also performs the existing gap refusal (#104). `load_for_resume` is a thin
read plus that check, and `apply_forced_modes` sends the mode switches. The
CLI and both TUI entry points (one shared function) cannot drift.

## Consequences

- For `build`, the fallback is the same persona under its new name and the
  same default authority. For `explore`/`research`, the persona becomes
  `general`, but the read-only posture survives as the `research` mode. That
  posture was always a permission fact, and a mode is where ADR-0207 put it.
- The user sees every fallback. Nothing is silent.
- A gap tombstone still refuses. That is lost data, not a retirement.
