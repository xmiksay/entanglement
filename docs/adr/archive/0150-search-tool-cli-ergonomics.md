# 0150 — Search-tool CLI ergonomics: directory auto-expansion, brace sets, and no silent empty results

- Status: accepted (amends [ADR-0016](0016-host-tool-empty-result-contract.md); issue #540)
- Date: 2026-08-01

## Context

Session-log analysis over this repo's own agent sessions (143 logs) showed
**57% of `grep` calls (550/950) and 13% of `glob` calls returned the empty
string**. The models driving the tools are trained on CLI (`grep -r`,
ripgrep) and Claude-Code (`Grep`/`Glob`) semantics; our tools silently
mismatch those expectations, and the zero-match empty string gives the model
no signal to self-correct — observed in-session as retry storms (`grep
{"pattern":"…","path":"entanglement-core/src"}` → empty → bare retry →
matches).

Ranked root causes, from the logs cross-checked against the implementation:

1. **`grep.path` = directory → silent empty (451 of the 550).** The filter is
   a file glob fed to `list_files`; a directory matches only the (filtered
   out) directory entry, so zero files are scanned.
2. **Brace sets `{a,b}` unsupported** — the `glob` crate treats `{`/`}` as
   literals; `**/*.{rs,md}` silently matches nothing.
3. **Zero matches → the empty string, by design** (ADR-0016 point 4 exempted
   `grep` from hints; `glob`'s clean no-match was also empty). An empty
   string becomes an empty `tool_result` content block — indistinguishable
   from a malformed call.
4. **Silent caps** — the 1000-file walk cap and 1000-match grep cap truncate
   with no notice.
5. **Out-of-root matches silently dropped** by containment — a leading-`/`
   typo reads as a clean no-match.
6. **Unknown input fields silently ignored** — a Claude-Code-style
   `{"pattern":"*.rs","path":"src"}` on `glob` (which had no `path` param)
   searched the whole root; `-i`/`output_mode`/`type` were dropped wordlessly.
7. `grep` was always case-sensitive, with only the undocumented inline `(?i)`.

Verified *not* broken: `**` in `grep.path` works; the 2026-07-16
false-negative cluster was the scan-cap bug already fixed by ADR-0091's
follow-up commit.

## Decision

All in the runtime's `host` module — core and the wire protocol are untouched.

1. **Directory auto-expansion in the shared walk** (`walk.rs`): a
   brace-expanded pattern alternative with no glob metachar (`*?[{`) that
   names an existing directory is rewritten to `dir/**/*` before globbing.
   This narrowly revisits ADR-0016's rejected "auto-rewrite the pattern"
   alternative: unlike the rejected bare-`**` rewrite, a directory pattern
   matched **zero files by construction**, so no legitimate use is lost.
   Expansion is **containment-gated** — the directory's canonical path must
   sit under the canonical root or carry a durable `read` grant
   ([ADR-0132](0132-glob-grep-escape-root-search-via-durable-grant.md)) —
   because out-of-root entries never count toward the walk cap, so expanding
   an ungranted absolute directory (`/`) would walk the whole filesystem.
2. **Brace-set expansion** (`brace.rs`): a small hand-rolled expander
   (first top-level comma-bearing `{…}`, recursive/cartesian, capped at 64
   alternatives, no escape syntax — `[{]` remains the escape hatch; comma-less
   and unmatched braces stay literal, matching the `glob` crate). Applied to
   `glob.pattern`, `grep.path`, and every `exclude` entry; results are
   deduped across alternatives and still capped at `MAX_RESULTS` total.
3. **No silent empty results — every zero-result call explains itself**,
   superseding ADR-0016's point 4 (`grep` hint exemption) and its clean-
   no-match empty-string allowance. `grep` distinguishes *"path filter
   matched no files — nothing was searched"* (call-shape error) from *"no
   matches for `X` in N file(s) scanned"* (real no-match); `glob`'s clean
   no-match returns *"pattern `X` matched no files."* (keeping the
   `` pattern ` `` prefix the TUI hint renderer already keys on). Cap hits
   append one-line notices (`[file walk capped at 1000 files — narrow
   `path`]`, `[match cap: first 1000 matches shown]`, `[capped at 1000
   results — narrow the pattern]`), and `FileList` grows `capped` +
   `out_of_root` counters so containment drops are reported instead of
   reading as a clean no-match.
4. **Input-surface compatibility**: `grep` gains `case_insensitive: bool`
   (serde alias `"-i"`, the literal CLI spelling seen in logs; inline `(?i)`
   keeps working), `glob` gains `path` (base directory, Claude-Code `Glob`
   shape; the permission arg grades the joined `{path}/{pattern}` string the
   walk actually uses), and both inputs are `deny_unknown_fields` — a
   silently-dropped field is indistinguishable from an honored one, which is
   ADR-0016's own principle applied to inputs. This mirrors the provider
   catalog's `deny_unknown_fields` precedent.

## Consequences

- The dominant real-world failure shape (`{"pattern":"foo","path":"src"}`)
  now searches recursively instead of returning nothing.
- `glob {"pattern":"src"}` behavior changes from the ADR-0016 "matched 1
  directory — try `src/*`" hint to an actual recursive listing; the hint
  remains for glob-y dir-only matches (`.github/**`). Downstream consumers of
  the hint text for that one case see a listing instead.
- Embedders passing extra input fields now fail loudly with the field named —
  deliberate; it was a silent bug before.
- A `Once`-scoped grant still never widens a search (unchanged from
  ADR-0132); dir expansion composes with durable grants only.
- Permission grading is unchanged for `grep` (the raw `path` is graded, and
  expansion happens after grading, in the walk); `glob` rules/grants now see
  `{path}/{pattern}` joined — landed in the same change as the new param.
- `.gitignore` awareness stays deferred (ADR-0008 → ADR-0016 → here); now
  tracked as a deferred-work-ledger row so it survives issue closure.
