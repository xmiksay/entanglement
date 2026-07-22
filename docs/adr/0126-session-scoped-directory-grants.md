# 0126. Session-scoped directory grants via `ApprovalScope::SessionDir` and TUI `/allow`

- Status: Accepted
- Date: 2026-07-22

## Context

[#486](https://github.com/xmiksay/entanglement/issues/486), blocked by and
following [#485](https://github.com/xmiksay/entanglement/issues/485)/[ADR-0125](0125-permission-arguments-for-path-tools-are-normalized-root-relative.md):
grants (#174, [ADR-0052](0052-approval-scope-and-persisted-grants.md)) are
exact `(tool, argument)` matches (`grants.rs`'s `GrantKey`, `HashSet::contains`
in `is_granted`). Approving `read src/a.rs` never covers `read src/b.rs` — a
session doing repeated reads under one directory (exploring a module,
grepping across a package) re-prompts for every file. The wanted shape is
"allow this directory for the rest of the session," not "allow this one
call again."

ADR-0125 is the prerequisite because it defines the grading-time argument
(`permission_path::grading_arg`, root-relative, lexically normalized) this
ADR's directory matching is built on top of — without it, a directory grant
recorded against an absolute spelling and checked against a relative one (or
vice versa) would silently fail to widen.

## Decision

**A new `ApprovalScope::SessionDir` variant, not a flag on `Approve`.** A
directory grant is only ever session-scoped (no persisted "always" directory
grant, see "Alternatives" below) and only ever widens the read-only triad —
encoding both as a dedicated enum variant makes the invalid combinations
(`Once` + directory, `Always` + directory) unrepresentable, rather than a
runtime check on a `(scope, is_dir)` tuple. `ApprovalScope` already has a
stable additive-variant precedent (ADR-0064's shim pattern): a new variant
is backward compatible on the wire (`#[serde(rename_all = "snake_case")]`
gives it `"session_dir"` for free), and `Approve` is already wire-allowed
(#155/ADR-0069), so the `serve`/`pipe` heads get the reactive `[d]` path for
free with no protocol version bump.

### Scope: the read-only triad only

`SessionDir` covers exactly `read`/`grep`/`glob` — the ADR-0114 `read`
capability's member list (`tool_names::CAPABILITIES`), reused directly so the
two "is this tool read-like" checks (the grant store's widening decision and
the TUI's `[d]`-key/footer gate) can never drift from the capability table.
Mutation tools (`edit`/`write`/`apply_patch`) and exec tools (`bash`/`call`)
are excluded on purpose: the repeated-prompt annoyance this issue exists to
fix comes from read-only exploration, and a directory-wide write/exec grant
is a materially larger blast radius a user should approve per-call. A
`SessionDir` approval on any other tool **degrades to an exact `Session`
grant** rather than silently widening — never a no-op, never an error, just
the safer of the two behaviors the scope could plausibly mean.

### Matching: path-component prefix on the grading argument

`grants::dir_covers(dir, arg)`: `arg == dir || arg.starts_with("{dir}/")`, or
`dir == "."` (covers every relative argument — granting the project root
covers everything under it). Operates directly on the already-#485-normalized
root-relative grading argument, so a glob pattern's wildcard tail composes
for free: `dir_covers("src", "src/*.rs")` is a plain string check, no
separate glob-aware comparison needed. A pattern whose literal root doesn't
match the granted directory (`s*/foo.rs` against `src`) fails the same plain
`starts_with` check and correctly still asks — no bespoke wildcard-boundary
logic required.

### Deriving a directory from an approved call

`grants::dir_for(tool, arg) -> Option<String>` turns a specific `(tool, arg)`
call into the directory a `[d]` approval should grant: `read`/`edit`/`write`/
`apply_patch` → the argument's parent directory (a root-level file's parent
is the project root itself, `"."`); `grep` → the path-filter argument
verbatim (already directory-shaped — a specific file or a directory, not
narrowed to a parent); `glob` → the pattern's literal prefix up to its first
wildcard, truncated to the last path separator (`"src/*.rs"` → `"src"`,
`"*.rs"` → `"."`). Defined for the full `PATH_ARG_TOOLS` shape (mirroring
#485's table) even though only the read-only triad currently invokes it —
`record`'s caller is what restricts scope, keeping `dir_for` itself a plain,
reusable derivation function.

### Storage: a separate, never-persisted set

`FileGrantStore` gains `session_dirs: HashMap<SessionId, BTreeSet<String>>` —
distinct from the existing exact-match `session`/`always` sets, since a
directory string is a different key shape than a `GrantKey`. Session-only by
construction: there is no `Always`-directory scope, so `session_dirs` is
never written to the managed `grants.yml` file and never touches
`persist`/`read_grants`/`write_grants`. `forget_session` clears it alongside
the exact-match session set. `GrantStore::grant_session_dir(session, dir) ->
String` is the second way to add one (besides approving a prompted call with
`[d]`): it normalizes `dir` via `permission_path::normalize_lexical` and
returns the stored (normalized) form for the caller's confirmation message —
the TUI `/allow <path>` command's entry point.

### Escape-root interaction: degrade, don't extend

`ExtraRootStore` (ADR-0109) is a separate store keyed by `(tool,
resolved-absolute-path)` for out-of-root access grants — a different key
space than `grants.yml`'s `(tool, argument)`. A `SessionDir` approval on an
escape-forced prompt has no meaning there (there is no "directory" concept
in an absolute-path escape grant that wouldn't just be "allow this whole
external tree," a materially different and unrequested capability), so
`ExtraRootStore::record` degrades `SessionDir` to an exact `Session` grant on
that one out-of-root path — the same degrade-not-widen posture as a
`SessionDir` approval on a non-triad tool. `/allow` itself refuses an
out-of-root path outright (see below) rather than routing through the
escape-root store at all.

### TUI surfaces: `[d]` on the prompt, `/allow <path>` proactively

- **Reactive (`[d]`):** `tui/event_loop.rs`'s `WaitingForApproval` key match
  gains `KeyCode::Char('d')`, guarded on the pending tool being a read-only
  triad member (`tool_names::is_read_capability_member`) — pressing `d` for
  an `edit`/`bash` prompt is simply not offered, rather than offered and then
  silently degraded, so the key only ever does what its footer hint (also
  gated the same way, `tui/transcript.rs`) promises.
- **Proactive (`/allow <path>`):** a new `Command::Allow` and a sibling
  `tui/allow_command.rs` module (mirroring `mcp_command.rs`'s "own file"
  reasoning — `commands.rs`/`event_loop.rs` are both already past the
  400-line cap), since `/allow` needs neither `holly` nor an engine
  round-trip: it normalizes the path against the head's root
  (`normalize_allow_dir`, rejecting anything that resolves outside root —
  including a relative `../etc` that ADR-0125's `rooted_arg` deliberately
  leaves unflagged for path-arg *tool* grading, since that case is the
  escape-root gate's problem there; `/allow` has no escape-root counterpart,
  so it rejects outright) and calls `grant_session_dir` directly through a
  cloned `Arc<DefaultGrantStore>` handle threaded into the TUI (`tui()` gains
  a `grants` parameter, `App::set_grants`) — mirroring how the TUI already
  holds `watch::LiveDefinitions`'s `grants` clone for the same store. A
  target that doesn't exist on disk yet is accepted with a note in the
  confirmation line, not rejected — the grant is for future reads under that
  directory, so requiring it to exist first would defeat "set up access
  before the model asks."
- **Deliberately not wire-facing.** `/allow` is head policy executed
  synchronously against a local store handle, exactly like the escape-root
  approval's local bookkeeping — it introduces no new `InMsg`/`OutEvent`, so
  the #472/ADR-0124 fail-closed wire allowlist is untouched. A `serve`/`pipe`
  client cannot trigger `/allow` remotely; it can still exercise the
  reactive path by sending `Approve { scope: SessionDir }` for a call it
  already sees offered, since `Approve` was already wire-allowed.

### `GrantStore` trait: additive, non-breaking

`GrantStore::grant_session_dir` is **default-implemented** (echoes `dir`
back unnormalized, does nothing else) so the #311 pluggable-policy seam's
existing custom implementations (`tests/policy_seam.rs`'s test double,
`examples/embedded.rs`'s `NoGrants`) keep compiling with zero changes. Only
`DefaultGrantStore` (the CLI/TUI's store) overrides it for real, delegating
to `FileGrantStore::grant_session_dir` under its mutex.

## Consequences

- Positive: repeated reads/greps/globs under one directory in a session stop
  re-prompting after a single `[d]` or one `/allow <path>`, without touching
  the exact-match semantics `Session`/`Always` already rely on for
  mutation/exec tools.
- Positive: the "is this tool read-like" check has exactly one definition
  (`tool_names::is_read_capability_member`, reusing the ADR-0114 capability
  table), shared by the grant store, the TUI key gate, and the TUI footer —
  no risk of the three drifting apart.
- Positive: zero wire-protocol risk — an additive `ApprovalScope` variant, no
  new `InMsg`/`OutEvent`, `Approve` already wire-allowed.
- Neutral: a `SessionDir` approval on a non-triad tool or an escape-forced
  prompt is not an error and not a silent no-op — it degrades to the
  well-understood `Session` behavior, so a client that doesn't know better
  (or a user pressing `[d]` somewhere it isn't offered, via a raw wire frame)
  gets a safe, exact-match grant rather than nothing or a crash.
- Neutral: directory grants are invisible to `grants.yml` — inspecting the
  managed file never shows a live `SessionDir` grant, by design (it's
  process-lifetime, like `Session` scope already is).

## Alternatives considered

- **Add a `directory: bool` (or similar) flag alongside the existing
  `scope` field on `Approve`, instead of a new enum variant.** Rejected:
  every combination (`Once` + directory, `Always` + directory) would need a
  runtime check to reject or silently ignore, where a dedicated variant makes
  the invalid states simply not exist. A flag would also force every
  `match scope { .. }` site to additionally branch on the flag instead of the
  compiler enumerating the new case for us (as it did here, surfacing every
  site — `grants.rs`, `extra_roots.rs` — that needed a `SessionDir` arm).
- **An `Always`-scoped directory grant, persisted to `grants.yml`.**
  Deferred, not rejected outright: a persisted directory-wide grant is a
  meaningfully bigger standing capability than a persisted single-command
  grant (`grants.yml` already documents itself as exact-match, and every
  existing row there names one literal call) and the UX for reviewing/
  revoking a persisted directory line needs more thought than this issue's
  scope covers. Session-only for now; the managed file's shape is unchanged.
- **Widen `edit`/`write`/`apply_patch` too, since a user editing under `src/`
  might want that covered as well.** Rejected: the repeated-prompt problem
  reported is specifically about read-only exploration; a mutation tool's
  blast radius per call is large enough that per-call approval (or the
  existing exact-match `Session`/`Always`) is the appropriate default. Left
  as a possible future widening if a concrete need appears, not preemptively
  built.
- **Route a `SessionDir` approval on an escape-forced prompt through a new
  "external directory" concept in `ExtraRootStore`.** Rejected as out of
  scope: an out-of-root directory grant is a materially different, larger
  capability ("the whole external tree under this path") that nothing in
  #486 asked for; degrading to the existing exact-match `Session` behavior
  there is the safe, no-new-surface choice, matching #482's ledger entry
  reasoning for why `glob`/`grep` escape-root access stays deferred too.
