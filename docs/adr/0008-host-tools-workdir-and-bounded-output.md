# 0008. Host tools: working-directory root + bounded output

- Status: Accepted
- Date: 2026-07-07

## Context

The agent profiles ([ADR-0003][0003]) reference tool names — `read`, `glob`,
`grep`, `edit`, `bash` — but the concrete tools have to live *somewhere* and
*touch the filesystem*. Filesystem and shell I/O is a **runtime** concern, not
an engine one ([ADR-0006][0006]): `entanglement-core` defines the `Tool`
**trait**, and `entanglement-runtime` supplies and executes the
implementations. This ADR settles the shared design of those implementations —
the working-directory containment, the output bounds, and the schema seam — for
the read-only trio (`read`/`glob`/`grep`); `edit` and `bash` extend it in
[ADR-0009][0009].

## Decision

1. **Host tools live in `entanglement-runtime`** under a `host` module, behind a
   `host_tools(root: PathBuf) -> ToolRegistry` builder. They touch only `std`,
   `tokio::fs`, `glob`, and `regex` — none of which core needs, so those deps
   live with the tools in the runtime, keeping core's hygiene gate lean
   ([ADR-0006][0006]).

2. **The `Tool` trait (in core) carries `schema() -> serde_json::Value`**
   (default: a permissive empty-object schema), surfaced via
   `ToolRegistry::specs()` → `ToolSpec`. The model sees a real `input_schema` /
   `parameters` per host tool, not an empty object.

3. **Each host tool is constructed with a working-directory `root`**
   (`PathBuf`). Model-supplied paths resolve against it; paths that escape via
   `..` (or absolute paths outside it) are rejected. Containment is **lexical
   only** — no symlink defense, no canonicalize requirement (which would break
   `edit`'s create-a-new-file path).

4. **Tool output is byte-capped** at `MAX_OUTPUT_BYTES` (32 KiB) on a UTF-8
   boundary, with a `... [truncated: N bytes total]` notice. `read` is
   additionally line-ranged via `offset` (1-based) + `limit` (default 2000).
   `glob` caps at 1000 paths; `grep` at 1000 matches and skips files larger than
   4× the output cap. Bounds keep a minified bundle or huge tree from silently
   consuming the context window.

5. **The empty-result contract** ([ADR-0016][0016]) applies: a host tool must
   not return a silent zero-output when multiple distinguishable states produce
   it. `list_files` returns `FileList { files, matched_dirs, skipped_errors }`;
   per-entry walk errors are `warn!`-logged and counted; `glob` emits an
   actionable hint for the bare-`**` trap.

6. **The runtime registers the trio** rooted at the current working directory,
   so profiles gate something real out of the box. `EngineConfig::default()`
   keeps an **empty** registry — embedders opt in via their own tools.

## Consequences

- **(+)** `explore`/`plan`/`build` gate real read-only capability; the engine is
  useful against a local repo without `bash`/`edit`.
- **(+)** `Tool::schema()` makes structured tool input first-class for any future
  tool.
- **(+)** Filesystem I/O and its deps (`glob`, `regex`) sit in the runtime, so
  core stays pure and reusable ([ADR-0006][0006]).
- **(−)** `glob` is synchronous, so `glob`/`grep` do brief blocking FS work on
  the tokio worker. Accepted for a local-repo agent (bounded walks, output
  caps); a `spawn_blocking` refactor is trivial if a hot path suffers.
- **(−)** Lexical containment is **not** a security boundary — a symlink under
  the root can resolve outside. Documented; a real sandbox is deferred to a
  security-focused ADR (the question sharpens with `bash`, [ADR-0009][0009]).
- **(−)** No `.gitignore` awareness: `glob`/`grep` enumerate everything under the
  root. Deliberate (see alternatives).

## Alternatives considered

- **Host tools in `entanglement-core`.** Rejected: filesystem/shell I/O and a
  fixed tool set do not belong in the pure engine; keeping them in the runtime
  lets embedders supply their own tools ([ADR-0006][0006]).
- **A dedicated `entanglement-tools` crate.** Rejected: the tools are the head's
  concern and share the head's root/config wiring; a third crate adds a boundary
  with no consumer that isn't the runtime.
- **The `ignore` crate** (ripgrep's gitignore-aware walker). Rejected for now:
  heavier, and re-evaluated together with `bash`/`edit` sandboxing. Swapping
  `glob` for `ignore` later is a localized change inside `list_files`.
- **Full canonicalize-based containment.** Rejected: `canonicalize` fails on
  non-existent paths, blocking `edit`'s "create file" path; lexical
  normalization keeps that seam open.

[0003]: 0003-agent-and-permission-profiles.md
[0006]: 0006-core-dependency-hygiene-gate.md
[0009]: 0009-edit-and-bash-host-tools.md
[0016]: 0016-host-tool-empty-result-contract.md
