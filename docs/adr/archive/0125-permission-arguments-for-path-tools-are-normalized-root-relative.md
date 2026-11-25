# 0125. Permission arguments for path tools are normalized root-relative

- Status: Accepted
- Date: 2026-07-22

## Context

[#485](https://github.com/xmiksay/entanglement/issues/485): a `read`/`grep`
call whose `path` is absolute but resolves *inside* the project root triggers
an approval prompt, while the identical file addressed relatively resolves
`allow`.

`permission_arg` (`entanglement-runtime/src/permission.rs`) extracts the
model-supplied `path`/`pattern` **verbatim** — no normalization, no access to
the project root. Argument-scoped rules ([ADR-0051](0051-argument-scoped-permission-rules.md),
#173) are authored root-relative (`read(src/*)`), and
`PermissionProfile::resolve_scoped` → `glob_match` matches literally:
`/root/src/main.rs` fails `src/*` and falls through to the profile `default`
(commonly `ask`). The escape-root gate ([ADR-0109](0109-escape-root-access-via-approval.md))
is **not** the cause — an absolute path inside root is correctly "contained";
it just never reaches the escape-root code path at all, since it isn't
escaping anything.

The same verbatim extraction also keys the "always allow" grants (#174,
`runtime::grants`, `tool_runner.rs`'s `apply_grant`/`await_decision`): a
Session/Always grant recorded against one spelling never matches the other,
so a model that mixes spellings across calls re-prompts even after the user
approved "the same file" once.

## Decision

**Normalize a path-arg tool's extracted argument root-relative before it is
used for grading or a grant key — but only when a project root is actually
known, and never for the raw verbatim value the TUI displays.**

### A new module, not a change to `permission_arg`

`entanglement-runtime/src/permission_path.rs` (new; keeps the already-large
`permission.rs` from growing past its cap):

- `normalize_lexical(path)` — folds `.`/`..`/`//` **lexically**, no filesystem
  access (a symlink component is left exactly as written; resolving those
  stays the escape-root gate's job, [ADR-0054](0054-canonicalizing-symlink-safe-root-containment.md)).
  A `..` with nothing to pop (a leading `..` in a relative path, or one
  immediately after an absolute root) is kept/dropped respectively rather than
  treated as an error — this is a normalizer, not a validator.
- `rooted_arg(root, tool, arg)` — for the path-arg tools only (`read`/`edit`/
  `write`/`apply_patch`/`glob`/`grep`; `bash`/`call`'s command line is never a
  path and is passed through untouched): normalize, then if the result is
  absolute and resolves under `root`, strip the root prefix (`root` itself
  becomes `"."`). An absolute path that does **not** resolve under `root`
  stays verbatim (still lexically normalized).
- `grading_arg(tool, input, root: Option<&Path>)` — `permission_arg` mapped
  through `rooted_arg` when `root` is `Some`; `root: None` (no escape-root
  policy wired — every test-only executor wrapper) yields the untouched
  verbatim value, byte-identical to pre-#485 behavior.

`permission_arg` itself is untouched and keeps its two existing consumers
outside grading: the TUI transcript render (`tui/transcript/render_run.rs`,
which must keep showing the model's literal input, not a rewritten one) and
the escape-root/workdir extractors (`escape_root_target`/`permission_workdir`),
which have their own resolution needs and are explicitly *not* relativized
here (see below).

### Wired from the one root the runtime already canonicalizes

`root` is not a new concept — `main.rs` already canonicalizes the project
root once at startup and threads it into `EscapeRoot { root, store }`
(ADR-0109). This ADR reuses that exact value rather than introducing a second
root parameter:

- `policy::ProfileResolver` gains a third constructor argument,
  `root: Option<PathBuf>`; its `resolve` calls `grading_arg` instead of
  `permission_arg`. `main.rs` passes `Some(escape_root.root.clone())` (cloned
  *before* `escape_root` is moved into `spawn_tool_executor_with_policy`).
  The two test-only convenience wrappers (`spawn_tool_executor`/
  `spawn_tool_executor_with_hooks`) pass `None`, keeping every existing test
  byte-identical.
- `tool_runner::dispatch` derives `root` from the `escape_root: Option<&EscapeRoot>`
  parameter it already receives — the same source `main.rs` fed the resolver
  — and computes the grading arg once, before resolving the base grade. That
  single value is then threaded into `apply_grant` (the pre-prompt grant
  lookup) *and* forwarded as a new parameter to `await_decision` (the
  post-approval grant record), replacing `await_decision`'s own
  `permission_arg` recomputation. This is the fix for the grant-key half of
  #485: the lookup and the record now provably share one key, rather than
  each independently re-deriving the argument from the raw call input.
- `script::BindingPolicy` gains a `root: Option<PathBuf>` field, set by
  `capture`'s new parameter (`tool_runner`'s `Intercept::Rhai` arm passes
  `escape_root.as_ref().map(|er| er.root.as_path())`, computed before
  `escape_root` is cloned for the script task). `decide` and the `call`/`bash`
  branch of `approval_cache_key` route through `grading_arg` instead of
  `permission_arg` — so a script's `read("/abs/inside/root/…")` grades and
  caches identically to `read("relative/…")`.

### Out-of-root and workdir stay exactly as before

Two deliberate non-changes, both because relativizing them would be wrong,
not merely unnecessary:

- **An absolute path outside root stays verbatim.** A root-relative rule
  (`read(src/*)`) matching an *outside* path would be a privilege escalation,
  not a convenience — out-of-root access is the escape-root gate's problem
  (ADR-0109), which already forces its own approval independent of the
  permission grade.
- **`permission_workdir` is not relativized.** ADR-0116's `tool{pattern}`
  workdir-scoped rules are host-filesystem-absolute by design
  (`bash{/tmp/*}`) — a working directory is not "inside the project" in the
  same sense a source path is, and nothing in #485 reported workdir rules
  behaving inconsistently.

## Consequences

- Positive: `read`/`edit`/`write`/`apply_patch`/`glob`/`grep` grade
  identically whether the model spells an in-root path relatively or
  absolutely — the bug's core symptom.
- Positive: a Session/Always grant recorded against one spelling now covers
  the other, since the grant key and the grading key are the same
  `grading_arg` call, computed once in `dispatch` and threaded into
  `await_decision` rather than re-derived.
- Positive: zero behavior change with no root wired (every test helper, and
  any embedder that doesn't opt into the escape-root policy) — `grading_arg`
  degrades to the pre-#485 verbatim `permission_arg`.
- Neutral: `bash`/`call`'s command-line argument is never touched (it was
  never a path); `approval_cache_key`'s `call`/`bash` branch now calls
  `grading_arg` for one canonical extraction path, but the result is
  unchanged since neither tool is in the path-arg set.
- Neutral: normalization is lexical only — a path reaching the project root
  through a symlink still isn't relativized here (nothing new: the escape-root
  gate's canonicalizing resolution is the layer that ever needed to see
  through symlinks, and it is unaffected by this change).

## Alternatives considered

- **Canonicalize (resolve symlinks) instead of lexical-only normalization.**
  Rejected: would require filesystem access from the permission-grading path,
  which today never touches disk and must stay synchronous/side-effect-free
  (grading runs ahead of, and independent of, actual execution — a `read`
  that will itself fail on a nonexistent path must still grade cleanly so the
  *right* error, not a permission error, surfaces). Symlink resolution is
  already the escape-root gate's job (ADR-0054) and stays there.
- **Relativize inside `permission_arg` itself**, giving it an optional root
  parameter. Rejected: `permission_arg` is also the TUI's display source
  (`render_run.rs`) — rewriting its output would show the user a path the
  model never actually sent, undermining the "what did the model ask for"
  transparency the transcript exists to provide. Keeping a separate
  `grading_arg` wrapper is the smaller, more legible change.
- **Also relativize `permission_workdir`.** Rejected: workdir-scoped rules
  are host-absolute by design (ADR-0116) — a working directory outside the
  project root is a normal, expected shape (`bash{/tmp/*}`), unlike a
  `read`/`edit` target path which is meant to name something *in* the
  project. Folding both into one extractor would conflate two different
  semantics for no reported bug.
