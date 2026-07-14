# 0060. `FileChange` audit is emitted by the executor as `path + kind + hash`

- Status: Accepted
- Date: 2026-07-14
- Makes real the `OutEvent::FileChange` surface introduced (but never wired) alongside the `write` tool in [0031](0031-write-host-tool-whole-file.md); rides the executor round-trip of [0058](0058-mid-turn-prompt-folds-into-live-turn.md)/#58.

## Context

`OutEvent::FileChange` and `FileChangeKind` existed in the protocol, the `edit`
and `write` tools carried `with_on_edit`/`with_on_write` callback hooks, and both
stdio (`run.rs`) and the TUI reducer rendered the event — but nothing ever
emitted it. `main.rs` built the plain `host_tools` registry;
`host_tools_with_callbacks` (the only wiring of the hooks) was `#[allow(dead_code)]`.
A protocol event documented as "emitted after each successful edit" that never
fires is worse than a missing one: heads render a branch that is unreachable, and
readers trust a guarantee the engine does not keep (#202, part of #200).

The dead design also carried a cost had it ever fired: the payload cloned the
whole-file `before`/`after` `Vec<u8>` into the event, which the `broadcast`
outbox then clones **once per subscriber**. A large edit would fan its full
before-and-after contents out to every attached head.

The structural reason the hooks were never wired: a registration-time callback
is set when the registry is built (startup), but the **session id** a change
belongs to only exists at execution time, inside the `tool_runner` executor that
owns the `ToolExec` → `ToolResult` round-trip. The callback could not name the
session, so there was nothing to stamp the event with.

## Decision

**Emit `FileChange` from the tool executor, carrying `path + change_kind + hash`
instead of before/after bytes. Delete the dead callback hooks.**

- The `FileChange` payload becomes `{ session, seq, path, change_kind, hash }`,
  where `hash` is the lowercase hex SHA-256 of the file's **after-content**. The
  audit answers "what path changed, how, and to what content-fingerprint" — the
  wire never carries file bytes, so the fan-out is bounded regardless of edit
  size. A head that wants a diff reads the file itself (it is local — ADR-0047).
- Wiring bridges tool and executor through a task-local capture scope
  (`runtime::file_change`): `edit`/`write` call `file_change::record(path, kind,
  after)` after a successful write; the executor runs the registry call inside
  `file_change::capture_and_emit`, which picks the record up and stamps it with
  the in-flight call's `session`/`seq` before broadcasting. This keeps the
  `Tool::run(&self, input)` signature untouched — no session id threaded through
  every tool — and isolates concurrent edits across sessions (each executor task
  owns its own task-local slot).
- The `edit`/`write` `with_on_edit`/`with_on_write` callback fields and the
  `host_tools_with_callbacks` builder are removed.

`change_kind` stays exact: `edit` reports `Create` for an empty-`oldString`
create and `Edit` otherwise; `write` reports `Create`/`Edit` from whether the
target existed before truncation — the tool knows, so the executor does not have
to re-stat. `ApplyDiff` remains reserved.

### Scope: only the executor's registry path emits

`record` is a no-op outside a capture scope. A tool run **not** through the
executor's registry dispatch — the `rhai` bindings' in-script `edit`/`write`, or
a unit test calling `run` directly — records nothing, so no `FileChange` fires
for it. This is deliberate: the audit tracks changes the *model* drove through a
`ToolExec` round-trip. If script-driven edits later need auditing, wrapping the
`rhai` binding calls in the same scope is the extension point.

## Alternatives rejected

- **Delete the variant + callbacks.** The issue's other option. Rejected because
  a file-change audit is a genuinely useful protocol surface that both heads
  already render; the only thing missing was the emit, which is a few lines once
  the session-id mismatch is resolved.
- **Keep before/after bytes.** Rejected for the per-subscriber clone cost; a
  content hash is enough to detect and correlate a change, and heads can read the
  local file for a diff.
- **Thread a session/sink through `Tool::run`.** Rejected as invasive: every tool
  and the `rhai` bindings would take a context argument for a concern only two
  tools have. The task-local keeps the trait clean.
