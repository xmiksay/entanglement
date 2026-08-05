# 0170. `glob`/`grep`'s shared walk is `.gitignore`-aware

- Status: Accepted
- Date: 2026-08-06
- Supersedes: the "No `.gitignore` awareness" consequence of
  [ADR-0008](0008-host-tools-workdir-and-bounded-output.md) (re-deferred by
  [ADR-0016](0016-host-tool-empty-result-contract.md) and
  [ADR-0150](0150-search-tool-cli-ergonomics.md)). Issue #629 (ledger row 9,
  part of #624).

## Context

`list_files` (`entanglement-runtime/src/host/walk.rs`), the shared engine
behind the `glob`/`grep` host tools, enumerated every entry under the working
directory with no notion of `.gitignore`. On a typical repo that means
`target/`, `node_modules/`, `dist/`, and similar build/dependency trees are
walked and matched like any other file: they consume the
[`MAX_RESULTS`]-file walk budget (ADR-0008/ADR-0150) before an agent's own
`exclude` pattern ever runs, and — when the model doesn't think to pass one —
they pollute `glob`/`grep` results with noise nobody wants searched.

ADR-0008 named the seam and rejected the `ignore` crate "for now", noting a
full swap would be "a localized change inside `list_files`." ADR-0150 later
made the *cap firing* itself visible, but that only reports the budget being
wasted — it doesn't stop `target/` from wasting it.

## Decision

`list_files_with_extra_roots` builds a second, independent path set —
`gitignore_allowed_paths` — via `ignore::WalkBuilder` rooted at the same
`root`, and drops any glob-matched candidate that isn't in it, the same
unconditional way the existing `.git`-path-component filter already works
(before either the `matched_dirs` or `skipped_errors` count, so a
`.gitignore`d subtree looks to the caller like it was never in the walk at
all — same contract `excludes` already gets).

This is additive, not a replacement of the `glob`-crate-driven enumeration:
that walk keeps deciding *which paths are candidates* (brace sets, the
metachar-free directory auto-expansion, the bare-`**` trap ADR-0016/ADR-0150
depend on, containment, extra-root widening); the `ignore`-crate walk only
answers "is this candidate `.gitignore`d", checked as one extra membership
test. Two configuration choices on the `ignore` walk matter:

- **`require_git(false)`** — a `.gitignore` is honored even in a directory
  that isn't (yet) a real git repository (no `.git` present). An agent
  scaffolding a fresh project shouldn't see gitignore support silently switch
  off just because `git init` hasn't run yet, and it means this walk never
  needs to search upward past `root` looking for a repo boundary — it stays
  contained to the sandboxed root, matching ADR-0054's spirit.
- **`follow_links(true)`** — mirrors the `glob`-crate walk's own symlink
  behavior, so a symlinked path is represented the same way in both sets. A
  symlink that escapes `root` still reaches (and increments) the existing
  containment check (`FileList::out_of_root`) instead of silently vanishing
  from the `.gitignore` membership check first.

`hidden(false)` keeps the `ignore` crate's own "skip dotfiles" heuristic off —
unrelated to `.gitignore`, and dotfiles were always enumerable before this
change (only `.git` and now `.gitignore` rules filter anything).

Nested `.gitignore` files, `.git/info/exclude`, and the user's global excludes
file (`core.excludesFile`) are honored — that's what makes a second full walk
worth doing instead of hand-parsing just the root `.gitignore`: the `ignore`
crate is ripgrep's own engine, so this gets git's actual layered precedence
for free instead of a partial reimplementation.

## Consequences

- **(+)** `target/`, `node_modules/`, and any other `.gitignore`d tree no
  longer burn the `glob`/`grep` walk budget or show up in results — the
  concrete problem #629 named — without the model needing to pass `exclude`.
- **(+)** Nested `.gitignore`, `.git/info/exclude`, and global excludes are
  all respected, matching what `git status`/ripgrep would show as untracked
  vs. ignored, not just a root-level guess.
- **(+)** Zero risk to the existing `glob`-crate walk's tested quirks (the
  bare-`**` trap, brace expansion, directory auto-expansion, containment,
  extra-root widening) — none of that code path changed; `.gitignore`
  filtering is a pure addition checked once per candidate.
- **(−)** A second full directory walk runs per `list_files_with_extra_roots`
  call, on top of the `glob`-crate enumeration. For the specific trees this
  ADR targets (`target/`, `node_modules/`) the `ignore` walk is cheap — it
  doesn't descend into an ignored directory at all — but a literal,
  non-wildcard `glob` pattern (e.g. a single known filename) now pays for a
  full-tree walk it didn't need before. Accepted: this is a P2/lowest-priority
  fix, correctness for the common case matters more than micro-optimizing the
  narrow-literal-pattern case, and no perf budget exists today for host-tool
  walks to regress against.
- **(−)** One new dependency, `ignore` (pure Rust, runtime-only — rides
  alongside `glob`/`regex` in the host-tools dependency set ADR-0008 already
  carved out; does not touch `entanglement-core`'s hygiene gate).

## Alternatives considered

- **Swap `glob` for `ignore::WalkBuilder` entirely**, walking once instead of
  twice. Rejected: `ignore`'s `WalkBuilder` and the `glob` crate's `Pattern`
  matcher disagree on bare-`**` semantics — `glob::glob("**")` (a real
  filesystem walk) only matches immediate directory entries, the deliberate
  "bare-`**` trap" ADR-0016/ADR-0150 build an actionable hint on top of, while
  `glob::Pattern::new("**").matches(...)` (string matching, the only way to
  reuse a `WalkBuilder`-produced candidate list) matches *any* string,
  including a deeply-nested file. Replacing the walk engine would silently
  invert that trap's behavior and require re-deriving `matched_dirs`/cap/
  containment semantics against a differently-shaped candidate stream — a much
  larger, riskier change for a P2 fix.
- **Parse only the root `.gitignore`** (skip nested files, `.git/info/exclude`,
  global excludes). Simpler and cheaper (no second full walk), but wrong for
  any repo relying on a nested `.gitignore` (common in multi-crate/monorepo
  layouts) or on `.git/info/exclude`/global excludes for local-only ignores —
  and reimplementing gitignore precedence by hand risks getting the
  negation/precedence rules subtly wrong where the `ignore` crate already gets
  them right.
- **Do nothing further** (leave it deferred). Rejected: this is the third ADR
  to re-defer the same named seam (ADR-0008 → ADR-0016 → ADR-0150); the fix
  is genuinely localized once the bare-`**` incompatibility above is worked
  around with an additive second walk instead of a replacement.
