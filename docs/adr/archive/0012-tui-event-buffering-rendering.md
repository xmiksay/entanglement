# 0012. TUI event-buffering & rendering model

- Status: Accepted
- Date: 2026-07-05

## Context

The TUI receives events from `Holly::subscribe()` (a `broadcast::Receiver<OutEvent>`) and must render them into a scrollable transcript. Two hard problems:

1. **Replay / multiplex fan-out:** The same event may arrive multiple times (replays from new subscribers, or a fan-out where one sub re-joins). How do we avoid rendering duplicates?
2. **Snapshot vs delta:** Some events are deltas (`TextDelta`), others are snapshots (`Plan`, `TaskList`). How do we render snapshots without duplicating them on every update?
3. **Session multiplexing:** The receiver streams events from all sessions. How does the TUI show only the active session's view?

Both reference projects solve this the same way: track the last-seen sequence number per session and discard events with `seq <= last_seen`. For snapshots, store the latest value and re-render in-place rather than appending.

## Decision

The TUI maintains:

- `App { transcript_by_session: HashMap<SessionId, Vec<TranscriptEntry>>, last_seq_by_session: HashMap<SessionId, u64>, ... }`
- Each `TranscriptEntry` corresponds to an `OutEvent` that adds content (`TextDelta`, `ToolOutput`, `Error`, `Done`).
- For snapshots (`Plan`, `TaskList`, `AgentChanged`), store the latest value separately and render it in a dedicated panel (not appended to the transcript).

On each `OutEvent`:

- If `session != active_session`: skip (store in background).
- If the event has `seq` (all content events): if `seq <= last_seq`, drop. Otherwise, increment `last_seq`.
- For deltas: append a new `TranscriptEntry`.
- For snapshots: replace the stored value and mark the panel dirty.

The event loop bridges crossterm and tokio:

```rust
// Spawn a crossterm poller task
let (key_tx, mut key_rx) = mpsc::channel(64);
tokio::spawn(poll_crossterm(key_tx));

// Main loop
loop {
    tokio::select! {
        Some(event) = sub.recv() => handle_out_event(event),
        Some(key) = key_rx.recv() => handle_key(key),
    }
    if dirty { terminal.draw(|f| app.render(f)); }
}
```

## Consequences

- **(+)** Replayed events don't corrupt the display (seq-dedup).
- **(+)** Snapshots don't create duplicate visual artifacts (re-render in place).
- **(+)** Multi-session support is straightforward: store transcripts per session, filter on the active one.
- **(−)** State lives in the TUI; if the TUI crashes and reconnects, it must reconstruct from re-streamed events (seq-dedup makes this safe).
- **(−)** Storing full transcripts can grow memory-bound for long sessions. A production system would cap history or persist, but that's out of scope for MVP.

## Alternatives considered

- **Ask the engine to dedupe:** Rejected. The broadcast channel already duplicates by design (fan-out requires it). Pushing dedup into the engine would couple it to head concerns.
- **Use a request/response correlation id:** Rejected. A simple monotonic `seq` per session (as in the `agent` and `design` references) is sufficient and lighter.
- **Store snapshots in the transcript too:** Rejected. It would cause visual duplication (plan content appearing repeatedly). Separate snapshot state is cleaner.