# Gates & host tools §8 — Host tools: the tool set, containment & sandbox

> Part of the [gates & host tools module](../gates-and-host-tools.md) of the [architecture overview](../../architecture.md).

## 8. Host tools — [ADR-0008](../../adr/0008-host-tools-workdir-and-bounded-output.md) (trio), [ADR-0009](../../adr/0009-edit-and-bash-host-tools.md) (`edit`/`bash`), [ADR-0010](../../adr/0010-single-head-crate-and-bash-opt-in.md) (`bash` opt-in), [ADR-0045](../../adr/0045-call-host-tool-argv-exec-tailed-output.md) (`call`), [ADR-0092](../../adr/0092-call-file-based-stdin-stdout.md) (`call` file-based stdin/stdout), [ADR-0093](../../adr/0093-call-registration-independent-of-bash-opt-in.md) (`call` always-registered + `workdir`), [ADR-0104](../../adr/0104-bubblewrap-sandbox-for-bash-call.md) (optional bubblewrap confinement), [ADR-0134](../../adr/0134-per-profile-sandbox-scoping-and-spawn-chain-clamp.md) (per-profile scoping + spawn-chain clamp)

Concrete filesystem + shell tools, dispatched under the active permission
profile ([ADR-0003](../../adr/0003-agent-and-permission-profiles.md)). The
`Tool` **trait** and `ToolRegistry` live in **`entanglement-runtime`**
(`entanglement-runtime::tools`, ✅ #206, [ADR-0059](../../adr/0059-tool-trait-and-registry-live-in-the-runtime.md)) —
core holds no executable tools, only advertises schemas (§tool round-trip);
the implementations live in **`entanglement-runtime::host`**
(✅ #57) and are assembled by `host_tools(root: PathBuf) -> ToolRegistry`.
Execution *and* permission dispatch now run in the runtime (✅ #58, #59):
`entanglement-runtime::tool_runner` subscribes to the engine, resolves each
`ToolExec`'s `Allow|Ask|Deny` against the session's active profile (§3), runs the
cleared tool against the registry, and replies with `InMsg::ToolResult`.
`ToolRegistry::execute(&self, call: &ToolCall, session: &SessionId)` threads the
caller's `SessionId` through to `Tool::run_with_meta` (#681,
[ADR-0186](../../adr/0186-exit-code-joins-the-structured-tool-result-side-channel.md))
— a defaulted method returning `ToolRun{content, exit_code, is_error}` that
delegates to `Tool::run_for_session` (#360,
[ADR-0088](../../adr/0088-session-aware-tool-execution.md)) with `exit_code: None`
and `is_error: false`, so only the process runners (`bash`/`call`) override it
to surface their numeric exit status as a real field, and `McpTool` to carry a
server's in-band `isError` onto the ADR-0176 `is_error` channel (#727).
`ToolRun` derives `Default` and has `ToolRun::new(content)`; implementors build
it with struct-update syntax so a future metadata field is non-breaking.
`run_for_session` itself stays a default-delegating
method (falls back to `run_content`) so every in-tree tool is unaffected; a
multi-tenant embedder overrides it to dispatch per-tenant MCP endpoints or scope
a DB-backed tool's writes to the caller, since a shared `ToolRegistry` otherwise
can't tell tenants apart at execution time even though the executor's
`.resolver(..)` seam (#311) already resolves *permission* per session. `Ask`
emits the `ToolRequest` prompt and waits for the head's decision on
`Holly::subscribe_inbound()` (the engine's inbound `InMsg` fan-out). The executor
is **idempotent by `request_id`** (✅ #274,
[ADR-0071](../../adr/0071-parked-turn-reoffer-timer.md)): it keeps a per-session set
of **in-flight** request ids — dispatched but not yet resolved — and skips a
`ToolExec` whose id is still in flight, so core's re-offer timer (which re-emits a
parked batch after `reoffer_interval` of silence to recover an offer dropped
under `broadcast` lag, see [engine.md](../engine.md)) never double-runs a call it is
already executing. An id is dropped again on the resolving `ToolOutput` (and on
`SessionEnded`), so a later round that reuses the id still dispatches. Core only
advertises the tool *schemas* (`EngineConfig.tool_specs`) — it holds no executable
tools and makes no policy decision.

**Unknown-tool short-circuit (#437).** A hallucinated tool name is checked
against the freshly-snapshotted registry at the **top** of `dispatch`, before
permission resolution or the `pre_tool_use` hook (§9) run — a name the registry
doesn't hold (and isn't a state tool — `update_tasks`, exempt since it's never
registered, [ADR-0049](../../adr/0049-plan-task-tools-as-runtime-state-tools.md);
`propose_plan` never reaches `dispatch` at all, intercepted earlier like
`ask_user`/the spawn family) can never execute, so it would be
pointless to prompt the user for `Ask` approval, run a hook that could veto it,
or let an `Always`-scoped approval record a grant for it. (The former
carve-out for an unregistered-but-advertised `bash` is gone: ADR-0195
registers `bash` at startup, so there is no advertised-but-unregistered
built-in whose decline needed its own wording. A call to a tool that is
**registered but unadvertised** — the script-only `call`/`glob`/`grep` — is
not "unknown": ADR-0192 already dispatches any registered tool regardless of
advertisement, so it reaches permission resolution normally. An unregistered
`mcp__<server>__*` name connects its server lazily first (§10, [ADR-0226](../../adr/0226-one-advertised-surface-mcp-envelope-registered-is-available.md)); a
malformed `mcp` envelope is declined with the envelope's schema. Only a
name the registry genuinely doesn't hold falls to this short-circuit, whose
fuzzy-match hint is the unknown-tool leg of [ADR-0196](../../adr/0196-tool-search-and-lazy-discovery-replace-the-invoke-envelope.md)'s
error taxonomy, [one surface](one-surface.md).)
`ToolRegistry::unknown_tool_message` backs both this short-circuit and `execute`'s own
registry-miss fallback: it enriches `unknown tool: `name`` with a closest-match
hint (smallest Levenshtein distance over the registered names, capped so a
wildly different name surfaces no hint) plus the full name list when the
registry is short, so a weak model can self-correct in one round instead of
guessing again:

| tool | input | output |
| --- | --- | --- |
| `read` | `{path, offset?, limit?}` | text file → contents as `{lineno}: {line}`, 1-based, line-ranged; an **image file** (`.png`/`.jpg`/`.jpeg`/`.gif`/`.webp`, by extension) → a base64 **image content block** the provider renders natively (Anthropic `image` / OpenAI `image_url`), routed through the `ToolResult` `content` path (`offset`/`limit` ignored) — #221 |
| `glob` | `{pattern, path?, exclude?}` | matching paths (relative to root), one per line; a metachar-free `pattern` naming an existing directory lists it recursively (`dir/**/*`, containment-gated), `{a,b}` brace sets expand (both [ADR-0150](../../adr/0150-search-tool-cli-ergonomics.md)), `path` is an optional base dir the pattern resolves under; `.git` is always excluded (path-component check plus walker pruning, can't be disabled) and so is anything `.gitignore`'d — root or nested `.gitignore`, `.git/info/exclude`, the user's global excludes file: the walk *is* an `ignore`-crate traversal that prunes ignored subtrees before descending (so they cost nothing), matches each entry against the compiled glob pattern with iterator-parity shims, survives symlink loops, and stops after a 100k-entry total-scan budget (`scan_capped`, reported as a "narrow the pattern" notice) — an extra-root-granted walk root, below, disables `.gitignore` filtering so a granted directory's own ignores can't hide what the grant exposed ([ADR-0170](../../adr/0170-gitignore-aware-glob-grep-walk.md) semantics, single-pass + bounded mechanism [ADR-0178](../../adr/0178-single-pass-gitignore-pruned-walk-with-scan-budget.md), #629/#678) — `exclude` adds caller glob patterns on top of all of that ([ADR-0099](../../adr/0099-glob-grep-exclude-and-default-git-exclusion.md)); unknown input fields are refused (`deny_unknown_fields`) and a zero-result call always returns a one-line explanation, never the empty string |
| `grep` | `{pattern, path?, exclude?, case_insensitive?}` | matches as `path:lineno:line` over files matched by `path` — a **directory (searched recursively)** or a glob filter, brace sets included (default `**/*`, [ADR-0150](../../adr/0150-search-tool-cli-ergonomics.md)) — minus `exclude`, the always-on `.git` exclusion, and `.gitignore` ([ADR-0099](../../adr/0099-glob-grep-exclude-and-default-git-exclusion.md), [ADR-0170](../../adr/0170-gitignore-aware-glob-grep-walk.md)); `case_insensitive` (serde alias `"-i"`) complements the inline `(?i)`; unknown input fields are refused; a zero-match call names its cause (*"path filter matched no files — nothing was searched"* vs *"no matches for `X` in N file(s) scanned"*); a file over the 1 MiB **scan** cap (independent of the 32 KiB output cap, [ADR-0091](../../adr/0091-grep-file-scan-size-cap-decoupled-from-output-cap.md)) or sniffed as binary (a NUL byte in its content) is skipped and named in a labeled notice appended to the result — regardless of match count |
| `edit` | `{path, oldString, newString, replaceAll?}` | exact-string replace; empty `oldString` creates (refused if exists → hints `write`); non-unique match errors unless `replaceAll` |
| `write` | `{path, content}` | whole-file create/overwrite; missing parent dirs created; `created <path> (N lines)` / `overwrote <path> (N lines, was M)` — confirmation only, never echoes content (ADR-0031) |
| `apply_patch` | `{path, patch}` | apply a unified diff (one or more `@@ -oldStart,oldLen +newStart,newLen @@` hunks) against the current file; each hunk's context/deleted lines are matched **exactly** at the position its header declares (offset by the net line-count delta of hunks already applied in the same patch) — no fuzzy alternate-position search, a mismatch hard-errors before any write and leaves the file untouched; emits `FileChangeKind::ApplyDiff` (#455, the first producer of that previously-reserved variant). Parsing/applying is a small hand-rolled module (`host::unified_diff`), not the `diffy` crate — `diffy` is `tui`-feature-gated and named in `LEAN_FORBIDDEN` ([hygiene gates](hygiene-gates.md)), and `apply_patch` is unconditional lean-library code alongside `edit`/`write` |
| `bash` ⚠ | `{command, timeout?, workdir?, background?, tail?}` | `sh -c` rooted at root (or at `workdir`, a subdir validated under root by the same symlink-safe containment as the fs tools, #170); `[exit N]` + stdout + `[stderr]`; default 120 s timeout, capped at 600; spawned in its **own process group** (`process_group(0)`) so an expiry SIGKILLs the whole tree — grandchildren (a launched server/pipeline) can't orphan (#168); a `Stop`-driven task abort drops the wait future, whose group-kill guard SIGKILLs the same group so cancellation matches the timeout's containment rather than orphaning under bare `kill_on_drop` (#167). Output is drained incrementally, so a timeout returns the **partial output buffered before the kill** under a `[killed: timed out after Ns]` header instead of discarding it (#169). Output is tailed to the last `tail` lines per stream (default 30, matching `call`; `tail=0` = full, byte-cap still applies, #622), then the exit/killed status line plus that body is byte-capped **head + tail** (¼ head / ¾ tail on the body, status line always kept in full — `bounded_result`) so the trailing error survives — head-only truncation dropped exactly what a failing build needs (#170). `background: true` (renamed from `run_in_background`, #606, [ADR-0161](../../adr/0161-unified-async-work-background-flag-and-one-poll.md) §1) spawns the command **detached** and returns a job id instead of blocking — wait on it with the runtime-owned `poll` tool (#605, described in [host-tools contracts](host-tools-contracts.md)); **`timeout` still applies** to a backgrounded job ([ADR-0165](../../adr/0165-background-bash-jobs-are-bounded-by-timeout.md), #617) — a deadline task kills the job's process group once it outlives the same default-120s/600s-cap bound a foreground call uses, so backgrounding no longer means unbounded. Stdin is always closed (`Stdio::null()`), never inherited from the engine — the same leaked-by-default class ADR-0092 closed for `call`, applying uniformly to both the foreground and `background` paths since both share the one command builder (#389); use shell-native `< file` redirection if a command needs input. Its spec description is the **single home of the exec-tool decision rule** (#740) — `bash` for shell syntax, `call` for one fixed argv command, `rhai` for multi-step/structured-data logic (that sentence compiled in only with the `rhai` feature), all graded by the same mode — because `bash` is the kernel's shell tool and its description rides the cached tools block every session sees; no shipped agent body repeats it (`built_in_bodies_leave_the_exec_rule_to_the_bash_spec`) |
| `call` ⚠ | `{command, args?, tail?, timeout?, input_file?, output_file?, workdir?, background?}` | **argv, no shell** — `command`+`args` exec verbatim (no `sh -c`, so no pipe/glob/`$VAR`/metachar interpretation); output tailed to the last `tail` lines per stream (default 30, `tail=0` = full, byte-cap still applies), with a `(… N earlier lines omitted, tail=30 — rerun with tail=0 …)` notice; the outer byte cap is head + tail on the body, not head-only (`bounded_result`, #622) — same shape `bash` uses; same envelope as `bash` (`[exit N]` + stdout + `[stderr]`, 120 s/600 s, own-process-group kill on timeout #168, partial output preserved on timeout #169) — ADR-0045. `input_file`/`output_file` (ADR-0092, #381), both root-contained via `resolve_under_root` and validated **before spawn** (relative to the **root**, not `workdir`): `input_file` is read and piped to the child's stdin (fed concurrently with the stdout/stderr drain to avoid a full-pipe deadlock); its **absence closes stdin** (`Stdio::null()`) rather than inheriting the engine's own (a leaked-by-default behavior until now). With an explicit `output_file`, the full **untruncated raw** stdout is persisted there (missing parent dirs created) with a `<output_file>.stderr` sibling always alongside, and its path is named in the result header (`[output: …]`, plus `[stderr: …]` when stderr is non-empty) regardless of truncation; a write failure there is a hard error. With **no** `output_file`, nothing is written to disk — a result that overflows the tail/byte cap instead mints a fresh retained-output handle (`o-…`, ADR-0164) in `RetainedOutputRegistry` and names *that* in the header instead of a path (#608, [ADR-0161](../../adr/0161-unified-async-work-background-flag-and-one-poll.md) §7); a small, untruncated result carries no header at all. Either way, **a truncated result keeps its handle** — poll it (`poll`, described in [host-tools contracts](host-tools-contracts.md)) to page the retained remainder, or to be reminded of the `output_file` path. `workdir` (#386) sets the child's **cwd** to a subdirectory validated under root via the shared `resolve_workdir` (same containment as `bash`'s); a non-directory or escaping `workdir` errors before spawn. A `command` that is really a whole shell line — multiple whitespace-separated tokens with empty `args` (a non-path `command`), or any shell metachar (`| & ; < > $ \` ( )`) — is rejected **before spawn** by `call::validate::check_no_shell` with an actionable error that names the fix (split the line into `command`+`args`, or use `bash` for a real pipeline), rather than failing with an opaque ENOENT on a "binary" named after the entire line (the loop that stranded a delegated PR review). **Registered unconditionally** ([ADR-0093](../../adr/0093-call-registration-independent-of-bash-opt-in.md); since [ADR-0195](../../adr/0195-bash-is-the-default-exec-and-curated-read-only-rules.md) `bash` registers unconditionally too — and since [ADR-0226](../../adr/0226-one-advertised-surface-mcp-envelope-registered-is-available.md) `call` is never advertised: registered, reachable as a `rhai` binding, graded under its own name). `background: true` (#606, ADR-0161 §1) spawns detached into the same shared job registry `bash` uses (`CallTool::with_jobs`) and returns a job id to `poll` instead of blocking; refused up front alongside `input_file`/`output_file` — a backgrounded job's output streams through `poll`, not a file artifact, and there is no running wait left to pipe stdin into once the call has already returned |
| `rhai` | `{script` XOR `path, args?, timeout?, background?}` | behind the crate's default-on `rhai` feature (#502, [ADR-0135](../../adr/0135-deferred-build-speed-trims-tokio-rhai-syntect.md) amending ADR-0025). The **model-facing spec is self-sufficient**: it inlines the full binding signature list plus a chained-call example (`script/spec.rs`'s `BINDING_REFERENCE`, pinned to the registration code by `spec_description_lists_every_registered_binding`; #826 measured that GLM models only chain calls inside one script when shown how); the embedded `rhai` skill (`skills/rhai.md`) keeps the long-form prose and worked examples, and the same reference is appended to an unknown-name script error (`spec::binding_hint`). Exactly one of inline `script` or a script file `path` ([ADR-0228](../../adr/0228-rhai-path-and-args-skill-tools-frontmatter-removed.md), §`rhai(path)` in [script surface](script-surface.md) — the launch is graded like any tool on its `path`; a `rhai.trusted` script runs with no card, an `Ask` grade gets one Run all / Ask per command / Reject card); `args` (an object) is the constant map `args` in either. A **capability-sandboxed** engine — no fs/network/process/env access beyond what is bound. **Every binding call goes through the executor's dispatch seam** (`Dispatcher::run` with `Via::Script`, [ADR-0227](../../adr/0227-one-binding-path-namespaces-http-skill-libraries.md)): mode grade, overlay, ceiling, grants, escape-root (ADR-0109/[0119](../../adr/0119-rhai-bindings-route-through-the-escape-root-gate.md)), `pre_tool_use`/`post_tool_use` hooks, pre-dispatch arg validation, lazy MCP connect (bounded by the script's remaining budget, #831), `FileChange` capture, the in-call grader — so a binding grades, runs and is observed exactly like a direct call; the nested call's approval card is `ApprovalKind::Rhai`, tool `<tool> (rhai)`, request id `<rhai id>:rhai:<tool>`. Bindings: `read`/`read_raw` (exact content; grades as `read`)/`glob`/`grep`/`glob_json`/`grep_json` (structured, ADR-0206; grade as `glob`/`grep` via `grade::graded_name`)/`edit`/`write`; `exec(command[, args[, workdir]])` (the `call` tool — `call` is a reserved Rhai keyword) and `bash(command[, workdir])` (bound only when `bash` is registered), both with a `timeout` derived from the script's remaining budget (#419) and an explicit `workdir` ([ADR-0130](../../adr/0130-rhai-exec-bindings-marshal-workdir.md)); `http(method, url[, #{headers, body}])` (§`http()` in [script surface](script-surface.md)); the generated `mcp::<server>::<tool>(#{…})` and `skill::<skill>::<fn>(…)` namespaces (§[Script surface](script-surface.md)); pure `parse_json`/`to_json`/`parse_yaml`/`to_yaml`. The seam's context is captured at script launch (the `Dispatcher` is a per-run snapshot: registry, chain, overlays, mode). The per-run approval cache keys on **tool + full input** for every binding; every other nested call — except the file-mutating `write`/`edit`/`apply_patch` — runs under a bridge timeout clamped to the remaining budget less a short grace, and a cut call reports its outcome as unknown (`Outcome::TimedOut`), never "never ran"; a refusal throws (catchable), a run returns its text — `http()`/MCP failures throw too. Nested `rhai`, the `mcp` envelope and every runtime-owned/`Control` tool are unreachable (`dispatch::script::unreachable`). Last-expression value serialized + captured `print(...)`; bounded by op/string/array/map caps + wall-clock (default 5 s, max 30). `background: true` (#637, [ADR-0185](../../adr/0185-rhai-joins-background-and-poll.md)) detaches after the launch gate and returns an `x-` handle to `poll`: the `bash`/`call` background regime (default 120 s, cap 600), live `print` streaming into the `ScriptRegistry`, survives a session `Stop`, keeps mid-run binding `Ask`s with session-state transitions suppressed; `poll kill=true` is cooperative — [ADR-0046](../../adr/0046-rhai-sandboxed-script-tool.md) (amended by [ADR-0115](../../adr/0115-rhai-exec-bindings-call-bash.md), [ADR-0130](../../adr/0130-rhai-exec-bindings-marshal-workdir.md), [ADR-0227](../../adr/0227-one-binding-path-namespaces-http-skill-libraries.md), [ADR-0228](../../adr/0228-rhai-path-and-args-skill-tools-frontmatter-removed.md)), [ADR-0098](../../adr/0098-rhai-json-yaml-loader-and-read-raw.md) |

- **Working directory:** each tool holds a `root` (the cwd, **canonicalized once
  at startup**); model-supplied paths resolve against it and are rejected on `..`
  escape **and on symlink escape** — `resolve_under_root` canonicalizes the
  resolved target's deepest existing ancestor and requires it under the canonical
  root, so a `root/link -> /etc` symlink can't be followed out of tree by
  `read`/`edit`/`write`/`apply_patch` (the create path still works: only the existing ancestor
  is canonicalized), and `glob`/`grep` (`list_files`) drop any match whose
  canonical path escapes — ADR-0008 upgraded by [ADR-0054](../../adr/0054-canonicalizing-symlink-safe-root-containment.md)
  (#163), unless a durable escape-root grant widens it (#482, below). Not
  TOCTOU-tight (an OS sandbox via `openat2(RESOLVE_BENEATH)` is deferred).
- **Approval-gated escape (ADR-0109):** containment is no longer absolute — a
  `read`/`edit`/`write`/`apply_patch` path, a `rhai` script `path` (ADR-0228) or a `bash`/`call` `workdir` that resolves *outside*
  root can be reached after the user explicitly approves it. The executor detects
  the out-of-root target (`permission::escape_root_target` + `host::escaping_path`),
  forces an approval prompt even when the profile would `Allow` (a `Deny` floor
  still wins), and records the grant in a shared `ExtraRootStore`
  (`extra_roots.rs`; managed `extra-roots.yml` in the managed-state dir,
  `ENTANGLEMENT_STATE_DIR`) keyed by `(tool, resolved-absolute-path)` —
  **per tool** (a `read` grant never unlocks `write`) at `Once`/`Session`/`Always`
  scope. `Once` is additionally bound to the approving call's `request_id`
  (#449, [ADR-0120](../../adr/0120-once-scoped-escape-root-grant-bound-to-request-id.md)):
  per-call executor tasks are detached and run concurrently, so without that
  binding a single-use token approved for one call could be spent by a
  different in-flight call to the same `(tool, path)`; `Session`/`Always`
  still match `(tool, path)` alone, since a durable grant is meant to cover
  every later call. `Tool::run_for_session` carries the `request_id` (the
  `ToolCall.id` `ToolRegistry::execute` already had) into the six
  escape-root-capable host tools for this. The host tools consult the store via
  `resolve_under_root_or_grant` to relax containment for the approved path
  (checked against the symlink-resolved target). No store wired (`None`) is
  byte-identical to strict containment. **`glob`/`grep` reach outside root
  through a distinct, narrower mechanism** (#482,
  [ADR-0132](../../adr/0132-glob-grep-escape-root-search-via-durable-grant.md)
  amending ADR-0109): unlike the six-tool path above, a search never forces
  its own `Ask` — the executor's `escape_root_target` still returns `None` for
  `glob`/`grep`, so it never trips the gate at all. Instead
  `list_files_with_extra_roots` widens its containment check per match: a
  match whose canonical path escapes root is admitted anyway when it (or an
  ancestor of it) already carries a **durable** (`Session`/`Always`) `read`
  grant (`ExtraRootStore::is_durably_allowed_under`, an ancestor walk over the
  existing per-tool grant set — no new store, no enumeration API). A `Once`
  grant never widens a search, only `read`/`edit`/etc.'s own exact-path check
  — a search's match count is unbounded ahead of time, so treating it as
  consuming a single-use token would let one approval cover arbitrarily many
  reads. `GlobTool`/`GrepTool` gained the same `with_extra_roots` builder the
  other four escape-root tools have; `host/mod.rs`'s `list_files`/`FileList`
  moved into `host/walk.rs` to stay under the 400-line file cap. `rhai`'s file/exec bindings route through the identical (six-tool) gate
  (#446, [ADR-0119](../../adr/0119-rhai-bindings-route-through-the-escape-root-gate.md)):
  `service_binding` forces the same approval + warning for a first-time
  out-of-root binding call and records the grant into the same
  `ExtraRootStore` — keyed by the binding's own `bind_rid`, threaded into the
  delegated call so a script-obtained `Once` grant is redeemed by that exact
  binding invocation too — so a script is no more (and no less) able to escape
  root than an equivalent direct tool call. See
  [ADR-0109](../../adr/0109-escape-root-access-via-approval.md). `bash`/`call` set only the **cwd** (root, or `workdir` if given,
  through the shared `resolve_workdir` helper both tools call) and run with the
  engine's full privileges **by default** — unsandboxed unless opted in
  (ADR-0009/ADR-0045); permission profiles gate whether they run at all. `call`
  is the injection-free sibling: a fixed argv can't be shell-injected, so a
  profile may `Allow` `call` while keeping `bash` at `Ask`/`Deny` — and, since
  [ADR-0093](../../adr/0093-call-registration-independent-of-bash-opt-in.md),
  `call` is registered regardless of whether `bash` is even opted in.
- **Session working directory** (#764,
  [ADR-0224](../../adr/0224-session-working-directory.md), `workdir.rs`): a
  session's `/cd` directory (core's `ToolExec.workdir`) is the default cwd of
  `bash`/`call`/`rhai` exec and the base of relative path arguments. The
  executor applies it as a **rewrite of the call's input** on each `ToolExec`,
  before grading, hooks, the escape-root gate and execution
  (`workdir::rewrite`; a rhai binding on the script bridge before it enters the seam):
  `read`/`read_raw`/`edit`/`write`/`apply_patch` and `rhai`'s script `path`
  ([ADR-0228](../../adr/0228-rhai-path-and-args-skill-tools-frontmatter-removed.md)) join a relative `path`;
  `glob`/`grep` (+ `_json`) join `path` or default it to the workdir;
  `bash` joins or defaults `workdir`; `call` also joins `input_file`/
  `output_file`. Inside the root the result is root-relative, outside it
  absolute — so ADR-0125 normalization, argument- and workdir-scoped rules,
  grants and containment see exactly what they would without a workdir, and
  containment is re-checked at every use (a workdir later swapped for a
  symlink out of the root trips the escape-root gate). `glob`/`grep` list
  matches relative to the workdir (absolute outside it) via the shared
  `SessionWorkdirs` view the executor fills from each `ToolExec`. The
  runtime's `workdir::resolver` (core's `workdir_resolver` seam) canonicalizes
  a `/cd` target, requires a directory inside the root, or — outside it — a
  durable `bash`+`call` grant in the `ExtraRootStore`, which a
  `Session`/`Always` approval on `SetWorkdir` records (`Once` is refused).
  That grant is process-wide (ADR-0109): any session, model-spawned
  children included, may then run `bash`/`call` there unasked. `call`'s
  `input_file`/`output_file` outside the root resolve under a durable `call`
  grant on the path or an ancestor (`call::output::resolve_call_file`) —
  except a sandboxed call's `output_file`, which the unsandboxed host process
  writes, so it keeps strict root containment under bubblewrap.
  File tools keep their per-path escape-root rules outside the root.
- **The trusted scratch dir is exempt from the escape-root prompt** (#524,
  [ADR-0142](../../adr/0142-trusted-scratch-dir-and-plans-folder-carve-outs.md),
  amends ADR-0109): the runtime-owned per-project scratch dir
  (`SessionStore::scratch_dir`, already the default `call`-output target)
  needs no approval and no prior grant, for `read`/`edit`/`write`/
  `apply_patch`, `glob`/`grep`, and a `bash`/`call` `workdir` — in every
  profile. `ExtraRootStore` gains an optional `scratch: Option<PathBuf>`
  (`.with_scratch`, set once at startup), consulted by `is_durably_allowed`
  and `take_allowance` **before** the ordinary per-`(tool, path)` grant
  lookup: a `starts_with` check against the canonicalized scratch path, not
  the exact-match key every other grant in the store uses, so every file
  under it is covered with no per-file approval. It composes for free with
  the `is_durably_allowed_under` search-widening above (ADR-0132) — no
  separate wiring for `glob`/`grep`. Not per-tool (a trusted *location*, not a
  one-off tool approval) and never persisted (re-derived from the cwd at
  every startup, nothing to revoke). Only the escape-root tax disappears — the
  session mode's own grade for the call (e.g. `research` mode's `default:
  prompt` for a `bash` command outside its spliced read-only exec allow list,
  such as `cargo build`) is untouched, since the scratch dir being the
  `workdir` says nothing about the command being run there. The generated `<env>` system-prompt block
  (`system_prompt::EnvBlock`) names the scratch dir and steers the model to
  prefer it over `/tmp`.
- **The trust boundary for exec, stated plainly:** root containment applies to
  a `bash`/`call` invocation's **`workdir` only, never its command body** —
  `escape_root_target` inspects no command line, so `bash: allow` (or a live
  tool-overlay `allow: true` entry, #611/ADR-0163) hands the model the engine
  process's full local privileges: it can read/write any path the process can, outside root
  included. The layers that actually bound an exec tool are the session's
  permission mode (whether/what it may run), the config ceiling (#172), and
  the opt-in bubblewrap sandbox below — not filesystem containment, which
  governs only the six path-arg tools.
- **OS sandbox, opt-in and now a mode fact (#399/#479,
  [ADR-0104](../../adr/0104-bubblewrap-sandbox-for-bash-call.md), scoping
  superseded by
  [ADR-0207](../../adr/0207-permission-modes-replace-agent-borne-authority.md)
  §6):**
  `ENTANGLEMENT_SANDBOX=bwrap` confines every `bash`/`call` spawn under
  bubblewrap by default — `--ro-bind / /` plus the project
  root re-bound read-write at the same path (so `resolve_under_root`'s
  containment above keeps working unmodified inside the sandbox), a fresh
  `/tmp`/`/dev`/`/proc`, and its own pid/ipc/uts/cgroup namespaces.
  `--unshare-net` cuts network by default; `ENTANGLEMENT_SANDBOX_NETWORK=1`
  shares the host network namespace back in. **Fail-closed by omission**: there
  is no fallback to unsandboxed execution when `bwrap` can't be entered (missing
  binary, unprivileged user namespaces disabled) — the spawn simply errors, like
  any missing binary (ADR-0016). Confinement is a **mode** fact now, not a
  per-agent one: each built-in mode carries `sandbox`/`sandbox_network` YAML
  keys, but none of the four ships `sandbox: bwrap` — declaring it in every
  built-in would make confinement mandatory instead of opt-in, which breaks
  ordinary work (a confined `git push` fails on `/etc/ssh/ssh_config.d/`
  ownership once the namespace changes what ssh sees). A mode is still free
  to declare `sandbox` (and `config.yml` `modes:` tuning can add it to a
  built-in), applying to the session's **whole spawn sub-tree** uniformly
  once it does — there is no more
  per-spawn override or ancestor-walk clamp to compute, since every session
  under one mode already gets the identical policy. `BashTool`/`CallTool`
  still hold a `sandbox_resolver: Arc<dyn policy::SandboxResolver>` rather
  than a fixed `SandboxPolicy`, but it now resolves a session's *mode* (via
  `Mode::sandbox_policy`) instead of its agent profile; `ENTANGLEMENT_SANDBOX`/
  `ENTANGLEMENT_SANDBOX_NETWORK` layer on top via `SandboxPolicy::most_confined`
  — env may only *tighten* what the mode declares, never loosen it, since
  `most_confined` always picks the more-confined of the two. The existing
  process-group timeout/cancel kill
  (#167/#168/#169, above in the `bash`/`call` rows) needs no change — killing
  the outer `bwrap` process cascades through its PID-namespace death to the
  whole sandboxed tree.
- **Secret scrubbing (#164):** both exec tools `env_remove` the catalog's
  provider API-key env vars (`Catalog::key_envs()` — `ZAI_API_KEY`,
  `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, …) from the child before spawn, so a
  model-authored `env`/`printenv` can't read the engine's credentials. `call`'s
  no-shell design doesn't help — a plain `env` still inherits them — so the scrub
  covers both. The head wires the set via
  `BashTool::new(root).with_secret_env(catalog.key_envs())` (same for `CallTool`);
  a broader env-allowlist policy remains a possible follow-up to the sandbox
  above ([ADR-0104](../../adr/0104-bubblewrap-sandbox-for-bash-call.md)). `rhai`'s
  `exec`/`bash` bindings (#419) delegate to these same scrubbed tool instances
  via `tools.execute()` (the bridge holds a registry snapshot, not its own
  spawn path), so the scrub applies to script-issued exec identically — no
  separate wiring needed.

---

Previous: [§7 — Hygiene gates](hygiene-gates.md) · Next: [§8 — Host tools: output, wiring, orchestration & `poll`](host-tools-contracts.md)
