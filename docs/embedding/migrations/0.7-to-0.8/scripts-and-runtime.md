# Migration notes — 0.7 → 0.8: scripts & runtime (#926–#930)

> Part of the [0.7 → 0.8 migration notes](../0.7-to-0.8.md) of the [embedding guide](../../../embedding.md).

## Importable rhai modules removed (#926)

A `rhai` script is one file again. The run installs no module resolver, so
`import "<name>" as m;` throws, inline and `path` scripts alike — the
pre-ADR-0248 behaviour. Skill `lib.rhai` libraries
(`skill::<skill>::<fn>(…)`) are unchanged. Neither site nor nexial imported
a module. What changes:

- **Removed API:** the `skills::rhai_modules` module (`RhaiModule`,
  `load`, `valid_name`), `skills::RhaiModules`, `skills::module_dirs`, the
  `SkillRegistry::modules` field and `ToolExecutorBuilder::module_dirs`. An
  embedder that wired `.module_dirs(skills::module_dirs(..))` drops the
  call; a `SkillRegistry` struct update drops the field.
- **Removed built-in:** the `json_schema` module (`validate(value,
  schema)`).
- **Files:** `${config_dir}/entanglement/rhai/modules/` and
  `<root>/.entanglement/rhai/modules/` are no longer read. A `*.rhai` there
  is an ordinary file: editing it triggers no definitions reload, and a
  `write`/`edit`/`apply_patch` of it is graded like any other write (the
  floor stays on trusted-folder scripts and skill `SKILL.md`/`lib.rhai`).
- **Model-facing:** the `rhai` tool spec says `import`/`eval` are disabled
  again, and the `rhai` guide's `load_skill` result no longer ends with a
  module listing.
- **Dependency:** the workspace `rhai` requirement is `1` again (it was
  `=1.25.1`, pinned only for the module builder's use of Rhai internals).
- **Instead:** put shared functions in a skill's `lib.rhai` and call them as
  `skill::<skill>::<fn>(…)`, or inline them in the script.

## `UnknownSession::Refuse` removed (#927)

`EngineConfig::unknown_session` is `SpawnBlank` (default) or
`Resume(Arc<dyn SessionLoader>)`. Neither site nor nexial used `Refuse`
(nexial installs `Resume`). What changes:

- **Removed API:** the `UnknownSession::Refuse` variant. A `match` on
  `UnknownSession` drops that arm; its `Debug` no longer prints `"Refuse"`.
- **Behaviour kept:** a `SessionLoader` answering `Ok(None)` still emits
  one `Error` naming the session per queued message (seq `0` for an id this
  engine never ran) and spawns nothing — exactly what `Refuse` did, after
  one load call.
- **Instead:** refuse ids your head does not know before calling
  `Holly::send`, or install a `Resume` loader whose `load` returns
  `Ok(None)` for every id you want refused.

## The custom `IdGen` trait removed (#928)

Ids come from one built-in function, `entanglement_core::mint_id(kind,
lineage)`, with the same ADR-0164 `<kind>-<hex>` format and the same
ADR-0211 tenant-prefix inheritance as the old `DefaultIdGen`. Neither site
nor nexial installed a generator; both mint roots with
`SessionId::new_uuid()`, which is unchanged. What changes:

- **Removed API:** the `IdGen` trait, `DefaultIdGen`, the
  `EngineConfig::id_gen` field, `IdContext` (`child_of`, `successor_of`,
  `with_user`, `inherited_prefix`), `Holly::next_id` and
  `Holly::next_id_in`, and the `entanglement_core::id_gen` module
  (`mint_id` and `IdKind` live at the crate root). An `EngineConfig` struct
  update drops the field.
- **Behaviour kept:** sub-agent children, compaction successors and forks of
  a `{tenant}:` id keep its prefix, so a `tenant_of` filter still holds for
  the whole lineage.
- **Instead:** `holly.next_id(kind)` → `mint_id(kind, None)`;
  `holly.next_id_in(kind, &IdContext::child_of(&parent))` →
  `mint_id(kind, Some(&parent))`. An embedder that needs ids in its own
  scheme mints its root ids itself (`SessionId::new(..)`); ids the engine
  mints always use the built-in format.

## Gemini explicit cache cut, `HttpClient` shared state opt-in (#929)

`UserBudget` was already removed by #918. Of the other two items #929
verified, one is cut and one changes its default:

- **Gemini explicit `cachedContents` cache removed.** `GeminiLlm` no longer
  creates (or deletes) a `cachedContents` resource per session; every
  request inlines `systemInstruction` + `tools` again. Gemini 2.5+ caches
  that stable leading prefix implicitly at the same cached-token discount,
  without the extra create call per tool-set change or the hourly storage
  bill. No public item changes; `Usage.cached_input_tokens` still reports
  implicit cache hits (`cachedContentTokenCount`).
- **`HttpClient::new()` / `with_config()` no longer write to disk.** They
  build the client with `SharedStateLocation::Disabled` instead of
  `SharedStateLocation::from_env()`, so the first request no longer creates
  `${data_dir}/entanglement/endpoints/*.state` under the server user's
  home. Limits are still enforced per process. Site (0.6) builds one
  `HttpClient::new()` per provider row and `with_config` for its MCP pool;
  both now stay off disk. Nexial already passes `Disabled`.
- **Instead:** to keep sharing endpoint limits between several processes
  on one key, build the client with
  `HttpClient::with_shared_state(SharedStateLocation::from_env(), config)`
  (what `skutter` does) or a `SharedStateLocation::Dir(path)` of your own.

## Generic helpers out of `host`, path grading on the tools (#930)

Split prep for moving the host tools into their own crate: the permission
pipeline, the executor, the script gate and the rhai core no longer import
`host`, `file_backup`, `workdir` or the sandbox. No behaviour change.
Nexial uses none of these items; site (0.6) names only
`policy::SandboxConfig::none()`. What changes:

- **Moved (path only):** `host::{MAX_OUTPUT_BYTES, DEFAULT_TAIL,
  truncate_output, truncate_head_tail, tail_lines, bounded_result}` →
  `output::…`; the `host::exec` module → `exec`; the `host::jobs` module and
  `host::JobRegistry` → `jobs` / `jobs::JobRegistry`; `host::HttpTool`
  (`host::http`) → `http_tool::HttpTool`; `policy::{SandboxResolver,
  ModeSandboxResolver, SandboxConfig}` → `host::sandbox::…` (also
  `host::SandboxResolver`, `host::SandboxConfig`);
  `file_backup::with_exec_seq` → `exec_scope::with_exec_seq`;
  `workdir::SessionWorkdirs` → `path_grading::SessionWorkdirs`;
  `host::{resolve_under_root, escaping_path}` →
  `path_grading::{resolve_under_root, escaping_path}`.
- **Removed API:** `Mode::sandbox_policy()` — call
  `host::sandbox::mode_policy(&mode)`; `permission::escape_root_target` —
  the escape-root target is now the tool's own answer (below).
- **New trait methods (defaulted):** `Tool::join_workdir(args, base)` (which
  relative path arguments join onto a session workdir, ADR-0224) and
  `Tool::escape_target(input)` (the path the escape-root gate checks,
  ADR-0109). The host tools implement both; an embedder tool keeps the
  defaults (no path arguments). The pipeline asks the *registered* tool, so
  an embedder tool that reuses a host name (`read`, `bash`, …) no longer
  inherits that host tool's rewrite or escape check by name — implement the
  two methods if it takes a filesystem path. The argument a scoped
  `tool(pattern)` rule matches was still chosen by tool name here; #962
  moved it onto the tool too
  ([crate split](crate-split.md#permission-arguments-come-from-the-tool-962)).
- **Instead:** update the `use` paths above. Site's
  `policy::SandboxConfig::none()` argument to the 0.6
  `spawn_tool_executor_with_policy` has no 0.8 counterpart — no executor
  API takes a `SandboxConfig`, so drop it. Only an embedder that builds a
  `BashTool` resolver through `SandboxConfig::resolver()` keeps it, at
  `host::SandboxConfig`.
