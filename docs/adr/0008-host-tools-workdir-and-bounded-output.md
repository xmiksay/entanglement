# 0008. Host tools: working-directory root + bounded output

- Status: Accepted
- Date: 2026-07-04

## Context

The agent profiles ([ADR-0003][0003]) reference tool names — `read`, `glob`,
`grep`, `bash`, `edit` — but no concrete host tools existed, so the `build`/
`plan`/`explore` permission rules gated nothing. Two seams were already in
place: `ToolSpec.schema` ([ADR-0007][0007]) and the `Tool` trait. But the
`Tool` trait had **no `schema()` method**, so `ToolRegistry::specs()` advertised
empty-object schemas and the model had no way to pass structured arguments.

This ADR covers the read-only trio (`read`, `glob`, `grep`) — the tools the
`explore` and `plan` profiles gate first. `bash` (timeout, process model) and
`edit` (search/replace vs. full-rewrite semantics) bring their own
hard-to-reverse decisions and ship separately.

## Decision

1. **Host tools live in `entanglement-core`** under a new `host` module, behind
   a `host_tools(root: PathBuf) -> ToolRegistry` builder. They touch only
   `std`, `tokio::fs`, `glob`, and `regex` — none forbidden by
   [ADR-0006][0006] — so they stay hygiene-gate-clean and every head shares
   them with zero extra wiring.

2. **`Tool` gains `schema() -> serde_json::Value`** (default: a permissive
   empty-object schema), and `ToolRegistry::specs()` surfaces it via
   `ToolSpec::with_schema`. The model now sees a real `input_schema` /
   `parameters` per host tool, not an empty object.

3. **Each host tool is constructed with a working-directory `root`**
   (`PathBuf`). Model-supplied paths resolve against it; paths that escape the
   root via `..` (or absolute paths outside it) are rejected. Containment is
   **lexical only** for now — no symlink defense, no canonicalize requirement
   (which would also break reading files that don't yet exist for `edit`
   later).

4. **Tool output is byte-capped** at `MAX_OUTPUT_BYTES` (32 KiB) on a UTF-8
   boundary, with a `... [truncated: N bytes total]` notice. `read` is
   additionally line-ranged via `offset` (1-based) + `limit` (default 2000).
   `glob` caps at 1000 paths; `grep` at 1000 matches and skips files larger
   than 4× the output cap. Bounds exist so a minified bundle or huge tree can't
   silently consume the context window.

5. **Deps added to core: `glob` and `regex`** (both pure-Rust, hygiene-gate
   -clean). `glob` drives `glob` and the file enumeration behind `grep`;
   `regex` drives `grep`.

6. **`skutter` always registers the trio** rooted at the current working
   directory, so profiles gate something real out of the box. `EngineConfig::
   default()` keeps an **empty** registry — embedders opt in via `host_tools`.

## Consequences

- **(+)** `explore`/`plan`/`build` now gate real read-only capability; the
  engine is useful against a local repo without `bash`/`edit`.
- **(+)** `Tool::schema()` makes structured tool input a first-class concern
  for any future tool, not just the built-ins.
- **(−)** `entanglement-core` grows two deps. Both are light and
  hygiene-gate-clean; `make tree` still passes.
- **(−)** `glob` is synchronous, so `glob`/`grep` do brief blocking FS work on
  the tokio worker. Accepted for a local-repo agent (bounded walks, output
  caps); a `spawn_blocking` refactor is trivial if a hot path ever suffers.
- **(−)** Lexical containment is not a security boundary — a symlink under the
  root can still resolve outside. Documented as a known limitation; a real
  sandbox is out of scope until `bash`/`edit` force the question.
- **(−)** No `.gitignore` awareness: `glob`/`grep` enumerate everything under
  the root. Deliberate (see alternatives).

## Alternatives considered

- **The `ignore` crate** (ripgrep's gitignore-aware walker, also provides
  `globset`/regex-dir walking). Rejected for now: heavier, more to learn, and
  we'd re-evaluate it together with `bash`/`edit` sandboxing. Swapping `glob`
  for `ignore` later is a localized change inside `list_files`.
- **Host tools in a separate crate** (`entanglement-tools`). Rejected: they
  carry no transport deps, and keeping them in core lets every head share them
  with zero extra wiring and no new crate-boundary decision per tool.
- **Per-tool process isolation / sandboxing.** Rejected as overkill for a
  read-only trio; deferred to when `bash`/`edit` (which mutate/exec) land.
- **Full canonicalize-based containment.** Rejected: `canonicalize` fails on
  non-existent paths, which would block `edit`'s "create file" path later;
  lexical normalization keeps that future seam open.

[0003]: 0003-agent-and-permission-profiles.md
[0006]: 0006-core-dependency-hygiene-gate.md
[0007]: 0007-streaming-llm-and-provider-crate.md
