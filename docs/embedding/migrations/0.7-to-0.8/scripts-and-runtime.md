# Migration notes — 0.7 → 0.8: scripts & runtime (#926–#929)

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
