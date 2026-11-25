# 0197. Compound bash commands grade per segment, shell escape constructs fail closed

- Status: Accepted
- Date: 2026-09-14
- Relates to: [ADR-0051](0051-argument-scoped-permission-rules.md)/[ADR-0114](0114-capability-level-permission-keys.md)
  (the `tool(pattern)` argument-scoped rule machinery this closes a hole in),
  [ADR-0195](0195-bash-is-the-default-exec-and-curated-read-only-rules.md)
  (the curated read-only `bash(find *)`-style Allow defaults this hole was
  found under), [ADR-0093](0093-call-registration-independent-of-bash-opt-in.md)
  (`call`'s argv-exec shape — explicitly out of scope here), [ADR-0052](0052-approval-scope-and-persisted-grants.md)
  (the exact-match grant semantics left untouched).

## Context

An argument-scoped permission rule like `bash(find *)` grades the *entire
raw* `command` string of a `bash` call through
[`PermissionProfile::resolve_scoped`]'s full-string `glob_match`
(`entanglement-core/src/protocol.rs`). Two defects fell out of that, both
sharpened by ADR-0195 shipping curated read-only Allow rules
(`bash(find *)`, `bash(grep *)`, `bash(ls *)`, …) as an embedded default
every profile now inherits:

1. **Over-match (security).** `glob_match("find *", "find . && rm -rf /")`
   is `true` — the trailing `*` swallows `&&`, `|`, `;`, and newlines along
   with everything after them. A curated read-only Allow rule therefore
   authorizes *any* command appended to an allowed verb via a shell
   metacharacter, with no prompt.
2. **Under-match (UX).** A compound command that doesn't *start* with an
   allowed verb (`git status && find .`) never matches any rule — it falls
   through to `Ask` on every call. Worse, the interactive grant store
   ([`entanglement-runtime/src/grants.rs`], exact whole-string key equality)
   never re-fires for a compound either, so the user is re-prompted for the
   same shape of call indefinitely; a curated allow-list that only ever
   helps single-verb commands is a partial fix.

Both defects trace to the same root cause: a `bash` command is a shell
program, not an opaque string, and grading it as one glob-matched blob
either grants too much (segments after the first) or too little (compounds
that don't start with a matched verb).

## Decision

**Per-segment grading: every top-level segment of a compound command must
pass, and any shell construct the splitter can't fully reason about fails
closed.**

### 1. A conservative, quote-aware splitter (`entanglement-runtime/src/shell_split.rs`)

Splits a command on top-level `&&`, `||`, `;`, `|`, `&` (background), and
newlines, honoring single/double quotes and backslash escapes. Returns
either the trimmed, non-empty segments, or `Opaque` — a hard "can't tell,
don't guess" signal — for any construct that can smuggle a side effect the
splitter doesn't fully model: output redirection (`>`/`>>`), command
substitution (`$(...)`, backticks — including inside double quotes, where
both still expand), process substitution (`<(...)`/`>(...)`), a heredoc
(`<<`), subshell/arithmetic grouping (`(...)`, not implemented), or an
unmatched quote. Bare stdin redirection (`<`) is left as plain segment
text — it only reads, it cannot append a side effect. A leading
`NAME=value` assignment prefix is left in its segment unmodified (v1 keeps
this simple: such a segment just won't match a verb-shaped rule, falling
through to `Ask` — a UX gap, not a security one).

### 2. Grading integration (`entanglement-runtime/src/permission_bash.rs`), runtime-only

Core's `PermissionProfile::resolve_scoped`/`glob_match`
(`entanglement-core/src/protocol.rs`) are **untouched** — `make tree` keeps
policy interpretation out of core. `resolve_scoped_bash_aware` is a
drop-in wrapper with the identical `(profile, tool, arg, workdir)` shape,
applied for `tool == "bash"` only:

1. Grade the whole raw command first (the legacy full-string behavior). A
   `Deny` there short-circuits immediately — **deny is never weakened by
   splitting** (this is also the only way an arg-scoped deny rule fires
   against an `Opaque` command, since the opaque path below drops the
   argument entirely).
2. `Opaque` ⇒ re-resolve with `arg: None`. An arg-scoped **Allow** rule
   cannot fire against a construct the splitter can't account for — only
   the tool's bare/workdir-scoped rules can still grade it. This is the
   fix for defect 1's redirection/substitution corner: `find . > out.txt`
   no longer rides `bash(find *)`.
3. `Segments(segs)` ⇒ grade each segment independently and fold with
   `min_permission` (`Deny < Ask < Allow`): any segment `Deny` denies the
   whole command; all-`Allow` is `Allow`; otherwise the fold lands on the
   most restrictive non-deny grade — exactly what an unmatched argument
   already resolves to today (a profile's own `Ask`-by-default
   fallthrough). A simple (non-compound) command splits into exactly one
   segment equal to the whole string, so step 3 reduces to step 1's result
   — **byte-identical to today for every non-compound call.**

The wrapper is applied independently at every existing call site that
resolves a `bash` argument through `resolve_scoped` — the ancestor-chain
fold (`permission_for`), the config permission ceiling clamp
(`clamp_to_base`), and the tool-overlay grade (both in `tool_runner.rs`'s
`dispatch` and `script.rs`'s `BindingPolicy::decide`, covering the `rhai`
`bash()`/`exec()` bindings the same way) — rather than as one refactored
top-level function. This is safe because `min`-folding a segment grade is
associative and commutative: folding per layer and then combining layers
with the existing `min_permission` calls gives exactly the same answer as
folding once over every `(layer, segment)` pair would, so the ceiling still
clamps *after* the ancestor-chain fold exactly as it did before, unchanged
in shape.

`call` is untouched: it execs `command`+`args` as argv with no shell
(ADR-0093), so a `&&` or `;` in its command string is inert literal text,
not a shell operator — there is no compound-command surface to split.

Grants (`entanglement-runtime/src/grants.rs`) are untouched: `GrantKey`
stays exact whole-string equality. An `Always`-scoped approval on a
compound command still only re-fires for the identical compound; it does
not learn to cover the compound's individual segments. This is an accepted
gap, not a regression — the rule-based per-segment Allow (curated defaults
or a user's own `bash(pattern)` rules) now handles the common compound
case (`find . | grep x | wc -l`) without a prompt at all, which is the
larger share of the original under-match complaint.

## Consequences

### Positive

- Closes the over-match hole: a curated (or user-written) read-only Allow
  rule can no longer be ridden past its verb via a shell metacharacter.
- Closes the common under-match case: a compound built entirely from
  allowed verbs (`find . | grep x | wc -l`) now grades `Allow` with no
  prompt, instead of asking every time because the whole string never
  matched.
- Deny rules become segment-aware for free: `bash(rm *): deny` now denies
  `find . && rm x` even though `rm` isn't the leading verb.
- Zero behavior change for the overwhelming majority of calls — any
  non-compound command.

### Negative / neutral

- A command containing redirection, substitution, or an unmatched quote
  now falls through to `Ask` even when its allowed-looking prefix would
  previously (incorrectly) have matched — this is the intended fix, but it
  is a visible behavior change: `find . > out.txt` now prompts where it
  silently ran before.
- Grants still don't re-fire per-segment for a compound (see above) — a
  user who previously hit "Always allow" on a specific compound string
  will not see that grant generalize to a different compound built from
  the same segments. Left as a deliberate follow-up if it proves painful in
  practice, not bundled here to keep this change's blast radius to the
  grading path alone.
- The splitter is deliberately conservative: heredocs, subshell grouping,
  and process substitution are `Opaque` rather than parsed, so a legitimate
  compound using one of them always asks. Correct per this ADR's fail-closed
  stance; a future ADR can narrow `Opaque` if a real workflow needs it.

## Alternatives considered

- **A full POSIX shell parser.** Rejected: far more machinery than the
  permission layer needs, and every corner it *doesn't* model precisely
  becomes a silent security question again — the conservative splitter's
  `Opaque` escape hatch gets the same safety property (never mis-grade a
  construct it doesn't understand) for a fraction of the code.
- **Grade only the first segment / first verb.** Rejected: this is exactly
  today's over-match bug restated — it is what the trailing `*` in
  `bash(find *)` already effectively does. Every segment must pass.
- **Widen the grant store to segment-level keys.** Rejected for this
  change: reworking `GrantKey` semantics is a separate, larger decision
  (interacts with `ApprovalScope::SessionDir`'s existing widening rules)
  and isn't needed to close the security hole — rule-based per-segment
  grading already covers the common case grants existed to smooth over.
- **Push the splitter into core so `PermissionProfile::resolve_scoped`
  itself is compound-aware.** Rejected: core stays a dependency-free,
  policy-agnostic protocol crate (`make tree`); shell-command semantics are
  a runtime-only concern, like every other argument-extraction helper
  (`permission_arg`, `permission_workdir`) already is.

## References

- `entanglement-runtime/src/shell_split.rs` — the splitter
- `entanglement-runtime/src/permission_bash.rs` — the grading wrapper
- `entanglement-core/src/protocol.rs` — `PermissionProfile::resolve_scoped`/
  `glob_match` (untouched)
- [ADR-0051](0051-argument-scoped-permission-rules.md)/[ADR-0114](0114-capability-level-permission-keys.md):
  the argument-scoped rule machinery
- [ADR-0195](0195-bash-is-the-default-exec-and-curated-read-only-rules.md):
  the curated read-only Allow defaults
- [ADR-0093](0093-call-registration-independent-of-bash-opt-in.md): `call`'s
  argv-exec shape, exempt here
