# 0216. Resume modes ride the replay when any session is forced

- Status: Accepted
- Date: 2026-10-10
- Amends: [ADR-0215](0215-resume-fallback-per-session-and-read-only-narrowing.md)
  (§3: how the read-only narrowing and the head's own mode reach a resumed tree)

## Context

ADR-0215 §3 added one synthetic `ModeChanged { mode: "research" }` to the
replay for each forced session. The CLI then sent its `--mode` or config
`mode:` as a `SetMode` on the root and re-sent `research` to each forced
session afterwards. Core's `SetMode` always cascades over the target's whole
spawn sub-tree (`Supervisor::cascade_mode`). So the root switch widened every
forced child until its re-narrowing landed. A brand-new tool call in that
window would be graded under the wider mode (#757). That breaks ADR-0215's own
rule that a resumed session never gains authority.

No per-session `SetMode` can fix this. Giving the root `build` with a
`SetMode` always reaches a forced descendant as well.

## Decision

1. **When any session is forced, every mode rides the replay.**
   `ResumeCheck::paired_for_resume(records, explicit_mode, mode)` adds one
   synthetic `ModeChanged` per rebuilt session:
   - `research` for a session inside a forced sub-tree;
   - `mode` (the head's `--mode`, else config `mode:`, when set) for every
     other session.

   No `SetMode` is sent afterwards, so nothing cascades over a forced
   session.
2. **The narrowing covers the forced session's sub-tree.** A mode applies to
   the whole spawn sub-tree (ADR-0207 §6). So a forced session's rebuilt
   descendants start in `research` too. Under ADR-0215 they only reached it
   through the late cascade. `ResumeCheck` records each rebuilt session with
   its parent for this purpose.
3. **When nothing is forced, nothing changes.** `session_store::resume_with_mode`
   resumes the tree and then sends the ordinary cascading `SetMode` on the
   root. An explicit `--mode` disables forcing (ADR-0215 §2), so it always
   takes this path. `apply_forced_modes` is removed.
4. **TUI.** The TUI passes no mode, because it ignores `--mode` (#752). Its
   replay now narrows a forced session's descendants as well.

## Consequences

- No resumed session is ever visible in, or graded under, a mode wider than
  its forced one, not even for an instant.
- **Accepted trade-off:** when anything is forced, the root and every
  non-forced session take the configured mode silently through replay. Their
  model sees `[mode: X]` without the `— changed from Y` transition that a live
  `SetMode` produces. That transition only describes a configuration-driven
  switch at resume time, which the user chose, so losing it on this rare path
  is cheaper than a second mechanism.
- As in ADR-0215, the synthetic records are never logged. Each session's own
  start-up `ModeChanged` announcement is logged, so a later resume sees the
  applied mode in the log itself.
