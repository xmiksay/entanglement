# 0020. Event-sourced session persistence

- Status: Accepted
- Date: 2026-07-07

## Context

Sessions must survive process restarts. The engine (`entanglement-core`) is stateless and session-scoped, but history is currently transient (held in-memory in `Context`). To enable forensic replay and eventual resume capability, we need a durable event log that survives the process lifetime.

## Decision

### Storage format and layout

Session events are stored as an append-only JSONL (newline-delimited JSON) log. Each line is a single `LogRecord` wrapping either an `InMsg` or `OutEvent` with a timestamp and session ID.

**Layout:**
```
<data_dir>/entanglement/sessions/<safe_cwd>/<root_session_id>.jsonl
```

Where:
- `<data_dir>` = `dirs::data_dir()` (platform-specific: `~/.local/share` on Linux, `~/Library/Application Support` on macOS, `%APPDATA%` on Windows)
- `<safe_cwd>` = current working directory sanitized for filesystem safety
- `<root_session_id>` = the root session's UUID (sub-sessions interleave in the same file)

**Path sanitization (`safe_cwd_name`)**:
- Replace `/` and `\` with `-`
- Trim leading `-`
- Leave all other bytes as-is (including spaces and Unicode)
- Examples: `/mnt/nvme/agent` → `mnt-nvme-agent`, `/a-b` → `a-b`

**Known limitation (accepted):**
This scheme is not collision-proof. Two distinct paths can map to the same folder (e.g., `/a-b` and `/a/b`). Spaces and Unicode characters pass through unchanged. This is acceptable for the common case. A future enhancement can add a hash-suffix disambiguator (`mnt-nvme-agent-a1b2c3`) without breaking reads (fall back to the plain name).

### No index file

Session metadata (id, agent, created timestamp, last active timestamp, parent) is derived from the log itself:
- Read each `.jsonl` file in the sessions directory
- Parse the first line to extract `SessionStarted` event for meta
- Use file mtime as `last_active`
- No separate `index.json` required

### Stored format

Events are stored as protocol events (`InMsg` / `OutEvent`) — already serde-shaped. No new transform layer.

**LogRecord structure:**
```rust
struct LogRecord {
    ts: u64,              // Unix timestamp in milliseconds
    session: SessionId,   // Top-level session ID
    payload: LogPayload,  // Either In(InMsg) or Out(OutEvent)
}
```

Every record carries a top-level `session` id and `ts` for per-session replay and chronological ordering.

### One file per root session tree

Sub-session events interleave in the same file as their root session. The `session` field on each `LogRecord` allows filtering events for any specific session (root or sub) during replay.

### Lifecycle events

Two new `OutEvent` variants:
- `SessionStarted { session, parent: Option<SessionId>, profile, model, root, ts }`
- `SessionEnded { session, ts }`

Emitted by the session loop on spawn and exit. These events are persisted like any other `OutEvent`.

### Replay target

Replay rebuilds the core `Context` (the canonical session representation in `entanglement-core`) from the event log. No separate storage format — the event log is the source of truth.

### Crate boundary

- **`entanglement-core`** — gains lifecycle events and emits them; remains disk-free (no `dirs` dep, no I/O)
- **`entanglement-runtime`** — owns `SessionStore`, implements persistence, reads/writes JSONL
- **`entanglement-provider`** — unchanged (uses existing `LlmRequest` transform)

## Consequences

### Positive

- Sessions survive restarts
- Forensic replay is possible by loading past sessions read-only
- Event log is append-only, simplifying durability
- No transform layer — protocol events are the stored format
- Core crate stays pure (no disk deps)

### Negative

- Path sanitization limitation (accepted)
- One file per root session tree (sub-sessions interleave) — acceptable given `session` field filtering
- No resume capability yet (Phase 2)

### Alternatives considered

1. **SQLite** — Overkill for append-only log; JSONL is simpler and human-readable
2. **Per-session files** — Would scatter files for sub-sessions; root-tree grouping is cleaner
3. **Hash-based paths** — Could add later as enhancement without breaking reads

## Related

- Issue #42
- Issue #41 (audit events are already in the stream, so they persist for free)
- Issue #3 (resume capability — Phase 2)
- Issue #4 (sub-session hierarchy — Phase 2)