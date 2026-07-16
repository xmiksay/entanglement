# 0095. TUI `/set`/`/show` for generation parameters + persist-on-confirmation

- Status: Accepted
- Date: 2026-07-16
- Phase 2 of the model-parameters umbrella (#378), directly on top of
  [0094](0094-reasoning-effort-and-per-profile-generation-persistence.md)
  (Phase 1 — the `InMsg::SetGeneration`/`OutEvent::GenerationChanged` wire,
  `GenerationParams::apply_overrides`, `reasoning_effort`, and the
  `EngineConfig.generation_resolver`/`AgentGenerationStore` persistence seam,
  all already landed). Also mirrors the `/model` picker's persist-on-
  confirmation precedent of
  [0081](0081-per-profile-model-pinning-and-rebind-on-set-agent.md). Issue
  #376.

## Context

0094 landed the full engine-side mechanism — a live, partial generation-knob
change, its session memory, and a persisted-override resolver seam — but no
head actually *drives* it: there was no way to change a knob from the TUI, view
the current effective values, or write a confirmed change to
`agent-generation.yml` (0094 wires the store and the resolver seam, but
nothing calls `AgentGenerationStore::set` yet). Without this issue, the only
way to exercise `SetGeneration` would be hand-crafting the wire frame, and a
change would live only in that session's replay log — never surviving a
restart.

## Decision

**No new wire surface.** Everything here is TUI-side, driving the wire 0094
already shipped.

### `/set <key> <value>` (`tui/commands.rs`, `tui/event_loop.rs`)

`Command::Set`/`Command::Show` join `all_commands()` (palette
discoverability). `parse_command` still only matches the command *name*;
`/set`'s trailing `key value` pair is re-parsed from the raw input text in a
new `commands::parse_set_args`, the same raw-text re-parse pattern
`/compact`'s trailing instructions already established, since
`parse_command` drops everything after the name. Recognised keys:
`temperature` (`f32`), `effort` (`low|medium|high`),
`thinking_budget`/`thinking_budget_tokens` (`u32`),
`max_tokens`/`max_output_tokens` (`u32`) — building a partial
`GenerationParams` with only the named field set. An unknown key or an
unparseable value is a friendly `Err` message rendered as a transcript status
line, never sent to the engine.

`event_loop`'s Enter-arm intercepts `Command::Set`/`Command::Show` before the
generic sync `execute_command` dispatch, the same way it already special-cases
`Command::Compact` (both need `holly` + — for `Set` — the raw trailing text,
neither available to the sync dispatch). `send_set` sends
`InMsg::SetGeneration { overrides, .. }` and records a pending persist (below);
`send_show` sends `InMsg::SetGeneration` with every override field `None` —
0094's merge already treats this as a no-op that still replies with the
current effective params, so `/show` needs no new read event, just no pending
recorded. The command palette (`modal_events.rs`) mirrors both: `Show` sends
the same query directly (no trailing text needed); `Set` has no trailing text
to parse, so picking it from the palette surfaces the usage hint instead of
silently no-op'ing.

### Persist-on-confirmation (`tui/app/generation.rs`, new)

Mirrors `pickers.rs`'s model-pin persistence exactly, substituting
`GenerationParams` for `(provider, model)`:

- `App::record_pending_generation_persist(overrides)` — captures the active
  agent + the just-sent overrides into `pending_generation_persist: Option<(String,
  GenerationParams)>`, on `/set`'s Enter only (never on `/show`, which sends no
  overrides here).
- `App::handle_generation_changed(session, generation)`, wired into
  `App::handle_out_event` on every `OutEvent::GenerationChanged`: renders a
  transcript status line with the current effective params unconditionally
  (this is what `/show` surfaces) and, when the incoming `generation`
  **reflects** the pending overrides — every `Some` field in the pending
  overrides equals the corresponding field in `generation` — commits the write
  via `AgentGenerationStore::set` and clears the pending, appending
  `(persisted)`/`(persist failed)` to the status line. This "reflects" test
  replaces the model pin's exact `(provider, model)` tuple match: a partial
  override has no single expected value to compare against, only the fields it
  named.
- `App::clear_pending_generation_persist_on_error` clears the pending without
  writing on `OutEvent::Error` for the active session.
- A `GenerationChanged` with **no** pending recorded — a `/show` query, or
  0094's own `SetAgent`/session-start generation overlay reapplying a
  profile's remembered or persisted choice — is rendered but never mistaken
  for a `/set` confirmation, since nothing is pending to match against.

`App::set_agent_generation` installs the `Arc<Mutex<AgentGenerationStore>>`
handle (loaded once in `main.rs`, alongside the existing `AgentModelStore`
load) the same way `App::set_agent_models` does; `tui()`'s signature gains the
matching parameter, threaded from both `main.rs` call sites.

### File hygiene

Adding `Set`/`Show` (plus their doc comments and `parse_set_args`) pushed
`tui/commands.rs` past the 400-line cap. `CommandPalette` — a UI widget built
*over* the command set, not part of defining or parsing commands — is a
natural seam: it moves to a new sibling `tui/command_palette.rs`,
re-exported from `commands.rs` at its historical path so no call site
(`app.rs`, `app/state.rs`, `app/construct.rs`) needs to change its import.

## Consequences

- `/show`'s reuse of `SetGeneration`/`GenerationChanged` as a query keeps the
  wire from growing a parallel read-only event pair for what is, mechanically,
  a no-op write — 0094 already made that free.
- The "reflects the pending overrides" match is looser than the model pin's
  exact-tuple match: a `GenerationChanged` that happens to already carry the
  same values `/set` requested (e.g. an interleaved 0094 `SetAgent` overlay
  landing the same values by coincidence) would commit as if it were the
  confirmation. Accepted as a narrow, low-consequence edge case — the
  committed value is correct either way (it's the session's real current
  state), only the *cause* attributed in the status line could be wrong.

## Rejected alternatives

- **A dedicated read-only `ShowGeneration` event.** Adds a second wire pair for
  something a no-override `SetGeneration` already achieves as a side effect —
  0094's design already anticipated this reuse.
- **An exact-tuple match for the persist confirmation** (mirroring the model
  pin verbatim). A partial `/set` override only names some fields, so there is
  no single "expected full value" to compare the confirming event against —
  the reflects-check is the natural generalization, not an arbitrary
  loosening.
