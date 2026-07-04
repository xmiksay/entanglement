# 0009. Host tools: `edit` (search/replace) and `bash` (subprocess + timeout)

- Status: Accepted
- Date: 2026-07-04

## Context

ADR-0008 shipped the read-only trio (`read`/`glob`/`grep`) and deliberately
deferred the mutating/executing pair — `edit` and `bash` — because each brings
hard-to-reverse decisions: edit's replace semantics (exact-string vs. regex vs.
diff/patch vs. full-rewrite), and bash's process model (timeout, kill
behavior, sandboxing). The read-only trio is gated first by the `explore` and
`plan` permission profiles; the trio left the `build` profile (default `Allow`)
with nothing to run that actually changes anything.

This ADR closes that gap. Both tools join the same `host_tools(root)` builder
and the same permission model (ADR-0003): `build` auto-allows both (default
`Allow`), `plan` asks for both (default `Ask`), `explore` denies both (default
`Deny`). No profile changes were needed.

## Decision

### 1. `edit` — exact-string search/replace (opencode/Claude convention)

`EditTool` takes `{path, oldString, newString, replaceAll?}` and:

- **`oldString == ""` creates the file** with `newString` as content, refused
  if the path already exists (so the model can't accidentally clobber a file it
  meant to modify). Parent directories are created.
- **`oldString` must match exactly once** unless `replaceAll: true`. Zero
  matches error; N>1 matches without `replaceAll` error with the count so the
  model is forced to disambiguate (pick a larger context window or pass
  `replaceAll`). `replacen(.., 1)` / `replace` do the substitution.
- Reuses [`resolve_under_root`][host] (so `..` escape is rejected, lexical
  containment only — ADR-0008) and writes UTF-8 only.

### 2. `bash` — `sh -c` subprocess with timeout, **not** sandboxed

`BashTool` takes `{command, timeout?}` and:

- Runs `sh -c "<command>"` with `current_dir(root)`. `root` sets the **cwd
  only** — it is explicitly *not* a sandbox; the process inherits the engine's
  full filesystem/network/process privileges. Permission profiles gate whether
  `bash` runs at all.
- **Timeout:** default 120 s (matches opencode), model-supplied `timeout`
  clamped to `[1, 600]` (`.clamp(1, BASH_MAX_TIMEOUT_SECS)`). On expiry the
  child is dropped from `tokio::time::timeout`, and **`kill_on_drop(true)`**
  reaps it instead of orphaning.
- **Output:** `[exit N]\n<stdout>\n[stderr]\n<stderr>` — streams separated so
  the model can tell diagnostics from output; exit code prefixed so non-zero
  failures are obvious. Run through `truncate_output` (32 KiB cap, ADR-0008).

### 3. No new dependencies

`bash` uses `tokio::process::Command`, which is already enabled by the
workspace's `tokio = { features = ["full"] }` (the `process` feature is part
of `full`). `edit` uses `tokio::fs`, already in core. **`make tree` stays
green** — neither tool adds a crate to `entanglement-core`'s dep graph, and
the hygiene-gate forbidden list (clap/axum/tower/tonic/crossterm/ratatui/
reqwest/hyper, ADR-0006) is untouched.

### 4. Both tools live in `entanglement-core::host` behind `host_tools`

Same module, same builder, same conventions as the trio: each holds a `root`,
returns a short model-facing summary string, and ships a JSON `schema()` on
its `Tool` impl so the model sees a real `input_schema` per tool.

## Consequences

- **(+)** `build` can now write code and run commands; `plan` can ask to;
  `explore` stays read-only. The engine is useful for real coding work, not
  just inspection.
- **(+)** Zero new deps. The hygiene gate and the ADR-0008 containment/output
  helpers carry over unchanged.
- **(+)** `kill_on_drop` makes bash timeout cleanup correct by construction —
  no orphaned children, no manual signal ladder in v1.
- **(−)** **`bash` is not sandboxed.** A model running under `build` can read
  `~/.ssh`, hit the network, `rm -rf` outside the root, spawn daemons. This is
  accepted for now: the permission profile (Allow/Ask/Deny) is the *only*
  gate, and the user approves every `Ask`. A real sandbox (seccomp /
  namespaces / bubblewrap / firejail) is deferred to a focused security
  follow-up and re-evaluated together with the lexical-containment gap from
  ADR-0008.
- **(−)** Lexical containment (ADR-0008) is not a security boundary — `bash`
  makes that gap load-bearing. Documented as known; sandboxing is the fix.
- **(−)** Exact-string `edit` can't do whitespace-tolerant or fuzzy replaces;
  the model must reproduce whitespace exactly. This is the opencode/Claude
  tradeoff (unambiguous, scriptable, no false matches) and is accepted.
- **(−)** `sh -c` is Unix-only in practice. Windows would need `cmd /C`; out
  of scope (the project is linux-first; the hygiene gate implies a server
  context).

## Alternatives considered

- **`edit` via regex / diff-patch / line-number ranges / full-rewrite.**
  Rejected: exact-string search/replace is the opencode and Claude convention,
  is unambiguous, requires no regex engine on the write path, and the model is
  already trained on it. Regex would surprise (greedy matches, escaping); diff
  algorithms are heavier than the value; full-rewrite loses the
  minimal-change property that makes edits reviewable in an approval UI.
- **`edit` auto-creates on a missing `oldString` match.** Rejected: an empty
  `oldString` is an *explicit* create and is refused when the file exists, so
  the model can't silently overwrite a file it meant to edit. Two distinct
  intents (modify vs. create) deserve two distinct inputs.
- **`bash` via `std::process::Command` + `spawn_blocking`.** Rejected:
  `tokio::process` is already in the dep graph (`full`), integrates with
  `tokio::time::timeout` without blocking a worker thread, and gives async
  `wait_with_output` for free.
- **`bash` via a shell crate (`duct`, `sh-rs`, `run_script`).** Rejected: no
  value over `sh -c`; each would add a dep for no hygiene-gate benefit.
- **Combined stdout/stderr stream.** Rejected: separating them (with a
  `[stderr]` marker) lets the model distinguish command output from
  diagnostics, which matters for interpreting failures.
- **Graceful shutdown (SIGTERM → grace → SIGKILL) instead of `kill_on_drop`.**
  Rejected for v1: `kill_on_drop` is correct-by-construction and needs no
  signal ladder. A graceful variant is a localized change inside `BashTool::run`
  if a real command ever needs cleanup-on-SIGTERM.
- **Sandboxing (seccomp / namespaces / bubblewrap / firejail).** Rejected as
  out of scope for this change; re-evaluated together with the ADR-0008
  lexical-containment gap in a dedicated security ADR. Swapping in a sandbox
  later changes only `BashTool::spawn`'s prelude, not the permission model or
  the wire protocol.

[0003]: 0003-agent-and-permission-profiles.md
[0006]: 0006-core-dependency-hygiene-gate.md
[0008]: 0008-host-tools-workdir-and-bounded-output.md
[host]: ../../entanglement-core/src/host.rs
