# 0178. Single-pass `.gitignore`-pruned walk with a total scan budget

- Status: Accepted
- Date: 2026-08-08
- Supersedes the *mechanism* of
  [ADR-0170](0170-gitignore-aware-glob-grep-walk.md) (the two-pass
  allowed-set design and its "swap the walk engine" rejection); the
  `.gitignore` *semantics* ADR-0170 decided are unchanged. Issue #678.

## Context

ADR-0170 made `list_files` (the shared engine behind `glob`/`grep` and the
TUI's `@file` index) `.gitignore`-aware by adding a **second, independent
walk**: `ignore::WalkBuilder` eagerly enumerated every allowed path under
`root` into a `HashSet`, and the unchanged `glob`-crate walk then dropped any
candidate not in that set.

That shape made the walk **unbounded**, in three compounding ways:

1. The allowed-set walk visits the entire tree up front — no cap of any kind —
   before the first candidate is even considered.
2. The gitignore drop in the glob walk ran *before* anything counted toward
   `MAX_RESULTS`, so the old de-facto bound — a huge ignored tree
   (`target/`, `node_modules/`) tripping the 1000-file cap early — was gone:
   the glob walk now had to enumerate everything, ignored trees included,
   with a `canonicalize()` + `metadata()` syscall pair per entry.
3. The `glob` crate's `**` recursion has **no symlink-loop detection**, so a
   loop under `root` made the walk genuinely infinite.

The TUI built its `@file` completion index through this walk synchronously on
the main thread, after `EnterAlternateScreen` and before the first draw —
launching `skutter tui` from a large working directory froze on a black,
input-dead screen (#678). The same cost hit every `glob`/`grep` call in the
tool executor.

ADR-0170 had considered and rejected a single-pass replacement because the
`glob` crate's iterator and its `Pattern` matcher disagree on two points:
bare-`**` semantics (the iterator yields directories only — the ADR-0016/
ADR-0150 "bare-`**` trap" — while `Pattern` matches files too), and `Pattern`'s
default options let `*` cross `/` (the iterator matches per-component, so it
never does). Both turn out to be small, local shims, not walk-engine
rewrites.

## Decision

`list_files_with_extra_roots` runs **one traversal**, driven by
`ignore::WalkBuilder`, and matches each visited entry against the
already-compiled `::glob::Pattern`:

- **Pruning, not enumerating.** The walker starts at the deepest
  metachar-free prefix of the (brace-expanded, dir-expanded) pattern and
  refuses to descend into a `.gitignore`d directory — an ignored tree now
  costs *zero* entries, restoring and strengthening the pre-ADR-0170 bound
  while keeping #629's guarantee (ignored trees never burn the result
  budget). ADR-0170's filter configuration carries over verbatim:
  `require_git(false)`, `hidden(false)`, `follow_links(true)` — the latter
  now loop-safe, because the `ignore` crate detects symlink loops. A walk
  rooted **outside** `root` (reachable only via an ADR-0132 durable grant)
  disables the standard filters entirely, preserving ADR-0170's rule that a
  granted directory's own `.gitignore` must not hide what the grant was
  approved to expose. `.git` directories are pruned too (they were always
  dropped unconditionally, ADR-0099 — now they also cost nothing).
- **Iterator-parity matching.** The compiled pattern matches with
  `require_literal_separator: true` (a `*` component never crosses `/`,
  exactly like the per-component iterator) and the default leading-dot
  behavior (dotfiles stay enumerable). A pattern whose final component is a
  bare `**` counts only directory matches — the bare-`**` trap and the
  ADR-0016 hint built on it behave as before. Everything downstream —
  dedup, the `.git` component drop, caller excludes, canonicalizing
  containment, extra-root widening, `out_of_root`/`matched_dirs`/
  `skipped_errors` accounting, `MAX_RESULTS` + `capped` — is unchanged.
- **A total scan budget as the hard backstop.** The walk visits at most
  `MAX_SCANNED` (100 000) entries across all brace alternatives — matching
  or not — then stops, recording `FileList::scan_capped`; `glob`/`grep`
  surface it as a "narrow the pattern" notice alongside the existing cap
  notices. This bounds the one case pruning cannot: a huge tree that simply
  isn't gitignored.

Two narrow semantic deltas of pruning are accepted and recorded in the
deferred-work ledger (neither has a test or a known user): an in-root symlink
that is itself `.gitignore`d no longer has its (possibly extra-root-granted)
target reached — the walker prunes it before the containment check that used
to count or admit it; and a metachar pattern with a trailing slash
(`src*/`) no longer counts matched directories, since the trailing-slash form
was a `glob`-iterator quirk the compiled pattern doesn't reproduce.

## Consequences

- **(+)** The walk is bounded again — by pruning for ignored trees, by loop
  detection for symlink cycles, and by the scan budget for everything else.
  The TUI black screen (#678) and the same stall inside `glob`/`grep` are
  gone; a companion change builds the TUI index off the startup critical
  path entirely.
- **(+)** One walk instead of two: the literal-pattern case ADR-0170
  accepted as a regression (a single known filename paying for a full-tree
  walk) is fixed as a side effect — the walker starts at the pattern's
  literal prefix and, for a fully-literal pattern, visits one entry.
- **(+)** `ignore`'s parallel-free, sorted (`sort_by_file_name`) traversal
  keeps result order deterministic.
- **(−)** Matching is now string-based against the compiled pattern rather
  than the glob iterator's own enumeration; the parity shims
  (literal-separator, dirs-only bare `**`) are load-bearing and pinned by
  unit tests (`list_files_matches_dotfiles_and_star_stays_within_a_component`,
  the pre-existing bare-`**` and cap tests, and new pruning/loop/budget
  tests).
- **(−)** The two semantic deltas above, accepted as ledger rows rather than
  compatibility shims.
