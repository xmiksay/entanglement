# Migration notes — 0.7 → 0.8: sessions & approvals (#921–#925)

> Part of the [0.7 → 0.8 migration notes](../0.7-to-0.8.md) of the [embedding guide](../../../embedding.md).

## Session pause/resume removed (#921)

A live session can no longer be held. `Stop` cancels the turn (ADR-0017) and
`HibernateSession` evicts it; nothing in between. No embedder sent the pause
frames. What an embedder or user has to change:

- **Wire:** `InMsg::PauseSession` (`"kind": "pause_session"`) and
  `InMsg::ResumeSession` (`"kind": "resume_session"`) are gone — a frame with
  either tag is rejected like any unknown message. Send `Stop` to interrupt a
  turn.
- **`AgentState::Paused`** (`"paused"`) is gone; a `match` over `AgentState`
  drops that arm. A `Stop` always settles at `Done`. A session log that
  recorded `Status { state: "paused" }` (only a session someone paused) no
  longer deserializes.
- **Removed field:** `entanglement_core::session::Session::paused`.
- **Deferral:** `SetModel`, `SetGeneration`, `Oneshot`,
  `RevertFiles`, `ForkSession` and `SetWorkdir` still wait for a live turn to
  end, and a `Prompt` sent mid-turn still steers it.
- **TUI (`skutter`):** `/pause [--all]`, `/continue [--all]`, the
  Ctrl+Space toggle and the sessions dialog's `p`/`r` keys are gone. `/pause`
  and `/continue` are free for user slash commands.
- **Config:** the `tui.keybindings` action `pause_toggle` is gone. A
  key map still naming it is rejected as an unknown action (the TUI keeps its
  previous map and warns) — delete the entry.

## Retract/replace question frames removed (#922)

An open `ask_user` question is ended only by an `AnswerQuestion`, a `Stop`
for its session, or the mode's `question_timeout`. Nothing in site, nexial or
skutter sent the frames that withdrew or rewrote one. What an embedder or
user has to change:

- **Wire:** `InMsg::RetractQuestion` (`"kind": "retract_question"`) and
  `InMsg::ReplaceQuestion` (`"kind": "replace_question"`) are gone — a frame
  with either tag is rejected like any unknown message. To withdraw a
  question, answer it (a free-text answer saying so), or `Stop` the turn.
- **Removed items:** `seam::Decision::Retract` and `Decision::Replace`;
  `seam::QuestionDecision::Retract` and `QuestionDecision::Replace`. A
  `match` over either enum drops those arms; a router of your own maps
  only `AnswerQuestion` to a question decision.
- **Heads:** a `UserQuestion` never re-arrives with new content under a
  queued `request_id`; a repeated id is a resume's re-offer of the same
  question — keep the entry (and its drafts), do not queue a second one. The
  `serve` head's ownership gate covers `Approve`/`Reject`/`AnswerQuestion`.
- **Unchanged:** `InMsg::ListQuestions` / `OutEvent::QuestionList` and the
  `ToolOutput` that ends a question on the wire.

## Per-session tool overlay removed (#923)

The session's permission mode is the one thing that decides what a session
may call: `SetMode` (`/mode`, `request_mode`) plus the mode's rules, the
config ceiling, grants and approvals. The per-session tool overlay — a second,
live list of enable/deny entries layered over the mode — is gone. No embedder
sent it; nexial only matched its ack. What an embedder or user has to change:

- **Wire:** `InMsg::SetToolOverlay` (`"kind": "set_tool_overlay"`) and
  `OutEvent::ToolOverlayChanged` (`"kind": "tool_overlay_changed"`) are gone.
  A frame with the `set_tool_overlay` tag is rejected like any unknown
  message; a `match` over `OutEvent` drops the `ToolOverlayChanged` arm (and
  a TypeScript mirror its `tool_overlay_changed` kind).
- **Removed types/items:** `entanglement_core::ToolOverlayEntry` (also
  `entanglement_core::protocol::ToolOverlayEntry`);
  `WireError::OverlayEnable` (a refused wire frame is always
  `WireError::Privileged`); `entanglement_core::session::Session::tool_overlay`;
  `entanglement_runtime::permission::overlay_entry_grade`,
  `overlay_grade_entry` and `overlay_denies`;
  `entanglement_runtime::permission_bash` (module, with
  `resolve_scoped_bash_aware` — a mode's rules split compound `bash` commands
  themselves); `entanglement_runtime::mcp::tool::server_tools_pattern`.
- **Instead of an overlay entry:** to keep a tool away from a session, run it
  in a mode that denies it (a `deny:` rule in `config.yml`'s `modes:`, or a
  stricter built-in mode); to let it run past a mode's deny, switch the
  mode (a deny never parks a card); to stop being asked for a tool the mode
  grades Ask, answer its card with a session-scoped approval.
- **Session logs:** a log that recorded a `set_tool_overlay` frame or its
  `tool_overlay_changed` ack still loads and resumes — both tags are now in
  `RETIRED_IN_MSG_KINDS` / `RETIRED_OUT_EVENT_KINDS` and are skipped as an
  intentional retirement, not a gap. The overlay they carried is not
  restored: a resumed session is graded by its mode alone.
- **TUI (`skutter`):** `/enable` and `/disable` (and the bare-`/enable`
  session-tools checklist) are gone, as are the `/set` dialog's Tools tab, the
  `/mcp` panel's `d` (deny for session) key and its per-server session tag,
  and `/tools`' `Enter` (enable the highlighted row) — `/tools` is a read-only
  browser and no longer shows a `masked`/`usable` column. `/enable` and
  `/disable` are free for user slash commands.
- **CLI:** `skutter inspect session <id>` no longer prints a tool-overlay
  section; `skutter run`'s event rendering no longer prints a
  `# tools enabled:` line.

## Mode run budgets removed (#924)

A turn is bounded by the engine's one `max_turns` (`EngineConfig::max_turns`,
`config.yml` `max_turns:`, default 200) — the mode-level duplicate and its
wall-clock sibling are gone. No built-in mode set either. What changes:

- **Removed fields:** `entanglement_runtime::mode::Limits::max_turns` and
  `Limits::max_duration`. A struct literal drops them; deserializing `Limits`
  from YAML that names either key is an unknown-field error.
- **Removed module:** `entanglement_runtime::run_budget` (the watcher that
  sent `Stop` plus a `session ended: … exceeded` `Error` on a breach).
- **CLI:** `skutter inspect modes` no longer prints the two lines.
- **Instead:** lower `max_turns` in `config.yml`; for a wall-clock bound, send
  `InMsg::Stop` from the embedder's own timer.

## Directory-scoped approvals removed (#925)

An approval lasts `Once`, `Session` or `Always` — nothing else. The
`session_dir` scope, which widened one approval of a `read`/`grep`/`glob`
call to every later call under that call's directory for the session, is
gone, and so is the TUI's `/allow <path>` command that recorded the same
grant by hand. Nexial only folded it into `Session`. What changes:

- **Wire:** `ApprovalScope::SessionDir` (`"scope": "session_dir"`) is gone.
  An `Approve` carrying it is rejected like any unknown value; a card's
  `ToolRequest.scopes` never lists it. A `match` over `ApprovalScope` drops
  the arm (and a TypeScript mirror its `"session_dir"` literal).
- **Removed API:** `GrantStore::grant_session_dir` (an embedder's own
  `GrantStore` drops its override), `FileGrantStore::grant_session_dir` and
  `tool_names::is_read_capability_member`. `FileGrantStore`'s grants are all
  exact-match now.
- **Changed API:** `tool_names::CAPABILITIES` is now a name-only `&[&str]`
  (`read`/`write`/`call`); its read/write membership lists are gone with
  their last reader. Code that iterated it as `(name, members)` tuples reads
  the names directly.
- **TUI (`skutter`):** the approval card's `[d]` key and its
  `allow dir (session)` hint are gone; `/allow` is free for a user slash
  command.
- **Instead:** answer a card `[s]` for the session, or add an `allow:` rule
  for the directory (e.g. `read(src/*)`, `*` spans `/`) to the mode in
  `config.yml`'s `modes:`.
