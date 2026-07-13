# 0054. Canonicalizing, symlink-safe host-tool root containment

- Status: Accepted
- Date: 2026-07-13
- Supersedes-by-addition the **lexical-only** containment of point 3 of
  [0008](0008-host-tools-workdir-and-bounded-output.md) (the
  read-only trio) and its inheritance by `edit`/`write`
  ([0009](0009-edit-and-bash-host-tools.md)/[0031](0031-write-host-tool-whole-file.md)).

## Context

[ADR-0008](0008-host-tools-workdir-and-bounded-output.md) rooted
every host tool at a working-directory `root` and rejected model-supplied paths
that escape via `..` or an absolute path outside root. Containment was
**lexical only**: `resolve_under_root` normalized `.`/`..` components and checked
`starts_with(root)`, but never canonicalized — ADR-0008 called this out and
deferred symlink defense to "a security-focused ADR," because canonicalizing the
*full* path would break `edit`/`write`'s create-a-new-file path (a not-yet-
existing target has nothing to canonicalize).

That gap became a real hole once the resolver was inherited by the **write**
primitives (`edit` ADR-0009, `write` ADR-0031) without re-evaluation (#163):

- A symlink inside root (`root/link -> /etc`) passes the lexical
  `starts_with(root)` check, then `write`/`edit` **follow it out of tree** — an
  out-of-tree *write* primitive, strictly worse than the read-only escape
  ADR-0008 reasoned about.
- `glob`/`grep` skipped the resolver **entirely**: `list_files` did
  `root.join(pattern)`, so an absolute or `..` pattern (or a symlinked entry)
  walked outside root unchecked.
- `root` itself was `std::env::current_dir()` with a `"."` fallback, never
  canonicalized — so a symlinked cwd made every subsequent `starts_with(root)`
  comparison unreliable.

## Decision

1. **Canonicalize `root` once at startup.** The head resolves cwd and
   canonicalizes it (`current_dir().and_then(canonicalize)`) before constructing
   the tools, so the containment anchor is a real path. `resolve_under_root`
   *also* canonicalizes the `root` it is handed, defensively — cheap, idempotent,
   and it keeps embedders/tests that pass a non-canonical root correct.

2. **Canonicalize the deepest existing ancestor of the resolved target, and
   require it to stay under the canonical root.** After the existing lexical
   normalization + `starts_with` check, `resolve_under_root` peels
   not-yet-existing tail components off the target until an ancestor
   `canonicalize()`s, checks *that* against the canonical root, then re-appends
   the plain (`..`-free, symlink-free-because-nonexistent) tail. This blocks:
   - a **final-component** symlink (`root/link -> /etc/passwd`) — the whole path
     canonicalizes outside root, so `write`/`edit` refuse it; and
   - a **middle-component** symlinked directory (`root/link -> /etc`,
     target `root/link/x`) — the ancestor `root/link` canonicalizes to `/etc`.
   The create path still works: a genuinely-new nested file re-appends its tail
   past the deepest real ancestor.

3. **Route `glob`/`grep` through the same boundary.** `list_files` canonicalizes
   each matched entry and drops any whose canonical path escapes the canonical
   root, so an absolute/`..` pattern or a symlinked entry can no longer surface
   out-of-tree files.

Symlinks that point **inside** root stay allowed and resolve to their canonical
in-tree target — containment, not a symlink ban.

## Consequences

- **(+)** `edit`/`write` are now a genuinely root-contained write primitive; a
  symlink under root can't redirect a write out of tree. `glob`/`grep` honor the
  same boundary they always implied.
- **(+)** The create-a-new-file path (the reason ADR-0008 skipped canonicalize)
  keeps working — only the *existing ancestor* is canonicalized.
- **(−)** One `canonicalize()` syscall per resolve, and one per `glob`/`grep`
  entry (bounded by `MAX_RESULTS`). Negligible for a local-repo agent.
- **(−)** **Not** TOCTOU-proof: a path that canonicalizes safely could be
  swapped for a symlink before the subsequent `open`. Closing that needs
  `openat2(RESOLVE_BENEATH)`/`O_NOFOLLOW` (an OS sandbox), still deferred — but
  the lexical-plus-symlink boundary is now a real gate, not a comment. The
  unsandboxed `bash`/`call` exec pair remains out of scope (ADR-0009/0010): they
  set only cwd and run with full privileges by design.

## Alternatives considered

- **Canonicalize the full target path.** Rejected: a not-yet-existing
  `edit`/`write` target has no canonical form, which is exactly why ADR-0008
  punted. Canonicalizing the deepest *existing* ancestor gets the symlink
  guarantee without breaking create.
- **`openat2(RESOLVE_BENEATH)` / `O_NOFOLLOW` now.** Rejected for this change:
  Linux-only, and it re-plumbs every tool's file I/O through raw fds. It's the
  right endgame for a TOCTOU-tight sandbox and is noted as future work; this ADR
  closes the lexical-escape hole portably first.
- **Ban symlinks under root outright.** Rejected: symlinks pointing inside root
  are legitimate (repos use them); the boundary is *containment of the resolved
  target*, not the presence of a link.
