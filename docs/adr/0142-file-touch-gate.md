# ADR-0142: File-Touch Gate

## Status

Approved and implemented.

## Context

Agents can make blind edits to files they've never read or that have changed since they last read them. This creates safety issues:

- **Data loss**: Agent overwrites files it has never examined
- **Context mismatch**: Agent edits based on stale assumptions after external changes  
- **Race conditions**: User or another agent changes a file, agent proceeds with outdated context

Before this gate, an agent could invoke `edit`, `write`, or `apply_patch` on any file path it had permission to access, regardless of whether it had ever seen the file's contents or whether the file had changed since it last read it.

## Decision

Add a **file-touch gate with modification timestamp tracking** that tracks what files an agent has seen and requires re-reading if files have been modified externally.

### Definition of "Touched"

A file is considered **touched** in a session if:

1. **Read**: The `read` tool was called on the file successfully, capturing its modification timestamp
2. **Previously modified**: `edit`/`write`/`apply_patch` was called on the file in this session
3. **Doesn't exist**: File creation is always allowed (no prior read needed)

### Gate Behavior

For write-eligible tools (`edit`/`write`/`apply_patch`):

1. **Check if file exists**: If not, allow (creation)
2. **Check if touched**: If not touched, reject with *"File `{path}` was not read in this session. Read the file first to understand its current state before modifying it."*
3. **Check timestamp**: If touched but file modification time differs from last known timestamp, reject with *"File `{path}` has changed since it was last read in this session (by user or another agent). Re-read the file to see its current state before modifying it."*
4. **Allow**: Proceed with the tool call

### Implementation Architecture

The gate is split along the core↔runtime seam, exactly like every other piece
of executor-side per-session state.

#### 1. The data struct — `TouchedFiles` (`entanglement-core/src/session/state.rs`)

A plain, `serde`-capable map from canonical path to modification timestamp:

```rust
pub struct TouchedFiles {
    /// canonical path -> mtime (None = file didn't exist when touched)
    touched: HashMap<String, Option<u64>>,
}
```

Methods: `mark_touched`, `is_touched`, `get_known_mtime`, `matches_current`.
`Session` also carries a `touched_files: TouchedFiles` field (it serializes
with the session), kept as the serializable home for the data — but the live
gate does **not** read or write core's copy (see below).

#### 2. File timestamps — `get_file_mtime` (`entanglement-runtime/src/host/timestamp.rs`)

```rust
pub fn get_file_mtime(path: &Path) -> Result<Option<u64>>
```

Returns the mtime in milliseconds since the Unix epoch, or `Ok(None)` if the
file doesn't exist.

#### 3. The live gate — `touch_gate.rs` + `tool_runner.rs` (`entanglement-runtime`)

The gate logic lives entirely in the runtime:

```rust
pub fn check_touch_gate(call: &ToolCall, touched: &TouchedFiles, root: &Path)
    -> Result<(), TouchGateError>;
pub fn mark_touched(call: &ToolCall, touched: &mut TouchedFiles, root: &Path);
```

**Why the runtime owns the live state, not core.** Core's `session_loop` task
is the sole holder of a `Session`; the runtime's tool executor only ever sees a
`SessionId` (ADR-0001/0002). So the executor keeps its own
`TouchState { root, files: Arc<Mutex<HashMap<SessionId, TouchedFiles>>> }`,
mirroring how it already tracks `active`, `in_flight`, `active_skill`, and the
sandbox caches. `check_touch_gate` runs in `run_and_reply` (after permission
and any approval) and short-circuits with a `ToolResult` on rejection;
`mark_touched` runs there after a successful read/write. The state is dropped
on `SessionEnded`/`SessionHibernated`.

The gate is **inert when no `EscapeRoot` is wired** (every test/default
`spawn_tool_executor` wrapper): without the project root it can't canonicalize
paths, so it no-ops rather than guess. In production (`main.rs`) the root is
always present, so the gate is always active.

### Session Persistence

The *runtime-owned* `TouchState` is in-memory only — it does not survive a
hibernate/resume (a resumed session re-reads files as it works, re-establishing
context). Core's `Session.touched_files` field is retained as a serializable
home for the data and a future persistence hook, but the live gate does not
read or write it today.

### Subagent Behavior

**Each session maintains its own `TouchedFiles` state**. When a subagent is spawned:

- **Does NOT inherit** parent's touched files
- Starts fresh with empty state
- Each agent must explicitly establish context

**Rationale**: Different agents have different purposes; a child shouldn't assume it has the same context as its parent without explicit reads.

### Why Timestamps Instead of Hashing

**Advantages:**
- **Simpler**: No need to read entire file content for hashing
- **Faster**: `fs::metadata()` is much faster than reading and hashing file content
- **Sufficient**: Detects external modifications in practice
- **Standard**: Uses filesystem's built-in modification tracking

**Tradeoffs:**
- **Theoretical edge case**: Content changes that preserve mtime are possible but rare in practice
- **Not cryptographic**: This is not a security boundary, so precise content hashing isn't required

For the use case of detecting when a user or another agent has modified a file, timestamps are perfectly adequate and much simpler.

## Consequences

### Positive

1. **Prevents data loss**: Agents cannot blindly overwrite files they've never examined
2. **Fresh context**: Ensures agents see the latest file state before modifying
3. **Clear error messages**: Agents understand what they need to do (read first, or re-read)
4. **Simple implementation**: Uses standard filesystem metadata, no content hashing needed
5. **Subagent isolation**: Each agent starts with fresh context, reducing incorrect assumptions

### Negative

1. **Additional friction**: Agents must read files before editing them (intended safety trade-off)
2. **Edge case limitation**: Theoretical scenarios where mtime doesn't change but content does (extremely rare in practice)

### Neutral

1. **Per-session state**: Touched files don't persist across sessions, which is appropriate for the safety model
2. **Timestamp-based**: Uses milliseconds since Unix epoch, standard and portable

## Configuration

The gate is enabled by default for all profiles. There is no profile-specific opt-out in the current implementation, as the safety benefits apply universally.

## Testing

Unit tests cover:
- `TouchedFiles` operations (mark, check, match)
- `get_file_mtime()` function correctness
- Gate decision logic (allow creation, reject unread, reject modified, allow after read)

Integration tests cover:
- Edit without read → rejected
- Read then edit → allowed
- Write to new file → allowed
- File changed externally → rejected
- Agent's own edit → allowed

## References

- [ADR-0001](0001-actor-model-abi.md) — actor model / the core↔runtime seam
  that dictates the runtime-owned `TouchState` (not core's `Session`).
- [ADR-0109](0109-escape-root-access-via-approval.md) — the `EscapeRoot` whose
  `root` the gate canonicalizes against; the gate is inert without it.
- [ADR-0006](0006-core-dependency-hygiene-gate.md) — the related "gate before
  you act" pattern (`make tree`).