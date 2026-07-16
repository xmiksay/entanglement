# 0092. `call` gains `input_file`/`output_file` for file-based stdin/stdout

- Status: Accepted
- Date: 2026-07-16
- Builds on [0045](0045-call-host-tool-argv-exec-tailed-output.md) (`call`'s
  argv-exec + auto-tailed output) and reuses `resolve_under_root`'s
  containment ([0054](0054-canonicalizing-symlink-safe-root-containment.md)).
  Issue #381.

## Context

`call`'s only trace of having run is an in-memory, tailed (default 30 lines)
text blob — nothing persists past the tool result, which is why its
invocations feel invisible after the fact once the transcript scrolls past
them. A model that needs the *full* output of a large build/test run has no
way to capture it; it can only re-run with `tail=0` and hope the 32 KiB byte
cap doesn't still truncate it.

Separately, `call` never sets `.stdin(...)` on the spawned `Command`, so the
child **inherits the engine's real stdin** today. That's an unintentional
leak, not documented behavior — a spawned binary that happens to read stdin
(interactively or not) gets whatever the engine process itself was launched
with, which the model has no way to predict or control.

## Decision

**Add two optional `call` fields, `input_file` and `output_file`, both
resolved under the working directory with the same containment `read`/`edit`/
`write` use (`resolve_under_root`), validated *before* the child spawns.**

### `input_file` — file-based stdin, and a fix for the inherit leak

When given, `input_file` is read in full before spawn and piped to the
child's stdin (fed from a background task running concurrently with the
stdout/stderr drain in [`wait_or_kill_group`](../../entanglement-runtime/src/host/exec.rs),
so a chatty child can't deadlock against a full pipe buffer waiting on stdin
while `call` is waiting on stdout). **When absent, stdin is explicitly
`Stdio::null()`** — closed, not inherited. This is a behavior change as much
as a feature: the accidental inherit was never documented and never
intentional, so closing it by default is the correct fix regardless of
whether `input_file` is ever used.

### `output_file` — a durable artifact, always

When given, the full **untruncated raw** stdout is written to `output_file`
(not the `[exit N]`-framed, tailed text the model sees) — parent directories
are created if missing. A `<output_file>.stderr` sibling (suffix appended to
the whole filename, not replacing an extension) is **always** written
alongside, holding the equivalently full raw stderr.

**When `output_file` is absent, an artifact is written anyway**, auto-named
under `<root>/.entanglement/tmp/call-output/call-{pid}-{seq}.stdout` (+ a
`.stderr` sibling) — `{pid}` is the engine process id, `{seq}` a per-process
atomic counter, so concurrent `call`s never collide on a filename. This is
the load-bearing decision: **every** `call` invocation gets a durable,
inspectable artifact, not just ones that opted in. The result header always
names the artifact's root-relative path (`[output: <path>] [stderr:
<path>.stderr]`) so the model — and a human reading the transcript later —
can find it without having requested it.

### Failure handling is asymmetric by design

- **Explicit `output_file` write failure → hard error.** The model asked for
  a specific artifact; if the engine can't deliver it, that's a real failure
  worth surfacing (bad permissions, a full disk, a path that collides with a
  directory), not something to silently degrade.
- **Default-artifact write failure → best-effort.** A disk issue unrelated to
  the command being run must not fail a `call` that would otherwise have
  succeeded — this is bookkeeping the model never asked for. On failure it's
  logged (`tracing::warn!`) and a `[output artifact write failed: …]` notice
  is prepended to the result instead of returning `Err`.

### Validation precedes spawn

Both paths are resolved via `resolve_under_root` (rejecting `..`-escape and
symlink-escape, same as `read`/`edit`/`write`) and `input_file` is read to
completion **before** `Command::spawn()` — a missing `input_file` or an
escaping `input_file`/`output_file` returns a clean error with the child
**never launched**. This mirrors the existing "missing binary → clean error,
never a panic" contract ([ADR-0016](0016-host-tool-empty-result-contract.md))
and avoids the wasted (and potentially side-effecting) work of running a
command whose output has nowhere valid to land.

### Timeout path writes partial output too

`call`'s timeout path already preserves the output buffered before the
group-kill ([#169](0045-call-host-tool-argv-exec-tailed-output.md)) instead of
discarding it. The artifact write reuses exactly that buffer — a timed-out
call's `output_file`/default artifact holds whatever was captured before the
kill, not an empty file.

## Consequences

- **(+)** Every `call` leaves a durable, inspectable trace by default — no
  more "never saw the agent call `call`" — without requiring the model to
  remember to ask for one.
- **(+)** Closes an undocumented stdin-inherit leak as a side effect of adding
  the feature the leak's absence made necessary.
- **(+)** `output_file` gives the model (or a human) the full output of a
  build/test run that the 32 KiB tailed result can't carry, without a
  re-run.
- **(−)** Every `call` now does at least one filesystem write it didn't do
  before (the default artifact), even when the model never asked for
  persistence — accepted as the cost of "durable by default"; the write is
  best-effort so it can't turn a successful command into a failed tool call.
- **(−)** `.entanglement/tmp/call-output/` accumulates one `.stdout`/`.stderr`
  pair per unrequested `call` for the life of the working directory — no
  retention/GC policy yet. Deferred: a project can `bash`/`call`-delete the
  directory itself, or a future issue can add a sweep.
- **(−)** `bash` is unaffected — this lands on `call` only. `bash`'s shell
  already gives the model `< file`/`> file` redirection natively; duplicating
  that plumbing outside the shell buys nothing there.

## Alternatives rejected

- **Only persist when `output_file` is explicitly given.** Rejected: this is
  exactly today's status quo restated with an opt-in flag — the model still
  has to know in advance that a call's output will matter later. The
  motivating complaint ("never saw the agent call `call`") is about
  invocations nobody thought to flag ahead of time; a default-on artifact is
  the only shape that covers those.
- **Return the artifact content inline instead of tailing.** Rejected: that's
  just removing the tail cap, which reintroduces the exact context-window
  problem [ADR-0045](0045-call-host-tool-argv-exec-tailed-output.md) tail-ing
  solved. The artifact file is the escape hatch *outside* the context
  window — the model reads it back with `read`/`grep` if it needs more than
  the tail.
- **Apply the same auto-artifact default to `bash`.** Deferred, not rejected
  outright — out of scope per the issue. `bash` already has `run_in_background`
  + `bash_output` (#170) as its own durability story for long-running
  commands; folding this in too is a separate design decision.
