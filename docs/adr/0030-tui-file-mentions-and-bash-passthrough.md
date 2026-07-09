# 0030. TUI `@file` mention completion + `!bash` passthrough

- Status: Accepted
- Date: 2026-07-09

## Context

The opencode-parity backlog (issue #15) lists two input-line conveniences that
are usable head-side today, without the engine work the rest of that issue needs:

1. **`@file` references** — a fuzzy file finder over the working directory so a
   user can drop a path into a prompt (`explain @src/tui/app.rs`) instead of
   typing it out and mistyping it.
2. **`!bash` passthrough** — run a shell command from the input line and see its
   output inline, the shell-escape convention.

Both are TUI-only: they need the working directory (which the `Tui` head already
resolves as `cwd`) but no new protocol message, no engine round-trip, and no
change to the turn loop.

## Decision

Both features live entirely in `entanglement-runtime`'s TUI head. The event loop
passes `cwd` and the `ENTANGLEMENT_ENABLE_BASH` flag into `tui()`, which forwards
them to `App::init_head_context`.

### `@file` mention completion (`tui::mention`)

- **Index.** `FileIndex::build(root)` reuses the `glob` host tool's enumeration
  (`host::list_files(root, "**/*")`, bounded by that walk's file cap), normalizes
  to `/`-separated relative paths, and drops noisy non-hidden trees (`target`,
  `node_modules`, `dist`, `build`, `.venv`, and `.git`). Built **once at startup**
  — a flat snapshot, not a live watch. Stale entries after create/delete are an
  accepted first-cut limitation (rebuild-on-change is future work).
- **Trigger.** A pure `active_mention_query` scans the current input line left of
  the cursor: an `@` at a word boundary (start-of-line or after whitespace, so
  `user@host` never triggers) with no whitespace after it opens the popup. This
  keeps detection unit-testable without a terminal.
- **Popup.** `MentionPopup` mirrors the existing `CommandPalette` shape
  (persistent selection across frames, `query`/`matches` recomputed on each input
  change) and renders like the slash autocomplete, anchored above the input.
  Ranking is a small subsequence fuzzy score favoring basename and consecutive-run
  matches, shorter paths breaking ties.
- **Accept.** Tab/Enter replaces the `@query` byte range on the cursor's line with
  `@path ` (the `@` is kept, opencode-style, so the reference is legible in the
  sent prompt); Esc dismisses. The path is plain prompt text — the model reads the
  file via the normal `read` tool. No pre-expansion of file contents.

### `!bash` passthrough

- On Enter, an input whose first char is `!` (after the slash-command check) is
  **not** sent to the engine. The remainder runs head-side via the existing
  `host::bash::BashTool` (same `sh -c`, timeout, and truncation), and the command
  + captured output are recorded in the transcript as a `ToolCall`/`ToolOutput`
  pair labelled `!bash`, so it renders like any other tool run.
- **Gated on `ENTANGLEMENT_ENABLE_BASH=1`**, the same opt-in as the model-facing
  `bash` tool (ADR-0010): it runs unsandboxed with full privileges, so it must not
  be reachable by default. When disabled, a one-line hint is recorded instead of
  executing anything.
- **Local only.** Output is displayed, not fed back into the model's context.
  Injecting it as a real tool result requires an out-of-band `ToolResult` path the
  engine doesn't have yet (tracked in #15); that is deliberately deferred.

## Consequences

- **Positive:** two high-value opencode-parity conveniences ship with zero
  protocol or engine change; the token-detection and fuzzy-ranking logic is pure
  and unit-tested; both reuse existing head machinery (`list_files`, `BashTool`,
  the transcript entries, the autocomplete popup pattern).
- **Negative / neutral:** the file index is a startup snapshot (can go stale);
  `!bash` output is not visible to the model; cursor columns are treated as byte
  offsets, matching the input box's existing ASCII-oriented convention.

## Alternatives considered

- **Expand `@file` into the file's contents before sending.** Rejected for this
  cut: it duplicates the `read` tool, bloats the prompt, and needs size/encoding
  policy. Inserting the path lets the model choose to read it.
- **Send `!bash` output to the engine as a tool result** so the assistant sees it.
  Needs engine support for out-of-band tool-result injection (no matching `InMsg`
  today); kept in the backlog. The head-side shell escape is the useful, shippable
  subset.
- **A live-watched file index.** More correct across create/delete, but a
  filesystem watcher is disproportionate for a first cut; a startup snapshot
  covers the common case.

## References

- Issue #15: backlog — `@file` refs, `!bash` passthrough, LSP, MCP, share, …
- [ADR-0010](0010-single-head-crate-and-bash-opt-in.md): single head crate; `bash` opt-in (the gating this mirrors)
- [ADR-0011](0011-tui-head-ratatui-crossterm.md): TUI head
- [ADR-0016](0016-host-tool-empty-result-contract.md): `glob`/`list_files` enumeration the index reuses
