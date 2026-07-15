# 0068. Shared per-session `seq` counter: runtime mints fresh seqs via `Holly::emit_for_session`

- Status: Accepted
- Date: 2026-07-15
- Fixes the `(session, seq)`-uniqueness defect in the protocol contract of [0002](0002-session-multiplexed-protocol.md); absorbs #159 (supervisor `seq:0` TUI-invisibility). Rides the parked-turn round-trip of [0058](0058-mid-turn-prompt-folds-into-live-turn.md)/[0061](0061-parked-turn-state-batch-tool-resolution.md)/#58.

## Context

The protocol documents a **monotonic per-session `seq`** on content events so a
head can dedupe/order against replayed history (ADR-0002). The natural
WebSocket-reconnect implementation is a strict `seq > last_seen` guard — and it
was silently broken:

- The seq counter (`Session.seq`) lived **only** in the core session task. A
  runtime service that authors an event *while the session is parked* on a
  `ToolExec` — the approval `ToolRequest` (`tool_runner`, `propose_plan`,
  `rhai`), the `UserQuestion` (`ask_user`), the `Plan`/`TaskList` snapshot
  (`update_plan`/`update_tasks`), the `FileChange` audit — had no handle on that
  counter, so it **reused the parked `ToolExec` seq**. `(session, seq)` was
  therefore *not* unique: a `ToolExec` and its `ToolRequest` always shared a seq,
  and with batched tool calls (#270/ADR-0061) an earlier call's `ToolRequest`
  could carry a *lower* seq than a later call's `ToolCall`, so a `seq > last`
  dedupe dropped it — every approval prompt silently lost on reconnect. Event
  authorship was split across two crates and unauditable.
- Supervisor lifecycle errors (a refused resume/spawn of a closed/unknown id, a
  saturated channel) emitted `seq: 0` because the supervisor likewise couldn't
  mint the session's seq. The TUI dedupe guard is `seq > last_seen_seq` with init
  `0`, so `0 > 0` never passes: the ADR-0028 backpressure shed, the closed-id
  refusals, and failed replays were **structurally invisible** — a dropped
  `Prompt` vanished (#159).

The root cause is one counter with two writers that couldn't reach it.

## Decision

**Make the per-session seq counter a shared `Arc<AtomicU64>`, held in a
supervisor-owned registry, and give the runtime one sanctioned emit path that
mints from it.**

- `Session.seq` becomes `Arc<AtomicU64>` (was `u64`); `next_seq` is an atomic
  `fetch_add`. Every core emit draws from it exactly as before.
- `Holly` holds a `SeqRegistry = Arc<Mutex<HashMap<SessionId, Arc<AtomicU64>>>>`.
  A session task **registers** its counter on start (before the first turn, hence
  before any `ToolExec`) and **removes** it on exit — the counter's lifetime is
  the session's. On resume the registered counter is the replay-seeded one, so
  runtime seqs continue past the reconstructed tail.
- New `Holly::emit_for_session(session, |seq| OutEvent)` mints a fresh seq from
  the session's registered counter and broadcasts the built event. It is the
  **only** way the runtime emits a seq-bearing event; `tool_runner`,
  `ask_user`, `propose_plan`, `script`, `plan_tasks`, and `file_change` all route
  through it instead of reusing the parked `ToolExec` seq. The parked `ToolExec`
  seq is deliberately ignored runtime-side.
- Seq-less `Status` transitions the runtime emits around a parked call go through
  a sibling `Holly::emit_status(session, state)`. The raw outbound sender
  (`Holly::events()`) is **removed** — no runtime code can reintroduce the reuse.
- **Supervisor lifecycle errors** mint through the same registry: a *live*
  session (e.g. a saturated channel) has a counter, so the error takes a real,
  ordered seq. An id with **no live session** has no counter, so `next_seq_for`
  returns `0` — a value core never mints, so it can't collide — and heads render
  a `seq == 0` error **unconditionally** (the seq-`0` bypass in the TUI reducer),
  instead of dropping it under `seq > last`.

Race-freedom rests on the parked-turn model (ADR-0061): while a session is parked
on its `ToolExec` batch it mints nothing, so the runtime's `fetch_add` on that
session's counter is uncontended by the session task; the `AtomicU64` covers the
concurrent runtime tasks of a batch. The registry `Mutex` is never held across an
`.await`.

## Consequences

- **Positive.** `(session, seq)` is unique across every authored content event; a
  strict `seq > last` dedupe is now correct, unblocking the WS `serve` head
  (#153). Supervisor errors render. Event authorship is auditable — one counter,
  one mint path, no raw sender. Runtime orchestrators shed their now-dead `seq`
  parameters.
- **Neutral.** `Session.seq` is an `Arc<AtomicU64>` (one allocation per session).
  The supervisor keeps a `HashMap` entry per live session, torn down on exit.
- **Negative.** A seq-bearing event minted for an already-ended session falls
  back to `0`; harmless (no live content stream to collide with, and heads render
  it), but it means seq `0` is not *exclusively* supervisor-authored in that
  degenerate window. Accepted over adding an `Option<u64>` seq everywhere.

## Alternatives considered

- **`Option<u64>` seq on the runtime-authored / supervisor variants (exempt them
  from the contract).** The issue's other sanctioned direction. Rejected:
  `ToolRequest`/`UserQuestion` losing a seq would forfeit ordering against
  surrounding content on reconnect (they'd dedupe by `request_id` only), and
  `Plan`/`TaskList`/`FileChange` are genuine content snapshots that *need* a
  monotonic seq — so a shared counter was required for them regardless. Given
  that, one mechanism for all is simpler than a split. `Error` would also have
  needed a dual `Some`/`None` shape (core mints, supervisor doesn't), widening
  the wire change for no gain over the seq-`0` bypass.
- **Runtime keeps its own per-session seq high-watermark, minting above the max
  it observes.** Rejected: the minted seqs are never fed back to the session's
  counter, so when the session un-parks and mints `session.seq + 1` it collides
  with the runtime's — the exact defect, just moved.
- **Heads bypass dedupe whenever `seq == 0`, leave everything else reusing the
  `ToolExec` seq.** Rejected: fixes only the supervisor-error symptom (#159), not
  the `(session, seq)`-uniqueness defect for approval prompts (#157). The
  seq-`0` bypass is kept, but *only* for the genuinely counter-less supervisor
  path.
