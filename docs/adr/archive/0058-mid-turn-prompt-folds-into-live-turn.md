# 0058. Mid-turn `Prompt` folds into the live turn

- Status: Accepted
- Date: 2026-07-13

## Context

[ADR-0018](0018-turn-loop-stash-discipline.md) established that a command
arriving on the session inbox *during* a turn is pushed onto a `VecDeque`
replay stash and processed after the in-flight turn ends — fixing the silent
drop of mid-turn commands (#36). For a `Prompt` this means: the user's
mid-turn message is stashed (`session/turn.rs` streaming loop and tool-dispatch
loop, `session/tools.rs` while waiting on a `ToolResult`) and, once the turn
emits `Done`, `session_loop` pops it and runs it as a **fresh `run_turn`**.

That is correct queuing, but it is not *steering*. A model that queues user
messages folds each queued message into the **next model request within the
same turn** — the user redirects the running work. entanglement's stash instead
made a mid-turn `Prompt` wait for the current turn's full reply and then start
its own turn, so guidance sent while the engine was busy could not reach the
in-flight reasoning at all
([#182](https://github.com/xmiksay/entanglement/issues/182), part of the
engine-robustness epic #176).

ADR-0018 explicitly deferred this: its "process non-Stop commands immediately"
alternative was rejected partly because "a mid-turn `Prompt` has no clean
insertion point in the conversation." The inner LLM→tool loop supplies exactly
that insertion point — the boundary **before the next request**, where the
previous round's assistant + tool messages are already committed to `Context`.

## Decision

**Drain stashed `Prompt`s into `Context` at the top of each inner-loop
iteration, before building the next `LlmRequest`** (`session/turn.rs`). The
loop walks the stash, `push_user`s every `SessionCmd::Prompt` in order (with a
`tracing::debug!` line), and leaves every non-`Prompt` command
(`SetAgent`, a stale `ToolResult`) in the stash untouched for the session loop
to handle at turn end. So:

- A `Prompt` sent mid-turn reaches the model on the **very next round-trip of
  the same turn** — real steering, mirroring queued-user-message fold semantics.
- The fold site is reached **only when the previous round emitted tool calls**
  (a reply with no tool calls emits `Done` and returns *before* the loop tops
  again). A `Prompt` sent *after* the model's final answer therefore still lands
  in the stash with no further iteration to fold it, and `session_loop` correctly
  replays it as a fresh turn — the ADR-0018 path is preserved for that case.
- Non-`Prompt` commands keep ADR-0018's replay-after-turn discipline verbatim.
  Only `Prompt` gets the new fold; `SetAgent` mid-turn would still race the
  in-flight request's system prompt/tool list (ADR-0018's "one turn = one
  profile" invariant), so it stays deferred.

This refines ADR-0018 for `Prompt` specifically; the stash mechanism, the
`Stop`-interrupts rule, and the deferral of every other command are unchanged.

## Consequences

- **(+)** Mid-turn guidance steers the running turn instead of queuing behind
  its full reply — the capability #182 asked for, with no new wire variant.
- **(+)** The fold happens *before* `within_limit`, so the injected message is
  counted against the context budget for that request like any other history.
- **(+)** Ordering is preserved: multiple stashed prompts fold in arrival order,
  ahead of the next request.
- **(−)** A `Prompt` can only fold when the turn continues past the current
  round (i.e. the model called a tool). During a pure-text turn the model
  streams its reply and ends; the prompt then replays as a new turn. This is
  inherent — there is no mid-request insertion point once the model has decided
  to stop — and matches the intuition that steering only bites while the agent
  is still working.
- **(−)** The stash is still unbounded (ADR-0018); the fold adds no cap. A turn
  with many tool rounds drains any accumulated prompts as it goes, which in
  practice keeps the stash short.

## Alternatives considered

- **Fold after `Done` (status quo / ADR-0018).** Rejected by #182: the whole
  point is to inject guidance *into* the running turn, not to queue a successor.
- **Interrupt the in-flight request and re-issue with the folded prompt.**
  Rejected: it wastes the round-trip already in flight and reintroduces the
  cancel-and-replace complexity ADR-0018 called out; folding at the natural
  request boundary gets the same guidance to the model one round-trip later with
  no aborted work.
- **Fold every command type mid-turn, not just `Prompt`.** Rejected: `SetAgent`
  mid-turn races the request already built from the old profile — ADR-0018's
  "one turn = one profile" invariant. Only `Prompt` has a clean, side-effect-free
  insertion point (append to history).
