# Migration notes — 0.7 → 0.8: the coding layer moves to `skutter` (#932)

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
