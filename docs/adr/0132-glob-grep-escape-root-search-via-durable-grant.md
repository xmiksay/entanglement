# 0132. `glob`/`grep` reach outside the project root by riding an existing durable `read` grant

- Status: Accepted (amends [0109](0109-escape-root-access-via-approval.md))
- Date: 2026-07-23

## Context

[ADR-0109](0109-escape-root-access-via-approval.md) made root containment
subordinate to an explicit approval for `read`/`edit`/`write`/`apply_patch`
(path) and `bash`/`call` (`workdir`): the first out-of-root access forces an
`Ask`, and on approval the resolved absolute path is recorded in
`ExtraRootStore` at `Once`/`Session`/`Always` scope. `glob`/`grep` were
deliberately excluded — its "Negative / accepted" section reasoned that a
recursive search has no single path to approve ("which external root? the
whole filesystem?"), and deferred the capability "until a concrete need —
reading a specific external file via `read` + approval covers the practical
case." That deferral is tracked as row 4 of the
[deferred-work ledger](../deferred-work-ledger.md) (#396), filed as #482.

In practice the asymmetry is awkward: a user who grants `Always` `read` access
to an external directory (say `/opt/some-lib`) still cannot ask the model to
*discover* what's in it — `glob`/`grep` still silently drop every match
outside root (`host::list_files`'s containment check, ADR-0054/#163), so the
model has to guess exact file names to `read` one at a time. The grant exists;
search just can't see it.

## Decision

**`glob`/`grep` never force their own approval prompt — they only widen their
existing root-containment check to also admit a match that already carries a
durable `read`-tool grant (`Session` or `Always`, never `Once`).** This
answers ADR-0109's "which external root?" question by construction: the
external root is whichever directory (or ancestor of it) a `read` grant
already names — nothing new is approved, search just becomes able to see what
`read` access already covers.

- **`ExtraRootStore::is_durably_allowed_under(tool, path)`** (new,
  `entanglement-runtime/src/extra_roots.rs`) walks `path` and each of its
  ancestors up to the filesystem root, returning `true` as soon as one has a
  durable grant (via the existing `is_durably_allowed`). A grant on
  `/opt/some-lib` therefore covers every path under it — `/opt/some-lib`
  itself and any depth of descendant — without a separate grant per file.
  Built on `is_durably_allowed`, which only ever consults the `always`/
  `session` sets, so a `Once` grant is structurally invisible to it: no
  separate exclusion logic is needed, the exclusion falls out of reusing the
  existing durable-only primitive. Deliberate, not an oversight — see
  "Consequences" below.
- **`host::list_files` gains an `extra_roots: Option<&ExtraRootStore>`
  parameter** (`list_files_with_extra_roots`; the existing 3-arg `list_files`
  becomes a thin `None` wrapper, byte-identical to before). A glob-walked
  match whose canonical path escapes the canonical root is still dropped
  **unless** `is_durably_allowed_under("read", canonical_match)` returns
  `true` — checked against the same symlink-canonicalized target the six
  escape-root tools already key their grants on, so a symlink inside the
  granted directory that points *outside* it resolves to a path with no
  covering grant and is still dropped (canonicalization happens before the
  ancestor walk, so the walk only ever sees the symlink's real target, never
  the granted directory it was reached through).
- **The check is hardcoded to the `"read"` tool**, not the tool actually
  calling `list_files` (`glob`/`grep` have no capability of their own in
  `ExtraRootStore`'s per-tool key space). This is the literal reading of the
  issue's proposed semantics: search rides a `read` grant specifically, so a
  `write`-only grant (nothing already grants bare `write` outside root without
  also implying read access to the same path in practice, but the key space
  keeps them distinct) never enables search either.
- **`GlobTool`/`GrepTool` gain the same `with_extra_roots(Arc<ExtraRootStore>)`
  builder** the other four escape-root tools have, and
  `host_tools_with_extra_roots` wires it identically. No `SessionId` or
  `request_id` is threaded in — unlike the six-tool `resolve_under_root_or_grant`
  path, search never consumes a `Once` token, so it needs none of the
  request-id plumbing #449 added for that.
- **No executor involvement.** `permission::escape_root_target` (the function
  the executor's `EscapeRoot` gate uses to decide whether a call needs a fresh
  `Ask`) is untouched — it still returns `None` for `glob`/`grep`, so a search
  targeting an ungranted external directory just returns an empty (or
  root-only) result, exactly as before. Search is purely a *consumer* of
  grants another call already earned; it never mints one of its own.
- **Root-relative grading is unaffected.** [ADR-0125](0125-permission-arguments-for-path-tools-are-normalized-root-relative.md)'s
  root-relative permission grading is a distinct concern (how a `tool(pattern)`
  rule matches a call's *argument*) from the containment relaxation here (does
  a *match* survive the drop) — this ADR doesn't touch grading.

## Consequences

- **Positive.** The common case — `Always` `read` access to an external
  library or config directory — now supports discovery: the model can `glob`/
  `grep` it instead of guessing paths to `read`. No new UX: the grant that
  already exists (from an earlier `read`/`edit`/`write` approval) is what
  unlocks it.
- **Positive.** Zero new wire surface, zero new prompt. `is_durably_allowed_under`
  is a pure read of existing state; nothing is recorded that wasn't already
  there.
- **Positive.** Least privilege is preserved on two axes: per-tool (only a
  `read` grant counts, matching the issue's proposed semantics) and per-scope
  (`Once` structurally cannot widen a search — see below).
- **Negative / accepted, by design.** A `Once`-scoped `read` grant does **not**
  enable search, even on the exact directory it names. A `Once` token models
  "the user approved reading *this one file*, this one time" — a search over
  the same directory can silently fan out over an unbounded number of matches
  under it with no further confirmation, which would make one single-use
  approval effectively spend itself an unbounded number of times. Requiring a
  durable (`Session`/`Always`) grant before search can ride it keeps a
  single-use approval single-use.
- **Neutral.** Search-widening is bound to the literal string `"read"`, not to
  whichever tool is calling `list_files`. If a future capability model ever
  lets `write`-only access exist without an implied `read`, that grant still
  would not unlock search — consistent with the issue's proposed semantics,
  revisit if that becomes a real case.
- **Neutral.** `is_durably_allowed_under`'s ancestor walk is `O(depth)` per
  non-contained match — acceptable given `list_files`'s existing
  `MAX_RESULTS` (1000) bound on the walk.

## Alternatives considered

- **Force a fresh `ToolRequest` approval per external search root** (the
  ADR-0109 "Negative / accepted" section's own alternative, and the issue's
  explicitly-flagged alternative). Rejected for v1: more explicit, but adds
  prompt fatigue for a capability that already has a natural answer (ride the
  `read` grant) with no new UX, and the executor has no clean way to key a
  *search* result set the way it keys one resolved path.
- **Let any durable grant on any of the six tools (not just `read`) enable
  search.** Rejected: the issue's proposed semantics are specifically "an
  existing **`read`-tool** grant," and widening to any tool would let a
  `write`-only or `bash`-workdir grant leak directory *discovery* the user
  never actually approved for reading.
- **Enumerate the store's full grant set and prefix-match, instead of an
  ancestor walk from each match.** Rejected: `ExtraRootStore` has no public
  enumeration API (by design — its two read methods are point lookups), and
  adding one would widen its surface for a rarely-hot path; the ancestor walk
  reuses the existing `is_durably_allowed` primitive unchanged and is bounded
  by `MAX_RESULTS`.
- **Let `Once` grants enable a single search call.** Rejected: a search's
  result count is unbounded ahead of time, so "single-use" has no coherent
  meaning applied to a whole `glob`/`grep` invocation — see "Consequences."
