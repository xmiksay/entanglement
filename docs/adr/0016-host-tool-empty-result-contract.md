# 0016. Host tools: empty-result contract (no silent zero-output)

- Status: Accepted
- Date: 2026-07-07

## Context

A reproduction of #33 surfaced an adjacent symptom: `glob` invoked with a
bare-`**` pattern (`{"pattern":"**"}`) returned an empty string with no error,
even when run inside a populated repository. The model then concluded "no
files" and either gave up or hallucinated, instead of retrying with `**/*`.

The cause is the interaction between two pieces of the read-only trio
([ADR-0008][0008]):

1. The `glob` crate's `**` token is a *recursive directory* matcher — its
   `Paths` iterator only yields a path when the current component is a
   directory. A bare `**` therefore enumerates directories, not files.
2. `list_files` filtered results through `is_file()` (silently dropping
   directories) **and** swallowed every per-entry error (`Err(_) => continue`).

The same output shape — empty string, no error — also arises from a genuinely
empty directory, a typo'd pattern, a permission-denied walk, and a successful
match of zero files. None of these are distinguishable to the model. The
"silent zero" failure mode is the problem, not the bare-`**` semantics
themselves.

The principle generalizes: **a host tool that returns `""` or `Ok(())` for
multiple distinguishable underlying states is itself buggy**, because the model
cannot self-correct what it cannot distinguish.

## Decision

1. **`list_files` returns `FileList { files, matched_dirs, skipped_errors }`**
   instead of `Vec<PathBuf>`. Callers (today: `glob` and `grep`) see not just
   what matched but *what was filtered out and why*, without re-implementing
   the walk.

2. **Per-entry errors are `tracing::warn!`-logged and counted**, never silently
   swallowed. The count is exposed via `FileList::skipped_errors` so the tool
   can surface a "(N entries skipped, see logs)" note in its output.

3. **`glob` (the path-listing tool) emits an actionable hint** when the result
   would otherwise be empty but the pattern matched something — most importantly
   the bare-`**` case:

   > `pattern \`**\` matched 7 directories but no files (files are filtered
   > out). Try \`**/*\` to list files inside those directories.`

   The suggested pattern (`<original>/*` unless already ending in `/*`) is
   computed by `suggest_files_pattern` and is directly copy-pasteable, so the
   model's retry is mechanical rather than inferential.

4. **`grep` does not emit a hint** — it would be noise in the "no regex match"
   common case, and `grep`'s default file filter is `**/*` (which already lists
   files), so the bare-`**` trap doesn't apply. `grep` just consumes
   `FileList::files` and silently skips files it can't read, same as before.

5. **The contract applies to new host tools going forward**: an empty result
   that can arise from multiple distinguishable causes must surface which one
   applied. A clean no-match (e.g. `glob` against a truly empty dir) may still
   return `""` — that's a single, well-defined state.

## Consequences

- **(+)** The bare-`**` trap (a common model paraphrase of "give me everything")
  becomes self-correcting instead of confusing.
- **(+)** Per-entry errors become visible at `warn!` level rather than
  vanishing; previously a permission-denied walk looked identical to success.
- **(+)** `FileList::matched_anything()` gives callers a clean predicate
  without poking at struct fields.
- **(−)** `list_files` is a `pub fn` and its return type changed from
  `Vec<PathBuf>` to `FileList`. Only `glob` and `grep` call it today (both
  in-tree); embedders with their own `host`-module callers would need a
  one-line `for p in list.files` update. Acceptable for an internal helper.
- **(−)** Slightly more allocation per `glob`/`grep` call (counting dirs we'll
  throw away). Negligible against the `MAX_RESULTS = 1000` walk cap.
- **(−)** The hint is in English prose; a future structured-output model may
  want it as a separate `OutEvent::ToolHint` variant. Deferred.

## Alternatives considered

- **Auto-rewrite bare `**` to `**/*` inside `list_files`.** Rejected: silent
  pattern rewriting hides the user's intent, breaks `grep`'s file-filter
  semantics (a user passing `**` to scope a search would get a different
  search), and doesn't address the broader "silent zero" problem.
- **Treat `glob` returning empty as an `Err`.** Rejected: an empty repository
  is a legitimate empty result, not an error. Conflating empty-with-error
  would force callers to parse error strings.
- **Keep `list_files` returning `Vec<PathBuf>` and have `glob` re-stat each
  result.** Rejected: doubles the FS work and re-introduces the swallow-or-not
  question at a second call site.
- **Add `.gitignore` awareness now.** Out of scope (still deferred per
  [ADR-0008][0008]'s alternatives); the empty-result contract is independent
  of which files the walk considers.

[0008]: 0008-host-tools-workdir-and-bounded-output.md
