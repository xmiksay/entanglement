# 0179. Lazily-registered built-ins advertise session-scoped, like lazy MCP servers

- Status: Accepted
- Date: 2026-08-06
- Amends: [0163](0163-live-bash-enablement-is-a-tool-overlay-entry.md) (whose
  §2 registration stays exactly as shipped — this ADR scopes only the
  *advertisement* of what §2 registers). Reuses the visibility pattern and
  parent map of [0152](0152-provider-bundled-mcp-servers-three-state-enablement.md)
  / #630. Issue #673 (prompt-cache stability).

## Context

ADR-0163 folded live `bash` enablement into the per-session tool overlay: an
enable entry naming a member of the closed `LAZY_BUILTINS` table registers it
into the process-global `SharedRegistry` on demand. Registration being global
is deliberate — the tool executor, its `JobRegistry`, and the `BashRegistered`
flag are all shared, and a second session enabling `bash` must reuse the same
instance (ADR-0163 §3).

But *visibility* rode along untouched: the runtime `tool_spec_resolver`
filtered only `mcp__*` names per session (`AvailableMcp::spec_visible`, #542,
ancestor walk per #630). Once session A's overlay registered `bash`, the
shared registry's `specs()` included it — and every other live session whose
profile mask admits `bash` (any inherit-all profile) suddenly grew a tool
mid-session. Two distinct harms:

1. **Prompt-cache bust for every live session at once.** The tools array is
   part of the cached request prefix on every wire (explicitly marked on
   Anthropic per #566, implicitly cached on OpenAI-compat, folded into the
   `cachedContents` hash on Gemini per #587). One session's private
   `/enable tool bash` rewrites every other session's prefix bytes.
2. **An un-opted-into tool.** Session B's user never enabled `bash`, yet
   their model now sees it advertised. Dispatch would still grade it through
   the permission ladder, but advertisement itself is the signal the model
   plans around.

## Decision

**A `LAZY_BUILTINS` tool stays registered process-globally but is advertised
per session**: visible to a session iff it was registered at startup
(`ENTANGLEMENT_ENABLE_BASH=1`) or that session's own overlay chain — itself,
or an ancestor, live-resolved with #630's semantics — carries an enable
disposition for it.

Mechanism, mirroring `spec_visible` exactly:

- A new `entanglement-runtime/src/builtin_visibility.rs` owns
  `BuiltinVisibility { startup, enabled: Mutex<HashMap<name, HashSet<SessionId>>> }`.
  `bash_live`'s responder — the same task that performs the lazy
  registration — folds every `OutEvent::ToolOverlayChanged` into it
  (full-list semantics: an enable disposition inserts the session's mark,
  anything else withdraws it; empty sets are dropped, the #561 shape) and
  clears a session's marks on `SessionEnded`.
- The runtime `tool_spec_resolver` closure filters
  `builtins.visible(name, session, &avail)` alongside the existing
  `spec_visible` check. Core's turn-time `advertised()` filter and the
  dispatch-side permission ladder are untouched — advertisement-only, the
  same contract `spec_visible` has (a hallucinated name still fail-closes at
  dispatch).
- The ancestor walk is **shared, not duplicated**: `AvailableMcp` gains
  `pub(crate) enabled_by_or_ancestor(sessions, session)`, extracted from
  `spec_visible`'s tail; both callers use it, so lazy built-ins inherit the
  identical live-resolved, cycle-guarded, `SessionEnded`-cleaned semantics —
  and the runtime keeps exactly two copies of the session parent tree
  (`SpawnGuard`'s and `AvailableMcp`'s), not three.
- Resume-safe for free: core replays `ToolOverlayChanged` on resume, so a
  resumed session's marks re-fold — unlike the lazy MCP enablement map,
  which is never replayed (the #630 hibernation caveat doesn't transfer).

**`McpAdd`/`McpRemove` visibility deliberately stays global.** `/mcp add` is
persisted configuration management (`save_mcp` → `config.yml`, ADR-0097):
after the next restart every session sees the server anyway, and scoping it
live would make live behavior diverge from restart behavior — the opposite of
ADR-0097's "live management ≡ config edit without restart" intent. Its
one-time, deliberate, runtime-wide cache bust is accepted; the session-scoped
tier for MCP already exists (ADR-0152 `allowed` servers).

## Consequences

- **(+)** One session's `/enable tool bash` no longer rewrites any other
  session's advertised tools array — the request prefix of uninvolved
  sessions stays byte-stable, and no session sees a tool it didn't opt into.
- **(+)** The enabling session's own sub-tree keeps inheriting the tool
  (ancestor walk), matching how its overlay *grade* already inherits (#628).
- **(+)** Startup registration via the env var is byte-for-byte unchanged:
  globally visible, no enablement needed.
- **(−)** A second store keyed by session id that must be kept lifecycle-clean
  (`SessionEnded` folding in the `bash_live` responder — a second subscriber
  doing lifecycle bookkeeping beside `mcp::responder`'s).
- **(−)** The overlay entry list is folded per `LAZY_BUILTINS` member on every
  `ToolOverlayChanged`; with one member today this is negligible.

## Alternatives considered

- **Fix in core's `advertised()`.** Rejected: core cannot distinguish a
  startup-registered tool from a lazily-registered one without a new
  runtime→core channel; the resolver seam (ADR-0076) exists precisely so the
  runtime owns per-session advertisement policy.
- **Per-session `ToolRegistry` instances.** Rejected: heavyweight, and breaks
  the shared-executor model (`JobRegistry` reuse, ADR-0163 §3) that fixed
  #616's job orphaning.
- **Fold lazy-builtin enablement into `AvailableMcp.enabled` under a
  pseudo-server name.** Rejected: conflates host built-ins with MCP servers
  in a map whose keys are server names with `mcp__{name}__` derivation; only
  the ancestor-walk tail is genuinely shared, so exactly that is extracted.
- **A third parent map owned by the new store.** Rejected: two copies of the
  session tree is already one more than ideal (#630 documents why the second
  exists); a third would compound the sync burden.
- **Scoping `McpAdd` visibility too.** Rejected per the Decision — persisted
  config must behave the same live as after a restart.

## References

- Issue [#673](https://github.com/xmiksay/entanglement/issues/673)
  (prompt-cache stability umbrella for this and the sibling provider-side
  changes)
- [ADR-0163](0163-live-bash-enablement-is-a-tool-overlay-entry.md): the lazy
  registration this scopes the advertisement of
- [ADR-0152](0152-provider-bundled-mcp-servers-three-state-enablement.md) /
  #630: the session-scoped visibility pattern and parent map this reuses
- [ADR-0149](0149-per-session-tool-overlay.md): the overlay whose entries
  feed the store
- [ADR-0076](0076-per-session-dynamic-tool-specs.md): the resolver seam the
  filter lives on
