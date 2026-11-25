# 0021. Hierarchical session data model

- Status: Accepted
- Date: 2026-07-07

## Context

The engine currently supports flat sessions with string-based IDs. Future work requires spawning sub-sessions (sub-agents) that can work in parallel or sequence under a parent session. To support this, we need a hierarchical session model where sessions can have parent-child relationships.

## Decision

### Data Model

- **Session IDs**: New sessions use UUID v4 IDs via `SessionId::new_uuid()`. The `SessionId` type remains `SessionId(pub String)` for backward compatibility with existing logs/tests.
- **Parent Links**: Each `Session` gains a `parent: Option<SessionId>` field. Root sessions have `None`, sub-sessions reference their parent.
- **Tree Walkable**: `SessionStore` provides `children_of(id)` and `root_of(id)` helpers for traversing the session hierarchy.
- **TUI Rendering**: The sessions list renders nested sessions with indentation to visualize the hierarchy.

### Protocol

- `SessionStarted` event already carries `parent: Option<SessionId>` (from PR #42).
- `SessionStarted.root` distinguishes root sessions (`true`) from sub-sessions (`false`).
- The supervisor stores parent links per session for tree traversal.

### Scope (This Phase)

**In:**
- `SessionId::new_uuid()` for UUID v4 generation
- `Session.parent: Option<SessionId>` field
- Supervisor stores parent links
- `SessionStore` tree-walk helpers (`children_of`, `root_of`)
- TUI nested rendering (indented sessions list)
- Forward-compatible log replay (sub-session events in logs rebuild trees correctly)

**Out (Explicitly Deferred):**
- The `task`/`spawn_agent` tool
- Child turn loop implementation
- Subagent profile wiring
- Isolation/permissions for sub-sessions
- Recursion limits
- `apply_diff` re-enable
- Plugin runtime

### Implementation

1. **Core Changes**:
   - Add `uuid = { version = "1", features = ["v4"] }` to `entanglement-core/Cargo.toml`
   - Implement `SessionId::new_uuid()` in `protocol.rs`
   - Add `parent: Option<SessionId>` to `Session` struct in `session.rs`
   - Update `session_loop` to accept and pass `parent` parameter
   - Update supervisor in `holly.rs` to store parent links and generate UUIDs

2. **CLI Changes**:
   - Generate UUIDs for new root sessions in `main.rs`
   - Update `SessionMeta` extraction to include `parent` from `SessionStarted` events
   - Add `children_of()` and `root_of()` helpers in `session_store.rs`
   - Update TUI sessions modal to render nested sessions with indentation
   - Add `parent` field to `SessionView` for TUI rendering

3. **Tests**:
   - `SessionId::new_uuid()` produces unique values
   - Replay of multi-session log builds correct parent/child links
   - `root_of` / `children_of` correct on synthetic 3-level tree
   - Forward-compatibility: log with sub-session events assembles correct tree

## Consequences

### Positive

- Forward-compatible: logs with sub-session events can be replayed and assembled into trees even before spawn is implemented
- TUI can visualize session hierarchy
- Foundation for future spawn/sub-agent work
- UUIDs prevent session ID collisions

### Negative

- Additional complexity in session management
- More state to track (parent links)

### Neutral

- `SessionId` remains `SessionId(pub String)` - no breaking changes to serialization
- Existing tests with hardcoded IDs (`"s1"`, `"run"`) continue to work

## References

- Issue #44: Hierarchical session data model
- PR #42: Session persistence (added `parent` field to `SessionStarted`)
- ADR-0006: Dependency hygiene (verified: `uuid` is not a UI/transport dep)