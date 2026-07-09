# 0031. Host tool `write`: whole-file create/overwrite (quartet → quintet)

- Status: Accepted
- Date: 2026-07-09

## Context

[ADR-0008][0008] set the shared host-tool design (root containment, bounded
output, the `schema()` seam) and shipped the read-only trio; [ADR-0009][0009]
added the mutating/executing pair `edit` and `bash`, giving the
`host_tools(root)` builder the **root-contained quartet**
(`read`/`glob`/`grep`/`edit`).

The only mutation path today is `edit`'s exact-string replace. Its empty-
`oldString` mode *creates* a file but is **refused if the path exists**
([ADR-0009][0009], deliberately, so the model can't clobber a file it meant to
modify). That leaves no clean way to **regenerate a whole file**: the model is
forced into a brittle mega-`edit` (reproduce the entire old content as
`oldString`) or a delete-then-create dance that doesn't exist. This is a common,
legitimate operation — regenerate a config, rewrite a small module, emit a
generated report — and it deserves a first-class tool.

This ADR **supersedes-by-addition**: it neither edits nor changes ADR-0008/0009.
`edit` stays exactly as specified (surgical replace, create-refused-if-exists);
`write` is the new sibling for whole-file writes.

## Decision

### 1. `write` — whole-file create or overwrite

`WriteTool` takes `{path, content}` and:

- **Create-or-overwrite**, both silent successes. If the target is absent it is
  created; if present its content is fully replaced (truncate + write). Empty
  `content` truncates to a zero-byte file.
- **`mkdir -p` semantics**: missing parent directories are created. Refusing on a
  missing parent would only cost the model a round-trip with no safety gain —
  the directory is within the same root anyway.
- Reuses `resolve_under_root` (`..` escape rejected, lexical containment only —
  [ADR-0008][0008]) and writes **UTF-8 text only** (`content: String`); binary is
  out of scope, same as `edit`.
- **Confirmation output, not an echo**: the result reports what happened —
  `created <path> (N lines)` or `overwrote <path> (N lines, was M)` — and never
  the content. The model already holds the content in its own turn; echoing it
  back wastes context and risks leaking it into logs. Line counts use
  `str::lines()` (a trailing newline is a terminator, not a new empty line), so
  `"a\nb\n"` and `"a\nb"` both count 2 and empty content counts 0.
- **`FileChange` audit** (#41 machinery, like `edit`): the `on_write` callback
  emits full `before`/`after` bytes and the correct `change_kind`
  (`Create` on first write — no `before`; `Edit` on overwrite — prior bytes as
  `before`), so the TUI diff view and audit log cover whole-file writes too.

### 2. Overwrite safety — plain overwrite, gated by the permission profile

`write` lands under the existing wildcard profile defaults with **zero profile
changes** (the [ADR-0009][0009] pattern): `build` → `Allow`, `plan` → `Ask`,
`explore` → `Deny`. Blind overwrite of a file the model never read is the main
hazard; the mitigations are the permission gate (a `plan`/`explore` session
can't silently clobber) and the `FileChange` audit (which preserves `before`, so
nothing is unrecoverable in the log).

### 3. Relation to `edit`

`edit` is unchanged. Its schema description now points regeneration cases at
`write` ("Replacing most of a file? Use `write` instead.") and its
create-refusal error for an existing file hints `write` ("… use `write` to
overwrite it") — mechanical self-correction in the [ADR-0016][0016] style.

### 4. Wiring

`write` joins `host_tools(root)`, making the builder register the **root-
contained quintet** (`read`/`glob`/`grep`/`edit`/`write`). `bash` remains opt-in
([ADR-0010][0010]).

## Consequences

- **(+)** Whole-file regeneration is one call instead of a mega-`edit`; the model
  gets a tool matched to a common operation.
- **(+)** No profile changes: wildcard defaults already give the right
  Allow/Ask/Deny per profile, and `explore`'s read-only allowlist simply never
  includes `write` (relevant to the #116 tool-mask work — nothing to change).
- **(+)** The confirmation-not-echo contract keeps output small and avoids
  re-emitting possibly-sensitive content.
- **(−)** A `build` session can overwrite an unread file with no extra guard
  beyond the profile. Accepted for v1; the audit's `before` makes it
  recoverable. See the rejected stale-write guard below.

## Alternatives considered

- **Stale-write guard (refuse overwrite unless the session previously `read` the
  file).** The `tool_runner` would track read paths per session (plumbing exists
  nearby in the #41 stale-file detection). Rejected for v1 as premature: it adds
  per-session state and a new failure mode (legitimate blind regeneration
  refused) for a hazard the permission profile + audit already bound. **Revisit
  trigger:** the first real incident of a model clobbering unseen content.
- **Refuse on a missing parent directory.** Rejected: costs a round-trip with no
  safety gain — creating a directory within the same root is already allowed.
- **Fold whole-file writes into `edit` (drop the create-if-exists refusal).**
  Rejected: it would erase `edit`'s clobber-protection invariant ([ADR-0009][0009])
  and conflate surgical replace with whole-file rewrite. Distinct operations
  deserve distinct tools with distinct inputs.
- **Echo the written content (or a diff) in the result.** Rejected: the model
  already has the content; echoing wastes context and risks leaking it. Line
  counts are enough to confirm the write landed.

[0008]: 0008-host-tools-workdir-and-bounded-output.md
[0009]: 0009-edit-and-bash-host-tools.md
[0010]: 0010-single-head-crate-and-bash-opt-in.md
[0016]: 0016-host-tool-empty-result-contract.md
