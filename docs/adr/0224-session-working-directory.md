# 0224. Session working directory (`/cd`, `Spawn.workdir`)

- Status: Accepted
- Date: 2026-10-11
- Relates to: [ADR-0054](0054-canonicalizing-symlink-safe-root-containment.md)
  (containment, unchanged), [ADR-0109](0109-escape-root-access-via-approval.md)/[ADR-0120](0120-once-scoped-escape-root-grant-bound-to-request-id.md)/[ADR-0126](0126-session-scoped-directory-grants.md)
  (escape-root approval and scopes), [ADR-0116](0116-workdir-scoped-permission-rules-for-bash-call.md)
  (workdir-scoped rules), [ADR-0125](0125-permission-arguments-for-path-tools-are-normalized-root-relative.md)
  (root-relative permission args), [ADR-0130](0130-rhai-exec-bindings-marshal-workdir.md)
  (rhai `workdir`), [ADR-0207](0207-permission-modes-replace-agent-borne-authority.md)
  §9 (the per-round trailing notice), [ADR-0205](0205-every-compaction-forks-a-successor-session.md)/[ADR-0223](0223-non-compacting-session-fork.md)
  (successors and forks), [ADR-0069](0069-trusted-untrusted-wire-frame-split.md)
  (wire allowlist)

## Context

Every session ran in the directory `skutter` started in. Working on one crate
of a workspace meant passing `workdir` on every `bash`/`call` and spelling
every path from the root (#764). The maintainer decision: an optional
`InMsg::Spawn.workdir` and a TUI `/cd <dir>`, resolving inside the project
root or inside an extra root approved through the existing escape-root
approval. It becomes the default cwd for `bash`/`call`/`rhai` exec and the
base for relative path-tool arguments. Root containment, permission grading
and path normalization stay as they are. Children inherit it, it is persisted
and replayed, and the status line warns when the focused session's workdir is
not the TUI root.

## Decision

1. **Protocol.** A trusted-only `InMsg::SetWorkdir { session, dir, approve? }`
   and `Spawn.workdir?`, answered by a point-in-time
   `OutEvent::WorkdirChanged { session, dir }` (no `seq`, like `ModeChanged`).
   `ToolExec` gains `workdir?`, the session's absolute workdir. It rides on
   the call, like `agent` (#156), so the runtime never depends on a lossy
   lifecycle fold to know where to run it. Every field is additive and
   omitted when unset, so a session that never changes directory is
   byte-identical on the wire and in its log. `SetWorkdir` is wire-refused:
   its `approve` is an escape-root approval, which only the user's own
   trusted head may give.
2. **Validation is a runtime seam.** Core does no file I/O, so
   `EngineConfig::workdir_resolver(current, dir, approve)` returns the
   validated absolute directory or a refusal. The runtime's resolver
   canonicalizes `dir` (relative to `current`, else the root), requires a
   directory, and accepts it inside the root. Outside the root it needs an
   escape-root grant for the exec tools (`bash`/`call`, the tools whose cwd
   it becomes): an existing durable one, or the `Session`/`Always` approval
   carried on the message, which it records in the shared `ExtraRootStore`.
   `Once` is refused, since a single-use token cannot back a directory every
   later command runs in. No resolver configured refuses every change. The
   supervisor calls the seam when the message arrives. It does not wait for
   the session to apply it, so a refusal is a seq-0 `Error` at once and never
   reaches the session task.
3. **Approval for `/cd` is the flag on the command.** There is no tool call
   to park an approval card on. A plain `/cd` outside the root is refused with
   a hint, and `/cd --session <dir>` or `/cd --always <dir>` resends it
   approved. The user asks once: the recorded grant then lets later `/cd`s to
   that directory, and every exec there, through silently. File tools keep
   their own per-path escape-root rules outside the root. The grant is for
   exec cwd, not a blanket read/write of the tree. Like every ADR-0109
   `Session`/`Always` grant it is process-wide, not tied to the session
   that asked: any session, including a model-spawned child, can then run
   `bash`/`call` with that directory as its workdir without asking. `call`'s
   `input_file`/`output_file` there ride the same `call` grant (a durable
   grant on the path or an ancestor), since a `call` allowed to run in that
   directory can already read and write its files through the command.
4. **Containment is re-checked at use.** The workdir is not trusted after
   validation. Every exec and path call resolves through the same
   `resolve_under_root_or_grant`/`resolve_workdir_or_grant` as before
   (ADR-0054). A workdir later swapped for a symlink out of the root
   therefore trips the escape-root gate on the next call instead of running
   there.
5. **Relative paths are rewritten, not reinterpreted.** On each `ToolExec`
   the executor rewrites relative path arguments onto the workdir *before*
   alias rewrite, grading, hooks, the escape-root gate and execution. A
   workdir inside the root yields root-relative paths, one outside it yields
   absolute paths. Every later stage sees the path exactly as the model
   would have spelled it from the root, so argument-scoped rules, grants,
   ADR-0125 normalization and ADR-0116 workdir-scoped rules apply unchanged.
   A workdir can't be used to slip a path past a rule. Per tool:
   `read`/`read_raw`/`edit`/`write`/`apply_patch` join `path`;
   `glob`/`grep` (and the rhai `_json` variants) join `path`, or default it
   to the workdir (a `glob` with an absolute `pattern` excepted); `bash`
   joins or defaults `workdir`; `call` does that plus `input_file`/
   `output_file`. Absolute paths and every other tool are untouched. rhai
   binding calls go through the same rewrite. Skill aliases are not
   rewritten: their preset arguments are authored from the root.
   `glob`/`grep` print matches relative to the workdir (absolute outside
   it), so a listed path passes straight back. They read the executor's
   per-session view (`SessionWorkdirs`, fed from each `ToolExec`) in
   `run_for_session`.
6. **The model learns it from the trailing notice.** The system prompt sits
   in the provider's cached prefix, so a mid-session `/cd` must not rewrite
   it. A note pushed into `Context` would land on the wrong side of a
   session's first prompt on replay (the pairing hazard in `mode.rs`).
   Instead the per-round trailing notice, rebuilt from session state every
   round and never persisted, gains
   `[workdir: <dir> — relative paths and bash/call commands resolve here]`
   while a workdir is set. Replay only restores the field. The `<env>`
   block's root line is untouched.
7. **Turn-stable and inherited.** `SetWorkdir` is deferred while a turn is
   live or paused (stash replay, like `SetModel`), so every call of a turn
   runs under one workdir. The supervisor caches the latest validated workdir
   per session. A child spawned later inherits it, and an explicit
   `Spawn.workdir` resolves against it. Existing children keep theirs: a `/cd`
   is the user's navigation in one session, not a move of running
   sub-agents. A compaction successor and a user fork carry it on
   `Inherited` (already validated). On resume the supervisor re-runs the
   resolver on the replayed workdir with no approval: a directory that is
   gone, was swapped for a symlink out of the root, or was covered only by a
   `Session` grant that died with the old process falls back to the project
   root, with an `Error` saying why. Either way the session re-announces its
   workdir, so a reconnecting head learns it and the log records the
   fallback.
8. **TUI.** `/cd [--session|--always] [<dir>]`: bare `/cd` returns to the
   project root, and `~` expands. The status line gains a `{workdir}`
   placeholder (relative to the TUI root, absolute outside it, empty while
   they agree) and a fixed `⚠ cd <dir>` attention counter. The status
   command payload gains `workdir`. `!bash` runs in the focused session's
   workdir.

## Consequences

- One rewrite point serves every grading, gate and hook stage. The cost is
  that approval cards and hooks show the root-relative path rather than the
  model's literal argument. That is the more honest thing to approve.
- `write`/`edit` result text names the root-relative path the call resolved
  to. Only the search tools' listings are relativized.
- A `Session`-scoped out-of-root grant lives for the process, as every
  ADR-0109 `Session` grant does. After a restart, resume finds it gone and
  returns the session to the root; `/cd --session` asks again.

## Alternatives rejected

- **Validate in the session task.** That would wait behind a live turn, and
  a child spawned meanwhile couldn't inherit it. The supervisor already calls
  the model resolver synchronously for `Spawn.model` (#725).
- **A head-side approval card for `/cd`.** No tool call exists to park it on.
  A synthetic `ToolRequest` would need special-casing in every approval path
  and the persistence tap. A scope flag on a trusted frame reuses the store
  and the scopes with none of that.
- **Per-tool workdir resolution inside each host tool.** Every path tool and
  the permission layer would each need the workdir, with a second place for
  grading and execution to disagree.
- **Telling the model through the system prompt or a `Context` note.** The
  first busts the prompt cache, and the second desyncs replay (point 6).
