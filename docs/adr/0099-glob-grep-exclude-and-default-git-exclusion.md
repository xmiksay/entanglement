# 0099. `glob`/`grep`: default `.git` exclusion + caller `exclude` patterns

- Status: Accepted
- Date: 2026-07-16

## Context

`list_files` (the shared enumeration `glob`/`grep` both walk through,
`entanglement-runtime/src/host/mod.rs`) had no exclude mechanism at all. A
scratch test against the `glob` crate confirmed `glob::glob("<root>/**/*")`
descends into `.git` — a stale comment in `tui/mention.rs` claiming `.git` is
"already skipped by the glob walk" was only true there because `mention.rs`
post-filters through its own `IGNORED_DIRS` list; the `glob`/`grep` *tools*
had no such filter. Every unscoped `glob`/`grep` call therefore surfaced
`.git` internals (loose objects, packed-refs, hooks) in the model's tool
result — noisy at best, and a potential leak vector if a hook or a stale
`config` ever carries something sensitive.

Separately, neither tool had any way to exclude a subtree at all (e.g.
`target/**`, `node_modules/**`) short of a caller structuring `path`/`pattern`
narrowly enough to avoid it, which doesn't compose for "search everything
except X."

## Decision

1. **`list_files` gains an `excludes: &[String]` parameter** — glob patterns
   matched against the root-relative path (`glob::Pattern::matches`,
   confirmed via scratch test to handle `**` mid-pattern, e.g. `target/**`
   matching `target/debug/build.log`). `GlobTool`/`GrepTool` each add an
   optional `exclude: string[]` input field (`#[serde(default)]`, so omitting
   it is unaffected) threaded straight through.

2. **`.git` is excluded unconditionally**, independent of `excludes` — any
   walked entry whose `Path::components()` contains a literal `.git` segment
   is dropped before the containment/metadata checks, so it can't be searched
   even by an explicit `.git/**` pattern and doesn't count toward
   `matched_dirs`/`skipped_errors` either. This is a path-component check, not
   a glob pattern in the `excludes` list — it can't be disabled or
   overridden by any input the model supplies.

3. Excluded entries are dropped **before** the file/dir metadata check, so an
   excluded subtree is invisible to the caller in every respect (not counted,
   not walked further than the `glob` crate's own iteration already did).

## Consequences

- **(+)** `.git` internals no longer leak into `glob`/`grep` results by
  default — no per-call opt-in required, matching what a human would expect
  from a code-search tool.
- **(+)** `exclude` lets a caller narrow a broad search (`**/*` minus
  `target/**`, `node_modules/**`, a generated-output dir) without having to
  enumerate every directory they *do* want.
- **(−)** `.git` exclusion has no override. Accepted: there is no legitimate
  reason for an agent to read `.git` internals directly (as opposed to using
  `bash`/`call` with `git` itself, which stays available), and an override
  knob would be a footgun (a mis-scoped `exclude: []` accidentally re-exposing
  `.git`) for zero real benefit.
- **(neutral)** `mention.rs`'s `@file` completion index now gets its `.git`
  filtering for real from `list_files` itself, instead of coincidentally from
  its own separate `IGNORED_DIRS` post-filter (which still handles
  `target`/`node_modules`/etc. — unchanged).

## Alternatives considered

- **Make the default `.git` exclusion just the default *value* of `exclude`
  (overridable by passing `exclude: []`).** Rejected: no legitimate use case
  for an agent to read `.git` internals, and overridability adds a footgun for
  zero benefit — see Consequences above.
- **Express the default exclusion as a `.git/**` glob pattern instead of a
  path-component check.** Rejected: confirmed via scratch test that
  `glob::Pattern::new(".git/**").matches(".git")` is `false` — a `**`
  glob-pattern default wouldn't catch the bare `.git` directory entry itself,
  only its contents. The component check catches every case (top-level,
  nested, submodule `.git` dirs at any depth) with no pattern compilation
  needed.
