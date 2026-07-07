# 0017. `InMsg::Stop` cancels the turn, not the session

- Status: Accepted
- Date: 2026-07-07

## Context

`InMsg::Stop` (per its docstring at `protocol.rs:169`) was specified as
"Cancel the current turn and park the session at idle." The implementation
in `holly.rs:147-152` violated its own docstring: on receipt of `Stop`, the
supervisor *removed the session from its map* before forwarding the command.
The session task then received the `Stop`, returned at `session.rs:113`, and
was dropped — taking its `Context` (the entire in-RAM conversation history)
with it.

The next `Prompt` to the same `SessionId` lazily spawned a **fresh, empty**
session. To the user this looked like the assistant had amnesia mid-session:
*"the assistant claims it never made the tool call it clearly just made."*

The most common trigger in the TUI was Esc in `WaitingForApproval`
(`tui/mod.rs:188-196`). The TUI carried explicit coping code
(`session_view.rs::stopped: bool` + `note_stop_sent`/`note_prompt_sent`) that
reset the seq dedupe guard against the engine's restart — confirming the
destroy-on-Stop behavior was load-bearing and the amnesia was real.

Two adjacent Stop paths existed but were already correct:
`try_recv` inside `run_turn` (added in #36) catches Stop during streaming /
between tool calls and aborts the turn cleanly; `wait_approval` returns
`Approval::Cancelled` on Stop. Both returned control to `session_loop`, which
then dutifully returned — confounding the cancel with a destroy.

## Decision

1. **The supervisor (`holly.rs`) no longer special-cases `Stop`.** It routes
   `Stop` as a regular `SessionCmd` to the session task. The session map entry
   stays; the task stays alive; `Context` is preserved.

2. **`session_loop` (`session.rs`) treats outer-level `Stop` as a no-op.** When
   `Stop` arrives while the session is idle (between turns), the loop simply
   continues — there is nothing to cancel. Inbox close (`None`) still ends the
   task. The pre-existing inner-loop `try_recv` handling (interrupt during
   streaming / between tool calls) is unchanged.

3. **`run_turn` returning `Err(())` (cancel-via-Esc-during-approval) no longer
   kills the task.** `session_loop`'s `Prompt` arm ignores the `Err`: the turn
   is aborted, the session goes back to waiting for the next command, with
   `Context` intact (whatever state the cancelled turn left it in).

4. **The TUI coping code is removed**: `SessionView::stopped`, the
   `note_stop_sent` / `note_prompt_sent` methods on `SessionView` and `App`,
   the seq-reset-on-prompt workaround, and the two
   `stop_then_prompt_resets_seq_guard` tests. With the engine no longer
   restarting the session, the seq counter is monotonic across Stop+Prompt and
   no head-side compensation is needed.

5. **Stop remains documented as cancel-semantics** in `protocol.rs`. The
   docstring and the implementation now agree.

## Consequences

- **(+)** Esc-in-approval and any Stop-while-idle no longer destroy the
  conversation. A user who reflexively hits Esc can resume by typing another
  prompt.
- **(+)** The TUI's session view becomes simpler — no `stopped` flag, no
  dual-method plumbing, no fragile seq-reset timing.
- **(+)** The fix aligns with the user's mental model: "stop" means "stop what
  you're doing," not "forget everything."
- **(−)** A cancelled mid-stream turn still leaves an orphaned `User` message
  in `Context` (the user message is pushed before `run_turn` runs; per
  ADR-0007 the partial assistant turn is *not* committed). The next turn's
  request to the model includes this user message with no assistant reply —
  occasionally confusing but not catastrophic, and addressed separately.
- **(−)** A cancelled-during-approval turn leaves an orphaned assistant
  `tool_calls` message in `Context` with no paired `tool` result. Same shape
  of problem, same deferral.
- **(−)** Sessions can no longer be forcibly reset by sending `Stop`. If a
  future caller wants true reset semantics, it will need a dedicated
  `InMsg::Reset` (or to drop and recreate the `Holly` engine). No current
  caller wants this.

## Alternatives considered

- **Persist `Context` outside the task** (snapshot on every push; restore on
  respawn). Rejected for this PR: it would also fix the orphan-message problem
  and harden against task panics, but it's substantially larger (where does
  the snapshot live? per-session KV? in-memory map?) and the destroy-on-Stop
  behavior was the *specified* bug. Persisted context remains a likely future
  ADR if crash-recovery becomes a goal.
- **Add a new `InMsg::Cancel` distinct from `InMsg::Stop`.** Rejected: the
  docstring already promised cancel semantics, and no caller wants destroy
  semantics. Splitting the command would require a TUI keybinding decision
  (which one does Esc emit?) for no user-visible benefit.
- **Catch the destroy at the TUI layer** (don't send Stop on Esc). Rejected:
  the destroy was an engine bug; papering over it in one head leaves the ABI
  broken for any future embedder. And the TUI genuinely wants to interrupt
  the in-flight turn (e.g. cancel a long tool call), which is what Stop is
  *for*.

[0007]: 0007-streaming-llm-and-provider-crate.md
