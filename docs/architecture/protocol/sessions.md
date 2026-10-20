# Wire protocol §2 — Trust split, session lifecycle & single-shot ops

> Part of the [wire protocol module](../protocol.md) of the [architecture overview](../../architecture.md).

**Trusted/untrusted frame split** (#155, [ADR-0069](../../adr/0069-trusted-untrusted-wire-frame-split.md)).
`InMsg` has two entry points. `Holly::send` is **privileged in-process**: an
embedder holding a `Holly` (a head, the runtime tool executor) authors any
frame. `Holly::send_from_wire` is the **untrusted** path a wire head (stdio
`pipe`, WebSocket `serve`) calls after deserializing a line — it enforces the
`InMsg::wire_allowed()` allowlist and refuses (`WireError::Privileged`, not
routed) the trusted-only variants: `ToolResult` (a forged one resolves
a parked turn on `request_id` alone, bypassing execution *and* permission),
`Spawn` (bypasses the tool path's `spawn_refusal` gate, #119), `Resume`
(internal, `#[serde(skip)]`), `HibernateSession` (an embedder memory-eviction
control — a wire head must not evict another session's in-memory state, #318),
`McpAdd`/`McpRemove` (#472,
[ADR-0124](../../adr/0124-wire-refused-mcp-mutation-and-stdio-key-scrub.md),
reversing #375's wire tier: an unapproved `McpAdd` spawns an arbitrary local
subprocess, and with the `serve` origin gate opt-in-off a hostile web page
could drive it cross-origin — the read-only `McpList` stays wire-allowed, and
the TUI `/mcp` path is unaffected since it sends over the privileged
`Holly::send`), `McpAuth`
([ADR-0153](../../adr/0153-mcp-server-oauth.md), sharpening ADR-0124: a forged
`Connect` opens a browser and mints a durable credential, a forged
`Disconnect` destroys one, and even `Check` mutates state by refreshing — so
unlike the read-only `McpList`, none of the three actions is wire-allowed),
`SetMode` ([ADR-0207](../../adr/0207-permission-modes-replace-agent-borne-authority.md):
the mode *is* the session's authority, so a forged one widens every later
tool grade — e.g. into `auto` — with no approval; the model's own way to
widen is the force-parked `request_mode` tool, and the TUI `/mode` sends over
`Holly::send`),
`RevertFiles` (#731, [ADR-0212](../../adr/0212-undo-rewind-via-per-write-file-backups.md):
it rewrites and deletes files in the user's working tree from the session's
backups), `ForkSession` (#786, [ADR-0223](../../adr/0223-non-compacting-session-fork.md):
it mints a session, so it stays a trusted head's call like `Spawn`), `SetWorkdir` (#764,
[ADR-0224](../../adr/0224-session-working-directory.md): its `approve` records an
escape-root grant), and, for an **enable** entry, `SetToolOverlay` (#539,
[ADR-0149](../../adr/0149-per-session-tool-overlay.md), the `McpAdd` rationale
again: it grades a tool `allow` past the session's own mode grade, with no
approval prompt (ADR-0163's lazily-registering-a-closed-table-built-in clause
— #611, folding in the bespoke `BashEnable`/`BashDisable` pair of #498/
[ADR-0133](../../adr/0133-live-bash-enablement-graded-by-permission.md), now
superseded — is itself retired by
[ADR-0195](../../adr/0195-bash-is-the-default-exec-and-curated-read-only-rules.md),
`bash` registering at startup; the grade-only form stands) — the TUI
`/enable`/`/disable` commands send over `Holly::send`). A
**deny-only** `SetToolOverlay` (every entry `deny: true`, including the empty
list) is wire-allowed (#634,
[ADR-0177](../../adr/0177-wire-allowed-deny-only-tool-overlay.md) amending
ADR-0149): it can only withdraw tools the profile already advertises, never
widen the model's surface, so `wire_allowed()` is content-aware for this one
variant — any enable entry present refuses the whole (full-replacement) frame
with the dedicated `WireError::OverlayEnable`, distinct from the blanket
`WireError::Privileged` every other refused variant gets.
`wire_allowed`
is an explicit exhaustive allowlist `match`
(ADR-0124), so a new variant is wire-refused until deliberately opted in — a
compile error to skip, mirroring `session()`/`variant_name()`. The executor
folds a completed tool round-trip back
over the named privileged handle `Holly::submit_tool_result` (used by
`seam::reply_content`, the single fold-back site). Under the local single-user
`serve` scope ([ADR-0048](../../adr/0048-serve-head-local-trust-model.md)) this is
robustness/UX — which cooperating local client owns a frame — not defence against
a remote attacker; the WS head routes every inbound frame through
`send_from_wire` and implements per-connection `Approve` ownership (#402,
[ADR-0107](../../adr/0107-ws-per-connection-approval-ownership.md)).

**Session lifecycle** (✅ #21, [ADR-0028](../../adr/0028-session-lifecycle-enumeration-and-backpressure.md)).
`ListSessions` and `CloseSession` are **supervisor-global**: the supervisor
answers/acts on them directly rather than routing to a session task.
`ListSessions` returns one `SessionList` snapshot of the live
`SessionInfo{session,parent?,predecessor?,agent,root,user?}` set (`predecessor`: a compaction successor's source, ADR-0211; the same record `Holly::session_info` reads synchronously; `parent` is the one field that can change later, on a `ParentChanged`, ADR-0234) — a reconnecting
head enumerates in one round-trip instead of folding the whole broadcast. Both
the query and the reply carry an opaque **`correlation_id`** the head mints and
the reply echoes — not an overloaded `SessionId` (#160, [ADR-0072](../../adr/0072-protocol-warts-settled-before-serve.md)),
so `InMsg::session()`/`OutEvent::session()` return `Option<&SessionId>` and are
`None` for these session-less queries (a head's event router drops a `None`
rather than keying a phantom per-session view). The **`profile_detail`** field
this paragraph used to describe (**#189**, on `AgentChanged`/`SessionInfo`) is
**deleted entirely** — [ADR-0207](../../adr/0207-permission-modes-replace-agent-borne-authority.md)
retired the `mode`/tool-mask/`PermissionProfile` posture it carried along
with `Agent`'s authority fields, so there is nothing left to resolve
into it. A head wanting a session's permission posture reads `ModeChanged`
(the opaque `mode: String`, fired once at session start and again on every
live `SetMode`) and, for the mode's actual rules, `skutter inspect modes
<name>` — there is no wire query for the resolved rule table itself yet.
`CloseSession` drops the session's command
channel so its task exits and emits `SessionEnded` — the explicit destroy `Stop`
(cancel-semantics, ADR-0017) does not perform. It **cascades** over the spawn
sub-tree (**#180**): the supervisor walks the child→parent links and closes every
transitive descendant alongside the target, so a spawned sub-agent is never left
orphaned — running with no consumer for its answers and burning provider tokens.
(This is the explicit-destroy path only; a parent `Stop` still does *not* cascade
to un-polled `agent { background: true }` children, ADR-0026 — collected via `poll`, #605.) Session ids are single-use: after
`SessionEnded`, mint a fresh id — `SessionId::new_uuid()` (kept name for
call-site compatibility; it mints the ADR-0164 `s-<hex>` scheme, not an actual
UUID) or, when a `Holly` handle is in scope, `Holly::next_id(IdKind::Session)`
to go through the engine's *configured* [`IdGen`](../../adr/0164-short-sortable-kind-tagged-ids.md)
— rather than reuse a closed id (which would restart `seq` at 0). The supervisor routes to sessions with a
non-blocking `try_send` + bounded retry, shedding to a saturated session rather
than parking its single loop and stalling every other session.

**Session hibernation** (#318, [ADR-0077](../../adr/0077-session-hibernation-evictable-resumable.md))
is a **third lifecycle state** between `live` and the terminal `closed`
tombstone. `HibernateSession{session}` (trusted-only — an embedder memory-eviction
control, not wire-allowed; `Holly::hibernate` is the wrapper) tears the session
task + its spawn sub-tree down (the same cascade `CloseSession` uses) and drops
each `Context`, but records **no** tombstone in the `closed` set — the map entry is
removed (memory released, gone from `ListSessions`) yet the id stays **resumable**:
a later `Holly::resume(id, records)` rebuilds it from the embedder's event log
exactly like the restart path, re-offering a turn parked mid-approval
([ADR-0061](../../adr/0061-parked-turn-state-batch-tool-resolution.md)/[ADR-0071](../../adr/0071-parked-turn-reoffer-timer.md)).
The task emits a distinct lifecycle `SessionHibernated{session,ts}` (no `seq`) so
heads/persistence taps tell eviction from termination; the runtime executor
releases its per-session bookkeeping on it as on `SessionEnded`. Hibernating a
turn **parked on approval** is safe (re-offer); a turn **mid-stream** is
*stop-then-hibernate* — the supervisor's command-sender drop cancels the round
(ADR-0017 cancel semantics), and its uncommitted text-only tail is discarded
exactly as `Session::replay` drops such a tail, so resume is lossless w.r.t. the
log. `closed` ids stay terminal (`resume` still refuses them); the embedder is
expected to `resume` before re-prompting a hibernated id. Core snapshots nothing —
rebuild is the embedder's log replay, keeping the no-DB-in-core boundary intact.
An **optional idle-TTL sweep** now drives `HibernateSession` automatically
(#363, [ADR-0090](../../adr/0090-idle-ttl-auto-hibernation.md)): `EngineConfig.idle_ttl`
(`None` by default — eviction stays embedder-driven when unset) arms a
supervisor-level poll that auto-hibernates a **settled** root (and its whole
spawn sub-tree) once idle past the TTL — see the engine doc for the mechanism.

**Session pause** (#516, [ADR-0208](../../adr/0208-pause-resume-a-hold-between-cancel-and-hibernate.md))
is a hold `Stop` and `HibernateSession` don't cover: `Stop` destroys the
in-flight round, `HibernateSession` evicts memory — neither is "hold this
session's next piece of work without losing it or evicting it."
`PauseSession{session}`/`ResumeSession{session}` (both wire-allowed, same
trust tier as `Stop`) drive a `Session.paused: bool` that is **not**
persisted/replayed (like `Stop`'s cancel — a hibernate/resume cycle always
comes back unpaused). Two holds depending on what the session was doing when
paused: an **idle** session defers its next `Prompt`/`SetModel`/
`SetGeneration`/`Oneshot` onto the existing turn-stash queue (`SetMode` is
the one exception, applied immediately even while paused — see
[engine §5 stop & pause](../engine/session-ops.md)); a **parked**
batch keeps folding arriving `ToolResult`s into `Context` as normal (stashing
them would deadlock — the stash only drains once the turn goes idle, which
needs every pending result resolved first) but does not re-enter `drive_turn`
once the batch drains, so the same round resumes with no new prompt once
`ResumeSession` arrives. A session **mid-stream** when paused is unaffected
until the round reaches its next safe point (turn end or park) — `Pause`/
`Unpause` are ordinary `SessionCmd`s, so a mid-stream arrival rides the exact
generic stash-and-replay mechanism `SetModel` already uses
(`session/stream.rs` needed no change). `Stop`/`HibernateSession` always take
priority over a pause and neither clears it: a `Stop`'d-but-still-paused
session reports `AgentState::Paused`, not `Done`, until an explicit
`ResumeSession`. A new `AgentState::Paused` (ADR-0139's dedicated-state
precedent) is never observed while genuinely streaming, for the reason above.

**Late-subscriber history fetch** (#160, [ADR-0072](../../adr/0072-protocol-warts-settled-before-serve.md)).
A head that connected after a turn started asks
`ReplayFrom{session,correlation_id,after_seq}` for the events it missed. Because
the event log is the **runtime's** persistence seam (core holds no log), this is
answered *out-of-core*: a runtime history responder (spawned beside the
persistence subscriber, `history.rs`) reads it off the inbound fan-out — like the
supervisor answers `ListSessions`, just runtime-side — and broadcasts one
`History{correlation_id,session,events}` snapshot of every persisted content event
whose `seq` exceeds `after_seq` (via the seq-less `Holly::emit_history`, keeping
the raw sender closed). The query and reply are transient — neither is persisted
nor folded on replay. Delivery is a `correlation_id`-matched broadcast; sending
the reply to only the requesting socket is the WS `serve` head's concern (#153).

- **Session-multiplexed** like the `agent` reference's `task_id`: one connection
  routes many sessions by `SessionId`.
- **Monotonic `seq`** on content events so a head can dedupe against replayed
  history (`agent`'s pattern); lifecycle/query frames (`Status`, `AgentChanged`,
  `SessionList`, `History`, …) carry no `seq`. `OutEvent::seq()` returns
  `Option<u64>` — `None` for those — so the real seq-`0` sentinel below is a
  distinct `Some(0)`, not confused with "no seq" (#160, [ADR-0072](../../adr/0072-protocol-warts-settled-before-serve.md)).
- **`(session, seq)` is unique across every authored content event** (#157). The
  seq comes from **one per-session counter** (`Arc<AtomicU64>`), shared by the
  core session task and the runtime through a supervisor-held registry: a session
  task registers its counter on start / retires it on exit, and a runtime service
  authoring an event for a *parked* session — an approval `ToolRequest`/
  `UserQuestion`, a `Plan`/`TaskList` snapshot, a `FileChange` — mints a **fresh**
  seq from that same counter via `Holly::emit_for_session` instead of reusing the
  parked `ToolExec` seq (the pre-#157 defect that split authorship across crates
  and made a strict `seq > last` dedupe drop every approval prompt). The seq-less
  `Status` transitions the runtime emits around a parked call go through
  `Holly::emit_status`; the raw outbound sender is no longer exposed.
  - **A retired session keeps a seq high-water** (#884,
    [ADR-0239](../../adr/0239-retired-session-keeps-a-seq-high-water.md)).
    Retiring a session (hibernate, end or close) keeps its counter's last value
    in the registry's bounded `retired` map. Detached work still running for the
    id (a background script's card, ADR-0237) mints above every seq already
    used, and a resume (or re-spawn) seeds its replay-seeded counter above the
    high-water with `fetch_max`, so no seq is reused even when the log missed
    those events. `Holly::is_live` reads only the live counters, so a
    high-water never makes a retired id live. The map holds 4096 ids and
    evicts the one touched longest ago (exact reclamation is #891). A task
    retires its *own* counter (`Arc::ptr_eq`), so a resume that registers
    before the old task's teardown finishes stays live and continues above it
    (ADR-0239 §7; the old task's own late mints are #892).
  - **Seq `0` is the sentinel for an id with no sequence**: an `Error` the
    supervisor emits for an id that **never started** (a refused spawn, an
    `UnknownSession::Refuse` of an unknown id), or one whose high-water was
    evicted (#891), has no counter or high-water to draw from, so it carries
    `seq == 0` — a value no counter mints, so it can't collide with content —
    and a head renders it **unconditionally** (the seq-`0` bypass) rather than
    dropping it under a `seq > last` dedupe (ex-#159, the reason
    supervisor-shed errors were invisible in the TUI). No entry is created for
    such an id. A supervisor error for a live or retired session (a saturated
    channel, a `Prompt` racing behind a close) mints a real seq and takes its
    ordered place in that stream.

**Single-shot session ops — `InMsg::Oneshot`** (#324, [ADR-0082](../../adr/0082-single-shot-session-ops-and-persisted-compaction.md)).
A generic **wire envelope** — `{session, op: String, args: Value}` — for a single
out-of-band LLM call outside the turn loop, not a plugin registry: the
genericity lives in the wire shape, so a future op needs no new `InMsg`
variant/`wire_allowed`/`SessionCmd`, just a new `match` arm in
`session::ops::run_oneshot`. `"compact"` (session compaction via LLM
summarization) is the first and only op today; an unknown `op` is a
recoverable `Error`. Wire-allowed (mutates only the caller's own session) and
deferred while a turn is live via the same stash gate as `SetModel`
(unlike `SetMode`, which applies immediately, #560)
— a oneshot never runs concurrently with a turn, which is what lets it reuse
the session's `&mut Llm` handle directly instead of racing the turn loop's
inbox `select!`. On success it emits the **persisted, seq-bearing**
`OutEvent::Compacted{session,seq,summary,kept,auto,mode}` — persistence and
`ReplayFrom` history cover it for free (both are variant-agnostic over any
`seq()`-bearing event). **Every compaction forks a successor and retires this
session** ([ADR-0205](../../adr/0205-every-compaction-forks-a-successor-session.md),
generalizing ADR-0101/0110): the source's `Context` is **never** mutated on any
path, so this event is always a *report*, and `Session::replay`'s `Compacted`
fold is a **no-op** in every case — including legacy `auto: true` records, whose
in-place mutation is deliberately not replayed, because the log still holds the
history the retired source actually had.

The engine mints the successor itself (`session/fork.rs`): it sends
`InMsg::Spawn { parent: None, predecessor: Some(source), agent: <source
profile>, prompt: <summary> }` followed by `InMsg::CloseSession { source }`.
**A head learns the successor's id from that successor's own
`SessionStarted { predecessor: Some(source), .. }`**, not from a field here —
the lineage edge already existed (ADR-0110) and is the one authoritative
statement of the relationship. A head tracking a single session rebinds to the
successor on that event, since the turn and its `Done` land there; a head that
just relays (`pipe`, `serve`) needs no change at all. A `ToolResult` still
addressed to the retired source is redirected to its successor by the
supervisor rather than refused as a closed id; `Holly::successor_of` reads the
same source → successor chain synchronously, and `Holly::predecessors_of`
walks it backwards, nearest first ([ADR-0242](../../adr/0242-compaction-heals-walk-the-whole-chain.md)).

**Sub-agents survive the fork** ([ADR-0234](../../adr/0234-live-sub-agents-survive-compaction.md)).
While handling the successor's `Spawn`, before the source's `CloseSession` is
dequeued, the supervisor moves the source's live direct children under the
successor and announces each with `ParentChanged{session,parent}`, so the close
cascade ends only the source. A sub-agent's own successor starts under that
sub-agent's parent (`SessionStarted{parent: Some(p), predecessor: Some(source),
root: false}`), so it needs no extra event. `/fork` moves nothing.

`kept` (#397, ADR-0102) is how many trailing messages ride verbatim inside
`summary` rather than being paraphrased — clamped to the nearest safe turn
boundary by `Context::safe_kept`; `0` (the default) means the whole history was
summarized with no verbatim tail, matching every pre-#397 record.

`auto` and `mode` (`#[serde(default)]`, so every older record deserializes
unchanged) name which path ran: `auto: false` + `mode: summary` is the manual
`/compact` op; `auto: true` + `mode: summary` is auto-summarize on context
overflow (#398, [ADR-0103](../../adr/0103-auto-summarize-on-context-overflow.md));
`auto: true` + `mode: prune` is the prune-only fallback, which has no LLM
summary — `summary` then carries the placeholder-pruned transcript that seeds
the successor. That last case is new wire surface: ADR-0121 had the prune stay
silent, which ADR-0205 retires, because a head cannot follow a session id
change it is never told about.

---

Previous: [§2 — Message types, questions & queries](messages.md) · Next: [§2 — Live generation, metadata & tool overlay](live-settings.md)
