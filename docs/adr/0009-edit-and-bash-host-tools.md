# 0009. Host tools: `edit` (search/replace) and `bash` (subprocess + timeout)

- Status: Accepted
- Date: 2026-07-07

## Context

[ADR-0008][0008] set the shared design of the host tools (root containment,
bounded output, the `schema()` seam) and shipped the read-only trio
(`read`/`glob`/`grep`). This ADR adds the mutating/executing pair — `edit` and
`bash` — each of which brings its own hard-to-reverse decisions: edit's replace
semantics (exact-string vs. regex vs. diff vs. full-rewrite), and bash's process
model (timeout, kill behavior, sandboxing). Both live in `entanglement-runtime`
alongside the trio ([ADR-0006][0006], [ADR-0010][0010]) and plug into the same
`host_tools(root)` builder and the same permission model ([ADR-0003][0003]):
`build` auto-allows both, `plan` asks, `explore` denies.

## Decision

### 1. `edit` — exact-string search/replace (opencode/Claude convention)

`EditTool` takes `{path, oldString, newString, replaceAll?}` and:

- **`oldString == ""` creates the file** with `newString` as content, refused if
  the path already exists (so the model can't clobber a file it meant to
  modify). Parent directories are created.
- **`oldString` must match exactly once** unless `replaceAll: true`. Zero matches
  error; N>1 without `replaceAll` errors with the count so the model
  disambiguates. `replacen(.., 1)` / `replace` do the substitution.
- Reuses `resolve_under_root` (`..` escape rejected, lexical containment only —
  [ADR-0008][0008]) and writes UTF-8 only.

### 2. `bash` — `sh -c` subprocess with timeout, **not** sandboxed

`BashTool` takes `{command, timeout?}` and:

- Runs `sh -c "<command>"` with `current_dir(root)`. `root` sets the **cwd
  only** — it is explicitly *not* a sandbox; the process inherits the runtime's
  full filesystem/network/process privileges. Permission profiles gate whether
  `bash` runs at all, and registration is opt-in ([ADR-0010][0010]).
- **Timeout:** default 120 s (matches opencode); model-supplied `timeout`
  clamped to `[1, 600]`. On expiry the child is dropped from
  `tokio::time::timeout`, and **`kill_on_drop(true)`** reaps it.
- **Output:** `[exit N]\n<stdout>\n[stderr]\n<stderr>` — streams separated so the
  model tells diagnostics from output; exit code prefixed. Run through
  `truncate_output` (32 KiB cap, [ADR-0008][0008]).

### 3. No provider/core deps; tools live in the runtime

`bash` uses `tokio::process::Command` (already in the runtime's `tokio` with
`full`); `edit` uses `tokio::fs`. Neither adds a crate to `entanglement-core` —
the tools and their deps live in `entanglement-runtime`, so core's hygiene gate
([ADR-0006][0006]) is untouched.

### 4. Same module, same builder as the trio

Each holds a `root`, returns a short model-facing summary string, and ships a
JSON `schema()` on its `Tool` impl. The runtime executes them and returns the
output to the engine's turn loop over the protocol.

## Consequences

- **(+)** `build` can write code and run commands; `plan` can ask to; `explore`
  stays read-only. Useful for real coding work, not just inspection.
- **(+)** `kill_on_drop` makes bash timeout cleanup correct by construction — no
  orphaned children, no manual signal ladder.
- **(−)** **`bash` is not sandboxed.** A model under `build` can read `~/.ssh`,
  hit the network, `rm -rf` outside the root, spawn daemons. Accepted for now:
  the permission profile is one gate and registration is opt-in
  ([ADR-0010][0010]); the user approves every `Ask`. A real sandbox (seccomp /
  namespaces / bubblewrap / firejail) is deferred to a focused security ADR,
  re-evaluated with the lexical-containment gap from [ADR-0008][0008].
- **(−)** Exact-string `edit` can't do whitespace-tolerant/fuzzy replaces; the
  model must reproduce whitespace exactly (the opencode/Claude tradeoff —
  unambiguous, no false matches).
- **(−)** `sh -c` is Unix-only in practice; Windows would need `cmd /C`. Out of
  scope (linux-first).

## Alternatives considered

- **`edit` via regex / diff-patch / line ranges / full-rewrite.** Rejected:
  exact-string is the opencode/Claude convention — unambiguous, no regex on the
  write path, and the model is trained on it. Full-rewrite loses the
  minimal-change property that makes edits reviewable in an approval UI.
- **`edit` auto-creates on a missing match.** Rejected: an empty `oldString` is
  an *explicit* create, refused when the file exists; modify vs. create deserve
  distinct inputs.
- **`bash` via `std::process` + `spawn_blocking`.** Rejected: `tokio::process`
  integrates with `tokio::time::timeout` without blocking a worker.
- **`bash` via a shell crate (`duct`, `sh-rs`).** Rejected: no value over
  `sh -c`; adds a dep for nothing.
- **Graceful shutdown (SIGTERM → grace → SIGKILL).** Rejected for v1:
  `kill_on_drop` is correct-by-construction; a graceful variant is a localized
  change inside `BashTool::run` if a command ever needs cleanup-on-SIGTERM.
- **Sandboxing now.** Deferred to a dedicated security ADR; swapping in a sandbox
  later changes only `BashTool::spawn`'s prelude, not the permission model or the
  wire protocol.

[0003]: 0003-agent-and-permission-profiles.md
[0006]: 0006-core-dependency-hygiene-gate.md
[0008]: 0008-host-tools-workdir-and-bounded-output.md
[0010]: 0010-single-head-crate-and-bash-opt-in.md
