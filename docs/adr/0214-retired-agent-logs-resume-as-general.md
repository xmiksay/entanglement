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

A log naming a retired agent resumes, under `general`, on every path. The
in-tree heads print one line instead of refusing:

```
note: session used retired agent `build`; resuming as `general` (ADR-0207)
```

(`stderr` for `run --resume`, a transcript status line in the TUI.) The check
and the existing gap refusal (#104) live in one shared helper,
`session_store::load_for_resume`, so the three resume paths cannot drift.

## Consequences

- For `build` the fallback is the same persona under its new name. For
  `explore`/`research` the session continues as `general`. Their read-only
  posture was always a permission fact, and it now belongs to the session's
  mode (`--mode research`), not to the agent. The note makes the change
  visible rather than silent.
- A gap tombstone still refuses. That is lost data, not a retirement.
