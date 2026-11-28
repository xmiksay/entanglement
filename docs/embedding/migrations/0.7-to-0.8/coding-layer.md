# Migration notes — 0.7 → 0.8: the coding layer moves to `skutter` (#932, #933)

> Part of the [0.7 → 0.8 migration notes](../0.7-to-0.8.md).

## The rest of the coding layer moves to `skutter` (#932)

The last step of the crate split (after #964, #965, #966, #962, #967, #968).
The library stores no session log, loads no agent or skill file, runs no aux
model and keeps no global MCP roster: those are `skutter`'s now. Site and
nexial use none of the moved items — they keep `session_store::{LogRecord,
LogPayload, pair_records, integrity_gap, resumable_prefix}`, the
`RecordSink` tap, `SkillRegistry::default()`, `McpScope(s)`,
`McpServerConfig`, `McpClient`, `HttpClient` and `McpCapabilityIndex`, all
unchanged.

| item | where it is now | why |
| --- | --- | --- |
| `SessionStore` (JSONL files, listing, resume checks, retention, tree/compaction readers), `read_path`, `safe_cwd_name` | `skutter::session_store` | on-disk storage is the head's; an embedder brings a `RecordSink` |
| `persistence::{FileSink, spawn_persistence_subscriber}` | `skutter::persistence` | the file sink over that store |
| `history::spawn_history_responder` (`ReplayFrom`) | `skutter::history` | reads the on-disk store |
| `file_backup` (per-write backups, `RevertFiles`) | `skutter::file_backup` | only `skutter`'s host tools write files |
| `aux_llm`, `session_title`, `narrate` | `skutter::{aux_llm, session_title, narrate}` | aux models are a CLI feature; core's `aux_llm_resolver` seam stays |
| `agents` (built-ins, frontmatter parse, layered discovery, inspect reports), `layers`, `frontmatter`, `system_prompt` | `skutter::{agents, layers, system_prompt}` | file-based definitions; the library takes a built `AgentCatalog` |
| `skills::{load_registry_in, resolve_registry_in, SkillResolution, SkillLayer, skill_dirs}`, the built-in `rhai` skill, `lib.rhai` loading from disk | `skutter::skills` | file-based discovery; the library keeps the registry |
| `mcp::{spawn_mcp_responder, mcp_add, mcp_remove, mcp_reconnect, mcp_disconnect_only, ServerConfigs, McpHandles}`, `mcp::{live, responder, oauth_ops, browser}` | `skutter::mcp` | the global roster, live add/remove and the OAuth browser/device flows |
| `LogRecord`, `LogPayload`, `pair_records`, `integrity_gap`, `resumable_prefix` (+ `ResumablePrefix`, `Truncation`) | kept: `entanglement_runtime::session_store` | an embedder resumes from its own rows with them (site, nexial) |
| `RecordSink`, `spawn_persistence_subscriber_with_sink` | kept: `entanglement_runtime::persistence` | the tap every sink inherits |
| `SkillMeta`, `SkillRegistry`, `SessionSkills`, `LoadSkillTool`, `lib_rhai::compile` | kept: `entanglement_runtime::skills` | skills are part of the engine; site uses `SkillRegistry::default()` |
| `ActiveServer(s)`, `mcp_list`, lazy connect, `McpScopes`, `TokenStore` | kept: `entanglement_runtime::mcp` | MCP transport, per-user scopes and the `mcp` envelope stay |
| `/fork` | already core (`InMsg::ForkSession`) + the TUI | the library had no part of it |
| cross-process shared state | already `entanglement-provider` (#929) | the provider owns LLM endpoint state |

- **Removed API:** the modules `agents`, `layers`, `frontmatter`,
  `system_prompt`, `aux_llm`, `session_title`, `narrate`, `history` and
  `file_backup`; `session_store::{SessionStore, read_path, safe_cwd_name,
  SessionMeta, children_of, first_prompt_snippet, resumable_roots, root_of,
  check_for_resume, resume_with_mode, Fallback, LaunchMode, ResumeCheck,
  revived_with}`; `persistence::{FileSink, spawn_persistence_subscriber}`;
  `skills::{load_registry_in, resolve_registry_in, SkillResolution,
  SkillLayer, skill_dirs}`; `mcp::{live, responder, oauth_ops, browser,
  McpHandles, ServerConfigs, mcp_add, mcp_remove, mcp_reconnect,
  mcp_disconnect_only, spawn_mcp_responder}`. The runtime drops its optional
  `futures` dependency.
- **Moved API:** `system_prompt::SkillDisclosure` → `skills::SkillDisclosure`;
  `mcp::live::{ActiveServer, ActiveServers, mcp_list}` → `mcp::active::…`
  (also re-exported at `mcp::`).
- **New API:** `skills::SKILL_FILE` (the marker `load_skill` leaves out of a
  skill's reference listing); `mcp::{install_server, drop_server,
  connect_client, record_last_seen}` and
  `AvailableMcp::clear_connect_failure` are public, for a head's own connect
  paths; `exec_scope::{ExecScope, current_exec_scope, without_exec_seq}` are
  public, so an embedder's file tools can tie a change to its `ToolExec`;
  `env_date::today_utc` (the date form the `<env>` pins use).
- **Changed API:** the `provider` feature gates only the throttle relay.
  `skutter`'s `PromptContext::load_with(root, scratch: Option<PathBuf>)` and
  `EnvBlock::detect(root, scratch)` take the scratch dir instead of a
  `SessionStore`.
- **Changed behaviour (library):** the library reads neither
  `ENTANGLEMENT_PREAMBLE_FILE` nor `ENTANGLEMENT_BRIEF_FILE` (the prompt
  assembly that reads them is `skutter`'s, which covers #975's intent).
  `EngineConfig::file_reverter` stays `None` unless the embedder sets it, so
  `RevertFiles` is a recoverable `Error` (unchanged default). Nothing in the
  library answers `ReplayFrom` or `McpList`/`McpAdd`/`McpRemove`/`McpAuth`
  any more: a head that sends them runs its own responder (the wire variants
  stay in `InMsg`).
- **Changed behaviour (`skutter`):** none — every file, command and wire
  answer works as in 0.7.
- **Instead:** persist through your own `RecordSink`; resume with
  `pair_records`/`integrity_gap`/`resumable_prefix` and `Holly::resume`;
  build agents with `AgentCatalog::insert` and skills with `SkillMeta::new`
  (+ `with_lib`); set `EngineConfig::aux_llm_resolver` for compaction's
  summary model; give MCP credentials through a `TokenStore` and per-user
  servers through `McpScopes`; set `EngineConfig::file_reverter` from your
  own backups if your file tools keep any.

## `rhai` is native; its file and exec bindings move to `skutter` (#933)

The library's script engine binds no file or process function of its own.
The functions a 0.7 script called — `read`, `read_raw`, `glob`, `grep`,
`glob_json`, `grep_json`, `edit`, `write`, `bash` — are now declared by
`skutter`'s host tools, so a `skutter` script calls exactly what it called
before. `http()`, the `mcp::`/`skill::`/host namespaces and the JSON/YAML
converters stay in the library. Site and nexial build with
`default-features = false` and register no `rhai` binding, but `rhai` is now
compiled into their build and reachable (see the library behaviour below).

- **Removed API:** the `rhai` Cargo feature (`default` is
  `["provider", "mcp-http"]`; `--no-default-features` keeps `rhai`);
  `tool_names::BINDING_TOOLS`.
- **New API:** `Tool::script_binding() -> Option<host_ns::ScriptBinding>`
  (default `None`) and `host_ns::{ScriptBinding, ScriptArg}`. A tool that
  returns one is a plain script function under its own name: one overload
  per parameter list, each positional argument sent under its input key, an
  optional JSON reply and an optional budget key (the run's remaining
  seconds). Its `doc` line joins the `rhai` spec. A tool name that is not a
  Rhai function name, or that is `http` or a converter, is skipped with a
  warning. A wrapping tool must
  forward the method (`skutter`'s hook wrapper does).
- **Changed API:** `discover::runtime_owned_specs(&ToolRegistry)` and
  `script::rhai_spec(&ToolRegistry)` take the registry whose bindings the
  `rhai` spec lists.
- **Changed behaviour (library):** a script calling `read(...)`, `bash(...)`
  or any other unbound name gets an unknown-function error, even when a tool
  of that name is registered without a binding. The spec and the
  unknown-name hint list only the functions the run binds. A
  `--no-default-features` build now routes and grades a model-issued `rhai`
  call (graded by your resolver as a Read+Write+Exec tool) and runs it,
  where 0.7 refused it as an unknown tool; the script reaches the `mcp::`
  namespaces, `skill::` libraries and, when the http gate is registered,
  `http()`. `discover::runtime_owned_specs`/`build_surface` now always
  include the `rhai` spec, so an embedder using them advertises `rhai` to
  its model.
- **If you don't want scripting:** filter the `rhai` spec out of the
  surface you advertise and deny `rhai` in your `PermissionResolver`.
- **Changed behaviour (`skutter`):** none — `skutter::host::script_bindings`
  declares the 0.7 set, `bash` with its budget-derived `timeout`. The
  `rhai` spec text the model sees changed once: the host bindings are listed
  alphabetically, the intro names "any check the host adds", and the
  escape-root hint moved from the intro onto the `read`/`read_raw` lines
  (`read` asks outside the project root; `read_raw` stays root-only and
  fails with no card). This busts the prompt cache on the first session
  after the upgrade.
- **Unchanged:** background scripts (`background: true` + `poll`) and
  trusted scripts (`rhai.trusted`, with the write floor on trusted scripts
  and skill files) stay in the library as in 0.7; neither reaches a file or
  a process without a host binding.
- **Instead:** to give scripts a file or command function, implement
  `script_binding` on the tool that does the work; for a per-session
  function set, register a host namespace (ADR-0249).

## Approval cards name the graded target (#934)

- **Changed behaviour (library):** a call whose tool returns a
  `Tool::call_grading` target is now carded under that target, as ADR-0249
  documents: `OutEvent::ToolRequest.tool` (and the pending listings) carry
  the graded name — `<target>` for a model call, `<target> (rhai)` for a
  script's nested call — and the card's reason says ``mode `<m>` grades
  `<target>` Ask``. 0.7 showed the envelope tool (`invoke`, a host
  namespace's backing tool) while grading, grants and `Always` approvals
  already keyed on the target. Request ids, `NestedCall.tool` and a
  rejection's error text still name the envelope tool.
- **Changed behaviour (`skutter`):** a nested `glob_json`/`grep_json` card
  reads `glob (rhai)`/`grep (rhai)`, and an in-root `read_raw` card
  `read (rhai)` — the names they grade and grant under.
- **If you match on card tool names:** match the target name your
  `call_grading` returns, not the envelope tool's.
- **New:** the guard `entanglement-runtime/tests/it/tool_surface_guard.rs`
  and its live variant `tests/live_tool_surface.rs` (`make test-live`, which
  now sources `~/.config/entanglement/live.env` when present).
