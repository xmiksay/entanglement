# Agents & permissions §3 — Mode tuning, approvals, grants & the permission ceiling

> Part of the [agents & permissions module](../agents-and-permissions.md) of the [architecture overview](../../architecture.md).

- **Tuning — `config.yml` `modes:`** ([ADR-0207](../../adr/archive/0207-permission-modes-replace-agent-borne-authority.md)
  §5): adds rules to one built-in mode in the same grammar, keyed by mode
  name; **no removal syntax** — longest-match already makes a shipped rule
  overridable by adding a longer, more specific one (to stop `bash(wc *)`
  being pre-allowed, add a longer `deny` covering the case, rather than
  deleting the shipped `allow`). Tuning may never set `default` (a loud
  error) and may never **weaken a capability-class `deny`**: because
  longest-match lets a long scoped rule out-rank a short class name,
  `research: allow: ["write(*)"]` would otherwise grant exactly what
  `deny: [write]` forbids. `mode::tune::apply` closes this by resolving each
  tuned rule's named tool to its real `Capability` set (via a live
  `ToolRegistry`-backed resolver — `skutter inspect modes` uses the same
  no-registry static capability fallback) and rejecting
  the rule outright if any of them is class-denied in that mode. The
  `config.yml` `permissions:` ceiling is a `PermissionProfile` re-interpreted
  through `Mode::from_permission_profile` into the identical longest-match
  engine (not its own struct's older last-matching-rule-wins `resolve`), so
  it grades by capability class exactly like a real mode and still clamps
  least-privilege over every mode's own grade.
- **Lag-proof decision delivery (✅ #156, [ADR-0070](../../adr/archive/0070-authoritative-tool-exec-profile-and-fail-closed-fallback.md)):**
  the `Ask` park (and `ask_user`/`propose_plan`/each `rhai` binding) no longer holds
  its own `broadcast` subscription of the inbound fan-out — that per-task subscriber
  could *lag* under burst and silently drop the `Approve`/`Reject`/`AnswerQuestion`
  it waited for, parking the request forever. Instead each registers a `oneshot` in
  a shared `runtime::pending::PendingDecisions` map keyed by `(session, request_id)`
  *before* emitting its request, and a **single light inbound router** (the
  executor's `Stop`/decision router, now the sole inbound consumer for
  decisions) fans each decision to its waiter and unwinds a session's waiters on
  `Stop`.
- **Approval scope + persisted grants (✅ #174, [ADR-0052](../../adr/archive/0052-approval-scope-and-persisted-grants.md)):**
  `InMsg::Approve` carries a `scope: Once | Session | Always` (core enum, default
  `Once`, `skip_serializing_if` so a bare approve is wire-identical to the pre-#174
  shape — older heads omit it). Approval semantics stay runtime-only: a
  `GrantStore` trait object (#311; the library default is
  `policy::InMemoryGrantStore`, `skutter` plugs in its file-backed
  `skutter::state::FileGrantStore` (#966); shared with the per-request
  dispatch tasks) records the wider scopes keyed by an exact
  `(tool, argument)` — the same (root-relative, ✅ #485, ADR-0125) argument
  #173 resolves against, computed once in `dispatch` and threaded into the
  post-approval record rather than re-derived, so the pre-prompt lookup and
  the record provably share one key. **Mode-scoped** since
  [ADR-0207](../../adr/archive/0207-permission-modes-replace-agent-borne-authority.md)
  §8: a grant also carries the mode it was earned in and matches only that
  mode exactly — a grant earned in `build` is inert in `research`, closing the
  old cross-agent leak where a grant earned under a permissive profile
  silently upgraded `Ask → Allow` under a restrictive one (a legacy
  pre-ADR-0207 grant with no mode on its entry loads as `mode: None`, which by
  construction never equals a live call's `Some(mode)`, so it simply stops
  matching rather than being silently re-honored). **After** the full
  resolution (the session's mode grade → config ceiling), a call that lands
  on `Ask` is upgraded to `Allow` when the store already grants it for this
  mode, so the *identical* later call skips the prompt. A grant **only raises
  `Ask` → `Allow`** — it never
  overrides a `Deny` (the ceiling still clamps first), is matched by exact equality
  (no pattern widening), is dropped on `SessionEnded` for `Session` scope, and is
  never inherited by a sub-agent. `Session` lives in memory. In the library
  `Always` does too — the `ToolExecutor::builder` default, so an embedder or
  a test never touches a file (#747, #966). `skutter` persists it to a
  **managed** file `<state dir>/grants.yml` (the managed-state dir,
  `${config_dir}/entanglement` unless `ENTANGLEMENT_STATE_DIR` relocates all
  seven managed files, [ADR-0213](../../adr/archive/0213-prune-env-vars-and-catalog-knobs.md)) — a top-level `grants:` list of `{mode, rule}` entries with
  `tool(arg)` rule keys, merged under the file lock on each new grant and
  re-read on a check whenever its `(mtime, size)` moved, so another
  instance's grant (or a deleted line) counts at once (`FileGrantStore::at` takes an explicit path, or `None` for
  memory only). Like the provider-key env file
  (#220) it sits *beside* `config.yml`, not inside it: `skutter` rewrites it
  freely, so it never clobbers the hand-edited, commented config. A missing/malformed
  file reads empty — fail *closed* (ask again), the safe direction, warned
  once per file version; a write
  failure is logged and the grant kept in memory for this process. The TUI modal offers `y` once / `s` session / `a` always /
  `n` reject / `e` edit-reason / `Esc` interrupt.
- **Per-user permission ceiling + grants (#522, [ADR-0147](../../adr/archive/0147-multi-user-mode-embedder-api.md);
  reframed embedder-side by [ADR-0181](../../adr/archive/0181-userid-leaves-the-runtime-crate.md)/[ADR-0184](../../adr/archive/0184-provider-hosted-multi-user-seams.md)):**
  built entirely on the #311 seams above, no core change. A session carries an
  optional `UserId` (`Session.user`, spawn-time-fixed like `parent` — a
  child/compaction-successor inherits its parent's/predecessor's user rather
  than being re-told). The runtime ships **no per-user module** (`make
  userid` enforces that the crate never names `UserId`): a multi-user
  embedder implements `PermissionResolver`/`GrantStore` directly over its own
  session→user map — it already knows the mapping, having chosen `user` when
  it sent the session's `InMsg::Spawn`. The recipe (documented in
  [`../embedding.md`](../../embedding/approvals-mcp-multi-user.md#7-multi-user-per-user-providers-permissions-tokens) §7, sketched compiling in
  `examples/embedded.rs`): wrap an inner resolver — typically
  `ModeResolver`, so the process-global #172 ceiling still applies first —
  and clamp its result a *second* time by the resolving session's own user's
  ceiling via the same `clamp_to_base` least-privilege composition #172
  itself uses; key `Always`-scope grants by the user (the storage key itself
  is what makes "one user's grant never leaks to another" true; `Session`
  scope stays keyed by `SessionId`, since a session belongs to exactly one
  user already). Reachable only through the embedder library API — `serve`
  stays single-user (ADR-0048).
- **Escape-root access via approval (✅ #escape-root, [ADR-0109](../../adr/archive/0109-escape-root-access-via-approval.md)):**
  root containment (ADR-0054) is no longer absolute. A `read`/`edit`/`write`/`apply_patch`
  path, a `rhai` script `path` or a `bash` `workdir` that resolves **outside** root is detected in
  the executor (the tool's `Tool::escape_target` + `path_grading::escaping_path`) and
  forces an approval prompt even when the profile would `Allow` (a `Deny` floor
  still wins). The approval is recorded in a **separate** store from the
  permission grants above — `runtime::extra_roots::ExtraRootStore`, in memory;
  its `Always` grants reach the embedder's `extra_roots::AlwaysRoots`
  (`skutter`: `state::FileExtraRoots`, the managed
  `<state dir>/extra-roots.yml`, `ENTANGLEMENT_STATE_DIR`, ADR-0213, #967) — keyed by `(tool, resolved-absolute-path)`,
  **per tool** (a `read` grant never unlocks `write`), at `Once` (single-use,
  additionally bound to the approving call's `request_id` so a concurrent
  in-flight call can't spend it, #449,
  [ADR-0120](../../adr/archive/0120-once-scoped-escape-root-grant-bound-to-request-id.md)) /
  `Session` (process-lifetime) / `Always` (persisted) scope. The host tools
  consult it via `resolve_under_root_or_grant` to relax containment for the
  approved path (matched against the symlink-canonicalized target). Reuses the
  `ToolRequest`/`Approve{scope}` wire (no new variant); `glob`/`grep` stay
  strictly root-contained. The store is separate from `grants.yml` because the
  key spaces differ — a permission grant upgrades `Ask→Allow` on a `tool(command)`
  key, an escape grant relaxes *containment* on a `(tool, absolute-path)` key.
- **User config file + permission ceiling (✅ #172, [ADR-0047](../../adr/archive/0047-local-trust-boundary.md)):**
  `skutter`'s general user settings file (`skutter::config`; the library reads
  none, #967 — the ceiling reaches it as a plain `PermissionProfile`), same
  layered loader as everything else — embedded
  default (`skutter/src/config/defaults.yml`) < user
  (`${config_dir}/entanglement/config.yml`, path override `ENTANGLEMENT_CONFIG_FILE`)
  < project (`<root>/.entanglement/config.yml`), deep-merged at the
  `serde_yaml::Value` level (a field override keeps its siblings) with
  `deny_unknown_fields` on the result. It carries the general settings `agent` /
  `provider` / `model` / `verbose` (each a *fallback*: an explicit CLI flag or env
  var still wins — env > config > embedded) and, as its first section,
  `permissions` (tool → `allow | ask | deny`, same shape as agent frontmatter). The
  `permissions` section is a **global ceiling**: the runtime executor clamps every
  resolved grade least-privilege against it
  (`runtime::permission::clamp_to_base`), so a user/repo `bash: ask` forces every
  agent to ask but never *loosens* what an agent restricts. The embedded default is
  allow-all, so an untouched config is a no-op. The ceiling honors argument-scoped
  rule keys too (✅ #173) — `bash(rm *): deny` in the config clamps that command for
  every agent — and capability keys (✅ #418, ADR-0114) exactly like agent
  frontmatter, since both share `agents::permission_from_value`. The
  `permissions` section stays a pure ceiling (it only *tightens*);
  the orthogonal "always allow" grants (✅ #174) that *raise* an `Ask` live in a
  separate managed file, not here (see the approval-scope bullet above). Because
  the **project** layer merges last (trusted, ADR-0047), a repo can also
  *re-loosen* a key the user's own layer set — that stays legal, but the loader
  now warns loudly about it (`config::ceiling_warn`): one `tracing::warn!` per
  `permissions` key the project file sets to a different value than the earlier
  layers resolved to, so a hostile repo's silent `bash: ask → allow` flip is at
  least visible at startup (ADR-0047's mitigation is inspection, not
  restriction). Loaded in the
  runtime only (core has neither `dirs` nor `serde_yaml`). On first run, if the
  user file is missing, the runtime scaffolds a **fully-commented** starter
  template next to it (✅ #219, `config::scaffold_if_missing` writing
  `config/template.yml`) — every setting commented out, so it parses to `Null`,
  is skipped in the merge (`read_layer`), and changes nothing until edited; it
  only exists as a discoverable starting point. Best-effort: a write failure is
  logged, never fatal. **Curated read-only Allow rules (ADR-0195,
  [0195](../../adr/archive/0195-bash-is-the-default-exec-and-curated-read-only-rules.md)):**
  the embedded defaults additionally ship a small set of exact-prefix
  argument-scoped **Allow** rules — `bash(find *)`, `bash(grep *)`,
  `bash(rg *)`, and ls/cat/head/tail/wc — so a read-only
  command stops costing an approval round-trip. They are ordinary
  `tool(pattern)` entries (#173/#418): user/project layers can tighten or
  widen them, and the ceiling still clamps least-privilege over every grade
  (a user `bash: ask` ceiling forces the prompt back over a curated Allow).
  The list is deliberately short and exact-prefix — no `git *` (it writes),
  no `echo *` (redirection writes), no class patterns — and grows only by a
  reviewed embedded-defaults change. Since [ADR-0197](../../adr/archive/0197-compound-bash-commands-grade-per-segment.md)
  these Allow rules grade each top-level segment of a compound command
  independently (`find . | grep x | wc -l` allows with no prompt) instead of
  the whole raw string, closing the over-match a trailing `*` used to open
  (`find . && rm -rf /` no longer rides `bash(find *)`).
- **Managed provider-key env file (✅ #220):** a sibling
  `${config_dir}/entanglement/.env` (path override `ENTANGLEMENT_ENV_FILE`) holds
  the provider API keys outside any repo (`skutter/src/config/env_file.rs`).
  Startup scaffolds a **commented** template listing the catalog's known key vars
  (`catalog.key_envs()` — `ZAI_API_KEY` / `OPENAI_API_KEY` / `ANTHROPIC_API_KEY`, …)
  when the file is missing, then loads its `KEY=VALUE` lines into the process
  environment **only for vars the real environment left unset** — the process env
  always wins (env > file), matching standard dotenv no-override. Both steps are
  best-effort (a read-only home or a malformed line is logged, never fatal) and run
  right after the catalog loads, before `select_provider` reads any key. The file is
  distinct from `config.yml`: it carries only secrets, so it stays out of the YAML
  config and out of version control.

---

Previous: [§3 — Identity vs authority, starting mode & permission modes](modes.md) · Next: [§3 — Agent definitions, built-in modes, pinning & live reload](definitions.md)
