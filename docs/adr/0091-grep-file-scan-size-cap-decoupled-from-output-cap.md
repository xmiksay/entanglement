# 0091. `grep` file-scan size cap decoupled from the output cap

- Status: Accepted (supersedes the grep clause of [0008](0008-host-tools-workdir-and-bounded-output.md) point 4)
- Date: 2026-07-16
- Issue #380.

## Context

`grep`'s per-file skip check reused `MAX_OUTPUT_BYTES` (32 KiB — the cap on
the *result string* `grep` returns) as the **file-scan-size** cutoff: any file
over 32 KiB was silently `continue`d with zero signal. A match that exists
only in such a file produced output identical to "no match," violating the
project's own empty-result contract ([ADR-0016][0016]): a host tool returning
`""` for multiple distinguishable underlying states is itself buggy — here,
"no match" and "match exists but the file was too big to scan" were
indistinguishable.

Two things compounded the bug:

- The code comment said the check skips files "far larger than the output
  cap," but it fired at exactly 1×.
- [ADR-0008][0008] point 4 claimed grep "skips files larger than 4× the
  output cap" — that multiplier was never actually implemented; the code
  always compared against 1× `MAX_OUTPUT_BYTES`.

There was also no binary-file detection: binary content was always
lossy-UTF8-decoded and searched (turning arbitrary bytes into `U+FFFD`
replacement characters) rather than skipped, which is wasted work and noisy
output for content a text search was never going to usefully match anyway.

The root confusion was conflating two unrelated bounds:

1. **How much file content is safe to read into memory and scan** — a
   function of how big files in a repo can reasonably get.
2. **How much matched-line output is safe to return to the model** — a
   function of the context window.

Reusing one constant for both meant tightening the output cap (to protect the
context window) would silently tighten the scan cap too, and vice versa.

## Decision

1. **`grep`-local `MAX_SCAN_BYTES` constant, 1 MiB**, independent of
   `MAX_OUTPUT_BYTES`. A file whose size exceeds this is not read at all.

2. **Binary detection via NUL-byte sniff.** After a file passes the size
   check, its bytes are read and checked for a NUL byte (`b'\0'`) before
   being lossy-UTF8-decoded and searched. A NUL byte is a reliable binary
   signal that never appears in genuine UTF-8 text, so this doesn't
   false-positive on non-ASCII text (accented characters, CJK, emoji, etc. —
   none of which encode a NUL byte in UTF-8).

3. **Skip reasons are tracked, not discarded**, as `Vec<(PathBuf,
   SkipReason)>` with `SkipReason::TooLarge(len) | Binary`. Whenever this list
   is non-empty — **regardless of whether any matches were found** — a
   labeled notice is appended to the result: one section per reason,
   naming each skipped path (and size, for too-large), with a capped preview
   (20 entries) collapsing the rest into an `... and N more` tail so a
   pathological skip list can't blow the output budget on its own.

4. **ADR-0008 point 4 is superseded, not edited**, for the grep clause only —
   its status line gains a pointer to this ADR; the rest of ADR-0008 (root
   containment, `read`/`glob` bounds, the empty-result contract itself)
   stands unchanged.

## Consequences

- **(+)** A match in a file between 32 KiB and 1 MiB — a common size for a
  generated file, a lockfile, or a merged log — is now found instead of
  silently missed.
- **(+)** The skip notice makes "some files were excluded from this search"
  visible and actionable: the model can re-run `grep` with a narrower `path`
  glob, or fall back to `read` on a named file, instead of trusting a
  false-negative empty result.
- **(+)** Binary files are no longer decoded-and-searched for no benefit;
  skipping them is both cheaper and produces a more honest notice than
  matching against `U+FFFD` noise.
- **(−)** Raising the scan cap to 1 MiB means `grep` now reads up to 1 MiB
  per candidate file into memory before deciding whether to search it (vs.
  32 KiB before). Accepted: files are read sequentially, one at a time, and a
  local-repo agent's working set doesn't have enough simultaneously-huge
  files for this to matter in practice; a `spawn_blocking`/streaming refactor
  remains available if a hot path ever suffers (ADR-0008's `(−)` already
  flagged `glob`/`grep`'s synchronous FS work as a known, accepted tradeoff).
- **(−)** The NUL-byte sniff isn't a general binary-format detector (e.g. a
  binary format with no embedded NUL in its early bytes would still pass
  through) — accepted as a cheap, zero-false-positive-on-text heuristic
  rather than a full magic-byte/mime-sniffing pass, which is more machinery
  than a search tool's skip check needs.

## Alternatives considered

- **Raise `MAX_OUTPUT_BYTES` instead of introducing a second constant.**
  Rejected: `MAX_OUTPUT_BYTES` is a context-window bound shared by every host
  tool's output truncation ([`truncate_output`]); widening it to fix grep's
  scan cap would loosen every other tool's output bound as a side effect.
- **Implement the 4× multiplier ADR-0008 already claimed.** Rejected: 4× 32
  KiB is 128 KiB, still well under files that legitimately warrant a scan (a
  generated SQL dump, a `Cargo.lock`, a bundled JS file can exceed that
  without being unreasonable to grep). A flat, independently-chosen cap is
  clearer than a derived multiplier that has to be re-justified whenever the
  output cap changes.
- **Stream/chunk-scan large files instead of capping.** Rejected as
  over-engineered for this fix: `grep`'s per-line match already requires the
  whole file's lines in memory to number them; a chunked scanner is real
  complexity for a bound that a 1 MiB flat cap already serves for a
  local-repo agent's working set. Left as a future improvement if a real
  workload needs multi-MiB file search.
- **Magic-byte/mime-type sniffing for binary detection.** Rejected: heavier
  than the tool needs. The NUL-byte heuristic is what `git` and `grep -I`
  themselves use to decide "is this binary," and it has no false positives
  on valid UTF-8 text.

[0008]: 0008-host-tools-workdir-and-bounded-output.md
[0016]: 0016-host-tool-empty-result-contract.md
